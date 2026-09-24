use gateway_plugin_sdk::call::provider::{
    CanonicalEvent, ExecutionEvent, FinishReason, ResponseObservation, SessionUpdate, WireEvent,
    WirePayload,
};
use serde_json::json;

#[test]
fn wire_facts_observation_and_checkpoint_share_one_envelope() {
    let raw =
        b"id: upstream\r\nevent: response.completed\r\ndata: {\"result\":true}\r\n\r\n".to_vec();
    let event = ExecutionEvent::wire(WireEvent {
        protocol: "openai".into(),
        payload: WirePayload::Json {
            event: Some("response.completed".into()),
            data: json!({"result":true}),
            id: Some("upstream".into()),
            retry: None,
            raw_sse: Some(raw.clone()),
        },
    })
    .with_fact(CanonicalEvent::Completed {
        id: "upstream-response".into(),
        model: None,
        reason: FinishReason::Stop,
    })
    .with_observation(ResponseObservation {
        status: Some(200),
        ..ResponseObservation::default()
    })
    .with_session_update(SessionUpdate {
        payload: json!({"turn":"opaque"}).as_object().unwrap().clone(),
    });
    let bytes = serde_json::to_vec(&event).unwrap();
    let decoded: ExecutionEvent = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.facts.len(), 1);
    assert_eq!(decoded.observation.unwrap().status, Some(200));
    assert_eq!(decoded.session_update.unwrap().payload["turn"], "opaque");
    let WirePayload::Json { raw_sse, .. } = decoded.wire.unwrap().payload else {
        panic!("JSON wire expected")
    };
    assert_eq!(raw_sse.unwrap(), raw);
}

#[test]
fn unknown_envelope_and_host_identity_fields_are_rejected() {
    for value in [
        json!({"type":"started","id":"old"}),
        json!({"facts":[],"account_id":"forged"}),
        json!({"session_update":{"payload":{},"credential_revision":2}}),
        json!({"observation":{"transport":"forged"}}),
    ] {
        assert!(serde_json::from_value::<ExecutionEvent>(value).is_err());
    }
}

#[test]
fn raw_json_remains_bytes_and_missing_usage_is_not_synthesized() {
    let event = ExecutionEvent::wire(WireEvent {
        protocol: "openai".into(),
        payload: WirePayload::RawJson {
            body: b"{ \"z\": 1, \"a\": 2 }\n".to_vec(),
        },
    });
    let serialized = serde_json::to_value(event).unwrap();
    assert!(serialized.get("facts").is_none());
    assert!(serialized["wire"]["payload"]["body"].is_array());
    let decoded: ExecutionEvent = serde_json::from_value(serialized).unwrap();
    let WirePayload::RawJson { body } = decoded.wire.unwrap().payload else {
        panic!("raw JSON expected")
    };
    assert_eq!(body, b"{ \"z\": 1, \"a\": 2 }\n");
}
