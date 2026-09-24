//! 固定 Host worker 每轮取得 Core 已发布代次；候选和 CLI 不会派发维护。

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use futures::{StreamExt as _, future::BoxFuture};
use gateway_admin::{
    model::{PageSize, Revision, provider_credentials::PluginAccountListQuery},
    ports::provider::ProviderAdmin as _,
};
use gateway_core::{
    account::ProviderAccountId,
    runtime::RuntimeSnapshotHandle,
    task::{
        ScheduledTask, WorkerContribution, WorkerCycleContext, WorkerDefinitionError, WorkerId,
        WorkerLeaseRequest, WorkerRegistration, WorkerRunnable, WorkerSchedule, WorkerTaskError,
    },
};
use tokio::sync::Mutex;

use super::PluginRuntime;
use crate::adapter::provider::{MaintenanceKind, PluginProvider};

const PAGE_SIZE: u16 = 16;
const CONCURRENCY: usize = 4;
const CYCLE_TIMEOUT: Duration = Duration::from_secs(150);

#[derive(Clone)]
pub(super) struct MaintenanceEntry {
    pub instance_id: String,
    pub artifact_sha256: String,
    pub revision: Revision,
    pub provider: Arc<PluginProvider>,
}

struct PluginMaintenanceTask {
    runtime: Arc<PluginRuntime>,
    snapshots: RuntimeSnapshotHandle,
    kind: MaintenanceKind,
    cursors: Mutex<BTreeMap<(String, u64), Option<ProviderAccountId>>>,
    last_instance: Mutex<Option<String>>,
}

impl PluginRuntime {
    /// 只返回任务定义；只有 Host 的统一启动入口可以执行任务。
    pub fn worker_contributions(
        self: &Arc<Self>,
        snapshots: RuntimeSnapshotHandle,
    ) -> Result<Vec<WorkerContribution>, WorkerDefinitionError> {
        MaintenanceKind::ALL
            .into_iter()
            .map(|kind| {
                let (worker_kind, owner, interval) = kind.worker();
                let id = WorkerId::try_new(worker_kind, owner)?;
                let lease_ttl = Duration::from_secs(15 * 60);
                let schedule = WorkerSchedule::try_new(
                    interval,
                    Duration::from_secs(1),
                    Duration::from_secs(60),
                    lease_ttl,
                    Duration::from_secs(5 * 60),
                )?;
                let lease = WorkerLeaseRequest::try_new(id.clone(), lease_ttl)?;
                Ok(WorkerContribution::Registration(
                    WorkerRegistration::try_new(
                        id,
                        WorkerRunnable::Scheduled {
                            schedule,
                            lease: Some(lease),
                            task: Box::new(PluginMaintenanceTask {
                                runtime: self.clone(),
                                snapshots: snapshots.clone(),
                                kind,
                                cursors: Mutex::new(BTreeMap::new()),
                                last_instance: Mutex::new(None),
                            }),
                        },
                    )?,
                ))
            })
            .collect()
    }
}

impl ScheduledTask for PluginMaintenanceTask {
    fn run_cycle(&self, context: WorkerCycleContext) -> BoxFuture<'_, Result<(), WorkerTaskError>> {
        Box::pin(async move {
            tokio::select! {
                () = context.cancellation().cancelled() => Ok(()),
                result = tokio::time::timeout(CYCLE_TIMEOUT, self.run(&context)) => {
                    result.map_err(|_| WorkerTaskError::safe("插件维护轮次超时"))?
                }
            }
        })
    }
}

