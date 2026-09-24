use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gateway_admin::model::AdminError;
use gateway_core::{
    error::{
        ContinuationFailure, ContinuationRecoveryDisposition, ProviderError, ProviderErrorKind,
    },
    operation::ProviderSessionState,
    upstream::UpstreamSendState,
};
use gateway_plugin_sdk::call::provider::{ContinuationStateDescriptor, SessionUpdate};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_FORMAT_BYTES: usize = 64;
const MAX_READABLE_VERSIONS: usize = 32;

#[derive(Clone)]
pub(super) struct PreparedContinuationState {
    format: String,
    write_version: u32,
    readable_versions: BTreeSet<u32>,
}

impl PreparedContinuationState {
    pub(super) fn prepare(
        descriptor: Option<ContinuationStateDescriptor>,
    ) -> Result<Option<Self>, AdminError> {
        let Some(descriptor) = descriptor else {
            return Ok(None);
        };
        let format_is_valid = !descriptor.format.is_empty()
            && descriptor.format.len() <= MAX_FORMAT_BYTES
            && descriptor.format != "."
            && descriptor.format != ".."
            && descriptor
                .format
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        let readable_versions = descriptor
            .readable_versions
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if !format_is_valid
            || descriptor.write_version == 0
            || descriptor.readable_versions.is_empty()
            || descriptor.readable_versions.len() > MAX_READABLE_VERSIONS
            || readable_versions.len() != descriptor.readable_versions.len()
            || readable_versions.contains(&0)
            || !readable_versions.contains(&descriptor.write_version)
        {
            return Err(AdminError::invalid("插件续写状态兼容声明无效"));
        }
        Ok(Some(Self {
            format: descriptor.format,
            write_version: descriptor.write_version,
            readable_versions,
        }))
    }

    fn accepts(&self, state: &StoredContinuationState) -> bool {
        self.format == state.format && self.readable_versions.contains(&state.version)
    }

    fn stored(&self) -> StoredContinuationState {
        StoredContinuationState {
            format: self.format.clone(),
            version: self.write_version,
        }
    }
}

#[derive(Clone)]
pub(super) struct ContinuationIdentity {
    provider: String,
    authorization: String,
    continuation_authorization: String,
    continuation_state: Option<PreparedContinuationState>,
    pub(super) owner: String,
    pub(super) issued: Arc<AtomicBool>,
}

pub(super) enum InterpreterTarget {
    Current { previous_owner: Option<String> },
    Retired { owner: String },
}

