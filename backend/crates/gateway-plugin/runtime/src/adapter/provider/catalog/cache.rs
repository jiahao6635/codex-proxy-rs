use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use chrono::{DateTime, Utc};
use gateway_admin::ports::provider::{ProviderAdminError, ProviderAdminErrorKind};
use gateway_core::{
    account::{ProviderAccount, ProviderAccountId},
    routing::ProviderCatalogGeneration,
};
use gateway_plugin_sdk::call::provider::{ModelDescriptor, models::AccountModels};
use tokio::time::Instant;

use super::super::administration::unavailable;

const MAX_ENTRIES: usize = 256;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_REFRESHES: usize = 64;

type AccountRefreshLocks = BTreeMap<(ProviderAccountId, u64), Weak<tokio::sync::Mutex<()>>>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Publication {
    Directory,
    Account,
}

struct CachedDirectory {
    models: Arc<BTreeMap<String, ModelDescriptor>>,
    expires: Instant,
    bytes: usize,
}

pub(super) struct CachedModels {
    pub(super) account: ProviderAccount,
    pub(super) catalog: AccountModels,
    pub(super) observed_at: DateTime<Utc>,
    expires: Instant,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct CatalogCache {
    state: Mutex<CacheState>,
    refreshes: Mutex<AccountRefreshLocks>,
    pub(super) directory_refresh: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct CacheState {
    generation: u64,
    account_invalidation_token: u64,
    directory_revision: u64,
    entries: BTreeMap<ProviderAccountId, Arc<CachedModels>>,
    directory: Option<CachedDirectory>,
}

impl CacheState {
    fn expire(&mut self) {
        self.entries
            .retain(|_, value| value.expires > Instant::now());
        // 账号缓存只是有限容量的查询加速器；淘汰不能让大账号目录永远无法稳定。
        if self
            .directory
            .as_ref()
            .is_some_and(|directory| directory.expires <= Instant::now())
        {
            self.invalidate_directory();
        }
    }

    fn invalidate_directory(&mut self) {
        self.directory = None;
        self.directory_revision = self.directory_revision.saturating_add(1);
        self.generation = self.generation.saturating_add(1);
    }

    fn reserve(&mut self, bytes: usize, maximum_entries: usize) -> Result<(), ProviderAdminError> {
        while self.entries.len() > maximum_entries
            || self
                .entries
                .values()
                .map(|entry| entry.bytes)
                .sum::<usize>()
                + bytes
                > MAX_BYTES
        {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires)
                .map(|(id, _)| id.clone())
            else {
                return Err(unavailable());
            };
            self.entries.remove(&oldest);
        }
        Ok(())
    }
}

impl CatalogCache {
    pub(super) fn generation(&self) -> ProviderCatalogGeneration {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire();
        ProviderCatalogGeneration::new(state.generation)
    }

    pub(super) fn read(&self, account: &ProviderAccount) -> Option<Arc<CachedModels>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire();
        state
            .entries
            .get(account.id())
            .filter(|entry| same_identity(&entry.account, account))
            .cloned()
    }

    pub(super) fn invalidate(&self, accounts: &[ProviderAccountId]) {
        if accounts.is_empty() {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for account in accounts {
            state.entries.remove(account);
        }
        // 即使缓存尚未写入，也要阻止正在查询的旧事实在失效之后重新发布。
        state.account_invalidation_token = state.account_invalidation_token.saturating_add(1);
        state.invalidate_directory();
    }

    pub(super) fn account_invalidation_token(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .account_invalidation_token
    }

    pub(super) fn directory(&self) -> Option<Arc<BTreeMap<String, ModelDescriptor>>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire();
        state
            .directory
            .as_ref()
            .map(|directory| Arc::clone(&directory.models))
    }

    pub(super) fn directory_revision(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .directory_revision
    }

    pub(super) fn publish_directory(
        &self,
        models: BTreeMap<String, ModelDescriptor>,
        ttl: Duration,
        directory_revision: u64,
    ) -> Result<Arc<BTreeMap<String, ModelDescriptor>>, ProviderAdminError> {
        let bytes = serde_json::to_vec(&models)
            .map_err(|_| unavailable())?
            .len();
        if bytes > MAX_BYTES {
            return Err(unavailable());
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.directory_revision != directory_revision {
            return Err(ProviderAdminError::new(ProviderAdminErrorKind::Conflict));
        }
        state.reserve(bytes, MAX_ENTRIES)?;
        let models = Arc::new(models);
        state.directory = Some(CachedDirectory {
            models: Arc::clone(&models),
            expires: Instant::now() + ttl,
            bytes,
        });
        state.generation = state.generation.saturating_add(1);
        Ok(models)
    }

    pub(super) fn refresh_lock(
        &self,
        account: &ProviderAccount,
    ) -> Result<Arc<tokio::sync::Mutex<()>>, ProviderAdminError> {
        let mut refreshes = self
            .refreshes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        refreshes.retain(|_, lock| lock.strong_count() > 0);
        // 同一凭据版本合并查询；回调轮换凭据后，新版本不能等待仍在执行的旧 RPC。
        let key = (account.id().clone(), account.revision().get());
        if let Some(lock) = refreshes.get(&key).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        if refreshes.len() >= MAX_REFRESHES {
            return Err(unavailable());
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        refreshes.insert(key, Arc::downgrade(&lock));
        Ok(lock)
    }

    pub(super) fn publish(
        &self,
        account: ProviderAccount,
        catalog: AccountModels,
        ttl: Duration,
        account_invalidation_token: u64,
        publication: Publication,
    ) -> Result<Arc<CachedModels>, ProviderAdminError> {
        let bytes = serde_json::to_vec(&catalog)
            .map_err(|_| unavailable())?
            .len();
        if bytes > MAX_BYTES {
            return Err(unavailable());
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire();
        if state.account_invalidation_token != account_invalidation_token {
            return Err(ProviderAdminError::new(ProviderAdminErrorKind::Conflict));
        }
        let unchanged = state.entries.get(account.id()).is_some_and(|entry| {
            same_identity(&entry.account, &account) && entry.catalog == catalog
        });
        state.entries.remove(account.id());
        if !unchanged && publication == Publication::Account {
            // 管理刷新与客户端发现可改变正在准备的全局目录；拒绝覆盖这类并发新事实。
            state.invalidate_directory();
        }
        // 以到期时间淘汰可重建目录，不保存凭据；单实例有独立的条目数和总字节预算。
        let directory_bytes = state
            .directory
            .as_ref()
            .map_or(0, |directory| directory.bytes);
        state.reserve(bytes + directory_bytes, MAX_ENTRIES - 1)?;
        let entry = Arc::new(CachedModels {
            account,
            catalog,
            observed_at: Utc::now(),
            expires: Instant::now() + ttl,
            bytes,
        });
        state
            .entries
            .insert(entry.account.id().clone(), entry.clone());
        Ok(entry)
    }
}

pub(super) fn same_identity(left: &ProviderAccount, right: &ProviderAccount) -> bool {
    left.id() == right.id()
        && left.provider() == right.provider()
        && left.revision() == right.revision()
        && left.authentication_kind() == right.authentication_kind()
        && left.upstream_user_id() == right.upstream_user_id()
        && left.upstream_account_id() == right.upstream_account_id()
        && left.plan_type() == right.plan_type()
        && left.outbound_proxy() == right.outbound_proxy()
}
