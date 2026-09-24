use std::time::Duration;

use async_trait::async_trait;
use gateway_admin::{
    model::{
        observability::{CalculatedBillingBreakdown, DashboardWireProfile, ProviderBillingInput},
        provider_credentials::{
            AuthorizationStarted, CompleteAuthorization, ConsumeProviderResetCredit,
            PollAuthorization, PrepareAuthorization, PrepareCredentialImport,
            PrepareCredentialRefresh, PrepareCredentialRotation, PreparedAuthorizationCommit,
            PreparedAuthorizationPoll, PreparedCredentialImport, PreparedCredentialRotation,
            ProviderExport, ProviderExportCredentialInput, ProviderModels, ProviderProfileAvatar,
            ProviderProfileStatistics, ProviderQuota, ProviderQuotaRequest,
            ProviderResetCreditResult, ProviderResetCredits, ProviderSubscription,
        },
    },
    ports::provider::{ProviderAdmin, ProviderAdminError, ProviderAdminErrorKind},
};
use gateway_core::{
    account::ProviderAccountId,
    operation::{GenerateRequest, Operation, ProtocolPayload},
    routing::{ProviderKind, UpstreamModelId},
};
use gateway_plugin_sdk::Stage;

use super::PluginProvider;

#[async_trait]
impl ProviderAdmin for PluginProvider {
    fn pricing_catalog(&self) -> gateway_admin::model::pricing::ProviderPricingCatalog {
        self.pricing.clone().unwrap_or_default()
    }

    fn provider_kind(&self) -> &ProviderKind {
        &self.kind
    }

    fn credential_capabilities(
        &self,
    ) -> gateway_admin::model::provider_capabilities::ProviderCredentialCapabilities {
        PluginProvider::credential_capabilities(self)
    }

    fn account_capabilities(
        &self,
        _account_id: &ProviderAccountId,
        _authentication_kind: &str,
    ) -> gateway_admin::model::provider_capabilities::ProviderAccountCapabilities {
        use gateway_admin::model::provider_capabilities::ProviderAccountCapabilities;
        use gateway_plugin_sdk::call::provider::account::AccountOperation;
        ProviderAccountCapabilities {
            quota: self.quota_enabled,
            quota_refresh: self.quota_enabled,
            profile: self.account_operations.contains(&AccountOperation::Profile),
            subscription: self
                .account_operations
                .contains(&AccountOperation::Subscription),
            avatar: self.account_operations.contains(&AccountOperation::Avatar),
            reset_credits: self
                .account_operations
                .contains(&AccountOperation::ResetCredits),
            consume_reset_credit: self
                .account_operations
                .contains(&AccountOperation::ConsumeResetCredit),
        }
    }

    fn client_profile_options(
        &self,
    ) -> Result<gateway_core::account::OpaqueProviderData, ProviderAdminError> {
        self.request_profiles
            .as_ref()
            .ok_or_else(|| ProviderAdminError::new(ProviderAdminErrorKind::Unsupported))?
            .options()
    }

    fn default_client_profile(&self) -> Option<gateway_core::account::OpaqueProviderData> {
        self.request_profiles
            .as_ref()
            .map(super::request_profile::PreparedRequestProfiles::default_configuration)
    }

    fn preview_client_profile(
        &self,
        configuration: &gateway_core::account::OpaqueProviderData,
    ) -> Result<gateway_core::account::OpaqueProviderData, ProviderAdminError> {
        self.request_profiles
            .as_ref()
            .ok_or_else(|| ProviderAdminError::new(ProviderAdminErrorKind::Unsupported))?
            .preview(configuration)
    }

    async fn account_unavailable(&self, account_id: &ProviderAccountId) {
        self.catalog.invalidate(std::slice::from_ref(account_id));
        self.invalidate_unavailable(account_id).await;
    }

    async fn account_facts_changed(&self, account_ids: &[ProviderAccountId]) {
        self.catalog.invalidate(account_ids);
        self.invalidate_facts(account_ids).await;
    }

    async fn connection_test_operation(
        &self,
        model: &UpstreamModelId,
        input: &str,
    ) -> Result<Operation, ProviderAdminError> {
        let reply = self
            .session
            .call(
                "provider.connection_test",
                self.session
                    .context(Stage::Configuration, Duration::from_secs(5)),
                serde_json::json!({"model":model.as_str(),"input":input}),
                vec![],
            )
            .await
            .map_err(|_| unavailable())?;
        let probe: gateway_plugin_sdk::call::provider::ProbeOperation =
            serde_json::from_value(reply.result).map_err(|_| invalid())?;
        if !reply.payload.is_empty() || !self.input_formats.contains(&probe.protocol) {
            return Err(invalid());
        }
        let payload =
            ProtocolPayload::json_object(probe.protocol, probe.body).map_err(|_| invalid())?;
        Ok(Operation::Generate(GenerateRequest::from_protocol_payload(
            payload,
        )))
    }

