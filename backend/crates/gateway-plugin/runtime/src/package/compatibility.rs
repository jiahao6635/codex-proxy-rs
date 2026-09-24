use std::sync::OnceLock;

use gateway_admin::model::{
    AdminError,
    plugins::{PluginCompatibilityRequirements, PluginHostCompatibility},
};
use gateway_plugin_sdk::Manifest;

const HOST_COMPATIBILITY_JSON: &str = include_str!("../../plugin-host-compatibility.json");

pub(crate) fn host_compatibility() -> Result<&'static PluginHostCompatibility, AdminError> {
    static COMPATIBILITY: OnceLock<Result<PluginHostCompatibility, ()>> = OnceLock::new();
    COMPATIBILITY
        .get_or_init(|| {
            let compatibility =
                serde_json::from_str::<PluginHostCompatibility>(HOST_COMPATIBILITY_JSON)
                    .map_err(|_| ())?;
            compatibility.is_valid().then_some(compatibility).ok_or(())
        })
        .as_ref()
        .map_err(|()| AdminError::internal("宿主插件兼容声明不合法"))
}

pub(crate) fn requirements(
    manifest: &Manifest,
) -> Result<PluginCompatibilityRequirements, AdminError> {
    let package = manifest
        .package
        .as_ref()
        .ok_or_else(|| AdminError::invalid("插件包缺少构建元数据"))?;
    Ok(PluginCompatibilityRequirements {
        host_version: manifest.engines.codex_proxy_rs.to_string(),
        manifest_schema_version: manifest.manifest_version,
        protocol_version: package.protocol_version,
        capabilities: manifest
            .contributes
            .iter()
            .map(|(capability, declaration)| Ok((identifier(*capability)?, declaration.version)))
            .collect::<Result<_, AdminError>>()?,
        permissions: manifest
            .permissions
            .iter()
            .map(|permission| identifier(*permission))
            .collect::<Result<_, _>>()?,
    })
}

pub(crate) fn supports(manifest: &Manifest) -> Result<bool, AdminError> {
    let compatibility = host_compatibility()?;
    let requirements = requirements(manifest)?;
    Ok(compatibility
        .manifest_schema_versions
        .contains(&requirements.manifest_schema_version)
        && compatibility
            .protocol_versions
            .contains(&requirements.protocol_version)
        && requirements
            .capabilities
            .iter()
            .all(|(capability, version)| compatibility.supports_capability(capability, *version))
        && requirements
            .permissions
            .iter()
            .all(|permission| compatibility.supports_permission(permission)))
}

fn identifier(value: impl serde::Serialize) -> Result<String, AdminError> {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| AdminError::internal("插件描述转换失败"))
}
