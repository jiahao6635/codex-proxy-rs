use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use gateway_admin::model::{
    provider_credentials::{
        ProviderDocument, ProviderQuotaWindow, ProviderQuotaWindowRole, QuotaLocalUsageAttribution,
    },
    quota_forecast_sampling::QuotaForecastObservation,
};
use gateway_core::{error::ProviderError, event::ProviderResponseMetadata};
use gateway_plugin_sdk::call::provider::quota::{QuotaForecast, QuotaWindowRole};

use super::execution::stream_protocol_error;

const METADATA_KEY: &str = "plugin_quota_forecast_v1";

pub(super) fn metadata(value: QuotaForecast) -> Result<ProviderResponseMetadata, ProviderError> {
    validate(&value).ok_or_else(stream_protocol_error)?;
    let json = serde_json::to_string(&serde_json::json!({METADATA_KEY: value}))
        .map_err(|_| stream_protocol_error())?;
    ProviderResponseMetadata::new(json).ok_or_else(stream_protocol_error)
}

/// 历史解释只消费已落库的稳定事实，不调用当前插件或读取当前额度后反推历史。
pub(super) fn observation(
    document: &ProviderDocument,
    window: &ProviderQuotaWindow,
) -> Option<QuotaForecastObservation> {
    if window.local_usage_attribution != QuotaLocalUsageAttribution::AccountWide {
        return None;
    }
    let value: QuotaForecast = serde_json::from_value(
        document
            .expose_to_provider()
            .expose_to_provider()
            .get(METADATA_KEY)?
            .clone(),
    )
    .ok()?;
    validate(&value)?;
    let observed = value.windows.iter().find(|candidate| {
        candidate.account_wide
            && candidate.key == window.key
            && candidate.group == window.group
            && candidate.limit_id == window.limit_id
            && same_role(candidate.role, window.role)
            && Some(candidate.window_seconds) == window.window_seconds
            && candidate.used_percent <= 100.0
    })?;
    Some(QuotaForecastObservation {
        used_percent: observed.used_percent,
        reset_at: DateTime::<Utc>::from_timestamp_millis(observed.reset_at_ms)?,
        plan_type: value.plan_type,
    })
}

fn validate(value: &QuotaForecast) -> Option<()> {
    if value.windows.is_empty()
        || value.windows.len() > 64
        || value
            .plan_type
            .as_deref()
            .is_some_and(|value| !valid_text(value))
    {
        return None;
    }
    let mut keys = BTreeSet::new();
    for window in &value.windows {
        if !keys.insert(&window.key)
            || !valid_text(&window.key)
            || !valid_text(&window.group)
            || window
                .limit_id
                .as_deref()
                .is_some_and(|value| !valid_text(value))
            || window.window_seconds == 0
            || i64::try_from(window.window_seconds).is_err()
            || !window.used_percent.is_finite()
            || window.used_percent < 0.0
            || DateTime::<Utc>::from_timestamp_millis(window.reset_at_ms).is_none()
        {
            return None;
        }
    }
    Some(())
}

fn valid_text(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
}

fn same_role(left: Option<QuotaWindowRole>, right: Option<ProviderQuotaWindowRole>) -> bool {
    matches!(
        (left, right),
        (None, None)
            | (
                Some(QuotaWindowRole::Primary),
                Some(ProviderQuotaWindowRole::Primary)
            )
            | (
                Some(QuotaWindowRole::Secondary),
                Some(ProviderQuotaWindowRole::Secondary)
            )
            | (
                Some(QuotaWindowRole::Monthly),
                Some(ProviderQuotaWindowRole::Monthly)
            )
    )
}
