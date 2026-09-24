//! Key 目录只传递非秘密身份，模型规则仍由 Core 在执行时复核。

use std::sync::{Arc, OnceLock, Weak};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use gateway_admin::{
    model::{
        AdminError, PageSize,
        plugin_client_keys::{PluginClientKeyCursor, PluginClientKeyListQuery},
    },
    ports::plugin_client_keys::PluginClientKeyAccess,
};
use gateway_core::policy::ClientApiKeyId;
use gateway_plugin_sdk::{
    PluginFault,
    call::host::{ClientKey, KeyListRequest, KeyListResult},
};

use super::{denied, invalid};
use crate::RpcReply;

pub(crate) struct PluginClientKeyPortSlot {
    access: OnceLock<Weak<dyn PluginClientKeyAccess>>,
}

impl PluginClientKeyPortSlot {
    pub(crate) const fn new() -> Self {
        Self {
            access: OnceLock::new(),
        }
    }

    pub(crate) fn bind(&self, access: &Arc<dyn PluginClientKeyAccess>) -> Result<(), AdminError> {
        self.access
            .set(Arc::downgrade(access))
            .map_err(|_| AdminError::conflict("插件 Key 目录端口已经绑定"))
    }

    pub(super) async fn list(
        &self,
        params: serde_json::Value,
        payload: &[u8],
    ) -> Result<RpcReply, PluginFault> {
        if !payload.is_empty() {
            return Err(invalid());
        }
        let request: KeyListRequest = serde_json::from_value(params).map_err(|_| invalid())?;
        let limit = PageSize::new(request.limit).map_err(|_| invalid())?;
        let cursor = request
            .cursor
            .map(|cursor| {
                if cursor.len() > 2048 {
                    return Err(invalid());
                }
                let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| invalid())?;
                let (name, id): (String, String) =
                    serde_json::from_slice(&bytes).map_err(|_| invalid())?;
                Ok(PluginClientKeyCursor {
                    name,
                    id: ClientApiKeyId::new(id).map_err(|_| invalid())?,
                })
            })
            .transpose()?;
        let access = self
            .access
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(denied)?;
        let page = access
            .list(PluginClientKeyListQuery { cursor, limit })
            .await
            .map_err(super::accounts::map_admin_error)?;
        let next_cursor = page
            .next_cursor
            .map(|cursor| {
                serde_json::to_vec(&(cursor.name, cursor.id.as_str()))
                    .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
                    .map_err(|_| invalid())
            })
            .transpose()?;
        Ok(RpcReply {
            result: serde_json::to_value(KeyListResult {
                keys: page
                    .items
                    .into_iter()
                    .map(|key| ClientKey {
                        id: key.id.as_str().to_owned(),
                        name: key.name,
                        enabled: key.enabled,
                    })
                    .collect(),
                next_cursor,
            })
            .map_err(|_| invalid())?,
            payload: Vec::new(),
        })
    }
}
