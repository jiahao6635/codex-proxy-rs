//! Provider 额度合同；展示窗口与账号准入事实分别声明，宿主不从百分比推断是否禁止调度。

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quota {
    pub plan_type: Option<String>,
    pub refresh_token_expires_at_ms: Option<i64>,
    pub access: QuotaAccess,
    pub windows: Vec<QuotaWindow>,
    pub provider_data: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuotaAccess {
    Unknown,
    Allowed,
    Exhausted {
        evidence: QuotaEvidence,
        reset_at_ms: Option<i64>,
    },
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaEvidence {
    ProviderDenied,
    AccountLimitReached,
    UsageLimitReached,
    PaymentRequired,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaWindow {
    pub key: String,
    pub group: String,
    pub label: String,
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub role: Option<QuotaWindowRole>,
    #[serde(default)]
    pub account_wide: bool,
    pub window_seconds: Option<u64>,
    pub used_percent: Option<f64>,
    pub reset_at_ms: Option<i64>,
    pub limit_reached: bool,
    pub provider_data: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaWindowRole {
    Primary,
    Secondary,
    Monthly,
}

/// 已筛选的上游额度观测，不携带凭据、任意响应头或供应商原始文档。
///
/// 宿主最多接受 64 个窗口及 32 KiB 持久化元数据；未知事实应省略整份观测。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaForecast {
    pub plan_type: Option<String>,
    pub windows: Vec<QuotaForecastWindow>,
}

/// 窗口身份必须与 `provider.quota` 的对应窗口完全一致；不以显示名称猜测关联。
///
/// `account_wide` 只有在本账号的所有模型用量均可归入该窗口时才能为 true。
/// 观测不含账号或 Provider 标识，关联身份由宿主实际执行上下文确定。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaForecastWindow {
    pub key: String,
    pub group: String,
    pub limit_id: Option<String>,
    pub role: Option<QuotaWindowRole>,
    #[serde(default)]
    pub account_wide: bool,
    pub window_seconds: u64,
    pub used_percent: f64,
    pub reset_at_ms: i64,
}
