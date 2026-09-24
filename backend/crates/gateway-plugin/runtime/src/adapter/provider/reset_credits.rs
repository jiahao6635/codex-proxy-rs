use std::collections::BTreeSet;

use super::management::ProviderOperation;
use gateway_admin::{
    model::provider_credentials::{
        ConsumeProviderResetCredit, ProviderResetCredit, ProviderResetCreditResult,
        ProviderResetCredits,
    },
    ports::provider::{ProviderAdminError, ProviderAdminErrorKind},
};
use gateway_core::account::ProviderAccountId;
use gateway_plugin_sdk::{
    call::provider::account::AccountOperation, call::provider::reset_credits as wire,
};

use super::{
    PluginProvider,
    administration::{invalid, unsupported},
    management::account_request,
};

impl PluginProvider {
    pub(super) async fn query_reset_credits(
        &self,
        id: &ProviderAccountId,
    ) -> Result<ProviderResetCredits, ProviderAdminError> {
        if !self
            .account_operations
            .contains(&AccountOperation::ResetCredits)
        {
            return Err(unsupported());
        }
        let credential = self.load_management_credential(id).await?;
        let call = self.management_call(
            ProviderOperation::ResetCredits,
            Some(&credential.account),
            credential.account.outbound_proxy().cloned(),
        )?;
        let reply: wire::Reply<wire::Credits> = call
            .invoke("provider.reset_credits", &account_request(&credential))
            .await?;
        self.ensure_management_identity(&credential.account).await?;
        let result = completed(reply)?;
        if result.credits.len() > 4096 {
            return Err(bad_result());
        }
        let credits = result
            .credits
            .into_iter()
            .map(credit)
            .collect::<Result<Vec<_>, _>>()?;
        let mut ids = BTreeSet::new();
        if credits.iter().any(|credit| !ids.insert(&credit.id)) {
            return Err(bad_result());
        }
        Ok(ProviderResetCredits {
            available_count: result.available_count,
            credits,
        })
    }

    pub(super) async fn redeem_reset_credit(
        &self,
        command: ConsumeProviderResetCredit,
    ) -> Result<ProviderResetCreditResult, ProviderAdminError> {
        if !self
            .account_operations
            .contains(&AccountOperation::ConsumeResetCredit)
        {
            return Err(unsupported());
        }
        if command.redeem_request_id.get_version() != Some(uuid::Version::Random)
            || command
                .credit_id
                .as_ref()
                .is_some_and(|id| !valid_text(id) || id.trim().is_empty())
        {
            return Err(invalid());
        }
        let credential = self.load_management_credential(&command.account_id).await?;
        let call = self.management_call(
            ProviderOperation::ConsumeResetCredit,
            Some(&credential.account),
            credential.account.outbound_proxy().cloned(),
        )?;
        let reply: wire::Reply<wire::ConsumeResult> = call
            .invoke(
                "provider.consume_reset_credit",
                &wire::ConsumeRequest {
                    account: account_request(&credential),
                    credit_id: command.credit_id.clone(),
                    redeem_request_id: command.redeem_request_id.hyphenated().to_string(),
                },
            )
            .await?;
        // 消费后身份变化不能作为安全失败交给前端重新生成幂等键，也不能写回新账号的额度。
        self.ensure_management_identity(&credential.account)
            .await
            .map_err(|_| ambiguous())?;
        let result = completed(reply)?;
        if !valid_text(&result.code) || result.code.trim().is_empty() {
            return Err(ambiguous());
        }
        let credit = result
            .credit
            .map(credit)
            .transpose()
            .map_err(|_| ambiguous())?;
        if command
            .credit_id
            .as_ref()
            .zip(credit.as_ref())
            .is_some_and(|(requested, returned)| requested != &returned.id)
        {
            return Err(ambiguous());
        }
        Ok(ProviderResetCreditResult {
            code: result.code,
            credit,
        })
    }
}

fn completed<T>(reply: wire::Reply<T>) -> Result<T, ProviderAdminError> {
    match reply {
        wire::Reply::Completed(result) => Ok(result),
        wire::Reply::CredentialRefreshRequired => Err(ProviderAdminError::new(
            ProviderAdminErrorKind::CredentialRefreshRequired,
        )),
        wire::Reply::Rejected => Err(bad_result()),
    }
}

fn credit(value: wire::Credit) -> Result<ProviderResetCredit, ProviderAdminError> {
    if value.id.trim().is_empty()
        || !valid_text(&value.id)
        || [&value.status, &value.title, &value.reset_type]
            .into_iter()
            .flatten()
            .any(|value| !valid_text(value))
    {
        return Err(bad_result());
    }
    let expires_at = value
        .expires_at_ms
        .map(|ms| chrono::DateTime::from_timestamp_millis(ms).ok_or_else(bad_result))
        .transpose()?;
    Ok(ProviderResetCredit {
        id: value.id,
        status: value.status,
        title: value.title,
        expires_at,
        reset_type: value.reset_type,
    })
}

fn valid_text(value: &str) -> bool {
    value.len() <= 1024 && !value.chars().any(char::is_control)
}

fn bad_result() -> ProviderAdminError {
    ProviderAdminError::new(ProviderAdminErrorKind::BadGateway)
}
fn ambiguous() -> ProviderAdminError {
    ProviderAdminError::new(ProviderAdminErrorKind::Ambiguous)
}
