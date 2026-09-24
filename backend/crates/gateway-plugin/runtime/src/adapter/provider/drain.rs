use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
    time::Instant,
};

use futures::{StreamExt as _, stream};
use gateway_admin::model::AdminError;

use super::PluginProvider;
use crate::generation::ContinuationDrainConfig;

pub(crate) struct ContinuationDrain {
    config: ContinuationDrainConfig,
    state: Mutex<DrainState>,
}

#[derive(Default)]
struct DrainState {
    entries: BTreeMap<String, DrainEntry>,
    next_sequence: u64,
    reaper_running: bool,
}

enum DrainEntry {
    Active {
        provider: Weak<PluginProvider>,
        superseded: bool,
    },
    Retired {
        provider: Arc<PluginProvider>,
        instance_id: String,
        deadline: Instant,
        sequence: u64,
    },
}

impl ContinuationDrain {
    pub(crate) fn new(config: ContinuationDrainConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            state: Mutex::new(DrainState::default()),
        })
    }

    pub(super) fn register(
        self: &Arc<Self>,
        provider: &Arc<PluginProvider>,
    ) -> Result<(), AdminError> {
        let mut state = lock(&self.state);
        state.entries.retain(|_, entry| match entry {
            DrainEntry::Active { provider, .. } => provider.strong_count() > 0,
            DrainEntry::Retired { .. } => true,
        });
        if state.entries.contains_key(&provider.continuation.owner) {
            return Err(AdminError::internal("插件续写执行 owner 冲突"));
        }
        state.entries.insert(
            provider.continuation.owner.clone(),
            DrainEntry::Active {
                provider: Arc::downgrade(provider),
                superseded: false,
            },
        );
        Ok(())
    }

    pub(super) fn resolve(&self, owner: &str) -> Option<Arc<PluginProvider>> {
        let now = Instant::now();
        let mut state = lock(&self.state);
        let mut shutdown = take_expired(&mut state, now);
        let provider = match state.entries.get(owner) {
            Some(DrainEntry::Active { provider, .. }) => provider.upgrade(),
            Some(DrainEntry::Retired { provider, .. }) => Some(Arc::clone(provider)),
            None => None,
        }
        .filter(|provider| provider.session.is_ready());
        if provider.is_none() {
            if let Some(DrainEntry::Retired { provider, .. }) = state.entries.remove(owner) {
                shutdown.push(provider);
            } else {
                state.entries.remove(owner);
            }
        }
        drop(state);
        shutdown_detached(shutdown);
        provider
    }

    /// 当前解释器已经接管该 owner 的全部同版本检查点，不再为它保留旧进程。
    pub(super) fn supersede(&self, owner: &str) {
        let mut state = lock(&self.state);
        let shutdown = match state.entries.get_mut(owner) {
            Some(DrainEntry::Active { superseded, .. }) => {
                *superseded = true;
                None
            }
            Some(DrainEntry::Retired { .. }) => match state.entries.remove(owner) {
                Some(DrainEntry::Retired { provider, .. }) => Some(provider),
                _ => None,
            },
            None => None,
        };
        drop(state);
        shutdown_detached(shutdown.into_iter().collect());
    }

    /// 代次撤下时同步转移 owner，避免发布与下一次续写之间出现空窗。
    pub(super) fn retire(self: &Arc<Self>, provider: Arc<PluginProvider>) -> bool {
        let now = Instant::now();
        let owner = provider.continuation.owner.clone();
        let mut state = lock(&self.state);
        let mut shutdown = take_expired(&mut state, now);
        let superseded = matches!(
            state.entries.get(&owner),
            Some(DrainEntry::Active {
                superseded: true,
                ..
            })
        );
        state.entries.remove(&owner);
        if !provider
            .continuation
            .issued
            .load(std::sync::atomic::Ordering::Acquire)
            || superseded
            || !provider.session.is_ready()
        {
            drop(state);
            shutdown_detached(shutdown);
            return false;
        }

        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.saturating_add(1);
        state.entries.insert(
            owner.clone(),
            DrainEntry::Retired {
                instance_id: provider.instance_id.clone(),
                provider,
                deadline: now.checked_add(self.config.retention).unwrap_or(now),
                sequence,
            },
        );
        shutdown.extend(trim_to_limits(&mut state, self.config));
        let retained = state.entries.contains_key(&owner);
        let start_reaper = retained && !state.reaper_running;
        if start_reaper {
            state.reaper_running = true;
        }
        drop(state);
        shutdown_detached(shutdown);
        if start_reaper {
            self.start_reaper();
        }
        retained
    }

    /// 新制品占用进程槽前先完成到期回收；未到期 owner 仍受 Supervisor 硬容量约束。
    pub(crate) async fn reap_expired(&self) {
        let providers = take_expired(&mut lock(&self.state), Instant::now());
        shutdown(providers).await;
    }

    /// Runtime 终止时只借用已有 owner 索引收集进程，不建立第二份生命周期权威。
    pub(crate) fn sessions(&self) -> Vec<Arc<crate::RpcSession>> {
        lock(&self.state)
            .entries
            .values()
            .filter_map(|entry| match entry {
                DrainEntry::Active { provider, .. } => provider.upgrade(),
                DrainEntry::Retired { provider, .. } => Some(Arc::clone(provider)),
            })
            .map(|provider| Arc::clone(&provider.session))
            .collect()
    }

    /// 按实例统计仍可恢复的旧 continuation；不暴露 owner 或任何会话状态内容。
    pub(crate) fn retained_by_instance(&self) -> BTreeMap<String, u32> {
        let now = Instant::now();
        let mut retained = BTreeMap::<String, u32>::new();
        for entry in lock(&self.state).entries.values() {
            if let DrainEntry::Retired {
                instance_id,
                deadline,
                ..
            } = entry
                && *deadline > now
            {
                let count = retained.entry(instance_id.clone()).or_default();
                *count = count.saturating_add(1);
            }
        }
        retained
    }

    /// 仅在所有已收集会话确认退出后释放保活引用；重复调用保持幂等。
    pub(crate) fn finish_shutdown(&self) {
        let entries = {
            let mut state = lock(&self.state);
            state.reaper_running = false;
            std::mem::take(&mut state.entries)
        };
        drop(entries);
    }

    fn start_reaper(self: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            lock(&self.state).reaper_running = false;
            return;
        };
        let weak = Arc::downgrade(self);
        runtime.spawn(async move {
            loop {
                let Some(registry) = weak.upgrade() else {
                    return;
                };
                let Some(delay) = registry.next_reap_delay() else {
                    return;
                };
                drop(registry);
                tokio::time::sleep(delay).await;
                let Some(registry) = weak.upgrade() else {
                    return;
                };
                registry.reap_expired().await;
            }
        });
    }

    fn next_reap_delay(&self) -> Option<std::time::Duration> {
        let mut state = lock(&self.state);
        let next = state
            .entries
            .values()
            .filter_map(|entry| match entry {
                DrainEntry::Retired { deadline, .. } => Some(*deadline),
                DrainEntry::Active { .. } => None,
            })
            .min();
        match next {
            Some(deadline) => Some(deadline.saturating_duration_since(Instant::now())),
            None => {
                state.reaper_running = false;
                None
            }
        }
    }
}

