//! Provider 执行与管理事实；宿主继续拥有账号、额度及提交事务。

pub mod account;
pub mod billing;
mod codec;
mod event;
mod execution;
mod failure;
mod input;
mod maintenance;
pub mod models;
mod observation;
pub mod quota;
mod request_profile;
pub mod reset_credits;

pub use codec::{ExecutionEncodingError, MAX_EXECUTION_PAYLOAD_BYTES};
pub use event::{CanonicalEvent, ExecutionEvent, SessionUpdate, WireEvent, WirePayload};
pub use execution::{
    ContentKind, ContinuationStateDescriptor, ExecutePrepared, FinishReason, ModelDescriptor,
    ModelFeature, OperationKind, PrepareExecution, PreparedExecution, ProbeOperation,
    ProviderDescriptor, ProviderHttpEndpoint, ProviderHttpHeader, ProviderHttpMethod,
    ProviderHttpRequest, Registration, Usage,
};
pub use failure::{ExecutionFailure, ExecutionFailureKind, FailureResponse};
pub use input::ExecutionInput;
pub use maintenance::RequestProfileRefresh;
pub use observation::{HttpVersion, ResponseHeader, ResponseObservation, ResponseTimings};
pub use request_profile::{
    RequestProfileAttribute, RequestProfileDescriptor, RequestProfileOption,
    RequestProfilePresentation, RequestProfileRelease, RequestProfileReleaseStatus,
    RequestProfileTarget,
};
