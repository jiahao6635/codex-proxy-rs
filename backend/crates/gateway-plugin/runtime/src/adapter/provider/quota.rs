use std::{collections::BTreeSet, time::SystemTime};

use super::management::ProviderOperation;
use chrono::{DateTime, Utc};
use gateway_admin::{
    model::provider_credentials::{
        ProviderDocument, ProviderQuota, ProviderQuotaRequest, ProviderQuotaWindow,
        ProviderQuotaWindowRole, QuotaLocalUsageAttribution,
    },
    ports::provider::{ProviderAdminError, ProviderAdminErrorKind},
};
use gateway_core::account::{
    OpaqueProviderData, QuotaEvidence, QuotaObservation, QuotaState, QuotaWriteOutcome,
};
use gateway_plugin_sdk::call::provider::quota::{self as wire, QuotaAccess};

use super::{
    PluginProvider,
    administration::{invalid, unavailable, unsupported},
    credentials::timestamp,
    management::ManagementOrigin,
};

impl PluginProvider {
    pub(super) async fn account_quota(
        &self,
        request: ProviderQuotaRequest,
    ) -> Result<ProviderQuota, ProviderAdminError> {
        self.account_quota_from(request, ManagementOrigin::Interactive)
            .await
    }

    pub(super) async fn account_quota_from(
        &self,
        request: ProviderQuotaRequest,
        origin: ManagementOrigin<'_>,
    ) -> Result<ProviderQuota, ProviderAdminError> {
        if !self.quota_enabled {
            return Err(unsupported());
        }
        let store = self.ports.accounts();
        let account = store
            .get_account(&request.account_id)
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(|| ProviderAdminError::new(ProviderAdminErrorKind::NotFound))?;
        self.ensure_provider(account.provider())?;
        let observation = if request.refresh {
            let credential = self.load_management_credential(&request.account_id).await?;
            // 按发起时间排序，迟到响应不能覆盖后来发起并已提交的额度观察。
            // 持久化事实采用 PostgreSQL 的微秒精度，首次返回与恢复后的投影保持一致。
            let observed_at = DateTime::from_timestamp_micros(Utc::now().timestamp_micros())
                .ok_or_else(unavailable)?
                .into();
            let call = self.management_call_from(
                ProviderOperation::Quota,
                Some(&credential.account),
                credential.account.outbound_proxy().cloned(),
                origin,
            )?;
            let quota: wire::Quota = call
                .invoke(
                    "provider.quota",
                    &serde_json::json!({
                        "account_id":credential.account.id().as_str(),
                        "credential_revision":credential.account.revision().get(),
                        "credential":credential.credential.expose_to_provider(),
                    }),
                )
                .await?;
            validate(&quota).map_err(|_| call.invalid_result())?;
            let state =
                quota_state(&quota.access, observed_at).map_err(|_| call.invalid_result())?;
            let plan_type = quota.plan_type.clone();
            let value = serde_json::to_value(&quota).map_err(|_| invalid())?;
            let observation = QuotaObservation {
                account_id: request.account_id,
                expected_revision: credential.account.revision(),
                quota: OpaqueProviderData::new(
                    serde_json::json!({"plugin_quota_v1":value})
                        .as_object()
                        .cloned()
                        .ok_or_else(invalid)?,
                ),
                plan_type,
                observed_at,
                state,
            };
            if store
                .compare_and_swap_quota(observation.clone())
                .await
                .map_err(|_| unavailable())?
                != QuotaWriteOutcome::Updated
            {
                return Err(ProviderAdminError::new(ProviderAdminErrorKind::Conflict));
            }
            Some(observation)
        } else {
            store
                .get_quotas(std::slice::from_ref(&request.account_id))
                .await
                .map_err(|_| unavailable())?
                .into_iter()
                .find(|observation| {
                    observation.account_id == request.account_id
                        && observation.expected_revision == account.revision()
                })
        };
        let Some(observation) = observation else {
            return Ok(ProviderQuota {
                plan_type: None,
                observed_at: None,
                refresh_token_expires_at: None,
                windows: vec![],
                limit_reached: false,
                provider_data: None,
            });
        };
        let quota: wire::Quota = serde_json::from_value(
            observation
                .quota
                .expose_to_provider()
                .get("plugin_quota_v1")
                .cloned()
                .ok_or_else(invalid)?,
        )
        .map_err(|_| invalid())?;
        validate(&quota)?;
        present(quota, DateTime::<Utc>::from(observation.observed_at))
    }
}

