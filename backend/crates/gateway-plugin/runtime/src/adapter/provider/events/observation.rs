use gateway_core::{
    error::ProviderError,
    event::{
        ProviderResponseHeader, ProviderResponseObservation, ProviderResponseTimings,
        UpstreamHttpVersion,
    },
    upstream::{OpaqueUpstreamValue, UpstreamTransport},
};
use gateway_plugin_sdk::call::provider::{HttpVersion, ResponseObservation};
use gateway_protocol::openai::response_header_is_forwardable;

use super::protocol_error;

pub(super) fn decode(
    value: ResponseObservation,
    transport: &UpstreamTransport,
    raw_http_body: bool,
) -> Result<ProviderResponseObservation, ProviderError> {
    if value
        .status
        .is_some_and(|status| !(100..=599).contains(&status))
        || value.client_headers.len() > 64
    {
        return Err(protocol_error());
    }
    let mut header_bytes = 0usize;
    let headers = value
        .client_headers
        .into_iter()
        .map(|header| {
            header_bytes = header_bytes
                .saturating_add(header.name.len())
                .saturating_add(header.value.len());
            if header_bytes > 16 * 1024
                || header.name.is_empty()
                || header.name.len() > 256
                || !header
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
                // 原始 HTTP 操作仍由 API owner 重建 Content-Length，并继续拒绝
                // 敏感/逐跳响应头；这里只允许与同一 RawBody 原子交付的媒体类型。
                || (!response_header_is_forwardable(&header.name, &[])
                    && !(raw_http_body && header.name.eq_ignore_ascii_case("content-type")))
                || header
                    .value
                    .iter()
                    .any(|byte| *byte == 127 || (*byte < 32 && *byte != b'\t'))
            {
                return Err(protocol_error());
            }
            Ok(ProviderResponseHeader::new(
                header.name,
                header.value.into(),
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let timings = value.timings;
    let mut result = ProviderResponseObservation::new(transport.clone())
        .with_client_headers(headers)
        .with_timings(ProviderResponseTimings {
            transport_decision_wait_ms: timings.transport_decision_wait_ms,
            connect_ms: timings.connect_ms,
            headers_ms: timings.headers_ms,
            first_event_ms: timings.first_event_ms,
            first_reasoning_ms: timings.first_reasoning_ms,
            first_text_ms: timings.first_text_ms,
            first_token_ms: timings.first_token_ms,
            provider_processing_ms: timings.provider_processing_ms,
        });
    if let Some(status) = value.status {
        result = result.with_status_code(status);
    }
    if let Some(request_id) = value.request_id {
        result = result.with_request_id(OpaqueUpstreamValue::new(request_id));
    }
    if let Some(tier) = value.service_tier {
        result = result
            .try_with_service_tier(tier)
            .map_err(|_| protocol_error())?;
    }
    if let Some(model) = value.response_model {
        result = result.with_upstream_response_model_if_valid(&model);
        if result.upstream_response_model().is_none() {
            return Err(protocol_error());
        }
    }
    if let Some(version) = value.http_version {
        result = result.with_http_version(match version {
            HttpVersion::Http09 => UpstreamHttpVersion::Http09,
            HttpVersion::Http10 => UpstreamHttpVersion::Http10,
            HttpVersion::Http11 => UpstreamHttpVersion::Http11,
            HttpVersion::Http2 => UpstreamHttpVersion::Http2,
            HttpVersion::Http3 => UpstreamHttpVersion::Http3,
        });
    }
    if let Some(forecast) = value.quota_forecast {
        result = result.with_provider_metadata(super::super::forecast::metadata(forecast)?);
    }
    Ok(result)
}
