use super::management::ProviderOperation;
use chrono::{DateTime, Utc};
use gateway_admin::{
    model::provider_credentials::{
        PrepareCredentialImport, PreparedCredentialCreate, PreparedCredentialImport,
        ProviderDocument, ProviderExport, ProviderExportCredentialInput,
    },
    ports::provider::ProviderAdminError,
};
use gateway_core::account::{
    CredentialState, OpaqueProviderData, OutboundProxy, ProviderAccountId,
};
use gateway_plugin_sdk::Stage;
use gateway_plugin_sdk::call::auth::{
    CredentialFacts, CredentialOperation, ExportCredential, ImportedCredentials,
};

use super::{
    PluginProvider,
    administration::{invalid, unavailable, unsupported},
};

impl PluginProvider {
    pub(super) async fn import_credentials(
        &self,
        command: PrepareCredentialImport,
    ) -> Result<PreparedCredentialImport, ProviderAdminError> {
        if !self
            .credential_operations
            .contains(&CredentialOperation::Import)
        {
            return Err(unsupported());
        }
        self.validate_credential_input(CredentialOperation::Import, &command.document)?;
        let call = self.management_call(
            ProviderOperation::Import,
            None,
            command.default_outbound_proxy.clone(),
        )?;
        let imported: ImportedCredentials = call
            .invoke(
                "provider.credentials.import",
                command.document.expose_to_provider().expose_to_provider(),
            )
            .await?;
        let credentials = self
            .prepare_created_credentials(imported.accounts, command.default_outbound_proxy)
            .map_err(|_| call.invalid_result())?;
        Ok(PreparedCredentialImport {
            provider_kind: self.kind.clone(),
            credentials,
        })
    }

    pub(super) fn prepare_created_credentials(
        &self,
        accounts: Vec<CredentialFacts>,
        proxy: Option<OutboundProxy>,
    ) -> Result<Vec<PreparedCredentialCreate>, ProviderAdminError> {
        if accounts.is_empty() || accounts.len() > 200 {
            return Err(invalid());
        }
        let mut credentials = Vec::with_capacity(accounts.len());
        for facts in accounts {
            validate_facts(&facts)?;
            let access_token_expires_at = timestamp(facts.access_token_expires_at_ms)?;
            let next_refresh_at = timestamp(facts.next_refresh_at_ms)?;
            credentials.push(PreparedCredentialCreate {
                model_access: None,
                outbound_proxy: proxy.clone(),
                account_id: ProviderAccountId::new(format!(
                    "acct_{}",
                    uuid::Uuid::new_v4().simple()
                ))
                .map_err(|_| invalid())?,
                provider_kind: self.kind.clone(),
                name: facts.name,
                email: facts.email,
                upstream_user_id: facts.upstream_user_id,
                upstream_account_id: facts.upstream_account_id,
                plan_type: facts.plan_type,
                authentication_kind: facts.authentication_kind,
                provider_material: ProviderDocument::new(OpaqueProviderData::new(facts.material)),
                has_refresh_token: facts.has_refresh_token,
                access_token_expires_at,
                next_refresh_at,
                enabled: true,
                credential_state: CredentialState::Ready,
                credential_observed_at: Utc::now(),
            });
        }
        Ok(credentials)
    }

    pub(super) async fn export_credentials_document(
        &self,
        credentials: Vec<ProviderExportCredentialInput>,
    ) -> Result<ProviderExport, ProviderAdminError> {
        if !self
            .credential_operations
            .contains(&CredentialOperation::Export)
        {
            return Err(unsupported());
        }
        if credentials.is_empty() || credentials.len() > 200 {
            return Err(invalid());
        }
        let mut account_ids = Vec::with_capacity(credentials.len());
        let mut documents = Vec::with_capacity(credentials.len());
        for credential in credentials {
            let account = credential.account;
            self.ensure_provider(&account.provider_kind)?;
            account_ids.push(ProviderAccountId::new(account.id.clone()).map_err(|_| invalid())?);
            documents.push(ExportCredential {
                account_id: account.id,
                credential_revision: account.credential_revision.get(),
                facts: CredentialFacts {
                    name: account.name,
                    authentication_kind: account.authentication_kind,
                    material: credential
                        .provider_material
                        .into_provider_data()
                        .into_inner(),
                    email: account.email,
                    upstream_user_id: account.upstream_user_id,
                    upstream_account_id: account.upstream_account_id,
                    plan_type: account.plan_type,
                    has_refresh_token: account.has_refresh_token,
                    access_token_expires_at_ms: account
                        .access_token_expires_at
                        .map(|time| time.timestamp_millis()),
                    next_refresh_at_ms: account.next_refresh_at.map(|time| time.timestamp_millis()),
                },
            });
        }
        let reply = self
            .session
            .call(
                "provider.credentials.export",
                self.session
                    .context(Stage::Management, Duration::from_secs(30)),
                serde_json::json!({}),
                serde_json::to_vec(&documents).map_err(|_| invalid())?,
            )
            .await
            .map_err(|_| unavailable())?;
        if reply.result != serde_json::json!({}) {
            return Err(invalid());
        }
        let document = serde_json::from_slice(&reply.payload).map_err(|_| invalid())?;
        Ok(ProviderExport {
            provider_kind: self.kind.clone(),
            account_ids,
            document: ProviderDocument::new(OpaqueProviderData::new(document)),
        })
    }
}

pub(crate) fn validate_facts(facts: &CredentialFacts) -> Result<(), ProviderAdminError> {
    let valid_text = |value: &str, maximum: usize| {
        !value.trim().is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
    };
    if !valid_text(&facts.name, 200)
        || !valid_text(&facts.authentication_kind, 64)
        || [
            &facts.email,
            &facts.upstream_user_id,
            &facts.upstream_account_id,
            &facts.plan_type,
        ]
        .into_iter()
        .flatten()
        .any(|value| !valid_text(value, 1024))
        || facts.material.is_empty()
    {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) fn timestamp(value: Option<i64>) -> Result<Option<DateTime<Utc>>, ProviderAdminError> {
    value
        .map(|value| DateTime::from_timestamp_millis(value).ok_or_else(invalid))
        .transpose()
}
use std::time::Duration;
