use super::super::management::ProviderOperation;
use super::super::{
    PluginProvider,
    administration::{invalid, unavailable},
    management::{ManagementOrigin, account_request},
};
use super::{
    QUERY_TIMEOUT,
    cache::{self, CachedModels},
    descriptor,
};
use gateway_admin::{
    model::provider_credentials::{ProviderModel, ProviderModels},
    ports::provider::{ProviderAdminError, ProviderAdminErrorKind},
};
use gateway_core::account::{
    CredentialState, LoadedCredential, ProviderAccount, ProviderAccountId,
};
use gateway_plugin_sdk::call::{auth::CredentialFacts, provider::models::AccountModels};
use std::{sync::Arc, time::Duration};

impl PluginProvider {
    pub(in super::super) async fn account_models(
        &self,
        id: &ProviderAccountId,
        refresh: bool,
    ) -> Result<ProviderModels, ProviderAdminError> {
        self.account_models_from(id, refresh, ManagementOrigin::Interactive)
            .await
    }

    pub(in super::super) async fn account_models_from(
        &self,
        id: &ProviderAccountId,
        refresh: bool,
        origin: ManagementOrigin<'_>,
    ) -> Result<ProviderModels, ProviderAdminError> {
        let account = self
            .ports
            .accounts()
            .get_account(id)
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(|| ProviderAdminError::new(ProviderAdminErrorKind::NotFound))?;
        if !self.catalog_account_allowed(&account) {
            return Err(invalid());
        }
        let (models, observed_at) = if self.catalog.discovers_accounts() {
            let result = self
                .discover_account_models_from(
                    &account,
                    refresh,
                    cache::Publication::Account,
                    origin,
                )
                .await?;
            (
                self.catalog
                    .for_account(&result.catalog)
                    .into_values()
                    .collect::<Vec<_>>(),
                Some(result.observed_at),
            )
        } else {
            (self.catalog.static_models.clone(), None)
        };
        Ok(ProviderModels {
            models: models
                .into_iter()
                .map(|model| {
                    let model = descriptor::compile(&model).map_err(|_| invalid())?;
                    Ok(ProviderModel {
                        id: model.upstream_model().clone(),
                        name: model.upstream_model().as_str().to_owned(),
                    })
                })
                .collect::<Result<_, ProviderAdminError>>()?,
            observed_at,
        })
    }

    pub(super) fn catalog_account_allowed(&self, account: &ProviderAccount) -> bool {
        account.provider() == &self.kind
    }

    pub(super) async fn discover_account_models(
        &self,
        account: &ProviderAccount,
        refresh: bool,
        publication: cache::Publication,
    ) -> Result<Arc<CachedModels>, ProviderAdminError> {
        self.discover_account_models_from(
            account,
            refresh,
            publication,
            ManagementOrigin::Interactive,
        )
        .await
    }

