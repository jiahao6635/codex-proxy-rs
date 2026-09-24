use gateway_plugin_sdk::call::provider::{
    RequestProfileAttribute, RequestProfileDescriptor, RequestProfileOption,
    RequestProfilePresentation, RequestProfileRelease, RequestProfileReleaseStatus,
    RequestProfileTarget,
};
use serde_json::{Map, Value, json};

fn object(value: Value) -> Map<String, Value> {
    value.as_object().expect("object fixture").clone()
}

pub(super) fn descriptor() -> RequestProfileDescriptor {
    let configuration = object(json!({"channel": "stable"}));
    RequestProfileDescriptor {
        default_configuration: configuration.clone(),
        options: vec![RequestProfileOption {
            id: "stable".to_owned(),
            label: "稳定版".to_owned(),
            description: Some("跟随已核验版本".to_owned()),
            configuration,
            resolved: object(json!({"authorizationShape": "sensitive-shape"})),
            presentation: RequestProfilePresentation {
                product: "Example CLI".to_owned(),
                version: "1.2.3".to_owned(),
                build: Some("123".to_owned()),
                target: RequestProfileTarget {
                    os_type: "linux".to_owned(),
                    os_version: "6.8".to_owned(),
                    arch: "x86_64".to_owned(),
                    terminal: "headless".to_owned(),
                },
                user_agent: "Example CLI/1.2.3".to_owned(),
                attributes: vec![RequestProfileAttribute {
                    label: "通道".to_owned(),
                    value: "stable".to_owned(),
                }],
                verified_at_ms: Some(1_700_000_000_000),
                release: Some(RequestProfileRelease {
                    status: RequestProfileReleaseStatus::Current,
                    checked_at_ms: Some(1_700_000_000_100),
                    latest_version: Some("1.2.3".to_owned()),
                    latest_build: Some("123".to_owned()),
                    published_at_ms: None,
                    minimum_system_version: None,
                    hardware_requirements: None,
                    download_url: None,
                    download_size: None,
                    signature_present: Some(true),
                    error: None,
                }),
            },
        }],
    }
}

#[test]
fn request_profile_descriptor_roundtrips_without_exposing_provider_documents_in_debug() {
    let descriptor = descriptor();
    let encoded = serde_json::to_value(&descriptor).expect("encode descriptor");
    let decoded: RequestProfileDescriptor =
        serde_json::from_value(encoded).expect("decode descriptor");
    assert_eq!(decoded, descriptor);

    let debug = format!("{descriptor:?}");
    assert!(!debug.contains("sensitive-shape"));
    assert!(debug.contains("[REDACTED]"));
}

#[test]
fn request_profile_descriptor_rejects_unknown_fields() {
    let mut encoded = serde_json::to_value(descriptor()).expect("encode descriptor");
    encoded
        .as_object_mut()
        .expect("descriptor object")
        .insert("unexpected".to_owned(), Value::Bool(true));
    assert!(serde_json::from_value::<RequestProfileDescriptor>(encoded).is_err());
}
