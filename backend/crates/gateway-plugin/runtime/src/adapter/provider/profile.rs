use std::{collections::BTreeSet, sync::Arc};

use super::management::ProviderOperation;
use chrono::{DateTime, NaiveDate, Utc};
use gateway_admin::{
    model::provider_credentials::{
        ProviderProfileActivityInsights, ProviderProfileAvatar, ProviderProfileAvatarStreamError,
        ProviderProfileDailyUsage, ProviderProfileInvocation, ProviderProfileStatistics,
        ProviderProfileStatisticsSummary, ProviderSubscription,
    },
    ports::provider::{ProviderAdminError, ProviderAdminErrorKind},
};
use gateway_core::account::{LoadedCredential, ProviderAccountId};
use gateway_plugin_sdk::call::provider::account::{self as wire, AccountOperation};

use super::{PluginProvider, administration::unsupported, management::account_request};

const MAX_PROFILE_ITEMS: usize = 4096;
const MAX_PROFILE_TEXT_BYTES: usize = 1024;

impl PluginProvider {
    async fn profile_credential(
        &self,
        id: &ProviderAccountId,
        operation: AccountOperation,
    ) -> Result<LoadedCredential, ProviderAdminError> {
        if !self.account_operations.contains(&operation) {
            return Err(unsupported());
        }
        self.load_management_credential(id).await
    }

    pub(super) async fn account_subscription(
        &self,
        id: &ProviderAccountId,
    ) -> Result<Option<ProviderSubscription>, ProviderAdminError> {
        let credential = self
            .profile_credential(id, AccountOperation::Subscription)
            .await?;
        let call = self.management_call(
            ProviderOperation::Profile,
            Some(&credential.account),
            credential.account.outbound_proxy().cloned(),
        )?;
        let result: Option<wire::Subscription> = call
            .invoke("provider.subscription", &account_request(&credential))
            .await?;
        self.ensure_management_identity(&credential.account).await?;
        result.map(subscription).transpose()
    }

    pub(super) async fn account_profile(
        &self,
        id: &ProviderAccountId,
    ) -> Result<ProviderProfileStatistics, ProviderAdminError> {
        let credential = self
            .profile_credential(id, AccountOperation::Profile)
            .await?;
        let call = self.management_call(
            ProviderOperation::Profile,
            Some(&credential.account),
            credential.account.outbound_proxy().cloned(),
        )?;
        let result: wire::Profile = call
            .invoke("provider.profile", &account_request(&credential))
            .await?;
        self.ensure_management_identity(&credential.account).await?;
        if result.image_url.is_some()
            && !self.account_operations.contains(&AccountOperation::Avatar)
        {
            return Err(invalid_result());
        }
        profile(result)
    }

    pub(super) async fn account_avatar(
        &self,
        id: &ProviderAccountId,
    ) -> Result<ProviderProfileAvatar, ProviderAdminError> {
        let credential = self
            .profile_credential(id, AccountOperation::Avatar)
            .await?;
        let call = self.management_call(
            ProviderOperation::Profile,
            Some(&credential.account),
            credential.account.outbound_proxy().cloned(),
        )?;
        let (metadata, stream): (wire::Avatar, _) = call
            .invoke_stream("provider.avatar", &account_request(&credential))
            .await?;
        for header in [metadata.content_type.as_deref(), metadata.etag.as_deref()]
            .into_iter()
            .flatten()
        {
            if header.len() > MAX_PROFILE_TEXT_BYTES
                || !header.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
            {
                return Err(invalid_result());
            }
        }
        self.ensure_management_identity(&credential.account).await?;
        let length = metadata.content_length;
        let body = futures::stream::try_unfold(
            (stream, Arc::clone(&call.scope), 0_u64),
            move |(mut stream, scope, received)| async move {
                match stream
                    .next()
                    .await
                    .map_err(|_| ProviderProfileAvatarStreamError)?
                {
                    Some(bytes) => {
                        let received = received
                            .checked_add(bytes.len() as u64)
                            .ok_or(ProviderProfileAvatarStreamError)?;
                        if length.is_some_and(|length| received > length) {
                            return Err(ProviderProfileAvatarStreamError);
                        }
                        Ok(Some((bytes.into(), (stream, scope, received))))
                    }
                    None if length.is_none_or(|length| received == length) => Ok(None),
                    None => Err(ProviderProfileAvatarStreamError),
                }
            },
        );
        Ok(ProviderProfileAvatar {
            content_type: metadata.content_type,
            content_length: metadata.content_length,
            etag: metadata.etag,
            body: Box::pin(body),
        })
    }
}

