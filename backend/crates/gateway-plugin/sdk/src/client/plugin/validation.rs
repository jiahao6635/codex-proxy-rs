use std::collections::{BTreeMap, BTreeSet};

use crate::{
    Capability as C, Manifest,
    call::{
        auth::CredentialOperation,
        provider::{ProviderDescriptor, account::AccountOperation},
    },
};

use super::{AuthorError, Handler, methods};

pub(super) fn validate(
    manifest: &Manifest,
    provider: Option<&ProviderDescriptor>,
    handlers: &BTreeMap<&'static str, Handler>,
) -> Result<(), AuthorError> {
    let required = |method| {
        handlers
            .contains_key(method)
            .then_some(())
            .ok_or(AuthorError::MissingMethod(method))
    };
    let matches_handler = |method, expected| match (handlers.contains_key(method), expected) {
        (true, true) | (false, false) => Ok(()),
        (false, true) => Err(AuthorError::MissingMethod(method)),
        (true, false) => Err(AuthorError::Provider),
    };
    let has = |capability| manifest.contributes.contains_key(&capability);

    for capability in manifest.contributes.keys() {
        match capability {
            C::Middleware => required(crate::call::middleware::HANDLE_METHOD)?,
            C::ModelRouter => required(methods::ROUTE_MODEL.name)?,
            C::Scheduler => required(methods::SCHEDULE_ACCOUNT.name)?,
            C::RequestLifecycle | C::Usage => required(methods::OBSERVE_REQUEST.name)?,
            C::WebSocketObserver => required(methods::OBSERVE_WEBSOCKET.name)?,
            C::Management => {
                required(methods::MANAGEMENT_REGISTER.name)?;
                required(methods::MANAGEMENT_HANDLE.name)?;
            }
            C::CommandLine => {
                required(methods::COMMAND_LINE_REGISTER.name)?;
                required(methods::COMMAND_LINE_EXECUTE.name)?;
            }
            C::FrontendAuthentication => {
                required(methods::FRONTEND_IDENTIFIER.name)?;
                required(methods::FRONTEND_AUTHENTICATE.name)?;
            }
            C::Executor => {
                required(methods::PREPARE_EXECUTION.name)?;
                required(methods::EXECUTE.name)?;
                required(methods::DISCARD_EXECUTION.name)?;
                required(methods::CONNECTION_TEST.name)?;
            }
            C::Models => required(methods::MODELS.name)?,
            C::Quota => required(methods::QUOTA.name)?,
            C::RequestProfile => required(methods::REFRESH_REQUEST_PROFILES.name)?,
            C::Authentication | C::AccountManagement | C::Billing | C::Maintenance => {}
        }
    }
    if manifest
        .state
        .iter()
        .any(|namespace| !namespace.migrates_from.is_empty())
    {
        required(methods::STATE_MIGRATE.name)?;
    }

    let has_provider_capability = manifest.contributes.keys().any(|capability| {
        matches!(
            capability,
            C::Models
                | C::Authentication
                | C::Executor
                | C::Quota
                | C::AccountManagement
                | C::Billing
                | C::RequestProfile
                | C::Maintenance
        )
    });
    if provider.is_some() != has(C::Executor) || provider.is_none() && has_provider_capability {
        return Err(AuthorError::Provider);
    }
    let Some(provider) = provider else {
        return Ok(());
    };
    validate_provider(provider, &has, &matches_handler)
}

fn validate_provider(
    provider: &ProviderDescriptor,
    has: &impl Fn(C) -> bool,
    matches_handler: &impl Fn(&'static str, bool) -> Result<(), AuthorError>,
) -> Result<(), AuthorError> {
    let credential_operations = provider
        .credential_operations
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let account_operations = provider
        .account_operations
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let supports_maintenance = credential_operations.contains(&CredentialOperation::Refresh)
        || has(C::Quota)
        || has(C::Models)
        || has(C::RequestProfile);
    if provider.id.trim().is_empty()
        || credential_operations.len() != provider.credential_operations.len()
        || account_operations.len() != provider.account_operations.len()
        || credential_operations.is_empty() == has(C::Authentication)
        || account_operations.is_empty() == has(C::AccountManagement)
        || provider.billing.is_some() != has(C::Billing)
        || provider.model_discovery.is_some() != has(C::Models)
        || has(C::RequestProfile) && provider.request_profiles.is_none()
        || has(C::Maintenance) && !supports_maintenance
        || provider
            .credential_input_schemas
            .keys()
            .any(|operation| !credential_operations.contains(operation))
        || provider.account_configuration.is_some()
            && !credential_operations.contains(&CredentialOperation::Rotate)
    {
        return Err(AuthorError::Provider);
    }

    matches_handler(
        methods::ACCOUNT_CONFIGURATION.name,
        provider.account_configuration.is_some(),
    )?;
    matches_handler(
        methods::IMPORT_CREDENTIALS.name,
        credential_operations.contains(&CredentialOperation::Import),
    )?;
    matches_handler(
        methods::EXPORT_CREDENTIALS.name,
        credential_operations.contains(&CredentialOperation::Export),
    )?;
    matches_handler(
        methods::ROTATE_CREDENTIALS.name,
        credential_operations.contains(&CredentialOperation::Rotate),
    )?;
    matches_handler(
        methods::REFRESH_CREDENTIALS.name,
        credential_operations.contains(&CredentialOperation::Refresh),
    )?;
    let login = credential_operations.contains(&CredentialOperation::Login);
    matches_handler(methods::LOGIN_START.name, login)?;
    matches_handler(methods::LOGIN_POLL.name, login)?;

    let invalidation = account_operations.contains(&AccountOperation::Unavailable)
        || account_operations.contains(&AccountOperation::FactsChanged);
    matches_handler(methods::ACCOUNT_CHANGED.name, invalidation)?;
    matches_handler(
        methods::PROFILE.name,
        account_operations.contains(&AccountOperation::Profile),
    )?;
    matches_handler(
        methods::SUBSCRIPTION.name,
        account_operations.contains(&AccountOperation::Subscription),
    )?;
    matches_handler(
        methods::AVATAR.name,
        account_operations.contains(&AccountOperation::Avatar),
    )?;
    matches_handler(
        methods::RESET_CREDITS.name,
        account_operations.contains(&AccountOperation::ResetCredits),
    )?;
    matches_handler(
        methods::CONSUME_RESET_CREDIT.name,
        account_operations.contains(&AccountOperation::ConsumeResetCredit),
    )?;
    Ok(())
}
