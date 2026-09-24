//! 表单 schema 在准备代次时编译；调用时只校验当前代次的输入，不解析外部引用。

use std::collections::{BTreeMap, BTreeSet};

use gateway_admin::{
    model::{
        AdminError,
        provider_capabilities::{
            AuthorizationCompletion, ProviderCredentialCapabilities, ProviderLoginCapability,
        },
        provider_credentials::ProviderDocument,
    },
    ports::provider::ProviderAdminError,
};
use gateway_plugin_sdk::call::auth::CredentialOperation;
use serde_json::{Value, json};

use super::{PluginProvider, administration::invalid};

pub(super) struct CredentialInput {
    schema: Value,
    validator: jsonschema::Validator,
}

pub(super) fn prepare_inputs(
    operations: &BTreeSet<CredentialOperation>,
    mut schemas: BTreeMap<CredentialOperation, Value>,
) -> Result<BTreeMap<CredentialOperation, CredentialInput>, AdminError> {
    if schemas.keys().any(|operation| {
        !operations.contains(operation)
            || !matches!(
                operation,
                CredentialOperation::Import
                    | CredentialOperation::Rotate
                    | CredentialOperation::Login
            )
    }) {
        return Err(AdminError::invalid("凭据表单只能描述已声明的输入操作"));
    }
    let mut inputs = BTreeMap::new();
    for operation in operations.iter().copied().filter(|operation| {
        matches!(
            operation,
            CredentialOperation::Import | CredentialOperation::Rotate | CredentialOperation::Login
        )
    }) {
        let schema = schemas.remove(&operation).unwrap_or_else(|| {
            json!({
                "type": "object",
                "additionalProperties": operation != CredentialOperation::Login,
            })
        });
        if schema.get("type").and_then(Value::as_str) != Some("object")
            || serde_json::to_vec(&schema).map_or(true, |encoded| encoded.len() > 64 * 1024)
        {
            return Err(AdminError::invalid(
                "凭据表单 schema 必须描述对象且不超过 64 KiB",
            ));
        }
        let validator = jsonschema::options()
            .offline()
            .with_pattern_options(jsonschema::PatternOptions::fancy_regex().backtrack_limit(20_000))
            .build(&schema)
            .map_err(|_| AdminError::invalid("凭据表单 schema 无效或引用外部资源"))?;
        inputs.insert(operation, CredentialInput { schema, validator });
    }
    Ok(inputs)
}

impl PluginProvider {
    pub(super) fn credential_capabilities(&self) -> ProviderCredentialCapabilities {
        let schema = |operation| {
            self.credential_inputs
                .get(&operation)
                .map(|input| input.schema.clone())
        };
        ProviderCredentialCapabilities {
            import: schema(CredentialOperation::Import),
            login: schema(CredentialOperation::Login).map(|input_schema| ProviderLoginCapability {
                input_schema,
                completion: AuthorizationCompletion::Poll,
            }),
            refresh: self
                .credential_operations
                .contains(&CredentialOperation::Refresh),
            export: self
                .credential_operations
                .contains(&CredentialOperation::Export),
        }
    }

    pub(super) fn validate_credential_input(
        &self,
        operation: CredentialOperation,
        document: &ProviderDocument,
    ) -> Result<(), ProviderAdminError> {
        let input = self
            .credential_inputs
            .get(&operation)
            .ok_or_else(super::administration::unsupported)?;
        let value = Value::Object(document.expose_to_provider().expose_to_provider().clone());
        if operation == CredentialOperation::Login
            && serde_json::to_vec(&value).map_or(true, |encoded| encoded.len() > 64 * 1024)
            || !input.validator.is_valid(&value)
        {
            // 校验错误可能携带输入值；仅返回稳定分类，不能把错误正文写入日志或响应。
            return Err(invalid());
        }
        Ok(())
    }
}
