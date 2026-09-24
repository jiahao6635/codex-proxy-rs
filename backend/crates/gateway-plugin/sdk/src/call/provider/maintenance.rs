//! 版本资料属于 Provider；宿主只接收完整、可校验、单调更新的目录。

use serde::{Deserialize, Serialize};

/// `provider.request_profiles.refresh` 的结果。序号必须大于零，且同一序号内容不可变。
/// 用户配置与默认选择不能随后台刷新变化，只更新其解析资料与展示事实。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestProfileRefresh {
    pub sequence: u64,
    pub profiles: super::RequestProfileDescriptor,
}

impl RequestProfileRefresh {
    /// 对齐宿主版本资料缓存的有界文档合同。
    pub const MAX_BYTES: usize = 16 * 1024;
    /// 序号需可由跨进程缓存精确比较。
    pub const MAX_SEQUENCE: u64 = (1_u64 << 53) - 1;
}

impl std::fmt::Debug for RequestProfileRefresh {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestProfileRefresh")
            .field("sequence", &self.sequence)
            .finish_non_exhaustive()
    }
}
