use super::management::ProviderOperation;
use gateway_admin::{
    model::{
        accounts::AccountRecord,
        provider_credentials::{
            CredentialCommitGuard, PreparedCredentialRotation, PreparedCredentialRotationFacts,
            ProviderDocument,
        },
    },
    ports::provider::{ProviderAdminError, ProviderAdminErrorKind},
};
use gateway_core::{
    account::{
        LoadedCredential, OpaqueProviderData, ProviderAccount, ProviderAccountId,
        ProviderAccountIdentity,
    },
    provider_ports::{
        ProviderLeaseAcquisition, ProviderLeaseGuard, ProviderLeaseRequest,
        ProviderRefreshCapacityRequest, ProviderRefreshLeaseRequest,
    },
};
use gateway_plugin_sdk::call::auth::{
    CredentialFacts, CredentialOperation, ExportCredential, RotateCredential,
};

use super::{
    PluginProvider,
    administration::{invalid, unavailable, unsupported},
    credentials::{timestamp, validate_facts},
    management::ManagementOrigin,
};

struct RotationGuard {
    _guards: Vec<Box<dyn ProviderLeaseGuard>>,
}

impl CredentialCommitGuard for RotationGuard {
    fn finish(self: Box<Self>) {}
}

pub(super) enum CredentialChange {
    Refresh,
    Rotate,
    Reauthorize,
}

impl PluginProvider {
    pub(super) async fn rotate_credentials(
        &self,
        account: AccountRecord,
        replacement: Option<ProviderDocument>,
    ) -> Result<PreparedCredentialRotation, ProviderAdminError> {
        self.rotate_credentials_from(account, replacement, ManagementOrigin::Interactive)
            .await
    }

    pub(super) async fn rotate_credentials_from(
        &self,
        account: AccountRecord,
        replacement: Option<ProviderDocument>,
        origin: ManagementOrigin<'_>,
    ) -> Result<PreparedCredentialRotation, ProviderAdminError> {
        let refresh = replacement.is_none();
        let operation = if refresh {
            CredentialOperation::Refresh
        } else {
            CredentialOperation::Rotate
        };
        if !self.credential_operations.contains(&operation) {
            return Err(unsupported());
        }
        let operation = if refresh {
            ProviderOperation::Refresh
        } else {
            ProviderOperation::Rotate
        };
        self.ensure_provider(&account.provider_kind)?;
        if let Some(document) = &replacement {
            self.validate_credential_input(CredentialOperation::Rotate, document)?;
        }
        let id = ProviderAccountId::new(account.id.clone()).map_err(|_| invalid())?;
        let (current, guard) = self.lock_credential(&id).await?;
        if current.account.revision().get() != account.credential_revision.get() {
            return Err(conflict());
        }
        let call = self.management_call_from(
            operation,
            Some(&current.account),
            current.account.outbound_proxy().cloned(),
            origin,
        )?;
        let input = RotateCredential {
            current: ExportCredential {
                account_id: id.as_str().into(),
                credential_revision: current.account.revision().get(),
                facts: CredentialFacts {
                    name: current.account.name().into(),
                    authentication_kind: current.account.authentication_kind().into(),
                    material: current.credential.into_inner(),
                    email: current.account.email().map(str::to_owned),
                    upstream_user_id: current.account.upstream_user_id().map(str::to_owned),
                    upstream_account_id: current.account.upstream_account_id().map(str::to_owned),
                    plan_type: current.account.plan_type().map(str::to_owned),
                    has_refresh_token: current.account.has_refresh_token(),
                    access_token_expires_at_ms: current
                        .account
                        .access_token_expires_at()
                        .map(chrono::DateTime::<chrono::Utc>::from)
                        .map(|value| value.timestamp_millis()),
                    next_refresh_at_ms: current
                        .account
                        .next_refresh_at()
                        .map(chrono::DateTime::<chrono::Utc>::from)
                        .map(|value| value.timestamp_millis()),
                },
            },
            replacement: replacement.map(|document| document.into_provider_data().into_inner()),
        };
        let method = if refresh {
            "provider.credentials.refresh"
        } else {
            "provider.credentials.rotate"
        };
        let facts: CredentialFacts = call.invoke(method, &input).await?;
        self.prepare_changed_credential(
            current.account,
            facts,
            if refresh {
                CredentialChange::Refresh
            } else {
                CredentialChange::Rotate
            },
            guard,
        )
        .map_err(|_| call.invalid_result())
    }

