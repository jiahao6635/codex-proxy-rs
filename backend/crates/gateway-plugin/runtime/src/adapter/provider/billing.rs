use std::collections::BTreeMap;

use gateway_admin::model::{AdminError, pricing::ProviderPricingCatalog};
use gateway_core::{
    metering::{
        CalculatedCost, CalculatedCostAmounts, CalculatedCostBreakdown, CalculatedCostRates,
        CurrencyCode, Decimal, ModelPriceOverride, Money, TokenPriceOverride, Usage,
    },
    routing::UpstreamModelId,
};
use gateway_plugin_sdk::{
    Capability, Manifest, Stage,
    call::provider::billing::{BillingDescriptor, PriceBand},
};

pub(super) fn prepare(
    descriptor: Option<BillingDescriptor>,
    manifest: &Manifest,
) -> Result<Option<ProviderPricingCatalog>, AdminError> {
    let declaration = manifest.contributes.get(&Capability::Billing);
    if declaration.is_some() != descriptor.is_some()
        || declaration.is_some_and(|declaration| !declaration.stages.contains(&Stage::Execution))
    {
        return Err(AdminError::invalid("插件计价数据与能力声明不符"));
    }
    let Some(BillingDescriptor::TokenV1 { prices }) = descriptor else {
        return Ok(None);
    };
    if prices.len() > 4096 {
        return Err(AdminError::invalid("插件价目超过模型数量限制"));
    }
    let mut catalog = BTreeMap::new();
    for (model, bands) in prices {
        UpstreamModelId::new(model.clone())
            .map_err(|_| AdminError::invalid("插件价目模型 ID 无效"))?;
        if !bands.contains_key(&PriceBand::Standard) {
            return Err(AdminError::invalid("插件价目必须包含完整标准档"));
        }
        let bands = bands
            .into_iter()
            .map(|(band, prices)| {
                Ok((
                    band_name(band).to_owned(),
                    TokenPriceOverride {
                        input: prices.input.try_into().map_err(AdminError::invalid)?,
                        output: prices.output.try_into().map_err(AdminError::invalid)?,
                        cache_read: prices.cache_read.try_into().map_err(AdminError::invalid)?,
                        cache_write: prices.cache_write.try_into().map_err(AdminError::invalid)?,
                    },
                ))
            })
            .collect::<Result<_, AdminError>>()?;
        catalog.insert(
            model,
            ModelPriceOverride {
                multiplier_bps: 10_000,
                bands,
            },
        );
    }
    Ok(Some(catalog))
}

/// 一次 attempt 只复制实际发送模型的价格；在途不读取新配置或新插件规则。
pub(super) struct BillingPlan {
    enabled: bool,
    price: Option<ModelPriceOverride>,
}

impl BillingPlan {
    pub(super) fn new(
        defaults: Option<&ProviderPricingCatalog>,
        model: Option<&str>,
        overrides: Option<&ProviderPricingCatalog>,
    ) -> Self {
        let price = model.and_then(|model| {
            let mut price = defaults.and_then(|prices| prices.get(model)).cloned();
            if let Some(custom) = overrides.and_then(|prices| prices.get(model)) {
                let price = price.get_or_insert_with(|| ModelPriceOverride {
                    multiplier_bps: 10_000,
                    bands: BTreeMap::new(),
                });
                price.multiplier_bps = custom.multiplier_bps;
                price.bands.extend(custom.bands.clone());
            }
            price
        });
        Self {
            enabled: defaults.is_some(),
            price,
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn calculate(&self, usage: &Usage, band: PriceBand) -> Option<CalculatedCost> {
        let price = self.price.as_ref()?;
        let long = matches!(
            band,
            PriceBand::LongStandard | PriceBand::LongFast | PriceBand::LongFlex
        );
        let standard = price
            .bands
            .get(if long { "long_standard" } else { "standard" })?;
        let selected = price.bands.get(band_name(band))?;
        // token_v1 明确分摊输入与缓存；缺少数量、不一致或存在单独图像计费时不能补零估算。
        let input = usage.input_tokens?;
        let output = usage.output_tokens?;
        let read = usage.cached_tokens?;
        let write = usage.cache_write_tokens?;
        if usage.image_input_tokens.is_some_and(|count| count > 0)
            || usage.image_output_tokens.is_some_and(|count| count > 0)
        {
            return None;
        }
        let uncached = input.checked_sub(read)?.checked_sub(write)?;
        let amounts = token_amounts(selected, uncached, output, read, write)?;
        let standard = token_amounts(standard, uncached, output, read, write)?.total()?;
        let total = amounts.total()?;
        let multiplier_percent = if standard == 0 {
            100
        } else {
            u32::try_from(
                total
                    .checked_mul(100)?
                    .checked_add(standard / 2)?
                    .checked_div(standard)?,
            )
            .ok()?
        };
        let service_tier = match band {
            PriceBand::Standard | PriceBand::LongStandard => "default",
            PriceBand::Fast | PriceBand::LongFast => "priority",
            PriceBand::Flex | PriceBand::LongFlex => "flex",
        };
        CalculatedCostBreakdown::new(
            CalculatedCostAmounts::new(
                money(amounts.input)?,
                money(amounts.output)?,
                money(amounts.read)?,
                money(amounts.write)?,
                money(standard)?,
                money(total)?,
            ),
            CalculatedCostRates::new(
                money(selected.input.ticks_per_token().checked_mul(1_000_000)?)?,
                money(selected.output.ticks_per_token().checked_mul(1_000_000)?)?,
                money(
                    selected
                        .cache_read
                        .ticks_per_token()
                        .checked_mul(1_000_000)?,
                )?,
                money(
                    selected
                        .cache_write
                        .ticks_per_token()
                        .checked_mul(1_000_000)?,
                )?,
            ),
            Some(service_tier.into()),
            multiplier_percent,
        )
        .with_long_context_billing(long)
        .with_custom_multiplier(price.multiplier_bps)
        .map(|breakdown| breakdown.calculated_cost())
    }
}

struct TokenAmounts {
    input: u128,
    output: u128,
    read: u128,
    write: u128,
}

impl TokenAmounts {
    fn total(&self) -> Option<u128> {
        self.input
            .checked_add(self.output)?
            .checked_add(self.read)?
            .checked_add(self.write)
    }
}

fn token_amounts(
    prices: &TokenPriceOverride,
    input: u64,
    output: u64,
    read: u64,
    write: u64,
) -> Option<TokenAmounts> {
    Some(TokenAmounts {
        input: u128::from(input).checked_mul(prices.input.ticks_per_token())?,
        output: u128::from(output).checked_mul(prices.output.ticks_per_token())?,
        read: u128::from(read).checked_mul(prices.cache_read.ticks_per_token())?,
        write: u128::from(write).checked_mul(prices.cache_write.ticks_per_token())?,
    })
}

fn money(ticks: u128) -> Option<Money> {
    Some(Money::new(
        Decimal::from_scaled(ticks).ok()?,
        CurrencyCode::new("USD").ok()?,
    ))
}

fn band_name(band: PriceBand) -> &'static str {
    match band {
        PriceBand::Standard => "standard",
        PriceBand::Fast => "fast",
        PriceBand::Flex => "flex",
        PriceBand::LongStandard => "long_standard",
        PriceBand::LongFast => "long_fast",
        PriceBand::LongFlex => "long_flex",
    }
}
