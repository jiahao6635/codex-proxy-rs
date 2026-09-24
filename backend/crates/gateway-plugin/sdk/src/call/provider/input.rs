use super::{ExecutionEncodingError, PrepareExecution};

/// provider.prepare 的完整二进制载荷，控制 params 必须为空对象。
/// 凭据、上下文和恢复状态不进入控制元数据，正文保持原始字节。
pub struct ExecutionInput {
    pub request: PrepareExecution,
    pub body: Vec<u8>,
}

impl ExecutionInput {
    /// # Errors
    /// 元数据无法编码或总长度超过执行载荷上限时返回错误。
    pub fn encode(self) -> Result<Vec<u8>, ExecutionEncodingError> {
        super::codec::encode_input(self)
    }

    /// # Errors
    /// 版本、分段长度或准备数据不合法时返回错误。
    pub fn decode(bytes: &[u8]) -> Result<Self, ExecutionEncodingError> {
        super::codec::decode_input(bytes)
    }
}
