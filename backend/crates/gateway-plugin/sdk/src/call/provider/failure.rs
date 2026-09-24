//! 执行失败的稳定分类及仅供原请求方使用的响应载荷。

use serde::{Deserialize, Serialize};

/// 放在执行事件的二进制载荷中，不通过控制面 fault 或诊断日志传递正文。
/// 实际状态码、请求 ID、公开响应头复用同一封套的 observation。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionFailure {
    pub kind: ExecutionFailureKind,
    pub send_state: crate::SendState,
    pub code: Option<String>,
    pub retry_after_ms: Option<u64>,
    pub response: Option<FailureResponse>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionFailureKind {
    InvalidRequest,
    Unsupported,
    Authentication,
    PermissionDenied,
    RateLimited,
    QuotaExhausted,
    Unavailable,
    Protocol,
    Timeout,
    Cancelled,
}

/// 最多 64 KiB；只能包含当前上游响应，不能放入凭据或请求上下文。
/// response.status 可表示客户端投影状态，不覆盖 observation.status 中的真实上游事实。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureResponse {
    pub status: u16,
    pub content_type: Option<Vec<u8>>,
    pub body: Vec<u8>,
}
