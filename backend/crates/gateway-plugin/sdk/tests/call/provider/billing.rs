use gateway_plugin_sdk::call::provider::{CanonicalEvent, ProviderDescriptor};
use serde_json::json;

#[test]
fn billing_prices_are_strings_and_the_algorithm_is_versioned() {
    let value = json!({
        "id":"example", "models":[],
        "billing":{"rule":"token_v1", "prices":{"model":{
            "standard":{"input":"1.25", "output":"2", "cache_read":"0", "cache_write":"0.5"}
        }}}
    });
    let descriptor: ProviderDescriptor = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(descriptor).unwrap()["billing"],
        value["billing"]
    );
    let mut invalid = value.clone();
    invalid["billing"]["rule"] = json!("token_v2");
    assert!(serde_json::from_value::<ProviderDescriptor>(invalid).is_err());
    let mut invalid = value;
    invalid["billing"]["prices"]["model"]["standard"]["input"] = json!(1.25);
    assert!(serde_json::from_value::<ProviderDescriptor>(invalid).is_err());
}

#[test]
fn billable_usage_does_not_fill_unknown_token_facts() {
    let value = json!({
        "type":"billable_usage", "band":"long_fast",
        "usage":{"input_tokens":10, "output_tokens":20}
    });
    let event: CanonicalEvent = serde_json::from_value(value).unwrap();
    let serialized = serde_json::to_value(event).unwrap();
    assert_eq!(serialized["band"], "long_fast");
    assert_eq!(serialized["usage"]["input_tokens"], 10);
    assert!(serialized["usage"]["cached_tokens"].is_null());
    assert!(serialized["usage"]["cache_write_tokens"].is_null());
}