fn subscription(value: wire::Subscription) -> Result<ProviderSubscription, ProviderAdminError> {
    let starts_at = value.starts_at_ms.map(timestamp).transpose()?;
    let expires_at = timestamp(value.expires_at_ms)?;
    if starts_at.is_some_and(|start| start > expires_at) {
        return Err(invalid_result());
    }
    Ok(ProviderSubscription {
        starts_at,
        expires_at,
        will_renew: value.will_renew,
        billing_period: text(value.billing_period)?,
        billing_currency: text(value.billing_currency)?,
        observed_at: timestamp(value.observed_at_ms)?,
    })
}

fn profile(value: wire::Profile) -> Result<ProviderProfileStatistics, ProviderAdminError> {
    let image_url = text(value.image_url)?;
    if let Some(source) = &image_url {
        let url = url::Url::parse(source).map_err(|_| invalid_result())?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(invalid_result());
        }
    }
    let daily_usage = value
        .daily_usage
        .map(|values| {
            if values.len() > MAX_PROFILE_ITEMS {
                return Err(invalid_result());
            }
            let mut dates = BTreeSet::new();
            values
                .into_iter()
                .map(|value| {
                    let date = NaiveDate::parse_from_str(&value.date, "%Y-%m-%d")
                        .map_err(|_| invalid_result())?;
                    if value.date.len() != 10
                        || date.to_string() != value.date
                        || !dates.insert(date)
                    {
                        return Err(invalid_result());
                    }
                    Ok(ProviderProfileDailyUsage {
                        date,
                        tokens: value.tokens,
                    })
                })
                .collect::<Result<Vec<_>, ProviderAdminError>>()
        })
        .transpose()?;
    let insights = value.activity_insights;
    let invocations = insights
        .invocations
        .map(|values| {
            if values.len() > MAX_PROFILE_ITEMS {
                return Err(invalid_result());
            }
            values
                .into_iter()
                .map(|value| {
                    validate_text(&value.invocation_type)?;
                    if value.invocation_type.is_empty() {
                        return Err(invalid_result());
                    }
                    Ok(ProviderProfileInvocation {
                        invocation_type: value.invocation_type,
                        plugin_id: text(value.plugin_id)?,
                        plugin_name: text(value.plugin_name)?,
                        skill_id: text(value.skill_id)?,
                        skill_name: text(value.skill_name)?,
                        usage_count: value.usage_count,
                    })
                })
                .collect::<Result<Vec<_>, ProviderAdminError>>()
        })
        .transpose()?;
    Ok(ProviderProfileStatistics {
        display_name: text(value.display_name)?,
        username: text(value.username)?,
        image_url,
        has_stats_error: value.has_stats_error,
        summary: ProviderProfileStatisticsSummary {
            total_text_tokens: value.summary.total_text_tokens,
            peak_tokens: value.summary.peak_tokens,
            longest_task_duration_ms: value.summary.longest_task_duration_ms,
            current_streak_days: value.summary.current_streak_days,
            longest_streak_days: value.summary.longest_streak_days,
        },
        daily_usage,
        activity_insights: ProviderProfileActivityInsights {
            fast_mode_percent: percent(insights.fast_mode_percent)?,
            reasoning_effort: text(insights.reasoning_effort)?,
            reasoning_effort_percent: percent(insights.reasoning_effort_percent)?,
            skills_explored: insights.skills_explored,
            total_skills_used: insights.total_skills_used,
            total_threads: insights.total_threads,
            invocations,
        },
    })
}

fn text(value: Option<String>) -> Result<Option<String>, ProviderAdminError> {
    if let Some(value) = &value {
        validate_text(value)?;
    }
    Ok(value)
}

fn validate_text(value: &str) -> Result<(), ProviderAdminError> {
    if value.len() > MAX_PROFILE_TEXT_BYTES || value.chars().any(char::is_control) {
        return Err(invalid_result());
    }
    Ok(())
}

fn percent(value: Option<f64>) -> Result<Option<f64>, ProviderAdminError> {
    if value.is_some_and(|value| !value.is_finite() || !(0.0..=100.0).contains(&value)) {
        return Err(invalid_result());
    }
    Ok(value)
}

fn timestamp(value: i64) -> Result<DateTime<Utc>, ProviderAdminError> {
    DateTime::from_timestamp_millis(value).ok_or_else(invalid_result)
}

fn invalid_result() -> ProviderAdminError {
    ProviderAdminError::new(ProviderAdminErrorKind::BadGateway)
}