impl PluginMaintenanceTask {
    async fn run(&self, context: &WorkerCycleContext) -> Result<(), WorkerTaskError> {
        let snapshot = self.snapshots.acquire().map_err(|_| failed())?;
        let Some(reference) = snapshot.extensions() else {
            return Ok(());
        };
        let set = self
            .runtime
            .prepared_set(reference)
            .await
            .map_err(|_| failed())?;
        let current = self
            .runtime
            .store
            .load_instances()
            .await
            .map_err(|_| failed())?;
        let mut entries: Vec<_> = set
            .maintenance
            .iter()
            .filter(|entry| {
                entry.provider.supports_maintenance(self.kind)
                    && current.instances.iter().any(|instance| {
                        instance.enabled
                            && instance.id == entry.instance_id
                            && instance.artifact_sha256 == entry.artifact_sha256
                            && instance.revision == entry.revision
                    })
            })
            .cloned()
            .collect();
        self.cursors.lock().await.retain(|(id, revision), _| {
            entries
                .iter()
                .any(|entry| &entry.instance_id == id && entry.revision.get() == *revision)
        });
        if entries.is_empty() {
            return Ok(());
        }
        if let Some(last) = self.last_instance.lock().await.as_ref()
            && let Some(index) = entries.iter().position(|entry| &entry.instance_id == last)
        {
            let next = (index + 1) % entries.len();
            entries.rotate_left(next);
        }
        let mut failed_instances = 0usize;
        let mut first_failure = None;
        for entry in entries {
            if context.cancellation().is_cancelled() {
                break;
            }
            // 整轮超时也从下一个实例继续，避免慢插件永久饿死后面的插件。
            *self.last_instance.lock().await = Some(entry.instance_id.clone());
            if let Err(error) = self.run_entry(&entry, context).await {
                failed_instances += 1;
                first_failure.get_or_insert(error);
            }
        }
        if let Some(error) = first_failure {
            tracing::warn!(worker = %context.worker(), failed_instances, "插件维护部分实例失败");
            Err(error)
        } else {
            Ok(())
        }
    }

    async fn run_entry(
        &self,
        entry: &MaintenanceEntry,
        context: &WorkerCycleContext,
    ) -> Result<(), WorkerTaskError> {
        if !self.is_current(entry).await? {
            return Ok(());
        }
        let task_id = format!(
            "worker:{}:instance:{}:revision:{}:cycle:{}",
            context.worker(),
            entry.instance_id,
            entry.revision.get(),
            uuid::Uuid::new_v4()
        );
        if self.kind == MaintenanceKind::RequestProfiles {
            return entry
                .provider
                .maintain_request_profiles(&task_id)
                .await
                .map_err(|error| provider_failure(&error));
        }
        let accounts = self.runtime.account_ports.upgrade().map_err(|_| failed())?;
        let cursor_key = (entry.instance_id.clone(), entry.revision.get());
        let cursor = self
            .cursors
            .lock()
            .await
            .get(&cursor_key)
            .cloned()
            .flatten();
        let page = accounts
            .list(PluginAccountListQuery {
                provider_kind: Some(entry.provider.provider_kind().clone()),
                cursor,
                limit: PageSize::new(PAGE_SIZE).map_err(|_| failed())?,
            })
            .await
            .map_err(|_| WorkerTaskError::safe("插件维护读取账号列表失败"))?;
        let mut work = futures::stream::iter(page.accounts)
            .map(|account| async {
                // 获得并发位置后才检查；停用后不继续派发排队账号。
                if !self.is_current(entry).await? {
                    return Ok(());
                }
                match entry
                    .provider
                    .maintain_account(self.kind, account, accounts.as_ref(), &task_id)
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(error)
                        if error.kind()
                            == gateway_admin::ports::provider::ProviderAdminErrorKind::Conflict =>
                    {
                        Ok(())
                    }
                    Err(error) => Err(provider_failure(&error)),
                }
            })
            .buffer_unordered(CONCURRENCY);
        let mut first_failure = None;
        while let Some(result) = work.next().await {
            if let Err(error) = result {
                first_failure.get_or_insert(error);
            }
        }
        self.cursors
            .lock()
            .await
            .insert(cursor_key, page.next_cursor);
        first_failure.map_or(Ok(()), Err)
    }

    async fn is_current(&self, entry: &MaintenanceEntry) -> Result<bool, WorkerTaskError> {
        let snapshot = self
            .runtime
            .store
            .load_instances()
            .await
            .map_err(|_| failed())?;
        Ok(snapshot.instances.iter().any(|instance| {
            instance.enabled
                && instance.id == entry.instance_id
                && instance.artifact_sha256 == entry.artifact_sha256
                && instance.revision == entry.revision
        }))
    }
}

fn failed() -> WorkerTaskError {
    WorkerTaskError::safe("插件维护未完成")
}

fn provider_failure(error: &gateway_admin::ports::provider::ProviderAdminError) -> WorkerTaskError {
    WorkerTaskError::safe(format!("插件维护 Provider 操作失败：{:?}", error.kind()))
}
