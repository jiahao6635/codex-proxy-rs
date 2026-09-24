//! 已发布 Provider 的账号能力；响应只包含静态 schema，不包含配置或凭据。

use gateway_admin::model::provider_capabilities::{
    AuthorizationCompletion, ProviderCredentialDescriptor,
};

use super::*;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountProviderData {
    pub provider: String,
    pub credentials: AccountCredentialCapabilitiesData,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountCredentialCapabilitiesData {
    pub import: Option<Value>,
    pub login: Option<AccountLoginCapabilityData>,
    pub refresh: bool,
    pub export: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountLoginCapabilityData {
    pub input_schema: Value,
    pub completion: AccountAuthorizationCompletion,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountAuthorizationCompletion {
    Callback,
    Poll,
}

impl From<ProviderCredentialDescriptor> for AccountProviderData {
    fn from(value: ProviderCredentialDescriptor) -> Self {
        let capabilities = value.capabilities;
        Self {
            provider: value.provider.as_str().to_owned(),
            credentials: AccountCredentialCapabilitiesData {
                import: capabilities.import,
                login: capabilities.login.map(|login| AccountLoginCapabilityData {
                    input_schema: login.input_schema,
                    completion: match login.completion {
                        AuthorizationCompletion::Callback => {
                            AccountAuthorizationCompletion::Callback
                        }
                        AuthorizationCompletion::Poll => AccountAuthorizationCompletion::Poll,
                    },
                }),
                refresh: capabilities.refresh,
                export: capabilities.export,
            },
        }
    }
}

pub(super) async fn providers<S>(
    _auth: AdminAuth,
    State(state): State<S>,
) -> Result<impl IntoResponse, AdminError>
where
    S: crate::auth::SessionState + Send + Sync,
{
    let descriptors = state
        .admin_services()
        .credentials()
        .providers()
        .map_err(map_service_error)?;
    let data: Vec<_> = descriptors
        .into_iter()
        .map(AccountProviderData::from)
        .collect();
    Ok(AdminResponse::new(StatusCode::OK, AdminEnvelope::ok(data)))
}
