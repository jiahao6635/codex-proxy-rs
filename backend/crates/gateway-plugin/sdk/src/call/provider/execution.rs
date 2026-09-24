//! Provider 的版本化数据合同；请求正文通过帧的独立二进制载荷传递。

use serde::{Deserialize, Serialize};

use crate::{Contributions, call::auth::CredentialOperation};

use super::account::AccountOperation;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeOperation {
    pub protocol: String,
    pub body: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    #[serde(deserialize_with = "crate::capability::deserialize_contributions")]
    pub contributes: Contributions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDescriptor {
    pub id: String,
    pub models: Vec<ModelDescriptor>,
    #[serde(default)]
    pub credential_operations: Vec<CredentialOperation>,
    /// 输入结构与表单提示；只能为已声明的 import、rotate、login 操作提供对象 schema。
    /// 每份最多 64 KiB，仅允许文档内部引用；缺省的 login 接受空对象，import/rotate 接受任意对象。
    #[serde(default)]
    pub credential_input_schemas:
        std::collections::BTreeMap<CredentialOperation, serde_json::Value>,
    /// 从当前凭据投影到账号页的非敏感连接字段；写入仍复用 rotate schema 与事务。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_configuration: Option<super::account::AccountConfigurationDescriptor>,
    /// 可选的账号管理操作；必须同时声明 account_management 能力。
    #[serde(default)]
    pub account_operations: Vec<AccountOperation>,
    #[serde(default)]
    pub exhaustive: bool,
    /// 已校验的本地计价数据；必须同时声明 billing 能力。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<super::billing::BillingDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_discovery: Option<super::models::ModelDiscovery>,
    /// 可选的请求画像目录；宿主准备后在请求同步路径中本地解析。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_profiles: Option<super::request_profile::RequestProfileDescriptor>,
    /// 由 Provider 显式登记的账号认证 HTTP 操作。`id` 只是符号名，不能是 URL。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub http_endpoints: Vec<ProviderHttpEndpoint>,
    /// 原生续写状态的显式兼容合同；缺省时任何制品或实例 revision 变化都严格拒绝旧状态。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_state: Option<ContinuationStateDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationStateDescriptor {
    /// 插件与宿主都视为不透明的稳定格式标识。
    pub format: String,
    /// 当前版本写入检查点时使用的非零格式版本。
    pub write_version: u32,
    /// 当前版本能够读取的格式版本；必须有界、去重并包含 `write_version`。
    pub readable_versions: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDescriptor {
    pub id: String,
    pub operations: Vec<OperationKind>,
    #[serde(default)]
    pub features: Vec<ModelFeature>,
    #[serde(default)]
    pub maximum_output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Generate,
    GenerateImage,
    Search,
    CountTokens,
    ProviderHttp,
}

/// 插件公开的一个受限 HTTP 操作。实际 origin 与路径只由插件实现持有。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderHttpEndpoint {
    pub id: String,
    pub methods: Vec<ProviderHttpMethod>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHttpMethod {
    Get,
    Post,
}

/// API 已过滤的入口 HTTP 信封；凭据、origin 与最终路径不由客户端提供。
///
/// Header 值可能包含不适合日志的客户端数据，因此该类型故意不实现 `Debug`。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderHttpRequest {
    pub endpoint: String,
    pub method: ProviderHttpMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<ProviderHttpHeader>,
}

/// 保留多值 header 的单个有序项。
///
/// 值按 HTTP 原始字节保存，因此该类型故意不实现 `Debug`。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderHttpHeader {
    pub name: String,
    pub value: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFeature {
    Tools,
    Vision,
    Reasoning,
    JsonSchema,
    /// Provider 能使用自己的响应标识与状态继续调用；不代表 WS 连接内保活。
    NativeContinuation,
}

// 不派生 Debug：配置、上下文及凭据内容不进入宿主或插件诊断。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareExecution {
    pub protocol: String,
    pub operation: OperationKind,
    pub model: Option<String>,
    pub account_id: String,
    pub credential_revision: u64,
    pub credential: serde_json::Map<String, serde_json::Value>,
    pub context: serde_json::Map<String, serde_json::Value>,
    pub request_profile: Option<serde_json::Value>,
    /// Attempt 中间件追加且经宿主复核的业务 header；不含凭据或传输管理字段。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<ProviderHttpHeader>,
    /// 宿主校验账号、凭据版本及实例授权后恢复的插件私有状态，不包含宿主绑定信息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_state: Option<serde_json::Map<String, serde_json::Value>>,
    /// 只有 [`OperationKind::ProviderHttp`] 可携带该信封。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_request: Option<ProviderHttpRequest>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedExecution {
    pub token: String,
    pub transport: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutePrepared {
    pub token: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Text,
    Reasoning,
    ToolCall,
    Image,
    Audio,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCall,
    ContentFilter,
    Other,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub image_input_tokens: Option<u64>,
    pub image_output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}
