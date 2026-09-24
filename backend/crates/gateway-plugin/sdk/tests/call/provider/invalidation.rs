use gateway_plugin_sdk::call::provider::account::AccountInvalidation;
use serde_json::json;

#[test]
fn invalidations_contain_only_account_identifiers() {
    for value in [
        json!({"kind":"unavailable", "account_id":"acct_a"}),
        json!({"kind":"facts_changed", "account_ids":["acct_a", "acct_b"]}),
    ] {
        let event: AccountInvalidation = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(event).unwrap(), value);
        let mut invalid = value;
        invalid["credential"] = json!({"key":"test-only"});
        assert!(serde_json::from_value::<AccountInvalidation>(invalid).is_err());
    }
}
