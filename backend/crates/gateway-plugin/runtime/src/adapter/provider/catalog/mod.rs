mod account;
mod cache;
mod descriptor;
mod directory;
mod selection;

use cache::CatalogCache;
use gateway_admin::model::AdminError;
use gateway_core::{
    account::ProviderAccountId,
    error::{ProviderError, ProviderErrorKind},
    routing::ProviderCatalogGeneration,
    upstream::UpstreamSendState,
};
use gateway_plugin_sdk::{
    Capability, Manifest, Stage,
    call::provider::{
        ModelDescriptor,
        models::{AccountModels, ModelDiscovery},
    },
};
use std::{collections::BTreeMap, time::Duration};

const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(20);

pub(super) struct ModelCatalog {
    static_models: Vec<ModelDescriptor>,
    discovery: Option<ModelDiscovery>,
    cache: CatalogCache,
}

impl ModelCatalog {
    pub(super) fn prepare(
        models: Vec<ModelDescriptor>,
        discovery: Option<ModelDiscovery>,
        manifest: &Manifest,
    ) -> Result<Self, AdminError> {
        descriptor::validate(&models)?;
        if let Some(discovery) = &discovery
            && (!(1..=3600).contains(&discovery.cache_ttl_seconds)
                || (!discovery.include_static && !models.is_empty())
                || !manifest
                    .contributes
                    .get(&Capability::Models)
                    .is_some_and(|declaration| declaration.stages.contains(&Stage::Management)))
        {
            return Err(AdminError::invalid(
                "账号模型发现需要有效缓存时长及 models/management 声明",
            ));
        }
        Ok(Self {
            static_models: models,
            discovery,
            cache: CatalogCache::default(),
        })
    }

    pub(super) fn generation(&self) -> ProviderCatalogGeneration {
        self.cache.generation()
    }

    pub(super) fn discovers_accounts(&self) -> bool {
        self.discovery.is_some()
    }

    pub(super) fn invalidate(&self, accounts: &[ProviderAccountId]) {
        self.cache.invalidate(accounts);
    }

    fn for_account(&self, discovered: &AccountModels) -> BTreeMap<String, ModelDescriptor> {
        let mut models: BTreeMap<_, _> = if self
            .discovery
            .as_ref()
            .is_some_and(|discovery| discovery.include_static)
        {
            self.static_models
                .iter()
                .map(|model| (model.id.clone(), model.clone()))
                .collect()
        } else {
            BTreeMap::new()
        };
        models.extend(
            discovered
                .models
                .iter()
                .map(|model| (model.id.clone(), model.clone())),
        );
        models
    }

    fn account_model<'a>(
        &'a self,
        discovered: &'a AccountModels,
        id: &str,
    ) -> Option<&'a ModelDescriptor> {
        discovered
            .models
            .iter()
            .find(|model| model.id == id)
            .or_else(|| {
                self.discovery
                    .as_ref()
                    .filter(|discovery| discovery.include_static)
                    .and_then(|_| self.static_models.iter().find(|model| model.id == id))
            })
    }
}

fn catalog_error() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::ProviderInfrastructureUnavailable,
        UpstreamSendState::NotSent,
    )
}
