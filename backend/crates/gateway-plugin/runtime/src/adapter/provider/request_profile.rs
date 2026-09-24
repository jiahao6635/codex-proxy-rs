//! 请求画像在准备和版本刷新时校验；同步路径读取本地目录快照，已解析请求不随刷新变化。

use std::{
    collections::BTreeSet,
    sync::{Arc, RwLock},
};

use chrono::{DateTime, Utc};
use gateway_admin::{
    model::{
        AdminError,
        observability::{
            DashboardDesktopRelease, DashboardWireAttribute, DashboardWireProfile,
            DashboardWireTarget, DesktopReleaseStatus,
        },
    },
    ports::provider::{ProviderAdminError, ProviderAdminErrorKind},
};
use gateway_core::{
    account::OpaqueProviderData,
    error::{ProviderError, ProviderErrorKind},
    routing::ProviderKind,
    upstream::UpstreamSendState,
};
use gateway_plugin_sdk::call::provider::{
    RequestProfileDescriptor, RequestProfileOption, RequestProfilePresentation,
    RequestProfileRelease, RequestProfileReleaseStatus,
};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

const MAXIMUM_OPTIONS: usize = 128;
const MAXIMUM_DOCUMENT_BYTES: usize = 64 * 1024;
const MAXIMUM_ID_BYTES: usize = 64;
const MAXIMUM_LABEL_BYTES: usize = 128;
const MAXIMUM_DESCRIPTION_BYTES: usize = 1024;
const MAXIMUM_PRESENTATION_TEXT_BYTES: usize = 4096;
const MAXIMUM_ATTRIBUTES: usize = 64;

pub(super) struct PreparedRequestProfiles {
    current: RwLock<Arc<ProfileDirectory>>,
}

struct ProfileDirectory {
    sequence: u64,
    fingerprint: [u8; 32],
    default: usize,
    options: Vec<PreparedRequestProfile>,
}

struct PreparedRequestProfile {
    option: RequestProfileOption,
    presentation: DashboardWireProfile,
}

impl PreparedRequestProfiles {
    pub(super) fn prepare(
        descriptor: Option<RequestProfileDescriptor>,
        provider: &ProviderKind,
    ) -> Result<Option<Self>, AdminError> {
        descriptor
            .map(|descriptor| {
                ProfileDirectory::prepare_descriptor(descriptor, provider).map(|directory| Self {
                    current: RwLock::new(Arc::new(directory)),
                })
            })
            .transpose()
    }

    fn snapshot(&self) -> Arc<ProfileDirectory> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(super) fn apply_refresh(
        &self,
        refresh: gateway_plugin_sdk::call::provider::RequestProfileRefresh,
        provider: &ProviderKind,
    ) -> Result<(), AdminError> {
        let next = ProfileDirectory::prepare_refresh(refresh, provider)?;
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        next.validate_successor(&current)?;
        if next.sequence > current.sequence {
            *current = Arc::new(next);
        }
        Ok(())
    }

    pub(super) fn validate_refresh(
        &self,
        refresh: &gateway_plugin_sdk::call::provider::RequestProfileRefresh,
        provider: &ProviderKind,
    ) -> Result<(), AdminError> {
        ProfileDirectory::prepare_refresh(refresh.clone(), provider)?
            .validate_successor(&self.snapshot())
    }

    pub(super) fn resolve(
        &self,
        configuration: &OpaqueProviderData,
    ) -> Result<OpaqueProviderData, ProviderError> {
        self.snapshot().resolve(configuration)
    }
    pub(super) fn default_resolved(&self) -> OpaqueProviderData {
        self.snapshot().default_resolved()
    }
    pub(super) fn default_configuration(&self) -> OpaqueProviderData {
        self.snapshot().default_configuration()
    }
    pub(super) fn options(&self) -> Result<OpaqueProviderData, ProviderAdminError> {
        self.snapshot().options()
    }
    pub(super) fn preview(
        &self,
        configuration: &OpaqueProviderData,
    ) -> Result<OpaqueProviderData, ProviderAdminError> {
        self.snapshot().preview(configuration)
    }
    pub(super) fn dashboard(
        &self,
        configuration: Option<&OpaqueProviderData>,
    ) -> Option<DashboardWireProfile> {
        self.snapshot().dashboard(configuration)
    }
}

