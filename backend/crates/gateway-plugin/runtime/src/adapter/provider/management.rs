use std::{sync::Arc, time::Duration};

use gateway_admin::{
    model::{AdminError, AdminErrorKind},
    ports::provider::{ProviderAdminError, ProviderAdminErrorKind},
};
use gateway_core::{
    account::{
        CredentialState, LoadedCredential, OutboundProxy, ProviderAccount, ProviderAccountId,
    },
    routing::ProviderKind,
    upstream::UpstreamSendState,
};
use gateway_plugin_sdk::{CallContext, ErrorCode, SendState, Stage};
use serde::{Serialize, de::DeserializeOwned};

use super::{
    PluginProvider,
    administration::{invalid, unavailable},
};
use crate::{RpcError, callback::NetworkScope};

/// 业务调用的副作用分类，不作为安装授权或资源白名单。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ProviderOperation {
    Import,
    Rotate,
    Refresh,
    Login,
    Quota,
    Models,
    Profile,
    ResetCredits,
    ConsumeResetCredit,
}

#[derive(Clone, Copy)]
pub(super) enum ManagementOrigin<'a> {
    Interactive,
    Maintenance {
        task_id: &'a str,
        credential_revision: Option<u64>,
    },
}

/// 管理调用独立持有网络范围；账号和代理来自宿主，不接受插件覆盖。
pub(super) struct ManagementCall<'a> {
    provider: &'a PluginProvider,
    context: CallContext,
    pub(super) scope: Arc<NetworkScope>,
    operation: ProviderOperation,
}

impl PluginProvider {
    // 只读投影与不可逆操作共用身份边界；调用方决定失配意味着冲突还是结果未知。
    pub(super) async fn ensure_management_identity(
        &self,
        account: &ProviderAccount,
    ) -> Result<(), ProviderAdminError> {
        let current = self
            .ports
            .accounts()
            .get_account(account.id())
            .await
            .map_err(|_| unavailable())?;
        if !current.is_some_and(|current| {
            current.provider() == account.provider()
                && current.revision() == account.revision()
                && current.upstream_account_id() == account.upstream_account_id()
                && current.upstream_user_id() == account.upstream_user_id()
        }) {
            return Err(ProviderAdminError::new(ProviderAdminErrorKind::Conflict));
        }
        Ok(())
    }

    pub(super) fn ensure_provider(&self, kind: &ProviderKind) -> Result<(), ProviderAdminError> {
        if kind != &self.kind {
            return Err(invalid());
        }
        Ok(())
    }

    pub(super) async fn load_management_credential(
        &self,
        id: &ProviderAccountId,
    ) -> Result<LoadedCredential, ProviderAdminError> {
        let account = self
            .ports
            .accounts()
            .get_account(id)
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(|| ProviderAdminError::new(ProviderAdminErrorKind::NotFound))?;
        self.ensure_provider(account.provider())?;
        self.ports
            .accounts()
            .load_credential(id, account.revision())
            .await
            .map_err(|error| {
                ProviderAdminError::new(match error.kind() {
                    gateway_core::error::StoreErrorKind::Conflict => {
                        ProviderAdminErrorKind::Conflict
                    }
                    _ => ProviderAdminErrorKind::Unavailable,
                })
            })
    }

    pub(super) fn management_call(
        &self,
        operation: ProviderOperation,
        account: Option<&ProviderAccount>,
        proxy: Option<OutboundProxy>,
    ) -> Result<ManagementCall<'_>, ProviderAdminError> {
        self.management_call_from(operation, account, proxy, ManagementOrigin::Interactive)
    }

    pub(super) fn management_call_from(
        &self,
        operation: ProviderOperation,
        account: Option<&ProviderAccount>,
        proxy: Option<OutboundProxy>,
        origin: ManagementOrigin<'_>,
    ) -> Result<ManagementCall<'_>, ProviderAdminError> {
        let (stage, request_id) = match origin {
            ManagementOrigin::Interactive => (Stage::Management, None),
            ManagementOrigin::Maintenance {
                task_id,
                credential_revision,
            } => {
                // 分页列举后账号可能被停用或冻结；按派发前重新装载的账号再校验一次。
                if account.is_some_and(|account| {
                    !account.enabled()
                        || !(account.credential_state() == CredentialState::Ready
                            || (operation == ProviderOperation::Refresh
                                && account.credential_state() == CredentialState::Expired))
                }) || credential_revision.is_some_and(|expected| {
                    account.is_none_or(|account| account.revision().get() != expected)
                }) {
                    return Err(ProviderAdminError::new(ProviderAdminErrorKind::Conflict));
                }
                (Stage::Maintenance, Some(task_id.to_owned()))
            }
        };
        let mut context = self.session.context(stage, Duration::from_secs(30));
        context.request_id = request_id;
        if let Some(account) = account {
            context.account_id = Some(account.id().as_str().into());
            context.credential_revision = Some(account.revision().get());
        }
        let scope = self
            .callbacks
            .prepare_management(&context, proxy)
            .map_err(|_| invalid())?;
        Ok(ManagementCall {
            provider: self,
            context,
            scope,
            operation,
        })
    }
}

