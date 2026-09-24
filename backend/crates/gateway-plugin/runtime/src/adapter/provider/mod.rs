mod administration;
mod authorization;
mod billing;
mod capabilities;
mod catalog;
mod configuration;
mod continuation;
mod credentials;
mod drain;
mod events;
mod execution;
mod forecast;
mod invalidation;
mod maintenance;
mod management;
mod profile;
mod quota;
mod request_profile;
mod reset_credits;
mod rotation;
mod selection;

pub(crate) use credentials::{timestamp as credential_timestamp, validate_facts};
pub(crate) use drain::ContinuationDrain;
pub(crate) use maintenance::MaintenanceKind;

use std::{
    collections::BTreeSet,
    sync::{Arc, Weak},
};

use async_trait::async_trait;
use gateway_admin::model::{AdminError, plugins::instances::PluginPermissionGrant};
use gateway_core::{
    account::ProviderAccountId,
    concurrency::ConcurrencyWaitQueue,
    engine::{
        AttemptContext,
        provider::{Provider, ProviderRequest, ProviderStream},
    },
    error::ProviderError,
    provider_ports::ProviderStorePorts,
    routing::{ProviderCatalogGeneration, ProviderKind, ProviderModelCapabilities},
};
use gateway_plugin_sdk::{
    Capability, Manifest,
    call::auth::CredentialOperation,
    call::provider::{ProviderDescriptor, ProviderHttpMethod},
};

use crate::{RpcSession, callback::PluginCallbacks};

pub(crate) struct ProviderInstanceBinding {
    instance_id: String,
    authorization: String,
    continuation_authorization: String,
    continuation_owner: String,
    continuation_drain: Weak<drain::ContinuationDrain>,
}

impl ProviderInstanceBinding {
    pub(crate) fn new(
        instance_id: String,
        authorization: String,
        continuation_authorization: String,
        continuation_drain: Weak<drain::ContinuationDrain>,
    ) -> Self {
        Self {
            instance_id,
            authorization,
            continuation_authorization,
            continuation_owner: uuid::Uuid::new_v4().to_string(),
            continuation_drain,
        }
    }
}

pub(crate) struct PluginProvider {
    kind: ProviderKind,
    instance_id: String,
    catalog: catalog::ModelCatalog,
    exhaustive: bool,
    pricing: Option<gateway_admin::model::pricing::ProviderPricingCatalog>,
    input_formats: Arc<[String]>,
    output_formats: BTreeSet<String>,
    session: Arc<RpcSession>,
    callbacks: Arc<PluginCallbacks>,
    ports: ProviderStorePorts,
    credential_operations: BTreeSet<CredentialOperation>,
    account_operations: BTreeSet<gateway_plugin_sdk::call::provider::account::AccountOperation>,
    credential_inputs:
        std::collections::BTreeMap<CredentialOperation, capabilities::CredentialInput>,
    account_configuration: Option<configuration::PreparedAccountConfiguration>,
    request_profiles: Option<request_profile::PreparedRequestProfiles>,
    request_profile_refresh_enabled: bool,
    http_endpoints: std::collections::BTreeMap<String, BTreeSet<ProviderHttpMethod>>,
    quota_enabled: bool,
    authorization_binding: String,
    continuation: continuation::ContinuationIdentity,
    continuation_drain: Weak<drain::ContinuationDrain>,
    waiting: ConcurrencyWaitQueue<ProviderAccountId>,
}

impl PluginProvider {
    pub(crate) fn prepare(
        descriptor: ProviderDescriptor,
        binding: ProviderInstanceBinding,
        manifest: &Manifest,
        grants: &[PluginPermissionGrant],
        session: Arc<RpcSession>,
        ports: ProviderStorePorts,
        callbacks: Arc<PluginCallbacks>,
    ) -> Result<Self, AdminError> {
        let kind = ProviderKind::new(descriptor.id)
            .map_err(|_| AdminError::invalid("插件 Provider ID 无效"))?;
        let authorization_binding = binding.authorization.clone();
        let continuation = continuation::ContinuationIdentity::prepare(
            kind.as_str().to_owned(),
            binding.authorization,
            binding.continuation_authorization,
            descriptor.continuation_state,
            binding.continuation_owner,
        )?;
        let request_profiles =
            request_profile::PreparedRequestProfiles::prepare(descriptor.request_profiles, &kind)?;
        let request_profile_refresh_enabled = manifest
            .contributes
            .get(&Capability::RequestProfile)
            .is_some_and(|declaration| {
                declaration
                    .stages
                    .contains(&gateway_plugin_sdk::Stage::Maintenance)
            });
        if request_profile_refresh_enabled && request_profiles.is_none() {
            return Err(AdminError::invalid("版本资料维护需要注册初始请求画像目录"));
        }
        let http_endpoints = prepare_http_endpoints(descriptor.http_endpoints)?;
        let executor = manifest
            .contributes
            .get(&Capability::Executor)
            .ok_or_else(|| AdminError::invalid("Provider 未声明执行能力"))?;
        if !grants.iter().any(|grant| grant.permission == "accounts") {
            return Err(AdminError::invalid("Provider 执行需要账号访问权限"));
        }
        let credential_operations: BTreeSet<_> =
            descriptor.credential_operations.iter().copied().collect();
        let authentication = manifest
            .contributes
            .contains_key(&Capability::Authentication);
        if credential_operations.len() != descriptor.credential_operations.len()
            || authentication == credential_operations.is_empty()
        {
            return Err(AdminError::invalid("插件凭据操作与认证能力声明不符"));
        }
        let account_operations: BTreeSet<_> =
            descriptor.account_operations.iter().copied().collect();
        let account_management = manifest.contributes.get(&Capability::AccountManagement);
        if account_operations.len() != descriptor.account_operations.len()
            || account_management.is_some() == account_operations.is_empty()
            || account_management.is_some_and(|declaration| {
                account_operations.iter().any(|operation| {
                    use gateway_plugin_sdk::{Stage, call::provider::account::AccountOperation};
                    let stage = match operation {
                        AccountOperation::Unavailable | AccountOperation::FactsChanged => {
                            Stage::Observation
                        }
                        _ => Stage::Management,
                    };
                    !declaration.stages.contains(&stage)
                })
            })
        {
            return Err(AdminError::invalid("插件账号操作与管理能力声明不符"));
        }
        let account_configuration = configuration::PreparedAccountConfiguration::prepare(
            descriptor.account_configuration,
            &credential_operations,
            descriptor
                .credential_input_schemas
                .get(&CredentialOperation::Rotate),
        )?;
        let credential_inputs = capabilities::prepare_inputs(
            &credential_operations,
            descriptor.credential_input_schemas,
        )?;
        let input_formats = executor
            .input_formats
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if descriptor.models.len() > 4096
            || executor.input_formats.is_empty()
            || input_formats.len() != executor.input_formats.len()
            || executor.output_formats.is_empty()
        {
            return Err(AdminError::invalid("Provider 模型目录或协议声明不合法"));
        }
        let pricing = billing::prepare(descriptor.billing, manifest)?;
        let catalog = catalog::ModelCatalog::prepare(
            descriptor.models,
            descriptor.model_discovery,
            manifest,
        )?;
        Ok(Self {
            kind,
            instance_id: binding.instance_id,
            continuation,
            continuation_drain: binding.continuation_drain,
            catalog,
            exhaustive: descriptor.exhaustive,
            pricing,
            input_formats: executor.input_formats.clone().into(),
            output_formats: executor.output_formats.iter().cloned().collect(),
            session,
            callbacks,
            ports,
            credential_operations,
            account_operations,
            credential_inputs,
            account_configuration,
            request_profiles,
            request_profile_refresh_enabled,
            http_endpoints,
            quota_enabled: manifest.contributes.contains_key(&Capability::Quota),
            authorization_binding,
            waiting: ConcurrencyWaitQueue::default(),
        })
    }