impl ProfileDirectory {
    fn prepare_refresh(
        refresh: gateway_plugin_sdk::call::provider::RequestProfileRefresh,
        provider: &ProviderKind,
    ) -> Result<Self, AdminError> {
        if refresh.sequence == 0
            || refresh.sequence
                > gateway_plugin_sdk::call::provider::RequestProfileRefresh::MAX_SEQUENCE
            || serde_json::to_vec(&refresh)
                .map_err(|_| invalid_descriptor())?
                .len()
                > gateway_plugin_sdk::call::provider::RequestProfileRefresh::MAX_BYTES
        {
            return Err(invalid_descriptor());
        }
        let mut next = Self::prepare_descriptor(refresh.profiles, provider)?;
        next.sequence = refresh.sequence;
        Ok(next)
    }

    fn validate_successor(&self, current: &Self) -> Result<(), AdminError> {
        // 版本刷新不能移除用户已保存的选项、改变配置语义或偷偷改默认选择。
        if self.default_configuration() != current.default_configuration()
            || self.options.len() != current.options.len()
            || current.options.iter().any(|previous| {
                !self.options.iter().any(|candidate| {
                    candidate.option.id == previous.option.id
                        && candidate.option.configuration == previous.option.configuration
                })
            })
        {
            return Err(invalid_descriptor());
        }
        if self.sequence == current.sequence && self.fingerprint != current.fingerprint {
            return Err(invalid_descriptor());
        }
        Ok(())
    }
    fn prepare_descriptor(
        descriptor: RequestProfileDescriptor,
        provider: &ProviderKind,
    ) -> Result<Self, AdminError> {
        if descriptor.options.is_empty() || descriptor.options.len() > MAXIMUM_OPTIONS {
            return Err(invalid_descriptor());
        }
        validate_document(&descriptor.default_configuration)?;
        let fingerprint =
            Sha256::digest(serde_json::to_vec(&descriptor).map_err(|_| invalid_descriptor())?)
                .into();
        let mut ids = BTreeSet::new();
        let mut configurations = Vec::<Map<String, Value>>::new();
        let mut options = Vec::with_capacity(descriptor.options.len());
        for option in descriptor.options {
            validate_identifier(&option.id)?;
            validate_text(&option.label, MAXIMUM_LABEL_BYTES, false)?;
            if let Some(description) = &option.description {
                validate_text(description, MAXIMUM_DESCRIPTION_BYTES, true)?;
            }
            validate_document(&option.configuration)?;
            validate_document(&option.resolved)?;
            if !ids.insert(option.id.clone())
                || configurations
                    .iter()
                    .any(|configuration| configuration == &option.configuration)
            {
                return Err(invalid_descriptor());
            }
            configurations.push(option.configuration.clone());
            let presentation = presentation(provider, &option.presentation)?;
            options.push(PreparedRequestProfile {
                option,
                presentation,
            });
        }
        let defaults = options
            .iter()
            .enumerate()
            .filter(|(_, option)| option.option.configuration == descriptor.default_configuration)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let [default] = defaults.as_slice() else {
            return Err(invalid_descriptor());
        };
        Ok(Self {
            sequence: 0,
            fingerprint,
            default: *default,
            options,
        })
    }

    pub(super) fn resolve(
        &self,
        configuration: &OpaqueProviderData,
    ) -> Result<OpaqueProviderData, ProviderError> {
        self.find(configuration)
            .map(|profile| OpaqueProviderData::new(profile.option.resolved.clone()))
            .ok_or_else(invalid_request_profile)
    }

    #[must_use]
    pub(super) fn default_resolved(&self) -> OpaqueProviderData {
        OpaqueProviderData::new(self.options[self.default].option.resolved.clone())
    }

