mod canonical;
mod failure;
mod observation;
mod wire;

use std::collections::BTreeSet;

use gateway_core::{
    error::{ProviderError, ProviderErrorKind},
    event::{GatewayEvent, ProviderEvent},
    upstream::{UpstreamSendState, UpstreamTransport},
};
use gateway_plugin_sdk::call::provider::ExecutionEvent;

use super::{billing::BillingPlan, execution::stream_protocol_error as protocol_error};

pub(super) fn decode(
    bytes: &[u8],
    formats: &BTreeSet<String>,
    billing: &BillingPlan,
    transport: &UpstreamTransport,
    binding: &super::continuation::SessionBinding,
    observed: UpstreamSendState,
) -> Result<ProviderEvent, ProviderError> {
    match decode_payload(bytes, formats, billing, transport, binding, observed)
        .map_err(|_| ProviderError::new(ProviderErrorKind::Protocol, observed))?
    {
        Decoded::Event(event) => Ok(event),
        Decoded::Failure(error) => Err(error),
    }
}

enum Decoded {
    Event(ProviderEvent),
    Failure(ProviderError),
}

fn decode_payload(
    bytes: &[u8],
    formats: &BTreeSet<String>,
    billing: &BillingPlan,
    transport: &UpstreamTransport,
    binding: &super::continuation::SessionBinding,
    observed: UpstreamSendState,
) -> Result<Decoded, ProviderError> {
    let event = ExecutionEvent::decode(bytes).map_err(|_| protocol_error())?;
    if event.facts.len() > 64 || (!event.facts.is_empty() && !formats.contains("canonical")) {
        return Err(protocol_error());
    }
    let mut facts = Vec::with_capacity(event.facts.len());
    for fact in event.facts {
        canonical::append(fact, billing, &mut facts)?;
    }
    let wire = event
        .wire
        .map(|wire| wire::decode(wire, formats))
        .transpose()?;
    let raw_http_body = wire
        .as_ref()
        .is_some_and(|value| value.raw_http_body_bytes().is_some());
    let mut observation = event
        .observation
        .map(|value| observation::decode(value, transport, raw_http_body))
        .transpose()?;
    // 续写检查点必须与终态事实原子交付，Core 才能以实际 response ID 保存账号 pin。
    if event.session_update.is_some() && !matches!(facts.last(), Some(GatewayEvent::Completed(_))) {
        return Err(protocol_error());
    }
    if event.failure.is_some()
        && (event.session_update.is_some()
            || facts
                .iter()
                .any(|fact| matches!(fact, GatewayEvent::Completed(_))))
    {
        return Err(protocol_error());
    }
    let failure = event
        .failure
        .map(|value| failure::decode(value, observation.as_ref(), observed))
        .transpose()?;
    let mut result = match wire {
        Some(wire) => ProviderEvent::canonical_with_wire(facts, wire),
        None => {
            let mut facts = facts.into_iter();
            if let Some(first) = facts.next() {
                facts.fold(ProviderEvent::canonical(first), ProviderEvent::with_fact)
            } else if let Some(observation) = observation.take() {
                ProviderEvent::observation(observation)
            } else if let Some(error) = failure {
                return Ok(Decoded::Failure(error));
            } else {
                return Err(protocol_error());
            }
        }
    };
    if let Some(observation) = observation {
        result.attach_observation(observation);
    }
    if let Some(update) = event.session_update {
        result.attach_session_update(binding.capture(update)?);
    }
    Ok(match failure {
        Some(error) => Decoded::Failure(error.with_atomic_client_events(vec![result])),
        None => Decoded::Event(result),
    })
}
