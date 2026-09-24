//! Provider 声明的请求画像选项；宿主在准备阶段校验并冻结本地解析目录。

use serde::{Deserialize, Serialize};

/// 一个 Provider 的有界请求画像目录。
///
/// `configuration` 是持久化的稳定选择，`resolved` 是本次请求交给插件的实际画像。
/// 插件刷新版本资料时应保留 configuration，并在新目录中替换 resolved/presentation。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestProfileDescriptor {
    pub default_configuration: serde_json::Map<String, serde_json::Value>,
    pub options: Vec<RequestProfileOption>,
}

impl std::fmt::Debug for RequestProfileDescriptor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestProfileDescriptor")
            .field("default_configuration", &"[REDACTED]")
            .field("options", &self.options)
            .finish()
    }
}

/// 一个可选画像；宿主不解释 Provider-owned 的 configuration/resolved 字段。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestProfileOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub configuration: serde_json::Map<String, serde_json::Value>,
    pub resolved: serde_json::Map<String, serde_json::Value>,
    pub presentation: RequestProfilePresentation,
}

impl std::fmt::Debug for RequestProfileOption {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestProfileOption")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("description", &self.description)
            .field("configuration", &"[REDACTED]")
            .field("resolved", &"[REDACTED]")
            .field("presentation", &self.presentation)
            .finish()
    }
}

/// Provider 已生成的实际上游身份展示；宿主不从 resolved 字段重新推导这些值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestProfilePresentation {
    pub product: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    pub target: RequestProfileTarget,
    pub user_agent: String,
    #[serde(default)]
    pub attributes: Vec<RequestProfileAttribute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<RequestProfileRelease>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestProfileTarget {
    pub os_type: String,
    pub os_version: String,
    pub arch: String,
    pub terminal: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestProfileAttribute {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestProfileRelease {
    pub status: RequestProfileReleaseStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_build: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_system_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_requirements: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_present: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestProfileReleaseStatus {
    Unchecked,
    Current,
    UpdateAvailable,
    Failed,
}
