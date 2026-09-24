use gateway_core::{
    error::ProviderError,
    event::{
        ContentItem, ContentKind, FinishReason, GatewayEvent, ReasoningDelta, ResponseMeta,
        TextDelta, ToolCallDelta,
    },
    metering::{Decimal, ProviderReportedCost, Usage},
};
use gateway_plugin_sdk::call::provider::{
    CanonicalEvent, ContentKind as WireContentKind, FinishReason as WireFinishReason,
};

use super::protocol_error;

pub(super) fn append(
    event: CanonicalEvent,
    billing: &super::super::billing::BillingPlan,
    facts: &mut Vec<GatewayEvent>,
) -> Result<(), ProviderError> {
    if let CanonicalEvent::BillableUsage { usage, band } = event {
        if !billing.enabled() {
            return Err(protocol_error());
        }
        let usage = normalized_usage(usage);
        let cost = billing.calculate(&usage, band);
        facts.push(GatewayEvent::Usage(usage));
        if let Some(cost) = cost {
            facts.push(GatewayEvent::CalculatedCost(cost));
        }
        return Ok(());
    }
    let event = match event {
        CanonicalEvent::Started { id, model } => GatewayEvent::Started(metadata(id, model)?),
        CanonicalEvent::Completed { id, model, reason } => {
            GatewayEvent::Completed(metadata(id, model)?.with_finish_reason(match reason {
                WireFinishReason::Stop => FinishReason::Stop,
                WireFinishReason::Length => FinishReason::Length,
                WireFinishReason::ToolCall => FinishReason::ToolCall,
                WireFinishReason::ContentFilter => FinishReason::ContentFilter,
                WireFinishReason::Other => FinishReason::Other,
            }))
        }
        CanonicalEvent::ContentAdded { index, kind } => {
            GatewayEvent::ContentAdded(ContentItem::new(
                index,
                match kind {
                    WireContentKind::Text => ContentKind::Text,
                    WireContentKind::Reasoning => ContentKind::Reasoning,
                    WireContentKind::ToolCall => ContentKind::ToolCall,
                    WireContentKind::Image => ContentKind::Image,
                    WireContentKind::Audio => ContentKind::Audio,
                },
            ))
        }
        CanonicalEvent::TextDelta { index, text } => GatewayEvent::TextDelta(TextDelta {
            content_index: index,
            text,
        }),
        CanonicalEvent::ReasoningDelta { index, text } => {
            GatewayEvent::ReasoningDelta(ReasoningDelta {
                content_index: index,
                text,
            })
        }
        CanonicalEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments,
        } => GatewayEvent::ToolCallDelta(ToolCallDelta {
            content_index: index,
            call_id: id,
            name,
            arguments_delta: arguments,
        }),
        CanonicalEvent::Usage { usage } => GatewayEvent::Usage(normalized_usage(usage)),
        CanonicalEvent::ProviderCost { amount, currency } => {
            if currency != "USD" {
                return Err(protocol_error());
            }
            let amount: Decimal = amount.parse().map_err(|_| protocol_error())?;
            GatewayEvent::ProviderCost(
                ProviderReportedCost::from_usd_ticks(amount.scaled())
                    .map_err(|_| protocol_error())?,
            )
        }
        CanonicalEvent::BillableUsage { .. } => {
            return Err(protocol_error());
        }
    };
    facts.push(event);
    Ok(())
}

fn normalized_usage(usage: gateway_plugin_sdk::call::provider::Usage) -> Usage {
    Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_tokens: usage.cached_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        image_input_tokens: usage.image_input_tokens,
        image_output_tokens: usage.image_output_tokens,
        total_tokens: usage.total_tokens,
    }
}

fn metadata(id: String, model: Option<String>) -> Result<ResponseMeta, ProviderError> {
    if id.is_empty() || id.len() > 512 {
        return Err(protocol_error());
    }
    Ok(match model {
        Some(model) => ResponseMeta::new(id, model),
        None => ResponseMeta::for_provider_endpoint(id),
    })
}
