//! 声明式 Token 计价；宿主冻结价目并保存计算明细，插件不写账本。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `token_v1` 只适用于费用完全由 Token 构成的响应；工具费等额外费用应上报实际总额。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case", deny_unknown_fields)]
pub enum BillingDescriptor {
    TokenV1 {
        /// 精确上游模型 ID 到各档单价；不得用公开别名或响应模型覆盖实际发送模型。
        prices: BTreeMap<String, BTreeMap<PriceBand, TokenPrices>>,
    },
}

/// 完整单价，单位为 USD / 百万 Token；宿主校验非负、上限及最多四位小数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenPrices {
    pub input: String,
    pub output: String,
    pub cache_read: String,
    pub cache_write: String,
}

/// Provider 确认的实际计费档位，不能由响应文本或宿主当前配置推测。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceBand {
    Standard,
    Fast,
    Flex,
    LongStandard,
    LongFast,
    LongFlex,
}