    #[must_use]
    pub(super) fn default_configuration(&self) -> OpaqueProviderData {
        OpaqueProviderData::new(self.options[self.default].option.configuration.clone())
    }

    pub(super) fn options(&self) -> Result<OpaqueProviderData, ProviderAdminError> {
        let presets = self
            .options
            .iter()
            .map(|profile| {
                json!({
                    "id": profile.option.id,
                    "label": profile.option.label,
                    "description": profile.option.description,
                    "configuration": profile.option.configuration,
                })
            })
            .collect::<Vec<_>>();
        object(json!({
            "presets": presets,
            "defaultConfiguration": self.options[self.default].option.configuration,
        }))
    }

    pub(super) fn preview(
        &self,
        configuration: &OpaqueProviderData,
    ) -> Result<OpaqueProviderData, ProviderAdminError> {
        let profile = self.find(configuration).ok_or_else(invalid_admin_profile)?;
        let presentation = &profile.presentation;
        object(json!({
            "configuration": profile.option.configuration,
            "product": presentation.product,
            "version": presentation.version,
            "build": presentation.build,
            "target": {
                "osType": presentation.target.os_type,
                "osVersion": presentation.target.os_version,
                "arch": presentation.target.arch,
                "terminal": presentation.target.terminal,
            },
            "userAgent": presentation.user_agent,
            "attributes": presentation.attributes.iter().map(|attribute| json!({
                "label": attribute.label,
                "value": attribute.value,
            })).collect::<Vec<_>>(),
            "verifiedAt": presentation.verified_at,
            "release": presentation.release.as_ref().map(release_view),
        }))
    }

    #[must_use]
    pub(super) fn dashboard(
        &self,
        configuration: Option<&OpaqueProviderData>,
    ) -> Option<DashboardWireProfile> {
        configuration
            .and_then(|configuration| self.find(configuration))
            .or_else(|| configuration.is_none().then(|| &self.options[self.default]))
            .map(|profile| profile.presentation.clone())
    }

    fn find(&self, configuration: &OpaqueProviderData) -> Option<&PreparedRequestProfile> {
        self.options
            .iter()
            .find(|profile| &profile.option.configuration == configuration.expose_to_provider())
    }
}

fn presentation(
    provider: &ProviderKind,
    value: &RequestProfilePresentation,
) -> Result<DashboardWireProfile, AdminError> {
    for text in [
        &value.product,
        &value.version,
        &value.target.os_type,
        &value.target.os_version,
        &value.target.arch,
        &value.target.terminal,
        &value.user_agent,
    ] {
        validate_text(text, MAXIMUM_PRESENTATION_TEXT_BYTES, false)?;
    }
    if let Some(build) = &value.build {
        validate_text(build, MAXIMUM_PRESENTATION_TEXT_BYTES, true)?;
    }
    if value.attributes.len() > MAXIMUM_ATTRIBUTES {
        return Err(invalid_descriptor());
    }
    let attributes = value
        .attributes
        .iter()
        .map(|attribute| {
            validate_text(&attribute.label, MAXIMUM_LABEL_BYTES, false)?;
            validate_text(&attribute.value, MAXIMUM_PRESENTATION_TEXT_BYTES, true)?;
            Ok(DashboardWireAttribute {
                label: attribute.label.clone(),
                value: attribute.value.clone(),
            })
        })
        .collect::<Result<Vec<_>, AdminError>>()?;
    Ok(DashboardWireProfile {
        provider: provider.as_str().to_owned(),
        product: value.product.clone(),
        version: value.version.clone(),
        build: value.build.clone(),
        target: DashboardWireTarget {
            os_type: value.target.os_type.clone(),
            os_version: value.target.os_version.clone(),
            arch: value.target.arch.clone(),
            terminal: value.target.terminal.clone(),
        },
        user_agent: value.user_agent.clone(),
        attributes,
        verified_at: value.verified_at_ms.map(timestamp).transpose()?,
        release: value.release.as_ref().map(release).transpose()?,
    })
}

