use super::super::PluginProvider;
use super::{CATALOG_TIMEOUT, cache, catalog_error, descriptor};
use futures::{StreamExt as _, stream};
use gateway_core::{
    account::AccountStatus, error::ProviderError, routing::ProviderModelCapabilities,
};
use gateway_plugin_sdk::call::provider::ModelDescriptor;
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime},
};

impl PluginProvider {
    pub(in super::super) async fn discover_model_capabilities(
        &self,
    ) -> Result<Vec<ProviderModelCapabilities>, ProviderError> {
        let query = async {
            if let Some(models) = self.catalog.cache.directory() {
                return Ok(models);
            }
            let _guard = self.catalog.cache.directory_refresh.lock().await;
            if let Some(models) = self.catalog.cache.directory() {
                return Ok(models);
            }
            let directory_revision = self.catalog.cache.directory_revision();
            let models = self.discover_model_catalog(None).await?;
            if let Some(discovery) = &self.catalog.discovery {
                self.catalog
                    .cache
                    .publish_directory(
                        models,
                        Duration::from_secs(u64::from(discovery.cache_ttl_seconds)),
                        directory_revision,
                    )
                    .map_err(|_| catalog_error())
            } else {
                Ok(Arc::new(models))
            }
        };
        tokio::time::timeout(CATALOG_TIMEOUT, query)
            .await
            .map_err(|_| catalog_error())??
            .values()
            .map(|model| descriptor::compile(model).map_err(|_| catalog_error()))
            .collect()
    }

    pub(in super::super) async fn discover_client_catalog(
        &self,
        scope: &gateway_core::account::scope::FrozenAccountScope,
    ) -> Result<
        Vec<gateway_core::routing::ProviderModelDescriptor>,
        gateway_core::routing::ProviderCatalogUnavailable,
    > {
        use gateway_core::routing::{
            ModelPresentation, ProviderCatalogUnavailable, ProviderModelContent,
            ProviderModelDescriptor, UpstreamModelId,
        };
        use gateway_plugin_sdk::call::provider::ModelFeature;
        self.discover_model_catalog(Some(scope))
            .await
            .map_err(|_| ProviderCatalogUnavailable)?
            .into_values()
            .map(|model| {
                Ok(ProviderModelDescriptor {
                    model: UpstreamModelId::new(model.id.clone())
                        .map_err(|_| ProviderCatalogUnavailable)?,
                    content: ProviderModelContent::Adapted(
                        ModelPresentation::new(Some(model.id), None)
                            .with_image_input(model.features.contains(&ModelFeature::Vision))
                            .with_agent_tools(model.features.contains(&ModelFeature::Tools), false),
                    ),
                })
            })
            .collect()
    }

    async fn discover_model_catalog(
        &self,
        scope: Option<&gateway_core::account::scope::FrozenAccountScope>,
    ) -> Result<BTreeMap<String, ModelDescriptor>, ProviderError> {
        let query = async {
            let mut models = BTreeMap::new();
            if scope.is_none() {
                descriptor::union(&mut models, self.catalog.static_models.clone())
                    .map_err(|_| catalog_error())?;
            }
            if self.catalog.discovers_accounts() {
                let mut accounts = self
                    .ports
                    .accounts()
                    .list_for_provider(&self.kind)
                    .await
                    .map_err(|_| catalog_error())?;
                accounts.retain(|account| {
                    matches!(
                        account.status_projection(SystemTime::now(), None).status,
                        AccountStatus::Normal | AccountStatus::QuotaExhausted
                    ) && self.catalog_account_allowed(account)
                        && scope.is_none_or(|scope| scope.allows(account.id()))
                });
                if accounts.len() > 4096 {
                    return Err(catalog_error());
                }
                accounts.sort_by(|left, right| left.id().cmp(right.id()));
                let mut results = stream::iter(accounts)
                    .map(|account| async move {
                        self.discover_account_models(
                            &account,
                            false,
                            if scope.is_none() {
                                cache::Publication::Directory
                            } else {
                                cache::Publication::Account
                            },
                        )
                        .await
                    })
                    .buffered(4);
                let mut failed = false;
                let mut succeeded = false;
                while let Some(result) = results.next().await {
                    match result {
                        Ok(result) => {
                            succeeded = true;
                            descriptor::union(
                                &mut models,
                                self.catalog
                                    .for_account(&result.catalog)
                                    .into_values()
                                    .filter(|model| {
                                        scope.is_none_or(|scope| {
                                            scope.allows_model(result.account.id(), &model.id)
                                        })
                                    }),
                            )
                            .map_err(|_| catalog_error())?;
                        }
                        Err(_) => failed = true,
                    }
                }
                // 空目录和所有账号读取失败是不同事实，不能把故障解释成没有模型。
                if failed && !succeeded && models.is_empty() {
                    return Err(catalog_error());
                }
            }
            Ok(models)
        };
        tokio::time::timeout(CATALOG_TIMEOUT, query)
            .await
            .map_err(|_| catalog_error())?
    }
}
