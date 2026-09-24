//! 宿主维护只复用已声明的 Provider 操作，不接受插件自定义任务名或周期。

use super::management::ProviderOperation;
use gateway_admin::{
    model::{
        AdminError, MutationActor, MutationContext,
        accounts::AccountRecord,
        plugins::instances::{PluginCapabilityBinding, PluginFailurePolicy},
        provider_credentials::{
            PluginAccountSaveReason, PreparedPluginAccountSave, ProviderQuotaRequest,
        },
    },
    ports::{
        plugin_accounts::PluginAccountAccess,
        provider::{ProviderAdmin, ProviderAdminError},
    },
};
use gateway_core::{
    account::{CredentialState, ProviderAccountId},
    task::WorkerKind,
};
use gateway_plugin_sdk::{Capability, Manifest, Stage, call::auth::CredentialOperation};
use std::time::{Duration, SystemTime};

use super::{PluginProvider, administration::invalid, management::ManagementOrigin};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MaintenanceKind {
    Credentials,
    Quota,
    Models,
    RequestProfiles,
}

impl MaintenanceKind {
    pub(crate) const ALL: [Self; 4] = [
        Self::Credentials,
        Self::Quota,
        Self::Models,
        Self::RequestProfiles,
    ];

    pub(crate) const fn worker(self) -> (WorkerKind, &'static str, Duration) {
        match self {
            Self::Credentials => (
                WorkerKind::OAuthRefresh,
                "plugin-credentials",
                Duration::from_secs(30),
            ),
            Self::Quota => (
                WorkerKind::QuotaCatalogHealth,
                "plugin-quota",
                Duration::from_secs(300),
            ),
            Self::Models => (
                WorkerKind::QuotaCatalogHealth,
                "plugin-models",
                Duration::from_secs(300),
            ),
            Self::RequestProfiles => (
                WorkerKind::QuotaCatalogHealth,
                "plugin-request-profiles",
                Duration::from_secs(3600),
            ),
        }
    }
}

impl PluginProvider {
    pub(crate) fn maintenance_enabled(
        &self,
        manifest: &Manifest,
        bindings: &[PluginCapabilityBinding],
    ) -> Result<bool, AdminError> {
        let mut selected = None;
        for binding in bindings {
            if crate::contribution::resolve(manifest, binding)?.capability
                == Capability::Maintenance
                && selected.replace(binding).is_some()
            {
                return Err(AdminError::invalid("同一插件实例的维护贡献项只能绑定一次"));
            }
        }
        let Some(binding) = selected else {
            return Ok(false);
        };
        if binding.stage != "maintenance"
            || binding.failure_policy != PluginFailurePolicy::Reject
            || binding.order != 0
            || !binding.client_key_ids.is_empty()
            || !binding.account_group_ids.is_empty()
            || !binding.models.is_empty()
            || (!binding.provider_ids.is_empty() && binding.provider_ids != [self.kind.as_str()])
            || !manifest
                .contributes
                .get(&Capability::Maintenance)
                .is_some_and(|declaration| declaration.stages == [Stage::Maintenance])
            || !MaintenanceKind::ALL
                .into_iter()
                .any(|kind| self.supports_maintenance(kind))
        {
            return Err(AdminError::invalid(
                "维护绑定需要 maintenance 阶段、宿主管理范围及可用的 Provider 维护操作",
            ));
        }
        Ok(true)
    }

    pub(crate) fn supports_maintenance(&self, kind: MaintenanceKind) -> bool {
        match kind {
            MaintenanceKind::Credentials => self
                .credential_operations
                .contains(&CredentialOperation::Refresh),
            MaintenanceKind::Quota => self.quota_enabled,
            MaintenanceKind::Models => self.catalog.discovers_accounts(),
            MaintenanceKind::RequestProfiles => {
                self.request_profile_refresh_enabled && self.request_profiles.is_some()
            }
        }
    }

