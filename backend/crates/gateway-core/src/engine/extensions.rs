//! 执行器按发布集合查找；索引不持有当前代次，也不延长旧代次寿命。

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock, Weak},
};

use crate::{identity::ProviderKind, runtime::extensions::ExtensionSetId};

use super::provider::{Provider, ProviderRegistry, RegistryError};

/// 一次请求已经进入过的插件实例集合。
///
/// Core 把它随子请求传播给 Provider、策略与观察边界；Runtime 只据此跳过对应
/// 实例，不能自行扩大授权或改变路由。集合同时用于拒绝 A → B → A 间接递归。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionCallScope {
    instance_ids: Arc<BTreeSet<String>>,
}

impl ExtensionCallScope {
    #[must_use]
    pub fn contains(&self, instance_id: &str) -> bool {
        self.instance_ids.contains(instance_id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.instance_ids.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.instance_ids.is_empty()
    }

    /// 返回加入当前发起实例后的新作用域；重复实例表示间接递归。
    #[must_use]
    pub fn extending(&self, instance_id: String) -> Option<Self> {
        if self.contains(&instance_id) {
            return None;
        }
        let mut instance_ids = self.instance_ids.as_ref().clone();
        instance_ids.insert(instance_id);
        Some(Self {
            instance_ids: Arc::new(instance_ids),
        })
    }
}

#[derive(Clone)]
pub struct ProviderExtensionIndex {
    base: Arc<BTreeMap<ProviderKind, Arc<dyn Provider>>>,
    sets: Arc<RwLock<BTreeMap<ExtensionSetId, Weak<ProviderRegistry>>>>,
}

impl ProviderExtensionIndex {
    #[must_use]
    pub fn new(base: ProviderRegistry) -> Self {
        Self {
            base: base.providers,
            sets: Arc::default(),
        }
    }

    /// 准备阶段检查原生与插件身份冲突；返回值由候选集合保活。
    pub fn register(
        &self,
        id: ExtensionSetId,
        providers: impl IntoIterator<Item = Arc<dyn Provider>>,
    ) -> Result<Arc<ProviderRegistry>, RegistryError> {
        let registry = Arc::new(ProviderRegistry::new(
            self.base.values().cloned().chain(providers),
        )?);
        let mut sets = self
            .sets
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sets.retain(|_, set| set.strong_count() > 0);
        if sets.contains_key(&id) {
            return Err(RegistryError::DuplicateGeneration);
        }
        sets.insert(id, Arc::downgrade(&registry));
        Ok(registry)
    }

    /// 组合根使用的解析入口；请求仍须显式提供冻结的集合引用。
    #[must_use]
    pub fn registry(&self) -> ProviderRegistry {
        ProviderRegistry {
            providers: self.base.clone(),
            extensions: Some(self.clone()),
            lease: None,
        }
    }

    pub(super) fn resolve(&self, id: &ExtensionSetId) -> Option<Arc<ProviderRegistry>> {
        self.sets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .and_then(Weak::upgrade)
    }
}
