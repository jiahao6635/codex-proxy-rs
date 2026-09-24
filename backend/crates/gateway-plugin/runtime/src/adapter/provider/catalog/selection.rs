use super::super::{PluginProvider, administration::invalid};
use super::{cache, catalog_error, descriptor};
use futures::{StreamExt as _, stream};
use gateway_admin::ports::provider::ProviderAdminError;
use gateway_core::{
    account::{AccountStatus, ProviderAccount},
    engine::AttemptContext,
    error::{ProviderError, ProviderErrorKind},
    operation::CapabilityRequirements,
    upstream::UpstreamSendState,
};
use std::time::SystemTime;

impl PluginProvider {
    pub(in super::super) async fn filter_model_accounts(
        &self,
        accounts: Vec<ProviderAccount>,
        model: Option<&str>,
        requirements: &CapabilityRequirements,
        context: &AttemptContext,
    ) -> Result<Vec<ProviderAccount>, ProviderError> {
        let Some(model) = model.filter(|_| self.catalog.discovers_accounts()) else {
            return Ok(accounts);
        };
        let remaining = context
            .deadline()
            .duration_since(SystemTime::now())
            .map_err(|_| {
                ProviderError::new(ProviderErrorKind::Timeout, UpstreamSendState::NotSent)
            })?;
        let query = async {
            let mut results = stream::iter(accounts.into_iter().filter(|account| {
                context.is_diagnostic_required_account()
                    || account.status_projection(SystemTime::now(), None).status
                        == AccountStatus::Normal
            }))
            .map(|account| async move {
                let catalog = self
                    .discover_account_models(&account, false, cache::Publication::Account)
                    .await?;
                let allowed = match self.catalog.account_model(&catalog.catalog, model) {
                    Some(model) => descriptor::compile(model)
                        .map_err(|_| invalid())?
                        .capabilities()
                        .match_requirements(requirements)
                        .is_some(),
                    None => !catalog.catalog.exhaustive,
                };
                Ok::<_, ProviderAdminError>(allowed.then_some(account))
            })
            .buffered(4);
            let mut eligible = Vec::new();
            let mut failed = false;
            while let Some(result) = results.next().await {
                match result {
                    Ok(Some(account)) => eligible.push(account),
                    Ok(None) => {}
                    Err(_) => failed = true,
                }
            }
            if failed && eligible.is_empty() {
                return Err(catalog_error());
            }
            Ok(eligible)
        };
        tokio::select! {
            biased;
            _ = context.cancellation().cancelled() => Err(ProviderError::new(ProviderErrorKind::Cancelled, UpstreamSendState::NotSent)),
            result = tokio::time::timeout(remaining, query) => result.map_err(|_| ProviderError::new(ProviderErrorKind::Timeout, UpstreamSendState::NotSent))?,
        }
    }
}
