use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use gateway_api::admin;
use tower::ServiceExt as _;

use super::super::{AdminTestFixture, AdminTestState};

#[tokio::test]
async fn standalone_credential_rotation_route_is_not_exposed() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    let response = admin::router::<AdminTestState>()
        .with_state(fixture.state())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/admin/accounts/rotate")
                .header(header::COOKIE, "cpr_session=valid-session")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn connection_update_requires_admin_and_validates_before_calling_the_service() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    let input = serde_json::json!({
        "accountId":"acct_api",
        "enabled":true,
        "concurrencyLimit":null,
        "weight":1,
        "groupIds":[],
        "connection":{"baseUrl":"https://api.example.invalid/v1", "transport":"http"}
    });
    for (authenticated, transport, expected) in [
        (false, "http", StatusCode::UNAUTHORIZED),
        (true, "invalid", StatusCode::BAD_REQUEST),
        // 夹具没有凭据 Store，合法输入必须进入服务，不能按普通设置静默保存。
        (true, "http", StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let mut input = input.clone();
        input["connection"]["transport"] = serde_json::json!(transport);
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/admin/accounts/update")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-request-id", "req_connection_update");
        if authenticated {
            request = request.header(header::COOKIE, "cpr_session=valid-session");
        }
        let response = admin::router::<AdminTestState>()
            .with_state(fixture.state())
            .oneshot(request.body(Body::from(input.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
}

#[test]
fn authorization_poll_response_preserves_every_committed_account() {
    use gateway_admin::model::{
        Revision,
        provider_credentials::{
            AuthorizationPollResult, AuthorizedAccount, CredentialBatchMutationResult,
        },
    };
    let data = gateway_api::admin::accounts::AccountAuthorizationPollData::from(
        AuthorizationPollResult::Complete(CredentialBatchMutationResult {
            config_revision: Revision::new(3).unwrap(),
            accounts: ["acct_first", "acct_second"]
                .into_iter()
                .map(|id| AuthorizedAccount {
                    account_id: gateway_core::account::ProviderAccountId::new(id).unwrap(),
                    credential_revision: Some(Revision::new(1).unwrap()),
                })
                .collect(),
        }),
    );
    assert_eq!(
        serde_json::to_value(data).unwrap(),
        serde_json::json!({"status":"complete", "accountIds":["acct_first", "acct_second"]})
    );
}

#[tokio::test]
async fn authorization_poll_requires_admin_validates_input_and_preserves_pending_result() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    for (input, authenticated, expected) in [
        (
            serde_json::json!({"provider":"xai", "flowId":"flow-test"}),
            false,
            StatusCode::UNAUTHORIZED,
        ),
        (
            serde_json::json!({"provider":"xai", "flowId":""}),
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({"provider":"xai", "flowId":"flow-test", "accountId":"acct_forged"}),
            true,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            serde_json::json!({"provider":"xai", "flowId":"flow-test", "callbackUrl":""}),
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({"provider":"xai", "flowId":"flow-test"}),
            true,
            StatusCode::OK,
        ),
    ] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/admin/accounts/oauth/poll")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-request-id", "req_login_poll");
        if authenticated {
            request = request.header(header::COOKIE, "cpr_session=valid-session");
        }
        let response = admin::router::<AdminTestState>()
            .with_state(fixture.state())
            .oneshot(request.body(Body::from(input.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = to_bytes(response.into_body(), 8192).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if expected == StatusCode::OK {
            assert_eq!(
                value["data"],
                serde_json::json!({"status":"pending", "retryAfterMs":1250})
            );
        }
    }
}

#[tokio::test]
async fn personal_info_requires_admin_and_a_valid_account_query() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    for (query, authenticated, expected) in [
        ("?accountId=acct_test", false, StatusCode::UNAUTHORIZED),
        ("", true, StatusCode::BAD_REQUEST),
        ("?accountId=bad", true, StatusCode::BAD_REQUEST),
        (
            "?accountId=acct_test&refresh=true",
            true,
            StatusCode::BAD_REQUEST,
        ),
        // 此夹具未提供账号 Store，合法查询应透传服务不可用，而非绕过查询。
        (
            "?accountId=acct_test",
            true,
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        let mut request = Request::builder()
            .uri(format!("/api/admin/accounts/personal-info{query}"))
            .header("x-request-id", "req_personal_info");
        if authenticated {
            request = request.header(header::COOKIE, "cpr_session=valid-session");
        }
        let response = admin::router::<AdminTestState>()
            .with_state(fixture.state())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{query}");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
}

#[tokio::test]
async fn quota_forecast_requires_admin_and_valid_account_query() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    for (uri, authenticated, expected) in [
        (
            "/api/admin/accounts/quota-forecast?accountId=acct_test",
            false,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "/api/admin/accounts/quota-forecast",
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "/api/admin/accounts/quota-forecast?accountId=bad",
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "/api/admin/accounts/quota-forecast?accountId=acct_test&refresh=true",
            true,
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let mut request = Request::builder()
            .uri(uri)
            .header("x-request-id", "req_forecast");
        if authenticated {
            request = request.header(header::COOKIE, "cpr_session=valid-session");
        }
        let response = admin::router::<AdminTestState>()
            .with_state(fixture.state())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{uri}");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = to_bytes(response.into_body(), 8192).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(value["data"].is_null());
        assert!(value["message"].is_string());
    }
}