/// 绑定来自已选择的账号与已发布实例，插件只能提供 opaque payload。
#[derive(Clone)]
pub(super) struct SessionBinding {
    identity: ContinuationIdentity,
    account_id: String,
    credential_revision: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSessionBinding {
    provider: String,
    account_id: String,
    credential_revision: u64,
    authorization: String,
    continuation_authorization: String,
    execution_owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    continuation_state: Option<StoredContinuationState>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredContinuationState {
    format: String,
    version: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    binding: StoredSessionBinding,
    payload: Map<String, Value>,
}

impl ContinuationIdentity {
    pub(super) fn prepare(
        provider: String,
        authorization: String,
        continuation_authorization: String,
        descriptor: Option<ContinuationStateDescriptor>,
        owner: String,
    ) -> Result<Self, AdminError> {
        Ok(Self {
            provider,
            authorization,
            continuation_authorization,
            continuation_state: PreparedContinuationState::prepare(descriptor)?,
            owner,
            issued: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(super) fn interpreter_target(
        &self,
        state: Option<&ProviderSessionState>,
    ) -> Result<InterpreterTarget, ProviderError> {
        let Some(state) = state else {
            return Ok(InterpreterTarget::Current {
                previous_owner: None,
            });
        };
        let checkpoint = parse_checkpoint(state)?;
        if state.provider() != self.provider
            || checkpoint.binding.provider != self.provider
            || checkpoint.binding.continuation_authorization != self.continuation_authorization
            || !valid_owner(&checkpoint.binding.execution_owner)
            || !bounded(state.payload())
        {
            return Err(invalid_checkpoint());
        }
        if self.can_interpret(&checkpoint.binding) {
            return Ok(InterpreterTarget::Current {
                previous_owner: (checkpoint.binding.execution_owner != self.owner)
                    .then_some(checkpoint.binding.execution_owner),
            });
        }
        if checkpoint.binding.execution_owner == self.owner {
            return Err(invalid_checkpoint());
        }
        Ok(InterpreterTarget::Retired {
            owner: checkpoint.binding.execution_owner,
        })
    }

    pub(super) fn bind_account(
        &self,
        account_id: String,
        credential_revision: u64,
    ) -> SessionBinding {
        SessionBinding {
            identity: self.clone(),
            account_id,
            credential_revision,
        }
    }

    pub(super) fn matches_authority(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.continuation_authorization == other.continuation_authorization
    }

    fn can_interpret(&self, stored: &StoredSessionBinding) -> bool {
        match (&self.continuation_state, &stored.continuation_state) {
            (None, None) => self.authorization == stored.authorization,
            (Some(current), Some(stored)) => current.accepts(stored),
            _ => false,
        }
    }
}

impl SessionBinding {
    pub(super) fn restore(
        &self,
        state: Option<&ProviderSessionState>,
    ) -> Result<Option<Map<String, Value>>, ProviderError> {
        let Some(state) = state else {
            return Ok(None);
        };
        let checkpoint = parse_checkpoint(state)?;
        if state.provider() != self.identity.provider
            || checkpoint.binding.provider != self.identity.provider
            || self.account_id != checkpoint.binding.account_id
            || self.credential_revision != checkpoint.binding.credential_revision
            || self.identity.continuation_authorization
                != checkpoint.binding.continuation_authorization
            || !self.identity.can_interpret(&checkpoint.binding)
            || !valid_owner(&checkpoint.binding.execution_owner)
            || !bounded(state.payload())
        {
            return Err(invalid_checkpoint());
        }
        Ok(Some(checkpoint.payload))
    }

    pub(super) fn capture(
        &self,
        update: SessionUpdate,
    ) -> Result<ProviderSessionState, ProviderError> {
        let Value::Object(payload) = serde_json::to_value(Checkpoint {
            binding: self.stored(),
            payload: update.payload,
        })
        .map_err(|_| super::execution::stream_protocol_error())?
        else {
            return Err(super::execution::stream_protocol_error());
        };
        if !bounded(&payload) {
            return Err(super::execution::stream_protocol_error());
        }
        let state = ProviderSessionState::new(self.identity.provider.clone(), payload)
            .map_err(|_| super::execution::stream_protocol_error())?;
        self.identity.issued.store(true, Ordering::Release);
        Ok(state)
    }

    fn stored(&self) -> StoredSessionBinding {
        StoredSessionBinding {
            provider: self.identity.provider.clone(),
            account_id: self.account_id.clone(),
            credential_revision: self.credential_revision,
            authorization: self.identity.authorization.clone(),
            continuation_authorization: self.identity.continuation_authorization.clone(),
            execution_owner: self.identity.owner.clone(),
            continuation_state: self
                .identity
                .continuation_state
                .as_ref()
                .map(PreparedContinuationState::stored),
        }
    }
}

fn parse_checkpoint(state: &ProviderSessionState) -> Result<Checkpoint, ProviderError> {
    serde_json::from_value(Value::Object(state.payload().clone())).map_err(|_| invalid_checkpoint())
}

fn valid_owner(owner: &str) -> bool {
    uuid::Uuid::parse_str(owner).is_ok()
}

fn bounded(payload: &Map<String, Value>) -> bool {
    serde_json::to_vec(payload).is_ok_and(|bytes| bytes.len() <= MAX_PAYLOAD_BYTES)
}

pub(super) fn invalid_checkpoint() -> ProviderError {
    // 不能在凭据轮换、授权变更或账号替换后静默丢状态并重发；由客户端显式开新链。
    ProviderError::new(
        ProviderErrorKind::InvalidRequest,
        UpstreamSendState::NotSent,
    )
    .with_continuation_failure(ContinuationFailure::HistoryUnavailable)
    .with_continuation_recovery_disposition(ContinuationRecoveryDisposition::ClientReplayRequired)
}
