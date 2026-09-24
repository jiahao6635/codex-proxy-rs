use gateway_plugin_sdk::call::provider::RequestProfileRefresh;
use serde_json::Value;

#[test]
fn refresh_roundtrips_without_exposing_provider_documents_in_debug() {
    let refresh = RequestProfileRefresh {
        sequence: 2,
        profiles: super::request_profile::descriptor(),
    };
    let encoded = serde_json::to_value(&refresh).unwrap();
    assert_eq!(encoded["sequence"], 2);
    let decoded: RequestProfileRefresh = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, refresh);
    let debug = format!("{refresh:?}");
    assert!(debug.contains("sequence: 2"));
    assert!(!debug.contains("sensitive-shape"));
    assert!(!debug.contains("authorizationShape"));
}

#[test]
fn refresh_rejects_unknown_fields() {
    let mut encoded = serde_json::to_value(RequestProfileRefresh {
        sequence: 2,
        profiles: super::request_profile::descriptor(),
    })
    .unwrap();
    encoded
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), Value::Bool(true));
    assert!(serde_json::from_value::<RequestProfileRefresh>(encoded).is_err());
}
