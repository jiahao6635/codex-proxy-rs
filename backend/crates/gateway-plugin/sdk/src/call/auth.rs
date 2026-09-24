//! 认证与凭据调用合同；输入输出放在帧载荷中，账号 ID、版本和提交事务由宿主生成。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialOperation {
    Import,
    Export,
    Rotate,
    Refresh,
    Login,
}

// 凭据文档与账号资料不可通过 Debug 写入诊断。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialFacts {
    pub name: String,
    pub authentication_kind: String,
    pub material: Map<String, Value>,
    pub email: Option<String>,
    pub upstream_user_id: Option<String>,
    pub upstream_account_id: Option<String>,
    pub plan_type: Option<String>,
    #[serde(default)]
    pub has_refresh_token: bool,
    pub access_token_expires_at_ms: Option<i64>,
    pub next_refresh_at_ms: Option<i64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportedCredentials {
    pub accounts: Vec<CredentialFacts>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportCredential {
    pub account_id: String,
    pub credential_revision: u64,
    pub facts: CredentialFacts,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RotateCredential {
    pub current: ExportCredential,
    pub replacement: Option<Map<String, Value>>,
}

/// 登录上下文不包含管理员身份；流程与账号归属由宿主绑定。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginStart {
    pub name: Option<String>,
    pub account_id: Option<String>,
    #[serde(default)]
    pub input: Map<String, Value>,
}

/// state 只在加密临时存储与插件载荷间流转，不返回浏览器。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginStarted {
    pub authorization_url: String,
    pub expires_at_ms: i64,
    pub state: Map<String, Value>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginPoll {
    pub state: Map<String, Value>,
    pub callback_url: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum LoginPollResult {
    Pending { retry_after_ms: u64 },
    Complete { accounts: Vec<CredentialFacts> },
}
