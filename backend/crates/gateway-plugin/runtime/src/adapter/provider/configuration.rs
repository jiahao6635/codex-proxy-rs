//! 账号连接设置只读投影；凭据原文仅进入获授权的插件调用，响应只接受声明字段。

use std::{collections::BTreeSet, time::Duration};

use gateway_admin::{
    model::{AdminError, provider_credentials::ProviderDocument},
    ports::provider::{ProviderAdminError, ProviderAdminErrorKind},
};
use gateway_core::{account::OpaqueProviderData, account::ProviderAccountId};
use gateway_plugin_sdk::{
    Stage,
    call::{
        auth::CredentialOperation,
        provider::account::{AccountConfiguration, AccountConfigurationDescriptor},
    },
};
use serde_json::{Map, Value, json};

use super::{
    PluginProvider,
    administration::invalid,
    management::{account_request, map_read_error},
};

const MAXIMUM_PUBLIC_FIELDS: usize = 64;
const MAXIMUM_FIELD_BYTES: usize = 128;
const MAXIMUM_CONFIGURATION_BYTES: usize = 64 * 1024;
const INDIRECT_OR_DYNAMIC_SCHEMA_KEYS: [&str; 16] = [
    "$ref",
    "$dynamicRef",
    "$recursiveRef",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "dependentSchemas",
    "dependencies",
    "patternProperties",
    "unevaluatedProperties",
    "unevaluatedItems",
    "contentSchema",
];

pub(super) struct PreparedAccountConfiguration {
    public_fields: BTreeSet<String>,
    validator: jsonschema::Validator,
}

impl PreparedAccountConfiguration {
    pub(super) fn prepare(
        descriptor: Option<AccountConfigurationDescriptor>,
        credential_operations: &BTreeSet<CredentialOperation>,
        rotate_schema: Option<&Value>,
    ) -> Result<Option<Self>, AdminError> {
        let Some(descriptor) = descriptor else {
            return Ok(None);
        };
        if !credential_operations.contains(&CredentialOperation::Rotate)
            || descriptor.public_fields.is_empty()
            || descriptor.public_fields.len() > MAXIMUM_PUBLIC_FIELDS
        {
            return Err(AdminError::invalid(
                "账号连接设置必须绑定已声明的凭据轮换操作",
            ));
        }
        let field_count = descriptor.public_fields.len();
        let public_fields = descriptor
            .public_fields
            .into_iter()
            .collect::<BTreeSet<_>>();
        if public_fields.len() != field_count
            || public_fields.iter().any(|field| {
                field.is_empty()
                    || field.len() > MAXIMUM_FIELD_BYTES
                    || field.chars().any(char::is_control)
            })
        {
            return Err(AdminError::invalid("账号连接设置公开字段无效或重复"));
        }
        let rotate_schema = rotate_schema
            .filter(|schema| safe_configuration_root(schema))
            .ok_or_else(|| AdminError::invalid("账号连接设置需要显式安全的轮换 schema"))?;
        let properties = rotate_schema
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| AdminError::invalid("账号连接设置需要显式的轮换字段 schema"))?;
        let mut projected = Map::new();
        for field in &public_fields {
            let schema = properties
                .get(field)
                .filter(|schema| safe_public_property(schema))
                .ok_or_else(|| AdminError::invalid("账号连接设置公开字段 schema 不安全"))?;
            projected.insert(field.clone(), schema.clone());
        }
        let schema = json!({
            "type": "object",
            "properties": projected,
            "additionalProperties": false,
        });
        let validator = jsonschema::options()
            .offline()
            .with_pattern_options(jsonschema::PatternOptions::fancy_regex().backtrack_limit(20_000))
            .build(&schema)
            .map_err(|_| AdminError::invalid("账号连接设置公开字段 schema 无效"))?;
        Ok(Some(Self {
            public_fields,
            validator,
        }))
    }

    fn validate(&self, values: &Map<String, Value>) -> bool {
        values
            .keys()
            .all(|field| self.public_fields.contains(field))
            && serde_json::to_vec(values)
                .is_ok_and(|encoded| encoded.len() <= MAXIMUM_CONFIGURATION_BYTES)
            && self.validator.is_valid(&Value::Object(values.clone()))
    }
}

impl PluginProvider {
    pub(super) async fn connection_configuration(
        &self,
        id: &ProviderAccountId,
    ) -> Result<Option<ProviderDocument>, ProviderAdminError> {
        let Some(projection) = &self.account_configuration else {
            return Ok(None);
        };
        let credential = self.load_management_credential(id).await?;
        if credential.account.id() != id || credential.account.provider() != &self.kind {
            return Err(ProviderAdminError::new(ProviderAdminErrorKind::Conflict));
        }
        let mut context = self
            .session
            .context(Stage::Configuration, Duration::from_secs(5));
        context.account_id = Some(id.as_str().to_owned());
        context.credential_revision = Some(credential.account.revision().get());
        let payload = serde_json::to_vec(&account_request(&credential)).map_err(|_| invalid())?;
        let reply = self
            .session
            .call(
                "provider.account_configuration",
                context,
                serde_json::json!({}),
                payload,
            )
            .await
            .map_err(map_read_error)?;
        if reply.result != serde_json::json!({}) {
            return Err(ProviderAdminError::new(ProviderAdminErrorKind::BadGateway));
        }
        let result: AccountConfiguration = serde_json::from_slice(&reply.payload)
            .map_err(|_| ProviderAdminError::new(ProviderAdminErrorKind::BadGateway))?;
        if !projection.validate(&result.values) {
            return Err(ProviderAdminError::new(ProviderAdminErrorKind::BadGateway));
        }
        self.ensure_management_identity(&credential.account).await?;
        Ok(Some(ProviderDocument::new(OpaqueProviderData::new(
            result.values,
        ))))
    }
}

fn safe_public_property(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    let scalar_type = object.get("type").and_then(Value::as_str);
    if !matches!(
        scalar_type,
        Some("string" | "number" | "integer" | "boolean")
    ) || object.get("writeOnly") == Some(&Value::Bool(true))
        || object.get("readOnly") == Some(&Value::Bool(true))
        || object.get("format").and_then(Value::as_str) == Some("password")
    {
        return false;
    }
    !contains_key_recursive(schema, &INDIRECT_OR_DYNAMIC_SCHEMA_KEYS)
        && !contains_sensitive_marker_recursive(schema)
}

fn safe_configuration_root(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    object.get("type").and_then(Value::as_str) == Some("object")
        && object.get("writeOnly") != Some(&Value::Bool(true))
        && object.get("readOnly") != Some(&Value::Bool(true))
        && object.get("format").and_then(Value::as_str) != Some("password")
        && !INDIRECT_OR_DYNAMIC_SCHEMA_KEYS
            .iter()
            .any(|key| object.contains_key(*key))
}

fn contains_key_recursive(value: &Value, keys: &[&str]) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            keys.contains(&key.as_str()) || contains_key_recursive(value, keys)
        }),
        Value::Array(values) => values
            .iter()
            .any(|value| contains_key_recursive(value, keys)),
        _ => false,
    }
}

fn contains_sensitive_marker_recursive(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.get("writeOnly") == Some(&Value::Bool(true))
                || object.get("format").and_then(Value::as_str) == Some("password")
                || object.values().any(contains_sensitive_marker_recursive)
        }
        Value::Array(values) => values.iter().any(contains_sensitive_marker_recursive),
        _ => false,
    }
}
