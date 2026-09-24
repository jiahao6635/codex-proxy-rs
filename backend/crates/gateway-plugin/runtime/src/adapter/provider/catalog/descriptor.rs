use std::collections::{BTreeMap, BTreeSet};

use gateway_admin::model::AdminError;
use gateway_core::{
    operation::{Feature, OperationKind},
    routing::{ModelCapabilities, ProviderModelCapabilities, SupportLevel, UpstreamModelId},
};
use gateway_plugin_sdk::call::provider::{
    ModelDescriptor, ModelFeature, OperationKind as WireOperation,
};

pub(super) fn validate(models: &[ModelDescriptor]) -> Result<(), AdminError> {
    if models.len() > 4096 {
        return Err(AdminError::invalid("插件模型目录超过 4096 项"));
    }
    let mut identities = BTreeSet::new();
    for model in models {
        UpstreamModelId::new(model.id.clone())
            .map_err(|_| AdminError::invalid("插件模型 ID 无效"))?;
        if !identities.insert(&model.id)
            || model.operations.is_empty()
            || model.operations.iter().collect::<BTreeSet<_>>().len() != model.operations.len()
            || model
                .operations
                .iter()
                .any(|operation| matches!(operation, WireOperation::ProviderHttp))
            || model.features.iter().collect::<BTreeSet<_>>().len() != model.features.len()
            || model.maximum_output_tokens == Some(0)
        {
            return Err(AdminError::invalid("插件模型能力重复、为空或上限无效"));
        }
    }
    Ok(())
}

pub(super) fn compile(model: &ModelDescriptor) -> Result<ProviderModelCapabilities, AdminError> {
    let operations = model
        .operations
        .iter()
        .map(|operation| match operation {
            WireOperation::Generate => Ok(OperationKind::Generate),
            WireOperation::GenerateImage => Ok(OperationKind::GenerateImage),
            WireOperation::Search => Ok(OperationKind::Search),
            WireOperation::CountTokens => Ok(OperationKind::CountTokens),
            WireOperation::ProviderHttp => {
                Err(AdminError::invalid("Provider HTTP 不能声明为模型操作"))
            }
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut capabilities = ModelCapabilities::new(operations, model.maximum_output_tokens);
    for feature in &model.features {
        capabilities = capabilities.with_feature(
            match feature {
                ModelFeature::Tools => Feature::Tools,
                ModelFeature::Vision => Feature::Vision,
                ModelFeature::Reasoning => Feature::Reasoning,
                ModelFeature::JsonSchema => Feature::JsonSchema,
                ModelFeature::NativeContinuation => Feature::NativeContinuation,
            },
            SupportLevel::Native,
        );
    }
    Ok(ProviderModelCapabilities::new(
        UpstreamModelId::new(model.id.clone())
            .map_err(|_| AdminError::invalid("插件模型 ID 无效"))?,
        capabilities,
    ))
}

/// 全局目录只合并可路由能力；实际选号必须再次匹配单个账号的完整声明。
pub(super) fn union(
    target: &mut BTreeMap<String, ModelDescriptor>,
    models: impl IntoIterator<Item = ModelDescriptor>,
) -> Result<(), AdminError> {
    for model in models {
        match target.entry(model.id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(model);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let previous = entry.get_mut();
                previous.operations.extend(model.operations);
                previous.operations.sort();
                previous.operations.dedup();
                previous.features.extend(model.features);
                previous.features.sort();
                previous.features.dedup();
                previous.maximum_output_tokens =
                    match (previous.maximum_output_tokens, model.maximum_output_tokens) {
                        (Some(left), Some(right)) => Some(left.max(right)),
                        _ => None,
                    };
            }
        }
        if target.len() > 4096 {
            return Err(AdminError::invalid("合并模型目录超过 4096 项"));
        }
    }
    Ok(())
}