    pub(super) async fn lock_credential(
        &self,
        id: &ProviderAccountId,
    ) -> Result<(LoadedCredential, Box<dyn CredentialCommitGuard>), ProviderAdminError> {
        let current = self.load_management_credential(id).await?;
        let revision = current.account.revision();
        let policy = self
            .ports
            .runtime_policy()
            .load_refresh_policy()
            .await
            .map_err(|_| unavailable())?;
        let leases = self.ports.leases();
        let capacity = leases
            .try_acquire(ProviderLeaseRequest::RefreshCapacity(
                ProviderRefreshCapacityRequest::new(policy.concurrency()),
            ))
            .await
            .map_err(|_| unavailable())?;
        let ProviderLeaseAcquisition::Acquired(capacity) = capacity else {
            return Err(conflict());
        };
        let mutex = leases
            .try_acquire(ProviderLeaseRequest::Refresh(
                ProviderRefreshLeaseRequest::new(id.clone(), revision),
            ))
            .await
            .map_err(|_| unavailable())?;
        let ProviderLeaseAcquisition::Acquired(mutex) = mutex else {
            return Err(conflict());
        };
        let guard = Box::new(RotationGuard {
            _guards: vec![capacity, mutex],
        });
        // 排队和取锁期间可能已有另一轮提交；在任何上游操作前重新读取版本。
        let current = self.load_management_credential(id).await?;
        if current.account.revision() != revision {
            return Err(conflict());
        }
        Ok((current, guard))
    }

    pub(super) fn prepare_changed_credential(
        &self,
        current: ProviderAccount,
        facts: CredentialFacts,
        change: CredentialChange,
        guard: Box<dyn CredentialCommitGuard>,
    ) -> Result<PreparedCredentialRotation, ProviderAdminError> {
        let refresh = matches!(change, CredentialChange::Refresh);
        validate_facts(&facts)?;
        if facts.authentication_kind != current.authentication_kind()
            || (refresh
                && (facts.upstream_user_id.as_deref() != current.upstream_user_id()
                    || facts.upstream_account_id.as_deref() != current.upstream_account_id()))
            || (!refresh
                && facts.upstream_user_id.is_none()
                && current.upstream_user_id().is_some())
        {
            return Err(invalid());
        }
        let access_token_expires_at = timestamp(facts.access_token_expires_at_ms)?;
        let next_refresh_at = timestamp(facts.next_refresh_at_ms)?;
        Ok(PreparedCredentialRotation::new(
            PreparedCredentialRotationFacts {
                account_id: current.id().clone(),
                provider_kind: self.kind.clone(),
                expected_credential_revision: gateway_admin::model::Revision::new(
                    current.revision().get(),
                )
                .map_err(|_| invalid())?,
                replacement_identity: if refresh {
                    None
                } else {
                    facts
                        .upstream_user_id
                        .map(|user| ProviderAccountIdentity::new(user, facts.upstream_account_id))
                },
                name: facts.name,
                email: facts.email,
                plan_type: facts.plan_type,
                preserve_profile: !matches!(change, CredentialChange::Rotate),
                provider_material: ProviderDocument::new(OpaqueProviderData::new(facts.material)),
                has_refresh_token: facts.has_refresh_token,
                access_token_expires_at,
                next_refresh_at,
            },
            guard,
        ))
    }
}

fn conflict() -> ProviderAdminError {
    ProviderAdminError::new(ProviderAdminErrorKind::Conflict)
}