fn release(value: &RequestProfileRelease) -> Result<DashboardDesktopRelease, AdminError> {
    for text in [
        value.latest_version.as_deref(),
        value.latest_build.as_deref(),
        value.minimum_system_version.as_deref(),
        value.hardware_requirements.as_deref(),
        value.error.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_text(text, MAXIMUM_PRESENTATION_TEXT_BYTES, true)?;
    }
    if let Some(source) = &value.download_url {
        validate_text(source, MAXIMUM_PRESENTATION_TEXT_BYTES, false)?;
        let url = url::Url::parse(source).map_err(|_| invalid_descriptor())?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(invalid_descriptor());
        }
    }
    Ok(DashboardDesktopRelease {
        status: match value.status {
            RequestProfileReleaseStatus::Unchecked => DesktopReleaseStatus::Unchecked,
            RequestProfileReleaseStatus::Current => DesktopReleaseStatus::Current,
            RequestProfileReleaseStatus::UpdateAvailable => DesktopReleaseStatus::UpdateAvailable,
            RequestProfileReleaseStatus::Failed => DesktopReleaseStatus::Failed,
        },
        checked_at: value.checked_at_ms.map(timestamp).transpose()?,
        latest_version: value.latest_version.clone(),
        latest_build: value.latest_build.clone(),
        published_at: value.published_at_ms.map(timestamp).transpose()?,
        minimum_system_version: value.minimum_system_version.clone(),
        hardware_requirements: value.hardware_requirements.clone(),
        download_url: value.download_url.clone(),
        download_size: value.download_size,
        signature_present: value.signature_present,
        error: value.error.clone(),
    })
}

fn release_view(value: &DashboardDesktopRelease) -> Value {
    json!({
        "status": match value.status {
            DesktopReleaseStatus::Unchecked => "unchecked",
            DesktopReleaseStatus::Current => "current",
            DesktopReleaseStatus::UpdateAvailable => "update_available",
            DesktopReleaseStatus::Failed => "failed",
        },
        "checkedAt": value.checked_at,
        "latestVersion": value.latest_version,
        "latestBuild": value.latest_build,
        "publishedAt": value.published_at,
        "minimumSystemVersion": value.minimum_system_version,
        "hardwareRequirements": value.hardware_requirements,
        "downloadUrl": value.download_url,
        "downloadSize": value.download_size,
        "signaturePresent": value.signature_present,
        "error": value.error,
    })
}

fn timestamp(value: i64) -> Result<DateTime<Utc>, AdminError> {
    DateTime::from_timestamp_millis(value).ok_or_else(invalid_descriptor)
}

fn validate_identifier(value: &str) -> Result<(), AdminError> {
    validate_text(value, MAXIMUM_ID_BYTES, false)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid_descriptor());
    }
    Ok(())
}

fn validate_document(value: &Map<String, Value>) -> Result<(), AdminError> {
    if serde_json::to_vec(value).map_or(true, |encoded| encoded.len() > MAXIMUM_DOCUMENT_BYTES) {
        return Err(invalid_descriptor());
    }
    Ok(())
}

fn validate_text(value: &str, maximum: usize, allow_empty: bool) -> Result<(), AdminError> {
    if value.len() > maximum
        || (!allow_empty && value.is_empty())
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(invalid_descriptor());
    }
    Ok(())
}

fn object(value: Value) -> Result<OpaqueProviderData, ProviderAdminError> {
    value
        .as_object()
        .cloned()
        .map(OpaqueProviderData::new)
        .ok_or_else(invalid_admin_profile)
}

fn invalid_descriptor() -> AdminError {
    AdminError::invalid("插件请求画像描述不合法")
}

pub(super) fn invalid_request_profile() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::InvalidRequest,
        UpstreamSendState::NotSent,
    )
}

fn invalid_admin_profile() -> ProviderAdminError {
    ProviderAdminError::new(ProviderAdminErrorKind::Invalid)
}
