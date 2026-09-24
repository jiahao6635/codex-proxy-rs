use std::time::Duration;

use gateway_core::{
    error::{ClientVisibleUpstreamResponse, ProviderError, ProviderErrorKind},
    event::ProviderResponseObservation,
    upstream::{OpaqueUpstreamValue, UpstreamSendState},
};
use gateway_plugin_sdk::call::provider::{ExecutionFailure, ExecutionFailureKind};

use super::{super::execution::send_watermark, protocol_error};

pub(super) fn decode(
    failure: ExecutionFailure,
    observation: Option<&ProviderResponseObservation>,
    observed: UpstreamSendState,
) -> Result<ProviderError, ProviderError> {
    let kind = match failure.kind {
        ExecutionFailureKind::InvalidRequest => ProviderErrorKind::InvalidRequest,
        ExecutionFailureKind::Unsupported => ProviderErrorKind::Unsupported,
        ExecutionFailureKind::Authentication => ProviderErrorKind::Unauthorized,
        ExecutionFailureKind::PermissionDenied => ProviderErrorKind::PermissionDenied,
        ExecutionFailureKind::RateLimited => ProviderErrorKind::RateLimited,
        ExecutionFailureKind::QuotaExhausted => ProviderErrorKind::QuotaExhausted,
        ExecutionFailureKind::Unavailable => ProviderErrorKind::Unavailable,
        ExecutionFailureKind::Protocol => ProviderErrorKind::Protocol,
        ExecutionFailureKind::Timeout => ProviderErrorKind::Timeout,
        ExecutionFailureKind::Cancelled => ProviderErrorKind::Cancelled,
    };
    let mut error = ProviderError::new(kind, send_watermark(observed, failure.send_state));
    if let Some(code) = failure.code {
        error = error.with_upstream_code(OpaqueUpstreamValue::new(code));
    }
    if let Some(delay) = failure.retry_after_ms {
        error = error.with_retry_after(Duration::from_millis(delay));
    }
    if let Some(observation) = observation {
        if let Some(status) = observation.status_code() {
            error = error.with_status(status);
        }
        if let Some(id) = observation.request_id() {
            error = error.with_upstream_request_id(id.clone());
        }
    }
    if let Some(response) = failure.response {
        if !(400..=599).contains(&response.status)
            || response.body.len() > 64 * 1024
            || response.content_type.as_ref().is_some_and(|value| {
                value.len() > 256 || value.iter().any(|byte| !(32..127).contains(byte))
            })
        {
            return Err(protocol_error());
        }
        let response = ClientVisibleUpstreamResponse::new(
            response.status,
            response.content_type,
            response.body.into(),
        )
        .with_headers(
            observation
                .map(|value| value.client_headers().to_vec())
                .unwrap_or_default(),
        );
        error = error.with_client_visible_upstream_response(response);
    }
    Ok(error)
}