    pub(crate) async fn maintain_account(
        &self,
        kind: MaintenanceKind,
        account: AccountRecord,
        accounts: &dyn PluginAccountAccess,
        task_id: &str,
    ) -> Result<(), ProviderAdminError> {
        // 禁用账号不由维护任务自行启用；过期 AT 仍可用 RT 刷新，但不使用它查询额度或目录。
        if !account.enabled
            || !(account.credential_state == CredentialState::Ready
                || (kind == MaintenanceKind::Credentials
                    && account.credential_state == CredentialState::Expired))
            || account.provider_kind != self.kind
        {
            return Ok(());
        }
        let id = ProviderAccountId::new(account.id.clone()).map_err(|_| invalid())?;
        let origin = ManagementOrigin::Maintenance {
            task_id,
            credential_revision: Some(account.credential_revision.get()),
        };
        match kind {
            MaintenanceKind::Credentials => {
                if !account.has_refresh_token {
                    return Ok(());
                }
                let policy = self
                    .ports
                    .runtime_policy()
                    .load_refresh_policy()
                    .await
                    .map_err(|_| super::administration::unavailable())?;
                let now = SystemTime::now();
                let due = account.next_refresh_at.map_or_else(
                    || {
                        account
                            .access_token_expires_at
                            .is_some_and(|expiry| policy.is_refresh_due(expiry.into(), now))
                    },
                    |next| SystemTime::from(next) <= now,
                );
                if !due {
                    return Ok(());
                }
                let authentication_kind = account.authentication_kind.clone();
                let prepared = self.rotate_credentials_from(account, None, origin).await?;
                let (facts, guard) = prepared.into_parts();
                // guard 保持全局刷新容量与账号 revision 租约，直到 Admin 原子提交和审计结束。
                let saved = accounts
                    .save(
                        PreparedPluginAccountSave::Replace {
                            facts,
                            authentication_kind,
                            reason: PluginAccountSaveReason::Maintenance,
                        },
                        &MutationContext {
                            actor: MutationActor::System,
                            request_id: task_id.to_owned(),
                        },
                    )
                    .await;
                guard.finish();
                let saved = saved.map_err(super::management::map_account_save_error)?;
                self.account_facts_changed(std::slice::from_ref(&saved.account_id))
                    .await;
            }
            MaintenanceKind::Quota => {
                self.account_quota_from(
                    ProviderQuotaRequest {
                        account_id: id,
                        refresh: true,
                        rolling_usage: None,
                    },
                    origin,
                )
                .await?;
            }
            MaintenanceKind::Models => {
                self.account_models_from(&id, true, origin).await?;
            }
            MaintenanceKind::RequestProfiles => return Err(invalid()),
        }
        Ok(())
    }

    pub(crate) async fn restore_request_profiles(&self) -> Result<(), AdminError> {
        let Some(profiles) = self
            .request_profiles
            .as_ref()
            .filter(|_| self.request_profile_refresh_enabled)
        else {
            return Ok(());
        };
        let cached = self
            .ports
            .artifact_profiles()
            .read(&self.kind, &self.profile_cache_key())
            .await
            .map_err(|_| AdminError::unavailable("插件版本资料缓存读取失败"))?;
        if let Some(cached) = cached {
            let refresh = serde_json::from_value(serde_json::Value::Object(
                cached.profile().expose_to_provider().clone(),
            ))
            .map_err(|_| AdminError::invalid("插件版本资料缓存无效"))?;
            profiles.apply_refresh(refresh, &self.kind)?;
        }
        Ok(())
    }

    pub(crate) async fn maintain_request_profiles(
        &self,
        task_id: &str,
    ) -> Result<(), ProviderAdminError> {
        use gateway_core::{account::OpaqueProviderData, provider_ports::ProviderArtifactProfile};
        use gateway_plugin_sdk::call::provider::RequestProfileRefresh;
        let profiles = self.request_profiles.as_ref().ok_or_else(invalid)?;
        let call = self.management_call_from(
            ProviderOperation::Profile,
            None,
            None,
            ManagementOrigin::Maintenance {
                task_id,
                credential_revision: None,
            },
        )?;
        let refresh: RequestProfileRefresh = call
            .invoke("provider.request_profiles.refresh", &serde_json::json!({}))
            .await?;
        profiles
            .validate_refresh(&refresh, &self.kind)
            .map_err(|_| call.invalid_result())?;
        let value = serde_json::to_value(&refresh).map_err(|_| invalid())?;
        let object = value.as_object().cloned().ok_or_else(invalid)?;
        let cache = self.ports.artifact_profiles();
        cache
            .replace_if_newer(
                ProviderArtifactProfile::new(
                    self.kind.clone(),
                    self.profile_cache_key(),
                    refresh.sequence,
                    SystemTime::now(),
                    OpaqueProviderData::new(object),
                ),
                Duration::from_secs(30 * 24 * 3600),
            )
            .await
            .map_err(|_| super::administration::unavailable())?;
        // 多宿主竞争时重读胜出的序号；一次请求已冻结的解析结果不受后续发布影响。
        self.restore_request_profiles()
            .await
            .map_err(|_| call.invalid_result())
    }

    fn profile_cache_key(&self) -> String {
        // 制品、实例配置与授权均参与指纹；旧插件轮次不可能覆盖新版本的资料。
        format!("plugin-{}", self.authorization_binding)
    }
}