    fn dashboard_wire_profile(&self) -> Option<DashboardWireProfile> {
        self.request_profiles
            .as_ref()
            .and_then(|profiles| profiles.dashboard(None))
    }

    fn configured_wire_profile(
        &self,
        configuration: &gateway_core::account::OpaqueProviderData,
    ) -> Option<DashboardWireProfile> {
        self.request_profiles
            .as_ref()
            .and_then(|profiles| profiles.dashboard(Some(configuration)))
    }

    fn calculated_billing(
        &self,
        _input: &ProviderBillingInput,
    ) -> Result<Option<CalculatedBillingBreakdown>, ProviderAdminError> {
        // 插件费用明细只能从请求落库的冻结快照恢复；当前代次价目不能重算历史请求。
        Ok(None)
    }

    async fn models(
        &self,
        account_id: &ProviderAccountId,
        refresh: bool,
    ) -> Result<ProviderModels, ProviderAdminError> {
        self.account_models(account_id, refresh).await
    }

    async fn prepare_import(
        &self,
        command: PrepareCredentialImport,
    ) -> Result<PreparedCredentialImport, ProviderAdminError> {
        self.import_credentials(command).await
    }
    async fn start_authorization(
        &self,
        command: PrepareAuthorization,
    ) -> Result<AuthorizationStarted, ProviderAdminError> {
        self.start_login(command).await
    }
    async fn poll_authorization(
        &self,
        command: PollAuthorization,
    ) -> Result<PreparedAuthorizationPoll, ProviderAdminError> {
        self.poll_login(command).await
    }
    async fn complete_authorization(
        &self,
        _command: CompleteAuthorization,
    ) -> Result<PreparedAuthorizationCommit, ProviderAdminError> {
        Err(unsupported())
    }
    async fn prepare_rotation(
        &self,
        command: PrepareCredentialRotation,
    ) -> Result<PreparedCredentialRotation, ProviderAdminError> {
        self.rotate_credentials(command.account, Some(command.provider_material))
            .await
    }
    async fn prepare_refresh(
        &self,
        command: PrepareCredentialRefresh,
    ) -> Result<PreparedCredentialRotation, ProviderAdminError> {
        self.rotate_credentials(command.account, None).await
    }
    async fn account_configuration(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<
        Option<gateway_admin::model::provider_credentials::ProviderDocument>,
        ProviderAdminError,
    > {
        self.connection_configuration(account_id).await
    }
    async fn quota(
        &self,
        request: ProviderQuotaRequest,
    ) -> Result<ProviderQuota, ProviderAdminError> {
        self.account_quota(request).await
    }
    fn quota_forecast_observation(
        &self,
        document: &gateway_admin::model::provider_credentials::ProviderDocument,
        window: &gateway_admin::model::provider_credentials::ProviderQuotaWindow,
    ) -> Option<gateway_admin::model::quota_forecast_sampling::QuotaForecastObservation> {
        self.quota_enabled
            .then(|| super::forecast::observation(document, window))
            .flatten()
    }
    async fn export_credentials(
        &self,
        credentials: Vec<ProviderExportCredentialInput>,
    ) -> Result<ProviderExport, ProviderAdminError> {
        self.export_credentials_document(credentials).await
    }

    async fn profile_statistics(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<ProviderProfileStatistics, ProviderAdminError> {
        self.account_profile(account_id).await
    }

    async fn subscription(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<Option<ProviderSubscription>, ProviderAdminError> {
        self.account_subscription(account_id).await
    }

    async fn profile_avatar(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<ProviderProfileAvatar, ProviderAdminError> {
        self.account_avatar(account_id).await
    }

    async fn reset_credits(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<ProviderResetCredits, ProviderAdminError> {
        self.query_reset_credits(account_id).await
    }

    async fn consume_reset_credit(
        &self,
        command: ConsumeProviderResetCredit,
    ) -> Result<ProviderResetCreditResult, ProviderAdminError> {
        self.redeem_reset_credit(command).await
    }
}

pub(super) fn unsupported() -> ProviderAdminError {
    ProviderAdminError::new(ProviderAdminErrorKind::Unsupported)
}
pub(super) fn invalid() -> ProviderAdminError {
    ProviderAdminError::new(ProviderAdminErrorKind::Invalid)
}
pub(super) fn unavailable() -> ProviderAdminError {
    ProviderAdminError::new(ProviderAdminErrorKind::Unavailable)
}
