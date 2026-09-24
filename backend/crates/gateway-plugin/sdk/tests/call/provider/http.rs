use gateway_plugin_sdk::call::provider::{
    OperationKind, PrepareExecution, ProviderDescriptor, ProviderHttpEndpoint, ProviderHttpMethod,
    ProviderHttpRequest,
};
use serde_json::json;

#[test]
fn http_contract_defaults_to_closed_and_preserves_binary_headers() {
    let descriptor: ProviderDescriptor = serde_json::from_value(json!({
        "id": "example",
        "models": []
    }))
    .unwrap();
    assert!(descriptor.http_endpoints.is_empty());

    let endpoint = ProviderHttpEndpoint {
        id: "models".into(),
        methods: vec![ProviderHttpMethod::Get],
    };
    let encoded = serde_json::to_value(&endpoint).unwrap();
    assert_eq!(encoded, json!({"id":"models","methods":["get"]}));

    let request: PrepareExecution = serde_json::from_value(json!({
        "protocol": "provider-http",
        "operation": "provider_http",
        "model": null,
        "account_id": "account",
        "credential_revision": 7,
        "credential": {},
        "context": {},
        "request_profile": null,
        "http_request": {
            "endpoint": "models",
            "method": "get",
            "query": "client_version=1",
            "headers": [{"name":"x-opaque","value":[255,0,1]}]
        }
    }))
    .unwrap();
    assert_eq!(request.operation, OperationKind::ProviderHttp);
    let http = request.http_request.unwrap();
    assert_eq!(http.endpoint, "models");
    assert_eq!(http.method, ProviderHttpMethod::Get);
    assert_eq!(http.headers.len(), 1);
    assert_eq!(http.headers[0].name, "x-opaque");
    assert_eq!(http.headers[0].value, vec![255, 0, 1]);
}

#[test]
fn unknown_http_fields_remain_rejected() {
    assert!(
        serde_json::from_value::<ProviderHttpRequest>(json!({
            "endpoint":"models",
            "method":"get",
            "url":"https://example.invalid"
        }))
        .is_err()
    );
}
