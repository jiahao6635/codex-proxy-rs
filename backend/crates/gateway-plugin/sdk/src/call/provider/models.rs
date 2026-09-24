//! 账号模型发现；`provider.models` 以 [`super::account::AccountRequest`] 为二进制输入，
//! 返回 [`AccountModels`] JSON 二进制载荷，控制参数和结果均为空对象。
//!
//! 未声明发现时只使用注册的静态 models。声明发现需要 models 能力以及 accounts 访问域；
//! 若插件通过宿主 HTTP 查询上游，还需要 network 访问域。目录结果不携带账号写入，
//! 更新凭据或其他权威事实必须经过独立的 prepared facts 与宿主提交合同。

use serde::{Deserialize, Serialize};

use super::ModelDescriptor;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDiscovery {
    /// 为 true 时静态模型适用于每个账号；同 ID 的发现结果覆盖该账号的静态声明。
    pub include_static: bool,
    /// 目录缓存有效期，必须在 1–3600 秒内；账号事实变化会提前失效。
    pub cache_ttl_seconds: u32,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountModels {
    /// 每次最多 4096 项；模型 ID、操作、特性均不得重复。
    pub models: Vec<ModelDescriptor>,
    /// 只有完整目录能因缺项拒绝账号；发现型目录的未知模型交由上游验证。
    pub exhaustive: bool,
    /// 可选的已准备账号事实；目标账号、Provider 和 revision 由宿主调用上下文绑定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_account_facts: Option<crate::call::auth::CredentialFacts>,
}

impl std::fmt::Debug for AccountModels {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccountModels")
            .field("models", &self.models)
            .field("exhaustive", &self.exhaustive)
            .field(
                "prepared_account_facts",
                &self.prepared_account_facts.as_ref().map(|_| "[PREPARED]"),
            )
            .finish()
    }
}
