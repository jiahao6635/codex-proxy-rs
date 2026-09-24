use std::{
    num::NonZeroU32,
    sync::Arc,
    time::{Duration, SystemTime},
};

use futures::StreamExt as _;
use gateway_admin::{
    model::observability::{CurrencyCost, ProviderBillingInput},
    ports::plugins::PluginPreparation,
};
use gateway_core::{
    account::{AccountSelectionPolicy, RotationStrategy},
    engine::{
        AccountAttemptContext, AttemptContext, ModelRequestId, RequestAttemptContext,
        provider::ProviderRequest,
    },
    event::{GatewayEvent, ProviderEvent},
    lifecycle::CancellationToken,
    metering::PricingOverrides,
    policy::ClientApiKeyId,
    routing::{ProviderKind, PublicModelId, RoutingContext, UpstreamModelId},
};
use serde_json::{Value, json};

use crate::support::environment::{Environment, account_grant, mutation};

fn configuration() -> Value {
    json!({
        "billing":{"rule":"token_v1", "prices":{"plugin-model":{
            "standard":{"input":"1", "output":"2", "cache_read":"0.5", "cache_write":"1.5"}
        }}},
        "execution_events":[
            {"facts":[{"type":"started", "id":"response-billing", "model":"untrusted-response-model"}]},
            {"facts":[{"type":"billable_usage", "band":"standard", "usage":{
                "input_tokens":1000, "output_tokens":500, "cached_tokens":100, "cache_write_tokens":50
            }}]},
            {"facts":[{"type":"completed", "id":"response-billing", "model":"untrusted-response-model", "reason":"stop"}]}
        ]
    })
}

async fn execute(
    runtime: &gateway_plugin_runtime::PluginRuntime,
    core: &gateway_core::CoreBundle,
    pricing: PricingOverrides,
) -> Vec<ProviderEvent> {
    let kind = ProviderKind::new("example").unwrap();
    let snapshot = core.snapshots().acquire().unwrap();
    let admin = runtime
        .admin_registry(core.snapshots())
        .require(&kind)
        .unwrap();
    let operation = admin
        .connection_test_operation(&UpstreamModelId::new("plugin-model").unwrap(), "billing")
        .await
        .unwrap();
    let plan = snapshot
        .plan(
            &PublicModelId::new("plugin-model").unwrap(),
            &operation,
            snapshot.all_account_scope(),
            &RoutingContext::default(),
        )
        .unwrap();
    let registry = runtime
        .provider_registry()
        .for_extensions(snapshot.extensions())
        .unwrap();
    let provider = registry.get(&kind).unwrap();
    let context = AttemptContext::new(
        RequestAttemptContext::new(
            ModelRequestId::new("req_plugin_billing").unwrap(),
            ClientApiKeyId::new("key_plugin_billing").unwrap(),
        )
        .with_pricing(Arc::new(pricing)),
        NonZeroU32::new(1).unwrap(),
        SystemTime::now() + Duration::from_secs(15),
        AccountSelectionPolicy::new(
            RotationStrategy::RoundRobin,
            NonZeroU32::new(1).unwrap(),
            Duration::ZERO,
        ),
        AccountAttemptContext::default().with_account_scope(snapshot.all_account_scope()),
        None,
        CancellationToken::new(),
    );
    Arc::clone(provider)
        .execute(
            ProviderRequest::new(operation, plan.candidates()[0].clone()),
            context,
        )
        .await
        .unwrap()
        .map(|event| event.unwrap())
        .collect()
        .await
}

