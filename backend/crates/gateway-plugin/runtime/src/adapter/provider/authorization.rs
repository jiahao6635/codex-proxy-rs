//! 登录只准备凭据事实；临时状态归宿主加密存储，账号提交与审计仍归 Admin。

use std::{sync::Arc, time::Duration};

use super::management::ProviderOperation;
use async_trait::async_trait;
use chrono::Utc;
use gateway_admin::{
    model::{
        AdminError,
        provider_credentials::{
            AuthorizationCommitGuard, AuthorizationMutationTarget, AuthorizationOwner,
            AuthorizationOwnerBinding, AuthorizationStarted, PendingAuthorizationMutation,
            PollAuthorization, PrepareAuthorization, PreparedAuthorizationCommit,
            PreparedAuthorizationCredential, PreparedAuthorizationPoll,
        },
    },
    ports::provider::{ProviderAdminError, ProviderAdminErrorKind},
};
use gateway_core::{
    account::OpaqueProviderData,
    provider_ports::{
        NewOAuthPendingFlow, OAuthPendingBinding, OAuthPendingClaimOutcome,
        OAuthPendingConsumeOutcome, OAuthPendingFlowPort, OAuthPendingPutOutcome,
        OAuthPendingReleaseOutcome,
    },
    routing::ProviderKind,
};
use gateway_plugin_sdk::call::auth::{
    CredentialOperation, LoginPoll, LoginPollResult, LoginStart, LoginStarted,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{
    PluginProvider,
    administration::{invalid, unavailable, unsupported},
    rotation::CredentialChange,
};

const CLAIM_TTL: Duration = Duration::from_secs(120);
const MAX_STATE_BYTES: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredLogin {
    version: u32,
    binding: String,
    expires_at_ms: i64,
    mutation: Map<String, Value>,
    state: Map<String, Value>,
}

impl PluginProvider {
    fn authorize_login(
        &self,
        pending: &PendingAuthorizationMutation,
    ) -> Result<(), ProviderAdminError> {
        if !self
            .credential_operations
            .contains(&CredentialOperation::Login)
        {
            return Err(unsupported());
        }
        if pending.provider_kind() != &self.kind {
            return Err(invalid());
        }
        Ok(())
    }

    pub(super) async fn start_login(
        &self,
        command: PrepareAuthorization,
    ) -> Result<AuthorizationStarted, ProviderAdminError> {
        let PrepareAuthorization { pending, input } = command;
        self.authorize_login(&pending)?;
        self.validate_credential_input(CredentialOperation::Login, &input)?;
        let login_input = input.into_provider_data().into_inner();
        let (input, account) = match pending.target() {
            AuthorizationMutationTarget::Create { name } => (
                LoginStart {
                    name: Some(name.clone()),
                    account_id: None,
                    input: login_input,
                },
                None,
            ),
            AuthorizationMutationTarget::Reauthorize { account_id } => {
                let credential = self.load_management_credential(account_id).await?;
                (
                    LoginStart {
                        name: None,
                        account_id: Some(account_id.as_str().into()),
                        input: login_input,
                    },
                    Some(credential.account),
                )
            }
        };
        let call = self.management_call(
            ProviderOperation::Login,
            account.as_ref(),
            pending.outbound_proxy().cloned(),
        )?;
        let started: LoginStarted = call.invoke("provider.login.start", &input).await?;
        let expires_at = chrono::DateTime::from_timestamp_millis(started.expires_at_ms)
            .ok_or_else(|| call.invalid_result())?;
        let ttl = (expires_at - Utc::now())
            .to_std()
            .map_err(|_| call.invalid_result())?;
        let url = url::Url::parse(&started.authorization_url).map_err(|_| call.invalid_result())?;
        if ttl.is_zero()
            || ttl > Duration::from_secs(30 * 60)
            || started.authorization_url.len() > 8192
            || !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || serde_json::to_vec(&started.state)
                .map_err(|_| call.invalid_result())?
                .len()
                > MAX_STATE_BYTES
        {
            return Err(call.invalid_result());
        }
        let flow_id = uuid::Uuid::new_v4().to_string();
        let owner = owner_binding(pending.owner_binding())?;
        let stored = StoredLogin {
            version: 1,
            binding: self.authorization_binding.clone(),
            expires_at_ms: started.expires_at_ms,
            mutation: pending.to_storage_v1(),
            state: started.state,
        };
        let payload = serde_json::from_value(serde_json::to_value(stored).map_err(|_| invalid())?)
            .map_err(|_| invalid())?;
        let flow = NewOAuthPendingFlow::try_new(
            self.kind.clone(),
            binding(&flow_id)?,
            owner,
            ttl,
            OpaqueProviderData::new(payload),
        )
        .map_err(|_| invalid())?;
        if self
            .ports
            .oauth_pending()
            .put_if_absent(flow)
            .await
            .inspect_err(|error| {
                tracing::warn!(provider = self.kind.as_str(), error = ?error, "插件登录状态写入失败");
            })
            .map_err(|_| unavailable())?
            != OAuthPendingPutOutcome::Stored
        {
            tracing::warn!(provider = self.kind.as_str(), "插件登录状态标识冲突");
            return Err(unavailable());
        }
        Ok(AuthorizationStarted {
            flow_id,
            authorization_url: started.authorization_url,
            expires_at,
        })
    }

    pub(super) async fn poll_login(
        &self,
        command: PollAuthorization,
    ) -> Result<PreparedAuthorizationPoll, ProviderAdminError> {
        if !self
            .credential_operations
            .contains(&CredentialOperation::Login)
        {
            return Err(unsupported());
        }
        if command
            .callback_url
            .as_ref()
            .is_some_and(|url| url.len() > 64 * 1024)
        {
            return Err(invalid());
        }
        let claim = Claim {
            store: self.ports.oauth_pending(),
            provider: self.kind.clone(),
            flow: binding(&command.flow_id)?,
            owner: owner_binding(&AuthorizationOwnerBinding::from_context(&command.context))?,
            token: binding(&uuid::Uuid::new_v4().to_string())?,
        };
        let payload = match claim
            .store
            .claim_if_owner(
                &claim.provider,
                &claim.flow,
                &claim.owner,
                &claim.token,
                CLAIM_TTL,
            )
            .await
            .map_err(|_| unavailable())?
        {
            OAuthPendingClaimOutcome::Claimed(payload) => payload,
            OAuthPendingClaimOutcome::NotFound => {
                return Err(ProviderAdminError::new(ProviderAdminErrorKind::NotFound));
            }
            OAuthPendingClaimOutcome::OwnerMismatch => return Err(invalid()),
            OAuthPendingClaimOutcome::InProgress => {
                return Err(ProviderAdminError::new(ProviderAdminErrorKind::Conflict));
            }
        };
        let guard = LoginGuard(Some(claim));
        let result = self.poll_claimed(payload, &command).await;
        match result {
            Ok(PreparedAuthorizationPoll::Complete(prepared)) => {
                Ok(PreparedAuthorizationPoll::Complete(Box::new(
                    prepared.with_authorization_guard(Box::new(guard)),
                )))
            }
            result => {
                // Pending、验证失败和上游错误都显式释放；取消由 Drop 尽力回收，TTL 负责崩溃恢复。
                Box::new(guard).abort().await.map_err(|_| unavailable())?;
                result
            }
        }
    }

    async fn poll_claimed(
        &self,
        payload: OpaqueProviderData,
        command: &PollAuthorization,
    ) -> Result<PreparedAuthorizationPoll, ProviderAdminError> {
        let stored: StoredLogin =
            serde_json::from_value(Value::Object(payload.into_inner())).map_err(|_| invalid())?;
        if stored.version != 1
            || stored.binding != self.authorization_binding
            || stored.expires_at_ms <= Utc::now().timestamp_millis()
        {
            return Err(invalid());
        }
        let pending = PendingAuthorizationMutation::from_storage_v1(Value::Object(stored.mutation))
            .map_err(|_| invalid())?;
        if !pending.owner_binding().matches_context(&command.context) {
            return Err(invalid());
        }
        self.authorize_login(&pending)?;
        let locked = match pending.target() {
            AuthorizationMutationTarget::Create { .. } => None,
            AuthorizationMutationTarget::Reauthorize { account_id } => {
                Some(self.lock_credential(account_id).await?)
            }
        };
        let call = self.management_call(
            ProviderOperation::Login,
            locked.as_ref().map(|(credential, _)| &credential.account),
            pending.outbound_proxy().cloned(),
        )?;
        let reply: LoginPollResult = call
            .invoke(
                "provider.login.poll",
                &LoginPoll {
                    state: stored.state,
                    callback_url: command.callback_url.clone(),
                },
            )
            .await?;
        if stored.expires_at_ms <= Utc::now().timestamp_millis() {
            return Err(call.invalid_result());
        }
        let accounts = match reply {
            LoginPollResult::Pending { retry_after_ms } => {
                if retry_after_ms == 0 || retry_after_ms > 300_000 {
                    return Err(call.invalid_result());
                }
                return Ok(PreparedAuthorizationPoll::Pending {
                    retry_after: Duration::from_millis(retry_after_ms),
                });
            }
            LoginPollResult::Complete { accounts } => accounts,
        };
        let credential = if let Some((current, guard)) = locked {
            if accounts.len() != 1 {
                return Err(call.invalid_result());
            }
            let facts = accounts
                .into_iter()
                .next()
                .ok_or_else(|| call.invalid_result())?;
            PreparedAuthorizationCredential::Reauthorize(Box::new(
                self.prepare_changed_credential(
                    current.account,
                    facts,
                    CredentialChange::Reauthorize,
                    guard,
                )
                .map_err(|_| call.invalid_result())?,
            ))
        } else {
            PreparedAuthorizationCredential::Create(
                self.prepare_created_credentials(accounts, pending.outbound_proxy().cloned())
                    .map_err(|_| call.invalid_result())?,
            )
        };
        Ok(PreparedAuthorizationPoll::Complete(Box::new(
            PreparedAuthorizationCommit::new(pending, credential),
        )))
    }
}

fn binding(value: &str) -> Result<OAuthPendingBinding, ProviderAdminError> {
    OAuthPendingBinding::try_new(value).map_err(|_| invalid())
}

fn owner_binding(
    owner: &AuthorizationOwnerBinding,
) -> Result<OAuthPendingBinding, ProviderAdminError> {
    binding(&match owner.owner() {
        AuthorizationOwner::AdminSession { admin_user_id } => {
            format!("admin_session:{admin_user_id}")
        }
        AuthorizationOwner::AdminApiKey => "admin_api_key".into(),
        AuthorizationOwner::System => "system".into(),
    })
}

struct Claim {
    store: Arc<dyn OAuthPendingFlowPort>,
    provider: ProviderKind,
    flow: OAuthPendingBinding,
    owner: OAuthPendingBinding,
    token: OAuthPendingBinding,
}

impl Claim {
    async fn release(&self) -> Result<(), AdminError> {
        match self
            .store
            .release_claim(&self.provider, &self.flow, &self.owner, &self.token)
            .await
        {
            Ok(OAuthPendingReleaseOutcome::Released | OAuthPendingReleaseOutcome::NotFound) => {
                Ok(())
            }
            _ => Err(AdminError::unavailable("插件登录状态释放失败")),
        }
    }
}

struct LoginGuard(Option<Claim>);

#[async_trait]
impl AuthorizationCommitGuard for LoginGuard {
    async fn commit(mut self: Box<Self>) -> Result<(), AdminError> {
        let claim = self
            .0
            .take()
            .ok_or_else(|| AdminError::internal("插件登录状态缺失"))?;
        match claim
            .store
            .consume_claim(&claim.provider, &claim.flow, &claim.owner, &claim.token)
            .await
        {
            Ok(OAuthPendingConsumeOutcome::Consumed) => Ok(()),
            _ => Err(AdminError::unavailable("插件登录状态结算失败")),
        }
    }

    async fn abort(mut self: Box<Self>) -> Result<(), AdminError> {
        self.0
            .take()
            .ok_or_else(|| AdminError::internal("插件登录状态缺失"))?
            .release()
            .await
    }
}

impl Drop for LoginGuard {
    fn drop(&mut self) {
        if let Some(claim) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = claim.release().await;
            });
        }
    }
}