impl Drop for ContinuationDrain {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let providers = std::mem::take(&mut state.entries)
            .into_values()
            .filter_map(|entry| match entry {
                DrainEntry::Retired { provider, .. } => Some(provider),
                DrainEntry::Active { .. } => None,
            })
            .collect();
        shutdown_detached(providers);
    }
}

fn take_expired(state: &mut DrainState, now: Instant) -> Vec<Arc<PluginProvider>> {
    let mut expired = Vec::new();
    for (owner, entry) in std::mem::take(&mut state.entries) {
        match entry {
            DrainEntry::Active { provider, .. } if provider.strong_count() == 0 => {}
            DrainEntry::Retired {
                provider, deadline, ..
            } if deadline <= now => expired.push(provider),
            entry => {
                state.entries.insert(owner, entry);
            }
        }
    }
    expired
}

fn trim_to_limits(
    state: &mut DrainState,
    config: ContinuationDrainConfig,
) -> Vec<Arc<PluginProvider>> {
    let mut remove = Vec::new();
    let mut by_instance = BTreeMap::<String, Vec<(u64, String)>>::new();
    let mut all = Vec::new();
    for (owner, entry) in &state.entries {
        if let DrainEntry::Retired {
            instance_id,
            sequence,
            ..
        } = entry
        {
            by_instance
                .entry(instance_id.clone())
                .or_default()
                .push((*sequence, owner.clone()));
            all.push((*sequence, owner.clone()));
        }
    }
    for entries in by_instance.values_mut() {
        entries.sort();
        let excess = entries
            .len()
            .saturating_sub(config.maximum_per_instance.get());
        remove.extend(entries.iter().take(excess).map(|(_, owner)| owner.clone()));
    }
    all.sort();
    let remaining = all.len().saturating_sub(remove.len());
    let global_excess = remaining.saturating_sub(config.maximum_generations.get());
    let removed = remove
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    remove.extend(
        all.into_iter()
            .filter(|(_, owner)| !removed.contains(owner))
            .take(global_excess)
            .map(|(_, owner)| owner),
    );
    remove.sort();
    remove.dedup();
    remove
        .into_iter()
        .filter_map(|owner| match state.entries.remove(&owner) {
            Some(DrainEntry::Retired { provider, .. }) => Some(provider),
            _ => None,
        })
        .collect()
}

fn shutdown_detached(providers: Vec<Arc<PluginProvider>>) {
    if providers.is_empty() {
        return;
    }
    for provider in &providers {
        provider.session.quiesce();
    }
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(shutdown(providers));
    }
}

async fn shutdown(providers: Vec<Arc<PluginProvider>>) {
    stream::iter(providers)
        .for_each_concurrent(Some(8), |provider| async move {
            provider.session.quiesce();
            provider
                .session
                .shutdown(std::time::Duration::from_secs(1))
                .await;
        })
        .await;
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
