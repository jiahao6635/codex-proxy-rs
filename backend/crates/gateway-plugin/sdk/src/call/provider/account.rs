//! 按需账号管理能力；资料操作不携带账号写入或额度更新指令。
//!
//! 注册时在 `ProviderDescriptor.account_operations` 声明可选操作，同时在清单中声明
//! `account_management` 能力。资料调用使用 Management 阶段和 profile 用途的凭据/HTTP 授权，
//! 重置卡查询及消费的用途与副作用合同见 [`super::reset_credits`]。
//! `provider.profile`、`provider.subscription`、`provider.avatar` 的请求都是 [`AccountRequest`]，
//! 编码为 JSON 二进制载荷，控制参数为空对象。前两个方法的控制结果为空对象，二进制结果
//! 分别为 [`Profile`] 与可空 [`Subscription`]；avatar 的控制结果为 [`Avatar`]，初始载荷为空，
//! 后续通过受信用窗口约束的流帧传递字节，声明了长度时必须与正常结束时的实际字节数一致。
//!
//! 文本字段最多 1024 字节且不含控制字符，日统计与调用排行各最多 4096 项，百分比在 0–100 内。
//! 时间戳以 Unix 毫秒表示，订阅开始时间不得晚于结束时间。结果不更新宿主账号或额度缓存；
//! 凭据版本或上游身份在查询期间发生变化时，宿主拒绝交付旧结果。
//! 返回头像标识须声明 avatar 操作；URL 必须有 HTTP(S) 主机且不含 userinfo。
//! 头像元数据的文本仅允许可打印 ASCII，图像正文不另设累计大小上限。

use serde::{Deserialize, Serialize};

/// 账号连接设置的显式公开字段。
///
/// 字段必须同时存在于凭据轮换 schema，并且其完整 schema 子树不得包含 secret 标记或
/// 间接组合。宿主只接受这些字段的有界投影；未声明时不会调用连接设置 RPC。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfigurationDescriptor {
    pub public_fields: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountOperation {
    /// 已提交停用、冻结或删除后的资源失效通知，不读取凭据。
    Unavailable,
    /// 已提交账号事实变更后的派生缓存失效通知，不读取凭据。
    FactsChanged,
    Profile,
    Subscription,
    Avatar,
    ResetCredits,
    ConsumeResetCredit,
}

/// `provider.account_changed` 的 JSON 二进制输入，控制参数和成功结果均为空对象。
///
/// 声明任一通知操作时，account_management 必须包含 observation 阶段。通知只包含
/// 本实例获准访问的账号 ID，最多 256 个，不能恢复资格或写回账号。事务已经提交；
/// 宿主在两秒总预算内按顺序发送有界批次，不自动重试，也不因通知失败回滚事务。
/// 插件只释放连接或使派生缓存失效；需要重新查询事实时使用后续独立、已授权调用。
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountInvalidation {
    Unavailable { account_id: String },
    FactsChanged { account_ids: Vec<String> },
}

// 凭据只能放在 RPC 二进制载荷内，不参与 Debug 或控制帧诊断。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountRequest {
    pub account_id: String,
    pub credential_revision: u64,
    pub credential: serde_json::Map<String, serde_json::Value>,
}

/// `provider.account_configuration` 的只读结果。
///
/// 结果只包含描述符显式声明的非敏感连接字段。类型故意不实现 `Debug`，即使插件违反
/// 合同返回敏感值，宿主也不会在校验失败时把正文写入诊断。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfiguration {
    pub values: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    pub starts_at_ms: Option<i64>,
    pub expires_at_ms: i64,
    pub will_renew: Option<bool>,
    pub billing_period: Option<String>,
    pub billing_currency: Option<String>,
    pub observed_at_ms: i64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub display_name: Option<String>,
    pub username: Option<String>,
    /// 可公开的 HTTP(S) 头像标识，不得包含认证材料；宿主不请求此 URL，图像走 avatar 流。
    pub image_url: Option<String>,
    pub has_stats_error: bool,
    pub summary: ProfileSummary,
    pub daily_usage: Option<Vec<ProfileDailyUsage>>,
    pub activity_insights: ProfileActivityInsights,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSummary {
    pub total_text_tokens: Option<u64>,
    pub peak_tokens: Option<u64>,
    pub longest_task_duration_ms: Option<u64>,
    pub current_streak_days: Option<u64>,
    pub longest_streak_days: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileDailyUsage {
    /// 严格的 YYYY-MM-DD 日历日期，同一天只能出现一次。
    pub date: String,
    pub tokens: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileInvocation {
    pub invocation_type: String,
    pub plugin_id: Option<String>,
    pub plugin_name: Option<String>,
    pub skill_id: Option<String>,
    pub skill_name: Option<String>,
    pub usage_count: Option<u64>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileActivityInsights {
    pub fast_mode_percent: Option<f64>,
    pub reasoning_effort: Option<String>,
    pub reasoning_effort_percent: Option<f64>,
    pub skills_explored: Option<u64>,
    pub total_skills_used: Option<u64>,
    pub total_threads: Option<u64>,
    pub invocations: Option<Vec<ProfileInvocation>>,
}

/// avatar 调用的初始结果，后续 Stream 帧直接传递原始图像字节。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Avatar {
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub etag: Option<String>,
}
