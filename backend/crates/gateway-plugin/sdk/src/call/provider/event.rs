//! 同一个上游事件的业务事实、原生表达与私有续写检查点。

use serde::{Deserialize, Serialize};

use super::{ContentKind, ExecutionFailure, FinishReason, ResponseObservation, Usage};

/// 每个流分块是一个完整封套；同一 wire 的事实必须合并，避免重复交付客户端。
/// 载荷及私有状态不实现 Debug，且不能用拆分封套绕过宿主大小和序列检查。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEvent {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<CanonicalEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire: Option<WireEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<ResponseObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_update: Option<SessionUpdate>,
    /// 失败时可附尚未交付的 wire；不能同时上报成功终态或续写检查点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<ExecutionFailure>,
}

impl ExecutionEvent {
    /// 编码为带长度与版本的事件载荷；原始 wire/错误正文不经过 JSON 数组或 base64。
    ///
    /// # Errors
    /// 总长度超过事件上限或元数据无法序列化时返回错误。
    pub fn encode(self) -> Result<Vec<u8>, super::ExecutionEncodingError> {
        super::codec::encode_event(self)
    }

    /// 从完整的执行流载荷解码，先验证各段长度再解析元数据。
    ///
    /// # Errors
    /// 版本、长度、元数据或二进制段声明不一致时返回错误。
    pub fn decode(bytes: &[u8]) -> Result<Self, super::ExecutionEncodingError> {
        super::codec::decode_event(bytes)
    }

    #[must_use]
    pub fn failure(failure: ExecutionFailure) -> Self {
        Self {
            failure: Some(failure),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_failure(mut self, failure: ExecutionFailure) -> Self {
        self.failure = Some(failure);
        self
    }
    #[must_use]
    pub fn canonical(fact: CanonicalEvent) -> Self {
        Self {
            facts: vec![fact],
            ..Self::default()
        }
    }

    #[must_use]
    pub fn wire(wire: WireEvent) -> Self {
        Self {
            wire: Some(wire),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn observation(observation: ResponseObservation) -> Self {
        Self {
            observation: Some(observation),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_fact(mut self, fact: CanonicalEvent) -> Self {
        self.facts.push(fact);
        self
    }

    #[must_use]
    pub fn with_observation(mut self, observation: ResponseObservation) -> Self {
        self.observation = Some(observation);
        self
    }

    #[must_use]
    pub fn with_session_update(mut self, update: SessionUpdate) -> Self {
        self.session_update = Some(update);
        self
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CanonicalEvent {
    Started {
        id: String,
        model: Option<String>,
    },
    ContentAdded {
        index: u32,
        kind: ContentKind,
    },
    TextDelta {
        index: u32,
        text: String,
    },
    ReasoningDelta {
        index: u32,
        text: String,
    },
    ToolCallDelta {
        index: u32,
        id: String,
        name: Option<String>,
        arguments: String,
    },
    Usage {
        usage: Usage,
    },
    /// 明确确认仅按 Token 计费的累计用量；缺少必要事实时仍保持费用未知。
    BillableUsage {
        usage: Usage,
        band: super::billing::PriceBand,
    },
    ProviderCost {
        amount: String,
        currency: String,
    },
    Completed {
        id: String,
        model: Option<String>,
        reason: FinishReason,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireEvent {
    pub protocol: String,
    pub payload: WirePayload,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WirePayload {
    Json {
        event: Option<String>,
        data: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry: Option<u64>,
        /// 一个完整 SSE 帧的原字节；存在时必须与解析后的 data 和元数据一致。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_sse: Option<Vec<u8>>,
    },
    /// 注释、非 JSON data 等原生 SSE 表达，不能夹带第二个帧。
    RawSse { frame: Vec<u8> },
    /// 一个完整的非流式 JSON 响应正文，保留空白与字段顺序。
    RawJson { body: Vec<u8> },
    /// 非流式 HTTP 响应正文的一个有序片段；Content-Type 与状态在首片 observation 中传递。
    /// 客户端 adapter 必须同时限制单片与聚合后的总字节数。
    RawBody { body: Vec<u8> },
}

/// 只可附在 Completed 事实所在的封套；最多 64 KiB。
/// 状态必须能跨连接恢复，不代表宿主提供连接池或允许切换账号。
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUpdate {
    pub payload: serde_json::Map<String, serde_json::Value>,
}
