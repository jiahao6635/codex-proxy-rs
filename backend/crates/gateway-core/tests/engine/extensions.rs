use std::sync::Arc;

use async_trait::async_trait;
use gateway_core::{
    engine::{
        AttemptContext,
        extensions::ProviderExtensionIndex,
        provider::{Provider, ProviderRegistry, ProviderRequest, ProviderStream, RegistryError},
    },
    error::ProviderError,
    routing::{
        ProviderCatalogGeneration, ProviderCatalogPort, ProviderKind, ProviderModelCapabilities,
    },
    runtime::extensions::{ExtensionSetId, ExtensionSetLease, ExtensionSetReference},
};

struct Generation(Arc<ProviderRegistry>);

impl ExtensionSetLease for Generation {
    fn is_ready(&self) -> bool {
        !self.0.is_empty()
    }
}

pub(super) fn register(
    index: &ProviderExtensionIndex,
    id: &str,
    providers: Vec<Arc<dyn Provider>>,
) -> ExtensionSetReference {
    let id = ExtensionSetId::new(id.into()).unwrap();
    let registry = index.register(id.clone(), providers).unwrap();
    ExtensionSetReference::new(id, Arc::new(Generation(registry)))
}

struct VersionedProvider {
    name: String,
    version: u64,
}

#[async_trait]
impl Provider for VersionedProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn catalog_generation(&self) -> ProviderCatalogGeneration {
        ProviderCatalogGeneration::new(self.version)
    }
    async fn query_model_capabilities(
        &self,
    ) -> Result<Vec<ProviderModelCapabilities>, ProviderError> {
        Ok(vec![])
    }
    async fn execute(
        self: Arc<Self>,
        _: ProviderRequest,
        _: AttemptContext,
    ) -> Result<ProviderStream, ProviderError> {
        panic!("目录验证不应启动执行")
    }
}

fn provider(name: &str, version: u64) -> Arc<dyn Provider> {
    Arc::new(VersionedProvider {
        name: name.into(),
        version,
    })
}

#[test]
fn generations_resolve_owned_identities_and_keep_only_inflight_versions_alive() {
    let index =
        ProviderExtensionIndex::new(ProviderRegistry::new([provider("native", 1)]).unwrap());
    let registry = index.registry();
    let old = register(&index, "old", vec![provider("dynamic", 1)]);
    let current = register(&index, "current", vec![provider("dynamic", 2)]);
    let kind = ProviderKind::new("dynamic").unwrap();
    let inflight = registry.for_extensions(Some(&old)).unwrap();
    drop(old);
    assert_eq!(inflight.catalog_generations()[&kind].get(), 1);
    assert_eq!(
        registry
            .for_extensions(Some(&current))
            .unwrap()
            .catalog_generations()[&kind]
            .get(),
        2
    );
    assert!(inflight.contains(&ProviderKind::new("native").unwrap()));
    assert!(registry.for_extensions(None).is_err());
    drop(inflight);
    // 原 ID 可再次登记证明索引没有强引用，且不会把旧 ID 偷换为 current。
    assert!(
        index
            .register(ExtensionSetId::new("old".into()).unwrap(), [])
            .is_ok()
    );
}

#[test]
fn preparation_rejects_native_identity_collisions_and_live_generation_replacement() {
    let index =
        ProviderExtensionIndex::new(ProviderRegistry::new([provider("native", 1)]).unwrap());
    let id = ExtensionSetId::new("candidate".into()).unwrap();
    assert!(matches!(
        index.register(id.clone(), [provider("native", 2)]),
        Err(RegistryError::Duplicate { .. })
    ));
    let generation = index
        .register(id.clone(), [provider("dynamic", 1)])
        .unwrap();
    assert!(matches!(
        index.register(id, [provider("dynamic", 2)]),
        Err(RegistryError::DuplicateGeneration)
    ));
    drop(generation);
}
