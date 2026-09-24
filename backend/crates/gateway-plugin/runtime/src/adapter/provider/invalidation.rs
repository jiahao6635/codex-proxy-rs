use std::{collections::BTreeSet, time::Duration};

use gateway_core::account::ProviderAccountId;
use gateway_plugin_sdk::{
    Stage,
    call::provider::account::{AccountInvalidation, AccountOperation},
};
use tokio::time::Instant;

use super::PluginProvider;

const TIMEOUT: Duration = Duration::from_secs(2);
const BATCH_SIZE: usize = 256;

impl PluginProvider {
    pub(super) async fn invalidate_unavailable(&self, account: &ProviderAccountId) {
        if !self
            .account_operations
            .contains(&AccountOperation::Unavailable)
        {
            return;
        }
        let _ = self
            .send_invalidation(
                &AccountInvalidation::Unavailable {
                    account_id: account.as_str().to_owned(),
                },
                Instant::now() + TIMEOUT,
            )
            .await;
    }

    pub(super) async fn invalidate_facts(&self, accounts: &[ProviderAccountId]) {
        if !self
            .account_operations
            .contains(&AccountOperation::FactsChanged)
        {
            return;
        }
        // 一轮固定总预算，不能让账号数量把已经提交的管理请求拖成无界等待。
        let deadline = Instant::now() + TIMEOUT;
        let accounts: BTreeSet<_> = accounts.iter().collect();
        let mut accounts = accounts.into_iter();
        loop {
            let account_ids: Vec<_> = accounts
                .by_ref()
                .take(BATCH_SIZE)
                .map(|account| account.as_str().to_owned())
                .collect();
            if account_ids.is_empty()
                || !self
                    .send_invalidation(&AccountInvalidation::FactsChanged { account_ids }, deadline)
                    .await
            {
                return;
            }
        }
    }

    async fn send_invalidation(&self, event: &AccountInvalidation, deadline: Instant) -> bool {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tracing::warn!(provider = self.kind.as_str(), "插件账号失效通知总预算耗尽");
            return false;
        }
        let Ok(payload) = serde_json::to_vec(event) else {
            return false;
        };
        // 只发送变更身份，凭据仍需通过独立的账号回调读取。
        let call = self.session.call(
            "provider.account_changed",
            self.session.context(Stage::Observation, remaining),
            serde_json::json!({}),
            payload,
        );
        let completed = matches!(
            tokio::time::timeout_at(deadline, call).await,
            Ok(Ok(reply)) if reply.result == serde_json::json!({}) && reply.payload.is_empty()
        );
        if !completed {
            tracing::warn!(
                provider = self.kind.as_str(),
                "插件账号失效通知未完成，已提交账号事实保持不变"
            );
        }
        completed
    }
}