#[tokio::test]
async fn token_pricing_uses_sent_model_and_frozen_prices_without_changing_usage() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    environment.account(None).await;
    let (runtime, core) = environment
        .provider(configuration(), vec![account_grant("accounts")])
        .await;
    let registry = runtime.admin_registry(core.snapshots());
    let frozen_admin = registry.freeze().unwrap();
    assert_eq!(
        String::from(
            frozen_admin.pricing_catalog().unwrap()["example"]["plugin-model"].bands["standard"]
                .input
        ),
        "1"
    );
    assert!(
        frozen_admin
            .calculated_billing(
                &ProviderKind::new("example").unwrap(),
                &ProviderBillingInput {
                    upstream_model_id: "plugin-model".to_owned(),
                    service_tier: None,
                    input_tokens: Some(1_000),
                    output_tokens: Some(500),
                    cached_tokens: Some(100),
                    cache_write_tokens: Some(50),
                    total: CurrencyCost {
                        currency: "USD".to_owned(),
                        amount: "0.001975".parse().unwrap(),
                    },
                },
            )
            .unwrap()
            .is_none(),
        "current plugin pricing must not reconstruct a request without its persisted snapshot"
    );
    let prices = serde_json::from_value(json!({"example":{"plugin-model":{
        "multiplierBps":20000,
        "bands":{"standard":{"input":"3","output":"4","cacheRead":"0.5","cacheWrite":"1.5"}}
    }}}))
    .unwrap();
    let events = execute(&runtime, &core, prices).await;
    let facts: Vec<_> = events
        .iter()
        .flat_map(ProviderEvent::canonical_facts)
        .collect();
    let usage = facts
        .iter()
        .find_map(|event| match event {
            GatewayEvent::Usage(usage) => Some(usage),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_tokens,
            usage.cache_write_tokens
        ),
        (Some(1000), Some(500), Some(100), Some(50))
    );
    let cost = facts
        .iter()
        .find_map(|event| match event {
            GatewayEvent::CalculatedCost(cost) => Some(cost),
            _ => None,
        })
        .unwrap();
    assert_eq!(cost.total().amount().canonical(), "0.00935");
    let estimate = (*cost).clone().into_estimate();
    let breakdown = estimate.breakdown().unwrap();
    // Core 明细保存应用自定义倍率后的有效单价，原始价目仍在冻结配置中。
    assert_eq!(
        breakdown.input_price_per_million().amount().canonical(),
        "6"
    );
    assert_eq!(breakdown.custom_multiplier_bps(), 20000);

    let store = environment.store.admin_ports().plugins();
    let mut snapshot = store.load_instances().await.unwrap();
    let mut instance = snapshot.instances.remove(0);
    instance.configuration["billing"]["prices"]["plugin-model"]["standard"]["input"] = json!("100");
    store
        .save_instance(instance, snapshot.config_revision, &mutation())
        .await
        .unwrap();
    let revision = store.load_instances().await.unwrap().config_revision;
    core.snapshot_control()
        .publish_committed(gateway_core::routing::ConfigRevision::new(revision.get()).unwrap())
        .await;
    assert_eq!(
        String::from(
            registry.pricing_catalog().unwrap()["example"]["plugin-model"].bands["standard"].input
        ),
        "100"
    );
    assert_eq!(
        String::from(
            frozen_admin.pricing_catalog().unwrap()["example"]["plugin-model"].bands["standard"]
                .input
        ),
        "1"
    );
    assert_eq!(cost.total().amount().canonical(), "0.00935");
    drop(frozen_admin);
    drop(registry);
    drop(store);
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn missing_usage_unknown_bands_and_non_token_charges_keep_cost_unknown() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    environment.account(None).await;
    let mut config = configuration();
    config["execution_events"] = json!([
        {"facts":[{"type":"started","id":"response-unknown","model":"plugin-model"}]},
        {"facts":[{"type":"billable_usage","band":"standard","usage":{"input_tokens":1,"output_tokens":1}}]},
        {"facts":[{"type":"billable_usage","band":"flex","usage":{"input_tokens":1,"output_tokens":1,"cached_tokens":0,"cache_write_tokens":0}}]},
        {"facts":[{"type":"billable_usage","band":"standard","usage":{"input_tokens":1,"output_tokens":1,"cached_tokens":2,"cache_write_tokens":0}}]},
        {"facts":[{"type":"billable_usage","band":"standard","usage":{"input_tokens":1,"output_tokens":1,"cached_tokens":0,"cache_write_tokens":0,"image_input_tokens":1}}]},
        {"facts":[{"type":"completed","id":"response-unknown","model":"plugin-model","reason":"stop"}]}
    ]);
    let (runtime, core) = environment
        .provider(config, vec![account_grant("accounts")])
        .await;
    let events = execute(&runtime, &core, PricingOverrides::new()).await;
    assert_eq!(
        events
            .iter()
            .flat_map(ProviderEvent::canonical_facts)
            .filter(|event| matches!(event, GatewayEvent::Usage(_)))
            .count(),
        4
    );
    assert!(
        !events
            .iter()
            .flat_map(ProviderEvent::canonical_facts)
            .any(|event| matches!(event, GatewayEvent::CalculatedCost(_)))
    );
    drop(core);
    drop(runtime);
    environment.close().await;
}

#[tokio::test]
async fn invalid_price_data_never_replaces_the_published_catalog() {
    let Some(environment) = Environment::create().await else {
        eprintln!("SKIP: plugin integration environment absent");
        return;
    };
    let (runtime, core) = environment
        .provider(configuration(), vec![account_grant("accounts")])
        .await;
    let registry = runtime.admin_registry(core.snapshots());
    let before = registry.pricing_catalog().unwrap();
    let snapshot = environment
        .store
        .admin_ports()
        .plugins()
        .load_instances()
        .await
        .unwrap();
    for invalid in [
        json!("-1"),
        json!("0.00001"),
        json!("1000001"),
        json!("NaN"),
    ] {
        let mut candidate = snapshot.clone();
        candidate.instances[0].configuration["billing"]["prices"]["plugin-model"]["standard"]["input"] =
            invalid;
        assert!(
            PluginPreparation::prepare(runtime.as_ref(), candidate.config_revision, candidate)
                .await
                .is_err()
        );
        assert_eq!(registry.pricing_catalog().unwrap(), before);
    }
    drop(registry);
    drop(core);
    drop(runtime);
    environment.close().await;
}
