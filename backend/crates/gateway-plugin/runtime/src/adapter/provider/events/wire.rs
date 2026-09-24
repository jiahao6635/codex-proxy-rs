use std::collections::BTreeSet;

use gateway_core::{error::ProviderError, event::ProtocolWireEvent};
use gateway_plugin_sdk::call::provider::{WireEvent, WirePayload};
use gateway_protocol::openai::sse::{parse_sse_events, sse_frame_end};

use super::protocol_error;

pub(super) fn decode(
    wire: WireEvent,
    formats: &BTreeSet<String>,
) -> Result<ProtocolWireEvent, ProviderError> {
    if !formats.contains(&wire.protocol) {
        return Err(protocol_error());
    }
    let result = match wire.payload {
        WirePayload::Json {
            event,
            data,
            id,
            retry,
            raw_sse,
        } => {
            if event
                .as_ref()
                .is_some_and(|value| value.contains(['\r', '\n', '\0']))
                || id
                    .as_ref()
                    .is_some_and(|value| value.contains(['\r', '\n', '\0']))
            {
                return Err(protocol_error());
            }
            if let Some(raw) = raw_sse {
                validate_frame(&raw)?;
                let parsed =
                    parse_sse_events(std::str::from_utf8(&raw).map_err(|_| protocol_error())?)
                        .map_err(|_| protocol_error())?;
                let [parsed] = parsed.as_slice() else {
                    return Err(protocol_error());
                };
                let parsed_data: serde_json::Value =
                    serde_json::from_str(&parsed.data).map_err(|_| protocol_error())?;
                if parsed.event != event
                    || parsed.id != id
                    || parsed.retry != retry
                    || parsed_data != data
                {
                    return Err(protocol_error());
                }
                ProtocolWireEvent::json_with_raw_sse_metadata(
                    wire.protocol,
                    event,
                    data,
                    raw.into(),
                    id,
                    retry,
                )
            } else {
                ProtocolWireEvent::json_with_sse_metadata(wire.protocol, event, data, id, retry)
            }
        }
        WirePayload::RawSse { frame } => {
            validate_frame(&frame)?;
            ProtocolWireEvent::raw_sse(wire.protocol, frame.into())
        }
        WirePayload::RawJson { body } => {
            serde_json::from_slice::<serde_json::Value>(&body).map_err(|_| protocol_error())?;
            ProtocolWireEvent::raw_json(wire.protocol, body.into())
        }
        WirePayload::RawBody { body } => {
            ProtocolWireEvent::raw_http_body(wire.protocol, body.into())
        }
    };
    result.map_err(|_| protocol_error())
}

fn validate_frame(frame: &[u8]) -> Result<(), ProviderError> {
    // 复用协议 owner 的边界定义；不允许合帧或把未结束片段交给客户端加工器。
    if frame.is_empty() || sse_frame_end(frame) != Some(frame.len()) {
        return Err(protocol_error());
    }
    Ok(())
}