fn validate(quota: &wire::Quota) -> Result<(), ProviderAdminError> {
    let valid_text = |value: &str| {
        !value.trim().is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
    };
    if quota.windows.len() > 64
        || quota
            .plan_type
            .as_ref()
            .is_some_and(|plan| !valid_text(plan))
    {
        return Err(invalid());
    }
    timestamp(quota.refresh_token_expires_at_ms)?;
    let mut keys = BTreeSet::new();
    for window in &quota.windows {
        if !keys.insert(&window.key)
            || ![&window.key, &window.group, &window.label]
                .into_iter()
                .all(|value| valid_text(value))
            || [&window.limit_id, &window.limit_name]
                .into_iter()
                .flatten()
                .any(|value| !valid_text(value))
            || window.window_seconds == Some(0)
            || window
                .used_percent
                .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            return Err(invalid());
        }
        timestamp(window.reset_at_ms)?;
    }
    quota_state(&quota.access, SystemTime::now())?;
    Ok(())
}

fn quota_state(
    access: &QuotaAccess,
    observed_at: SystemTime,
) -> Result<QuotaState, ProviderAdminError> {
    Ok(match access {
        QuotaAccess::Unknown => QuotaState::observed_unknown(observed_at),
        QuotaAccess::Allowed => QuotaState::allowed(observed_at),
        QuotaAccess::Exhausted {
            evidence,
            reset_at_ms,
        } => QuotaState::exhausted(
            match evidence {
                wire::QuotaEvidence::ProviderDenied => QuotaEvidence::ProviderDenied,
                wire::QuotaEvidence::AccountLimitReached => QuotaEvidence::AccountLimitReached,
                wire::QuotaEvidence::UsageLimitReached => QuotaEvidence::UsageLimitReached,
                wire::QuotaEvidence::PaymentRequired => QuotaEvidence::PaymentRequired,
            },
            observed_at,
            timestamp(*reset_at_ms)?.map(Into::into),
        ),
    })
}

fn present(
    quota: wire::Quota,
    observed_at: DateTime<Utc>,
) -> Result<ProviderQuota, ProviderAdminError> {
    let limit_reached = matches!(quota.access, QuotaAccess::Exhausted { .. })
        || quota.windows.iter().any(|window| window.limit_reached);
    let windows = quota
        .windows
        .into_iter()
        .map(|window| {
            Ok(ProviderQuotaWindow {
                key: window.key,
                group: window.group,
                label: window.label,
                limit_id: window.limit_id,
                limit_name: window.limit_name,
                role: window.role.map(|role| match role {
                    wire::QuotaWindowRole::Primary => ProviderQuotaWindowRole::Primary,
                    wire::QuotaWindowRole::Secondary => ProviderQuotaWindowRole::Secondary,
                    wire::QuotaWindowRole::Monthly => ProviderQuotaWindowRole::Monthly,
                }),
                local_usage_attribution: if window.account_wide {
                    QuotaLocalUsageAttribution::AccountWide
                } else {
                    QuotaLocalUsageAttribution::Unavailable
                },
                window_seconds: window.window_seconds,
                used_percent: window.used_percent,
                reset_at: timestamp(window.reset_at_ms)?,
                limit_reached: window.limit_reached,
                local_usage: None,
                provider_data: window
                    .provider_data
                    .map(|value| ProviderDocument::new(OpaqueProviderData::new(value))),
            })
        })
        .collect::<Result<_, ProviderAdminError>>()?;
    Ok(ProviderQuota {
        plan_type: quota.plan_type,
        observed_at: Some(observed_at),
        refresh_token_expires_at: timestamp(quota.refresh_token_expires_at_ms)?,
        windows,
        limit_reached,
        provider_data: quota
            .provider_data
            .map(|value| ProviderDocument::new(OpaqueProviderData::new(value))),
    })
}
