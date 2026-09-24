//! 插件发现 Client Key 公开身份的窄端口。

use async_trait::async_trait;

use crate::model::{
    AdminError,
    plugin_client_keys::{PluginClientKeyListQuery, PluginClientKeyPage},
};

#[async_trait]
pub trait PluginClientKeyAccess: Send + Sync {
    async fn list(
        &self,
        query: PluginClientKeyListQuery,
    ) -> Result<PluginClientKeyPage, AdminError>;
}
