use gateway_plugin_sdk::call::provider::{
    ProviderDescriptor,
    account::{AccountConfiguration, AccountConfigurationDescriptor},
};
use serde_json::json;

#[test]
fn account_configuration_is_optional_and_preserves_only_the_declared_shape() {
    let legacy: ProviderDescriptor =
        serde_json::from_value(json!({"id":"example", "models":[]})).unwrap();
    assert!(legacy.account_configuration.is_none());

    let descriptor = AccountConfigurationDescriptor {
        public_fields: vec!["base_url".into(), "transport".into()],
    };
    assert_eq!(
        serde_json::to_value(&descriptor).unwrap(),
        json!({"public_fields":["base_url", "transport"]})
    );
    assert!(
        serde_json::from_value::<AccountConfigurationDescriptor>(
            json!({"public_fields":[], "unexpected":true})
        )
        .is_err()
    );

    let result: AccountConfiguration = serde_json::from_value(json!({
        "values":{"base_url":"https://example.test", "transport":"http"}
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        json!({"values":{"base_url":"https://example.test", "transport":"http"}})
    );
}
