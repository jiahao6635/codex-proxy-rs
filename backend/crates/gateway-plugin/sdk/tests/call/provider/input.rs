use gateway_plugin_sdk::call::provider::{ExecutionEvent, ExecutionInput, PrepareExecution};
use serde_json::json;

#[test]
fn preparation_round_trips_large_checkpoint_credentials_and_raw_body_outside_control_metadata() {
    let request: PrepareExecution = serde_json::from_value(json!({
        "protocol":"openai","operation":"generate_image","model":"model","account_id":"account","credential_revision":9,
        "credential":{"key":"test-only-credential"},"context":{"trace":"test-context"},
        "provider_session_state":{"large":"x".repeat(65524)}
    })).unwrap();
    let body: Vec<u8> = (0..=255).cycle().take(32768).collect();
    let bytes = ExecutionInput {
        request,
        body: body.clone(),
    }
    .encode()
    .unwrap();
    assert!(bytes.len() > 64 * 1024);
    assert!(bytes.len() < 64 * 1024 + body.len() + 512);
    assert_eq!(&bytes[..4], b"GPQ1");
    assert!(bytes.ends_with(&body));
    let decoded = ExecutionInput::decode(&bytes).unwrap();
    assert_eq!(decoded.body, body);
    assert_eq!(decoded.request.credential["key"], "test-only-credential");
    assert_eq!(
        decoded.request.provider_session_state.unwrap()["large"]
            .as_str()
            .unwrap()
            .len(),
        65524
    );
    assert!(
        ExecutionEvent::decode(&bytes).is_err(),
        "输入和输出不能混用消息类型"
    );
    for length in [0, 4, 11, bytes.len() - 1] {
        assert!(ExecutionInput::decode(&bytes[..length]).is_err());
    }
    let mut invalid = bytes.clone();
    invalid[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(ExecutionInput::decode(&invalid).is_err());
    let mut invalid = bytes;
    invalid.push(0);
    assert!(ExecutionInput::decode(&invalid).is_err());
}