impl ManagementCall<'_> {
    fn mutating(&self) -> bool {
        matches!(
            self.operation,
            ProviderOperation::Import
                | ProviderOperation::Rotate
                | ProviderOperation::Refresh
                | ProviderOperation::Login
                | ProviderOperation::ConsumeResetCredit
        )
    }

    pub(super) async fn invoke_stream<T: DeserializeOwned>(
        &self,
        method: &str,
        input: &impl Serialize,
    ) -> Result<(T, crate::RpcStream), ProviderAdminError> {
        let payload = serde_json::to_vec(input).map_err(|_| invalid())?;
        let stream = self
            .provider
            .session
            .call_stream(method, self.context.clone(), serde_json::json!({}), payload)
            .await
            .inspect_err(|error| self.record_rpc_failure(method, error))
            .map_err(|error| self.map_error(error))?;
        if !stream.initial.payload.is_empty() {
            return Err(self.invalid_result());
        }
        let metadata = serde_json::from_value(stream.initial.result.clone())
            .map_err(|_| self.invalid_result())?;
        Ok((metadata, stream))
    }

    pub(super) async fn invoke<T: DeserializeOwned>(
        &self,
        method: &str,
        input: &impl Serialize,
    ) -> Result<T, ProviderAdminError> {
        let payload = serde_json::to_vec(input).map_err(|_| invalid())?;
        let reply = self
            .provider
            .session
            .call(method, self.context.clone(), serde_json::json!({}), payload)
            .await
            .inspect_err(|error| self.record_rpc_failure(method, error))
            .map_err(|error| self.map_error(error))?;
        if reply.result != serde_json::json!({}) {
            return Err(self.invalid_result());
        }
        serde_json::from_slice(&reply.payload).map_err(|_| self.invalid_result())
    }

    fn record_rpc_failure(&self, method: &str, error: &RpcError) {
        // RpcError/PluginFault 的 Debug 只保留分类，不展开插件正文、凭据或 fault.message。
        tracing::warn!(
            provider = self.provider.kind.as_str(),
            instance_id = self.context.instance_id,
            generation = self.context.generation,
            method,
            error = ?error,
            "插件管理 RPC 失败"
        );
    }

    pub(super) async fn save_discovered_facts(
        &self,
        account: &ProviderAccount,
        facts: gateway_plugin_sdk::call::auth::CredentialFacts,
    ) -> Result<
        gateway_admin::model::provider_credentials::PluginAccountSaveResult,
        ProviderAdminError,
    > {
        self.provider
            .callbacks
            .save_discovered_facts(&self.context, &self.scope, account, facts)
            .await
            .map_err(map_account_save_error)
    }

    pub(super) fn invalid_result(&self) -> ProviderAdminError {
        ProviderAdminError::new(
            if self.operation == ProviderOperation::ConsumeResetCredit
                || (self.mutating() && self.scope.send_state() != UpstreamSendState::NotSent)
            {
                ProviderAdminErrorKind::Ambiguous
            } else {
                ProviderAdminErrorKind::BadGateway
            },
        )
    }

    fn map_error(&self, error: RpcError) -> ProviderAdminError {
        let sent = self.scope.send_state() != UpstreamSendState::NotSent
            || matches!(&error, RpcError::Remote(error) if error.send_state != SendState::NotSent);
        // 不可逆消费的对端消失或协议损坏时，即使没有观察到 HTTP，也不能证明消费未执行。
        let uncertain_consumption = self.operation == ProviderOperation::ConsumeResetCredit
            && matches!(
                &error,
                RpcError::Closed | RpcError::Protocol | RpcError::Timeout | RpcError::Cancelled
            );
        if self.mutating() && (sent || uncertain_consumption) {
            return ProviderAdminError::new(ProviderAdminErrorKind::Ambiguous);
        }
        map_read_error(error)
    }
}

pub(super) fn map_read_error(error: RpcError) -> ProviderAdminError {
    ProviderAdminError::new(match error {
        RpcError::Remote(error) => match error.code {
            ErrorCode::InvalidInput | ErrorCode::Rejected | ErrorCode::PermissionDenied => {
                ProviderAdminErrorKind::Invalid
            }
            ErrorCode::Unsupported => ProviderAdminErrorKind::Unsupported,
            ErrorCode::Conflict => ProviderAdminErrorKind::Conflict,
            ErrorCode::Uncertain => ProviderAdminErrorKind::Ambiguous,
            _ => ProviderAdminErrorKind::Unavailable,
        },
        _ => ProviderAdminErrorKind::Unavailable,
    })
}

pub(super) fn map_account_save_error(error: AdminError) -> ProviderAdminError {
    ProviderAdminError::new(match error.kind() {
        AdminErrorKind::Invalid | AdminErrorKind::Unauthorized | AdminErrorKind::Forbidden => {
            ProviderAdminErrorKind::Invalid
        }
        AdminErrorKind::NotFound => ProviderAdminErrorKind::NotFound,
        AdminErrorKind::Conflict => ProviderAdminErrorKind::Conflict,
        AdminErrorKind::BadGateway => ProviderAdminErrorKind::BadGateway,
        AdminErrorKind::UpstreamResultUnknown => ProviderAdminErrorKind::Ambiguous,
        AdminErrorKind::Unavailable | AdminErrorKind::RateLimited => {
            ProviderAdminErrorKind::Unavailable
        }
        AdminErrorKind::Internal => ProviderAdminErrorKind::Internal,
    })
}

pub(super) fn account_request(
    credential: &LoadedCredential,
) -> gateway_plugin_sdk::call::provider::account::AccountRequest {
    gateway_plugin_sdk::call::provider::account::AccountRequest {
        account_id: credential.account.id().as_str().into(),
        credential_revision: credential.account.revision().get(),
        credential: credential.credential.expose_to_provider().clone(),
    }
}
