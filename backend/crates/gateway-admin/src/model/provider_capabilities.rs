//! 账号页面消费的能力描述；支持声明不代替实际操作时的账号范围与权限校验。

use gateway_core::routing::ProviderKind;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderCredentialDescriptor {
    pub provider: ProviderKind,
    pub capabilities: ProviderCredentialCapabilities,
}

/// 当前账号可用的管理能力，由已发布 Provider 投影；不代替执行时的权限与身份校验。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProviderAccountCapabilities {
    pub quota: bool,
    pub quota_refresh: bool,
    pub profile: bool,
    pub subscription: bool,
    pub avatar: bool,
    pub reset_credits: bool,
    pub consume_reset_credit: bool,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ProviderCredentialCapabilities {
    pub import: Option<Value>,
    pub login: Option<ProviderLoginCapability>,
    pub refresh: bool,
    pub export: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderLoginCapability {
    pub input_schema: Value,
    pub completion: AuthorizationCompletion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationCompletion {
    Callback,
    Poll,
}
