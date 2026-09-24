use std::sync::{Arc, Mutex};

use gateway_core::{
    account::OutboundProxy, engine::nested::ExecutionEffects, upstream::UpstreamSendState,
};
use gateway_plugin_sdk::{CallContext, Stage};

/// 一次执行或管理操作的网络事实独立于插件错误，由操作本身保活。
pub(crate) struct NetworkScope {
    stage: Stage,
    account_id: Option<String>,
    credential_revision: Option<u64>,
    pub(super) proxy: Option<OutboundProxy>,
    pub(super) extension_scope: gateway_core::engine::extensions::ExtensionCallScope,
    execution_effects: Option<Arc<ExecutionEffects>>,
    state: Mutex<ExecutionState>,
}

struct ExecutionState {
    active: bool,
    pending: usize,
    sent: bool,
    ambiguous: bool,
}

impl NetworkScope {
    pub(super) fn new(context: &CallContext, proxy: Option<OutboundProxy>) -> Self {
        Self {
            stage: if context.stage == Stage::Attempt {
                Stage::Execution
            } else {
                context.stage
            },
            account_id: context.account_id.clone(),
            credential_revision: context.credential_revision,
            proxy,
            extension_scope: Default::default(),
            execution_effects: None,
            state: Mutex::new(ExecutionState {
                active: context.stage != Stage::Attempt,
                pending: 0,
                sent: false,
                ambiguous: false,
            }),
        }
    }

    pub(super) fn for_call(context: &CallContext) -> Self {
        let mut scope = Self::new(context, None);
        scope.stage = context.stage;
        scope.activate();
        scope
    }

    pub(crate) fn activate(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active = true;
    }

    pub(super) fn with_execution_effects(mut self, effects: Arc<ExecutionEffects>) -> Self {
        self.execution_effects = Some(effects);
        self
    }

    pub(super) fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    pub(super) const fn credential_revision(&self) -> Option<u64> {
        self.credential_revision
    }

    pub(super) fn authorizes(&self, context: &CallContext) -> bool {
        context.stage == self.stage
            && context.account_id == self.account_id
            && context.credential_revision == self.credential_revision
            && self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .active
    }

    pub(super) fn start_http(self: &Arc<Self>) -> HttpAttempt {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending += 1;
        HttpAttempt {
            scope: self.clone(),
            completed: false,
        }
    }

    pub(super) fn observe_nested_execution(&self) {
        // Core 启动子请求时也可能经路由继续嵌套；父侧无法证明完全未发送。
        // 只收紧父 attempt 的重放判断，子请求的费用仍由它自己的 Core 会话结算。
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ambiguous = true;
    }

    pub(crate) fn send_state(&self) -> UpstreamSendState {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.sent {
            UpstreamSendState::Sent
        } else if state.ambiguous || state.pending != 0 {
            UpstreamSendState::Ambiguous
        } else {
            UpstreamSendState::NotSent
        }
    }
}

pub(super) struct HttpAttempt {
    scope: Arc<NetworkScope>,
    completed: bool,
}

impl HttpAttempt {
    pub(super) fn finish(mut self, observed: UpstreamSendState) {
        self.record(observed);
        self.completed = true;
    }

    fn record(&self, observed: UpstreamSendState) {
        let mut state = self
            .scope
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending -= 1;
        state.sent |= observed == UpstreamSendState::Sent;
        state.ambiguous |= observed == UpstreamSendState::Ambiguous;
        drop(state);
        if observed != UpstreamSendState::NotSent
            && let Some(effects) = &self.scope.execution_effects
        {
            effects.observe();
        }
    }
}

impl Drop for HttpAttempt {
    fn drop(&mut self) {
        // send future 被取消时无法证明上游未接收，不能把请求回退成可重试。
        if !self.completed {
            self.record(UpstreamSendState::Ambiguous);
        }
    }
}
