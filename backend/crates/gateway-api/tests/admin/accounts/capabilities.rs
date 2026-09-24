use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use gateway_api::admin;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::super::{AdminTestFixture, AdminTestState};

#[tokio::test]
async fn provider_capabilities_require_admin_and_do_not_invent_unsupported_operations() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    for authenticated in [false, true] {
        let mut request = Request::builder()
            .uri("/api/admin/accounts/providers")
            .header("x-request-id", "provider-capabilities-test");
        if authenticated {
            request = request.header(header::COOKIE, "cpr_session=valid-session");
        }
        let response = admin::router::<AdminTestState>()
            .with_state(fixture.state())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if authenticated {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            }
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        if authenticated {
            let body = to_bytes(response.into_body(), 8192).await.unwrap();
            let value: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                value["data"],
                json!([
                    {"provider":"openai", "credentials":{"import":null,"login":null,"refresh":false,"export":false}},
                    {"provider":"xai", "credentials":{"import":null,"login":null,"refresh":false,"export":false}}
                ])
            );
        }
    }
}

#[test]
fn schema_and_completion_mode_are_preserved_in_the_api_contract() {
    use gateway_admin::model::provider_capabilities::{
        AuthorizationCompletion, ProviderCredentialCapabilities, ProviderCredentialDescriptor,
        ProviderLoginCapability,
    };
    let schema = json!({"type":"object", "properties":{"code":{"type":"string", "writeOnly":true}}, "required":["code"]});
    for (completion, expected) in [
        (AuthorizationCompletion::Callback, "callback"),
        (AuthorizationCompletion::Poll, "poll"),
    ] {
        let data = admin::accounts::AccountProviderData::from(ProviderCredentialDescriptor {
            provider: gateway_core::routing::ProviderKind::new("custom").unwrap(),
            capabilities: ProviderCredentialCapabilities {
                login: Some(ProviderLoginCapability {
                    input_schema: schema.clone(),
                    completion,
                }),
                ..Default::default()
            },
        });
        let value = serde_json::to_value(data).unwrap();
        assert_eq!(
            value["credentials"]["login"],
            json!({"inputSchema":schema, "completion":expected})
        );
    }
}

#[test]
fn login_input_is_optional_but_must_be_a_bounded_object() {
    use admin::accounts::StartAccountAuthorizationRequest;
    let base = json!({"provider":"custom", "name":"account"});
    let request: StartAccountAuthorizationRequest = serde_json::from_value(base.clone()).unwrap();
    assert!(request.input.is_empty());
    request.validate().unwrap();
    for invalid in [Value::Null, json!([]), json!("token")] {
        let mut value = base.clone();
        value["input"] = invalid;
        assert!(serde_json::from_value::<StartAccountAuthorizationRequest>(value).is_err());
    }
    let mut value = base;
    value["input"] = json!({"token":"x".repeat(64 * 1024)});
    let request: StartAccountAuthorizationRequest = serde_json::from_value(value).unwrap();
    assert_eq!(request.validate().unwrap_err().field(), "input");
}
