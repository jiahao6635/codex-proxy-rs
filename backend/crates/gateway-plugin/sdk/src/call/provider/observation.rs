//! Provider 上游响应的安全观测；不会直接作为客户端正文输出。

use serde::{Deserialize, Serialize};

/// 不实现 Debug：请求标识及响应头属于上游私有数据。
/// transport 由宿主采用已准备调用的值，插件不能改写执行身份。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseObservation {
    pub status: Option<u16>,
    pub request_id: Option<String>,
    pub service_tier: Option<String>,
    pub response_model: Option<String>,
    pub http_version: Option<HttpVersion>,
    #[serde(default)]
    pub timings: ResponseTimings,
    /// 仅交付已筛选的客户端可公开头；宿主仍校验格式及禁止敏感/逐跳头。
    #[serde(default)]
    pub client_headers: Vec<ResponseHeader>,
    /// 同次上游响应的额度进度，仅供历史配对预测；不改变账号准入、用量或账单。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_forecast: Option<super::quota::QuotaForecast>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeader {
    pub name: String,
    pub value: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpVersion {
    Http09,
    Http10,
    Http11,
    Http2,
    Http3,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseTimings {
    pub transport_decision_wait_ms: Option<u64>,
    pub connect_ms: Option<u64>,
    pub headers_ms: Option<u64>,
    pub first_event_ms: Option<u64>,
    pub first_reasoning_ms: Option<u64>,
    pub first_text_ms: Option<u64>,
    pub first_token_ms: Option<u64>,
    pub provider_processing_ms: Option<u64>,
}
