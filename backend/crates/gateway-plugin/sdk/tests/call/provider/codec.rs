use gateway_plugin_sdk::{
    SendState,
    call::provider::{
        CanonicalEvent, ExecutionEvent, ExecutionFailure, ExecutionFailureKind, FailureResponse,
        MAX_EXECUTION_PAYLOAD_BYTES, WireEvent, WirePayload,
    },
};
use serde_json::json;

#[test]
fn raw_wire_and_failure_body_are_independent_binary_segments() {
    let wire = b": comment\r\nevent: error\r\ndata: {\"error\":true}\r\n\r\n".to_vec();
    let body = (0..=255).cycle().take(64 * 1024).collect::<Vec<u8>>();
    let event = ExecutionEvent::wire(WireEvent {
        protocol: "openai".into(),
        payload: WirePayload::RawSse {
            frame: wire.clone(),
        },
    })
    .with_failure(ExecutionFailure {
        kind: ExecutionFailureKind::Unavailable,
        send_state: SendState::Sent,
        code: None,
        retry_after_ms: None,
        response: Some(FailureResponse {
            status: 502,
            content_type: None,
            body: body.clone(),
        }),
    });
    let bytes = event.encode().unwrap();
    assert_eq!(&bytes[..4], b"GPE1");
    assert!(
        bytes.len() < body.len() + wire.len() + 512,
        "原始字节不能膨胀为 JSON 数组"
    );
    assert!(bytes.ends_with(&body));
    let decoded = ExecutionEvent::decode(&bytes).unwrap();
    let WirePayload::RawSse { frame } = decoded.wire.unwrap().payload else {
        panic!("SSE expected")
    };
    assert_eq!(frame, wire);
    assert_eq!(decoded.failure.unwrap().response.unwrap().body, body);
}

#[test]
fn malformed_headers_lengths_truncation_and_extra_segments_are_rejected() {
    let bytes = ExecutionEvent::canonical(CanonicalEvent::Started {
        id: "response".into(),
        model: None,
    })
    .encode()
    .unwrap();
    for length in 0..bytes.len() {
        assert!(ExecutionEvent::decode(&bytes[..length]).is_err());
    }
    let mut invalid = bytes.clone();
    invalid.push(0);
    assert!(ExecutionEvent::decode(&invalid).is_err());
    // 即使总长度自洽，元数据没有声明 wire 也不能附加字节。
    invalid[8..12].copy_from_slice(&1u32.to_be_bytes());
    assert!(ExecutionEvent::decode(&invalid).is_err());
    for position in [4, 8, 12] {
        let mut invalid = bytes.clone();
        invalid[position..position + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(ExecutionEvent::decode(&invalid).is_err());
    }
    let mut invalid = bytes;
    invalid[3] = b'2';
    assert!(ExecutionEvent::decode(&invalid).is_err());
}

#[test]
fn metadata_cannot_hide_binary_arrays_or_bypass_the_total_limit() {
    let metadata = serde_json::to_vec(
        &json!({"wire":{"protocol":"openai","payload":{"kind":"raw_json","body":[1,2,3]}}}),
    )
    .unwrap();
    let mut bytes = b"GPE1".to_vec();
    bytes.extend_from_slice(&u32::try_from(metadata.len()).unwrap().to_be_bytes());
    bytes.extend_from_slice(&[0; 8]);
    bytes.extend_from_slice(&metadata);
    assert!(ExecutionEvent::decode(&bytes).is_err());
    let event = ExecutionEvent::wire(WireEvent {
        protocol: "openai".into(),
        payload: WirePayload::RawJson {
            body: vec![b' '; MAX_EXECUTION_PAYLOAD_BYTES],
        },
    });
    assert!(event.encode().is_err());
    assert!(ExecutionEvent::decode(&vec![0; MAX_EXECUTION_PAYLOAD_BYTES + 1]).is_err());
}

#[test]
fn raw_http_body_uses_the_binary_segment_without_json_validation() {
    let body = vec![0, 255, b'\n', b'{'];
    let bytes = ExecutionEvent::wire(WireEvent {
        protocol: "provider-http".into(),
        payload: WirePayload::RawBody { body: body.clone() },
    })
    .encode()
    .unwrap();
    assert!(bytes.ends_with(&body));
    let decoded = ExecutionEvent::decode(&bytes).unwrap();
    let WirePayload::RawBody { body: decoded } = decoded.wire.unwrap().payload else {
        panic!("raw HTTP body expected")
    };
    assert_eq!(decoded, body);
}
