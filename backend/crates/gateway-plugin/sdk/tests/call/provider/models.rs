use gateway_plugin_sdk::call::provider::{ProviderDescriptor, models::AccountModels};
use serde_json::json;

#[test]
fn native_continuation_is_an_explicit_model_feature() {
    use gateway_plugin_sdk::call::provider::{ModelDescriptor, ModelFeature};

    let descriptor: ModelDescriptor = serde_json::from_value(json!({
        "id":"continuation-model", "operations":["generate"], "features":["native_continuation"]
    }))
    .unwrap();
    assert_eq!(descriptor.features, [ModelFeature::NativeContinuation]);
    assert_eq!(
        serde_json::to_value(&descriptor).unwrap()["features"],
        json!(["native_continuation"])
    );
    let plain: ModelDescriptor = serde_json::from_value(json!({
        "id":"plain-model", "operations":["generate"]
    }))
    .unwrap();
    assert!(plain.features.is_empty());
}

#[test]
fn account_discovery_is_optional_and_cannot_smuggle_account_writes() {
    let static_provider: ProviderDescriptor =
        serde_json::from_value(json!({"id":"example", "models":[]})).unwrap();
    assert!(static_provider.model_discovery.is_none());
    let provider: ProviderDescriptor = serde_json::from_value(json!({"id":"example", "models":[], "model_discovery":{"include_static":false,"cache_ttl_seconds":60}})).unwrap();
    assert!(!provider.model_discovery.unwrap().include_static);
    let mut catalog = json!({"models":[], "exhaustive":true});
    assert!(
        serde_json::from_value::<AccountModels>(catalog.clone())
            .unwrap()
            .exhaustive
    );
    catalog["credential"] = json!({"key":"test-only"});
    assert!(serde_json::from_value::<AccountModels>(catalog).is_err());
}

#[test]
fn continuation_state_compatibility_is_optional_and_closed() {
    let legacy: ProviderDescriptor =
        serde_json::from_value(json!({"id":"example", "models":[]})).unwrap();
    assert!(legacy.continuation_state.is_none());

    let provider: ProviderDescriptor = serde_json::from_value(json!({
        "id":"example",
        "models":[],
        "continuation_state":{
            "format":"example.responses",
            "write_version":2,
            "readable_versions":[1,2]
        }
    }))
    .unwrap();
    let state = provider.continuation_state.unwrap();
    assert_eq!(state.format, "example.responses");
    assert_eq!(state.write_version, 2);
    assert_eq!(state.readable_versions, [1, 2]);

    assert!(
        serde_json::from_value::<ProviderDescriptor>(json!({
            "id":"example",
            "models":[],
            "continuation_state":{
                "format":"example.responses",
                "write_version":1,
                "readable_versions":[1],
                "artifact":"must-not-be-plugin-selected"
            }
        }))
        .is_err()
    );
}

#[test]
fn catalog_completeness_must_be_explicit() {
    assert!(serde_json::from_value::<AccountModels>(json!({"models":[]})).is_err());
    let discovered: AccountModels =
        serde_json::from_value(json!({"models":[],"exhaustive":false})).unwrap();
    assert!(!discovered.exhaustive);
}

#[test]
fn prepared_account_facts_are_optional_and_closed() {
    let legacy: AccountModels =
        serde_json::from_value(json!({"models":[],"exhaustive":true})).unwrap();
    assert!(legacy.prepared_account_facts.is_none());

    let prepared: AccountModels = serde_json::from_value(json!({
        "models":[],
        "exhaustive":true,
        "prepared_account_facts":{
            "name":"refreshed",
            "authentication_kind":"oauth",
            "material":{"access_token":"controlled-test-value"},
            "email":null,
            "upstream_user_id":null,
            "upstream_account_id":null,
            "plan_type":null,
            "access_token_expires_at_ms":null,
            "next_refresh_at_ms":null
        }
    }))
    .unwrap();
    assert_eq!(
        prepared
            .prepared_account_facts
            .as_ref()
            .map(|facts| facts.name.as_str()),
        Some("refreshed")
    );
    assert!(
        serde_json::from_value::<AccountModels>(json!({
            "models":[],
            "exhaustive":true,
            "prepared_account_facts":{
                "name":"refreshed",
                "authentication_kind":"oauth",
                "material":{},
                "email":null,
                "upstream_user_id":null,
                "upstream_account_id":null,
                "plan_type":null,
                "access_token_expires_at_ms":null,
                "next_refresh_at_ms":null,
                "account_id":"forged"
            }
        }))
        .is_err()
    );
}
