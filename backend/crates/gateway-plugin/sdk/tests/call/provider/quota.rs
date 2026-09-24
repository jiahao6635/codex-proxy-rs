use gateway_plugin_sdk::call::provider::{ExecutionEvent, ResponseObservation};
use serde_json::json;

#[test]
fn quota_forecast_round_trips_without_changing_execution_facts() {
    let value = json!({"observation":{"quota_forecast":{
        "plan_type":"service-plan",
        "windows":[{"key":"week","group":"chat","limit_id":"chat","role":"secondary",
            "account_wide":true,"window_seconds":604800,"used_percent":0.0,"reset_at_ms":1800000000000_i64}]
    }}});
    let event: ExecutionEvent = serde_json::from_value(value.clone()).unwrap();
    let decoded = ExecutionEvent::decode(&event.encode().unwrap()).unwrap();
    assert!(decoded.facts.is_empty());
    assert!(decoded.wire.is_none());
    let observation = decoded.observation.unwrap();
    let forecast = observation.quota_forecast.as_ref().unwrap();
    assert_eq!(forecast.windows[0].used_percent, 0.0);
    assert_eq!(
        serde_json::to_value(observation).unwrap()["quota_forecast"],
        value["observation"]["quota_forecast"]
    );
}

#[test]
fn quota_forecast_is_optional_and_rejects_arbitrary_identity_or_raw_documents() {
    let legacy: ResponseObservation = serde_json::from_value(json!({"status":200})).unwrap();
    assert!(legacy.quota_forecast.is_none());
    assert!(
        serde_json::to_value(legacy)
            .unwrap()
            .get("quota_forecast")
            .is_none()
    );
    for field in [
        "account_id",
        "provider_id",
        "credential",
        "provider_data",
        "headers",
    ] {
        let mut value = json!({"plan_type":null,"windows":[]});
        value[field] = json!("private");
        assert!(
            serde_json::from_value::<ResponseObservation>(json!({"quota_forecast":value})).is_err(),
            "{field}"
        );
    }
}