    pub(crate) fn register_continuation(self: &Arc<Self>) -> Result<(), AdminError> {
        self.continuation_drain
            .upgrade()
            .ok_or_else(|| AdminError::unavailable("插件续写执行注册表不可用"))?
            .register(self)
    }

    pub(crate) fn retire_continuation(self: &Arc<Self>) -> bool {
        self.continuation_drain
            .upgrade()
            .is_some_and(|drain| drain.retire(Arc::clone(self)))
    }
}

fn prepare_http_endpoints(
    endpoints: Vec<gateway_plugin_sdk::call::provider::ProviderHttpEndpoint>,
) -> Result<std::collections::BTreeMap<String, BTreeSet<ProviderHttpMethod>>, AdminError> {
    if endpoints.len() > 64 {
        return Err(AdminError::invalid("插件 HTTP 端点超过 64 项"));
    }
    let mut prepared = std::collections::BTreeMap::new();
    for endpoint in endpoints {
        let valid_id = !endpoint.id.is_empty()
            && endpoint.id.len() <= 64
            && endpoint.id != "."
            && endpoint.id != ".."
            && endpoint
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        let methods = endpoint.methods.iter().copied().collect::<BTreeSet<_>>();
        if !valid_id
            || methods.is_empty()
            || methods.len() != endpoint.methods.len()
            || prepared.insert(endpoint.id, methods).is_some()
        {
            return Err(AdminError::invalid("插件 HTTP 端点声明无效或重复"));
        }
    }
    Ok(prepared)
}

#[async_trait]
impl Provider for PluginProvider {
    fn resolve_request_profile(
        &self,
        configuration: &gateway_core::account::OpaqueProviderData,
    ) -> Result<gateway_core::account::OpaqueProviderData, ProviderError> {
        match &self.request_profiles {
            Some(profiles) => profiles.resolve(configuration),
            None => Err(request_profile::invalid_request_profile()),
        }
    }
    fn default_request_profile(
        &self,
    ) -> Result<Option<gateway_core::account::OpaqueProviderData>, ProviderError> {
        Ok(self
            .request_profiles
            .as_ref()
            .map(request_profile::PreparedRequestProfiles::default_resolved))
    }
    fn name(&self) -> &str {
        self.kind.as_str()
    }
    fn catalog_generation(&self) -> ProviderCatalogGeneration {
        self.catalog.generation()
    }
    fn model_catalog_is_exhaustive(&self) -> bool {
        // 账号目录会随授权与上游事实变化，全局缺项不能替代单账号的完整目录判断。
        self.exhaustive && !self.catalog.discovers_accounts()
    }
    async fn query_model_capabilities(
        &self,
    ) -> Result<Vec<ProviderModelCapabilities>, ProviderError> {
        self.discover_model_capabilities().await
    }
    async fn query_client_model_catalog(
        &self,
        scope: &gateway_core::account::scope::FrozenAccountScope,
        _protocol: &str,
        _client_version: &str,
    ) -> Result<
        Option<Vec<gateway_core::routing::ProviderModelDescriptor>>,
        gateway_core::routing::ProviderCatalogUnavailable,
    > {
        if !self.catalog.discovers_accounts() {
            return Ok(None);
        }
        self.discover_client_catalog(scope).await.map(Some)
    }
    async fn execute(
        self: Arc<Self>,
        request: ProviderRequest,
        context: AttemptContext,
    ) -> Result<ProviderStream, ProviderError> {
        self.prepare_execution(request, context).await
    }
}
