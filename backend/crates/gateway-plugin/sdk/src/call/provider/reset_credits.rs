//! 主动额度重置卡合同；查询不保存库存，消费不直接更改宿主额度或账号状态。
//!
//! `account_management` 能力通过 `account_operations` 分别声明 `reset_credits` 和
//! `consume_reset_credit`。Management 阶段的 `provider.reset_credits` 与
//! `provider.consume_reset_credit` 分别接受 [`super::account::AccountRequest`] 与 [`ConsumeRequest`]；
//! 控制参数与结果均为空对象，输入输出使用 JSON 二进制载荷。
//! 两项操作都需要 accounts 访问域；消费仍由宿主按账号与凭据 revision 约束调用。
//!
//! 两项调用返回 [`Reply`]，数据分别为 [`Credits`] 和 [`ConsumeResult`]。明确上游拒绝与凭据需刷新
//! 属于已完成的业务响应；只有确认本次没有消费时才能返回这两种结果。传输中断、超时或无法判断上游
//! 是否完成必须返回 uncertain 错误，不能伪装为拒绝。通信层不自动重试消费，重试必须保留原始账号、
//! credit_id 和 canonical UUIDv4 redeem_request_id。插件须将该键传给支持幂等的上游，不能自行换键。
//!
//! 文本最多 1024 字节且不含控制字符，列表最多 4096 张卡且 ID 非空、不重复，时间为 Unix 毫秒。
//! code 使用宿主公共语义：reset、already_redeemed、no_credit、nothing_to_reset；未知公开结果保留原值。
//! available_count 可以大于返回的卡片数，以支持上游只提供数量或部分清单的情形。

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumeRequest {
    pub account: super::account::AccountRequest,
    pub credit_id: Option<String>,
    pub redeem_request_id: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "outcome",
    content = "result",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Reply<T> {
    Completed(T),
    CredentialRefreshRequired,
    Rejected,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credit {
    pub id: String,
    pub status: Option<String>,
    pub title: Option<String>,
    pub expires_at_ms: Option<i64>,
    pub reset_type: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credits {
    pub available_count: u64,
    pub credits: Vec<Credit>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumeResult {
    pub code: String,
    pub credit: Option<Credit>,
}