    async fn discover_account_models_from(
        &self,
        account: &ProviderAccount,
        refresh: bool,
        publication: cache::Publication,
        origin: ManagementOrigin<'_>,
    ) -> Result<Arc<CachedModels>, ProviderAdminError> {
        self.ensure_provider(account.provider())?;
        let discovery = self.catalog.discovery.as_ref().ok_or_else(invalid)?;
        if !refresh && let Some(cached) = self.catalog.cache.read(account) {
            return Ok(cached);
        }
        let started = std::time::Instant::now();
        let mut stage = "refresh_lock";
        let query = async {
            let lock = self.catalog.cache.refresh_lock(account)?;
            let guard = lock.lock().await;
            if !refresh && let Some(cached) = self.catalog.cache.read(account) {
                return Ok(cached);
            }
            let account_invalidation_token = self.catalog.cache.account_invalidation_token();
            stage = "credential";
            let credential = self.load_management_credential(account.id()).await?;
            if !cache::same_identity(account, &credential.account) {
                return Err(ProviderAdminError::new(ProviderAdminErrorKind::Conflict));
            }
            let call = self.management_call_from(
                ProviderOperation::Models,
                Some(&credential.account),
                credential.account.outbound_proxy().cloned(),
                origin,
            )?;
            // 单飞锁按凭据版本区分；回调保存新凭据不会阻塞新版本的目录查询。
            stage = "rpc";
            let mut result: AccountModels = call
                .invoke("provider.models", &account_request(&credential))
                .await?;
            stage = "validate";
            descriptor::validate(&result.models).map_err(|_| call.invalid_result())?;
            if self.catalog.for_account(&result).len() > 4096 {
                return Err(call.invalid_result());
            }
            let prepared_facts = result.prepared_account_facts.take();
            if let Some(facts) = prepared_facts
                && !same_credential_facts(&credential, &facts)
            {
                // 写入前释放账号 single-flight；Store revision CAS 仍是写入权威。
                drop(guard);
                let saved = call
                    .save_discovered_facts(&credential.account, facts)
                    .await?;
                self.catalog
                    .invalidate(std::slice::from_ref(&saved.account_id));
                self.invalidate_facts(std::slice::from_ref(&saved.account_id))
                    .await;
                let current = self
                    .ports
                    .accounts()
                    .get_account(account.id())
                    .await
                    .map_err(|_| unavailable())?
                    .ok_or_else(|| ProviderAdminError::new(ProviderAdminErrorKind::Conflict))?;
                if &saved.account_id != current.id()
                    || saved.credential_revision.get() != current.revision().get()
                    || current.provider() != credential.account.provider()
                    || current.authentication_kind() != credential.account.authentication_kind()
                    || current.upstream_user_id() != credential.account.upstream_user_id()
                    || current.upstream_account_id() != credential.account.upstream_account_id()
                    || current.outbound_proxy() != credential.account.outbound_proxy()
                {
                    return Err(ProviderAdminError::new(ProviderAdminErrorKind::Conflict));
                }
                // 本次模型结果由旧凭据查询得到，不能换绑到新 revision 后发布。
                // 失效代次会让 Core 的稳定性重试重新使用已提交的凭据查询。
                return Err(ProviderAdminError::new(ProviderAdminErrorKind::Conflict));
            }
            stage = "recheck_account";
            let current = self
                .ports
                .accounts()
                .get_account(account.id())
                .await
                .map_err(|_| unavailable())?;
            let Some(current) = current else {
                return Err(ProviderAdminError::new(ProviderAdminErrorKind::Conflict));
            };
            if !cache::same_identity(account, &current) {
                return Err(ProviderAdminError::new(ProviderAdminErrorKind::Conflict));
            }
            stage = "publish";
            self.catalog.cache.publish(
                current,
                result,
                Duration::from_secs(u64::from(discovery.cache_ttl_seconds)),
                account_invalidation_token,
                publication,
            )
        };
        let result = tokio::time::timeout(QUERY_TIMEOUT, query).await;
        let timed_out = result.is_err();
        result
            .map_err(|_| unavailable())
            .and_then(|result| result)
            .inspect_err(|error| {
                // 目录允许部分账号失败；在错误被合并前保留失败阶段，不记录模型正文或凭据。
                tracing::warn!(
                    provider = self.kind.as_str(),
                    account_id = account.id().as_str(),
                    stage,
                    timed_out,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    kind = ?error.kind(),
                    "插件账号模型查询失败"
                );
            })
    }
}

fn same_credential_facts(credential: &LoadedCredential, facts: &CredentialFacts) -> bool {
    let account = &credential.account;
    account.credential_state() == CredentialState::Ready
        && account.last_error_reason().is_none()
        && account.last_error_message().is_none()
        && facts.name == account.name()
        && facts.authentication_kind == account.authentication_kind()
        && &facts.material == credential.credential.expose_to_provider()
        && facts.email.as_deref() == account.email()
        && facts.upstream_user_id.as_deref() == account.upstream_user_id()
        && facts.upstream_account_id.as_deref() == account.upstream_account_id()
        && facts.plan_type.as_deref() == account.plan_type()
        && facts.has_refresh_token == account.has_refresh_token()
        && facts.access_token_expires_at_ms == timestamp_millis(account.access_token_expires_at())
        && facts.next_refresh_at_ms == timestamp_millis(account.next_refresh_at())
}

fn timestamp_millis(value: Option<std::time::SystemTime>) -> Option<i64> {
    value.map(|value| chrono::DateTime::<chrono::Utc>::from(value).timestamp_millis())
}
