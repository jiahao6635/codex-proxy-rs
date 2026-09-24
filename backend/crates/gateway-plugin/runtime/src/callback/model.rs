//! 嵌套模型执行；只驱动 Core 会话，不拥有路由、重试或计费规则。

use std::sync::{Arc, OnceLock, Weak};

use bytes::Bytes;
use gateway_admin::model::{AdminError, plugins::instances::PluginPermissionGrant};
use gateway_core::{
    account::ProviderAccountId,
    engine::{
        CommitRequirement, ModelRequestId,
        execution::{
            BoundModelExecutionContext, ClientTransport, ExecutionRequestMetadata, ExecutionSession,
        },
        nested::{
            BoundModelExecutionBinding, BoundModelExecutionRequest, NestedModelExecutionPort,
            NestedModelExecutionRequest,
        },
    },
    error::{GatewayError, GatewayErrorKind},
    event::{ContentKind, FinishReason, GatewayEvent, ProtocolWireEvent, ProviderEvent},
    identity::ProviderKind,
    operation::{
        GenerateRequest, ImageRequest, ImageRequestKind, Operation, ProtocolPayload,
        RawJsonPayload, StandaloneSearchRequest,
    },
    policy::ClientApiKeyId,
    routing::PublicModelId,
};
use gateway_plugin_sdk::{
    CallContext, ErrorCode, PluginFault,
    call::{
        host::{
            ModelEventBatch, ModelExecuteRequest, ModelExecuteResult, ModelListRequest,
            ModelListResult, ModelOperation, ModelStreamCloseRequest, ModelStreamReadRequest,
            ModelStreamReadResult, ModelStreamResult,
        },
        provider::{
            CanonicalEvent, ContentKind as WireContentKind, ExecutionEvent,
            FinishReason as WireFinishReason, Usage as WireUsage, WireEvent, WirePayload,
        },
    },
};

use super::{CallResources, NetworkScope, denied, invalid};
use crate::RpcReply;

const MAX_MODEL_STREAMS_PER_CALL: usize = 16;

pub(crate) struct PluginModelPortSlot {
    port: OnceLock<Weak<dyn NestedModelExecutionPort>>,
}

impl PluginModelPortSlot {
    pub(crate) const fn new() -> Self {
        Self {
            port: OnceLock::new(),
        }
    }

    pub(crate) fn bind(&self, port: &Arc<dyn NestedModelExecutionPort>) -> Result<(), AdminError> {
        self.port
            .set(Arc::downgrade(port))
            .map_err(|_| AdminError::conflict("插件模型执行端口已经绑定"))
    }

    fn upgrade(&self) -> Result<Arc<dyn NestedModelExecutionPort>, PluginFault> {
        self.port.get().and_then(Weak::upgrade).ok_or_else(denied)
    }
}

pub(super) struct PluginModels {
    slot: Arc<PluginModelPortSlot>,
    authorized: bool,
    maximum_payload: usize,
}

impl PluginModels {
    pub(super) fn new(
        slot: Arc<PluginModelPortSlot>,
        grants: &[PluginPermissionGrant],
        maximum_payload: usize,
    ) -> Self {
        Self {
            slot,
            authorized: grants.iter().any(|grant| grant.permission == "models"),
            maximum_payload,
        }
    }

    pub(super) async fn call(
        &self,
        context: &CallContext,
        call: &Arc<CallResources>,
        scope: Option<&NetworkScope>,
        method: &str,
        params: serde_json::Value,
        payload: Vec<u8>,
    ) -> Result<RpcReply, PluginFault> {
        if !self.authorized {
            return Err(denied());
        }
        match method {
            "host.models.list" => {
                if !payload.is_empty() {
                    return Err(invalid());
                }
                let request: ModelListRequest =
                    serde_json::from_value(params).map_err(|_| invalid())?;
                if request.protocol.is_empty()
                    || request.protocol.len() > 64
                    || request.client_version.len() > 128
                {
                    return Err(invalid());
                }
                let port = self.slot.upgrade()?;
                let bound = port
                    .bind(BoundModelExecutionBinding {
                        client_key_id: ClientApiKeyId::new(request.client_key_id)
                            .map_err(|_| invalid())?,
                        initiating_plugin_instance_id: context.instance_id.clone(),
                        timeout: call
                            .deadline
                            .saturating_duration_since(tokio::time::Instant::now()),
                        cancellation: call.cancellation.child_token(),
                        extension_scope: call.scope.extension_scope.clone(),
                    })
                    .await
                    .map_err(gateway_fault)?;
                let models = port
                    .models(bound, request.protocol, request.client_version)
                    .await
                    .map_err(gateway_fault)?;
                encode_result(ModelListResult {
                    models: models
                        .into_iter()
                        .map(|model| model.as_str().to_owned())
                        .collect(),
                })
            }
            "host.model.execute" | "host.model.execute_stream" => {
                let mut request: ModelExecuteRequest =
                    serde_json::from_value(params).map_err(|_| invalid())?;
                if payload.is_empty() || payload.len() > self.maximum_payload {
                    return Err(invalid());
                }
                let port = self.slot.upgrade()?;
                let stream = method == "host.model.execute_stream";
                let started = if matches!(
                    context.stage,
                    gateway_plugin_sdk::Stage::Management
                        | gateway_plugin_sdk::Stage::CommandLine
                        | gateway_plugin_sdk::Stage::Maintenance
                        | gateway_plugin_sdk::Stage::Authentication
                        | gateway_plugin_sdk::Stage::Observation
                ) {
                    let client_key_id = request.client_key_id.take().ok_or_else(invalid)?;
                    let bound = self.binding(&port, context, call, client_key_id).await?;
                    port.start_bound(self.bound_request(request, payload, stream, bound)?)
                        .await
                        .map_err(gateway_fault)?
                } else {
                    if request.client_key_id.is_some() {
                        return Err(invalid());
                    }
                    let request = self.request(context, request, payload, stream)?;
                    if let Some(scope) = scope {
                        scope.observe_nested_execution();
                    }
                    port.start(request).await.map_err(gateway_fault)?
                };
                if method == "host.model.execute" {
                    execute_buffered(started, self.maximum_payload).await
                } else {
                    let stream = Arc::new(ModelStream::new(started.session, self.maximum_payload));
                    let id = uuid::Uuid::new_v4().to_string();
                    let mut state = call
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state.closed || state.model_streams.len() >= MAX_MODEL_STREAMS_PER_CALL {
                        drop(state);
                        stream.close();
                        return Err(PluginFault::new(
                            ErrorCode::Capacity,
                            "model stream capacity is exhausted",
                        ));
                    }
                    state.model_streams.insert(id.clone(), stream);
                    encode_result(ModelStreamResult {
                        request_id: started.request_id.as_str().to_owned(),
                        stream: id,
                    })
                }
            }
            "host.model.stream_read" => {
                if !payload.is_empty() {
                    return Err(invalid());
                }
                let request: ModelStreamReadRequest =
                    serde_json::from_value(params).map_err(|_| invalid())?;
                if request.maximum_bytes == 0
                    || usize::try_from(request.maximum_bytes)
                        .map_or(true, |maximum| maximum > self.maximum_payload)
                {
                    return Err(invalid());
                }
                let stream = call
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .model_streams
                    .get(&request.stream)
                    .cloned()
                    .ok_or_else(denied)?;
                let read = stream
                    .read(usize::try_from(request.maximum_bytes).map_err(|_| invalid())?)
                    .await?;
                if read.end {
                    call.state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .model_streams
                        .remove(&request.stream);
                }
                Ok(RpcReply {
                    result: serde_json::to_value(ModelStreamReadResult {
                        events: read.events,
                        end: read.end,
                    })
                    .map_err(|_| invalid())?,
                    payload: read.payload,
                })
            }
            "host.model.stream_close" => {
                if !payload.is_empty() {
                    return Err(invalid());
                }
                let request: ModelStreamCloseRequest =
                    serde_json::from_value(params).map_err(|_| invalid())?;
                let stream = call
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .model_streams
                    .remove(&request.stream)
                    .ok_or_else(denied)?;
                stream.close();
                Ok(RpcReply {
                    result: serde_json::json!({}),
                    payload: Vec::new(),
                })
            }
            _ => Err(denied()),
        }
    }

    async fn binding(
        &self,
        port: &Arc<dyn NestedModelExecutionPort>,
        context: &CallContext,
        call: &CallResources,
        client_key_id: String,
    ) -> Result<BoundModelExecutionContext, PluginFault> {
        let key = ClientApiKeyId::new(client_key_id.clone()).map_err(|_| invalid())?;
        let mut bindings = call.model_bindings.lock().await;
        if let Some(bound) = bindings.get(&client_key_id) {
            return Ok(bound.clone());
        }
        if bindings.len() >= MAX_MODEL_STREAMS_PER_CALL {
            return Err(PluginFault::new(
                ErrorCode::Capacity,
                "model identity capacity is exhausted",
            ));
        }
        let bound = port
            .bind(BoundModelExecutionBinding {
                client_key_id: key,
                initiating_plugin_instance_id: context.instance_id.clone(),
                timeout: call
                    .deadline
                    .saturating_duration_since(tokio::time::Instant::now()),
                cancellation: call.cancellation.child_token(),
                extension_scope: call.scope.extension_scope.clone(),
            })
            .await
            .map_err(gateway_fault)?;
        // 调用持有身份直到 RPC 结束，流式执行不会因单次 callback 返回而提前取消；
        // 同一 Key 复用 Core 调用图，不能通过重复 bind 重置深度与并发限制。
        bindings.insert(client_key_id, bound.clone());
        Ok(bound)
    }

    fn request(
        &self,
        context: &CallContext,
        request: ModelExecuteRequest,
        body: Vec<u8>,
        stream: bool,
    ) -> Result<NestedModelExecutionRequest, PluginFault> {
        let parent_request_id = context
            .request_id
            .as_ref()
            .ok_or_else(denied)
            .and_then(|request_id| ModelRequestId::new(request_id.clone()).map_err(|_| denied()))?;
        let parent_account = context
            .account_id
            .as_ref()
            .map(|account| ProviderAccountId::new(account.clone()))
            .transpose()
            .map_err(|_| denied())?;
        let request = self.request_parts(request, body, stream)?;
        Ok(NestedModelExecutionRequest {
            parent_request_id,
            initiating_plugin_instance_id: context.instance_id.clone(),
            public_model: request.public_model,
            operation: request.operation,
            metadata: request.metadata,
            provider: request.provider,
            account: request.account,
            parent_account,
        })
    }

    fn bound_request(
        &self,
        request: ModelExecuteRequest,
        body: Vec<u8>,
        stream: bool,
        context: BoundModelExecutionContext,
    ) -> Result<BoundModelExecutionRequest, PluginFault> {
        let request = self.request_parts(request, body, stream)?;
        Ok(BoundModelExecutionRequest {
            context,
            public_model: request.public_model,
            operation: request.operation,
            metadata: request.metadata,
            provider: request.provider,
            account: request.account,
        })
    }

    fn request_parts(
        &self,
        request: ModelExecuteRequest,
        body: Vec<u8>,
        stream: bool,
    ) -> Result<ModelRequestParts, PluginFault> {
        if !self.authorized {
            return Err(denied());
        }
        let provider = request
            .provider
            .map(ProviderKind::new)
            .transpose()
            .map_err(|_| invalid())?;
        let account = request
            .account_id
            .map(ProviderAccountId::new)
            .transpose()
            .map_err(|_| invalid())?;
        let previous_response_id = request
            .previous_response_id
            .map(gateway_core::engine::continuation::PreviousResponseId::new);
        let operation = decode_operation(
            request.operation,
            request.protocol.clone(),
            body,
            previous_response_id.as_ref().map(|id| id.as_str()),
        )?;
        Ok(ModelRequestParts {
            public_model: PublicModelId::new(request.model).map_err(|_| invalid())?,
            operation,
            metadata: ExecutionRequestMetadata {
                protocol: request.protocol,
                endpoint: "host.model".to_owned(),
                transport: ClientTransport::InternalPlugin,
                stream,
                client_ip: None,
                user_agent: None,
                previous_response_id,
            },
            provider,
            account,
        })
    }
}

struct ModelRequestParts {
    public_model: PublicModelId,
    operation: Operation,
    metadata: ExecutionRequestMetadata,
    provider: Option<ProviderKind>,
    account: Option<ProviderAccountId>,
}

fn decode_operation(
    operation: ModelOperation,
    protocol: String,
    body: Vec<u8>,
    previous_response_id: Option<&str>,
) -> Result<Operation, PluginFault> {
    if serde_json::from_slice::<serde_json::Value>(&body).is_err() {
        return Err(invalid());
    }
    match operation {
        ModelOperation::Generate => {
            let mut body =
                serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&body)
                    .map_err(|_| invalid())?;
            if let Some(previous) = previous_response_id {
                match body.get("previous_response_id") {
                    Some(value) if value.as_str() != Some(previous) => return Err(invalid()),
                    Some(_) => {}
                    None => {
                        body.insert(
                            "previous_response_id".to_owned(),
                            serde_json::Value::String(previous.to_owned()),
                        );
                    }
                }
            }
            Ok(Operation::Generate(GenerateRequest::from_protocol_payload(
                ProtocolPayload::json_object(protocol, body).map_err(|_| invalid())?,
            )))
        }
        ModelOperation::GenerateImage | ModelOperation::EditImage => {
            let kind = if operation == ModelOperation::GenerateImage {
                ImageRequestKind::Generation
            } else {
                ImageRequestKind::Edit
            };
            Ok(Operation::GenerateImage(ImageRequest::from_raw_json(
                kind,
                RawJsonPayload::new(protocol, Bytes::from(body)).map_err(|_| invalid())?,
            )))
        }
        ModelOperation::Search => Ok(Operation::Search(StandaloneSearchRequest::from_raw_json(
            RawJsonPayload::new(protocol, Bytes::from(body)).map_err(|_| invalid())?,
        ))),
    }
}

async fn execute_buffered(
    mut started: gateway_core::engine::execution::StartedExecution,
    maximum_payload: usize,
) -> Result<RpcReply, PluginFault> {
    let events = match started.session.collect_uncommitted().await {
        Ok(events) => events,
        Err(error) => {
            let fault = gateway_fault(gateway_core::engine::execution::gateway_error_from_engine(
                &error,
            ));
            started.session.detach_finalize().await;
            return Err(fault);
        }
    };
    let (payload, events) = match encode_events(events) {
        Ok(encoded) => encoded,
        Err(error) => {
            started.session.detach_finalize().await;
            return Err(error);
        }
    };
    if payload.len() > maximum_payload {
        started.session.detach_finalize().await;
        return Err(PluginFault::new(
            ErrorCode::Capacity,
            "model response requires streaming",
        ));
    }
    if let Err(error) = started.session.commit_downstream(Some(200)).await {
        let fault = gateway_fault(gateway_core::engine::execution::gateway_error_from_engine(
            &error,
        ));
        started.session.detach_finalize().await;
        return Err(fault);
    }
    if !started.session.is_finalized() {
        started.session.detach_finalize().await;
        return Err(PluginFault::new(
            ErrorCode::Fault,
            "model execution did not finalize",
        ));
    }
    Ok(RpcReply {
        result: serde_json::to_value(ModelExecuteResult {
            request_id: started.request_id.as_str().to_owned(),
            events,
        })
        .map_err(|_| invalid())?,
        payload,
    })
}

struct PendingBatch {
    payload: Vec<u8>,
    events: u32,
    commit: bool,
}

struct ModelStreamState {
    session: Option<Box<dyn ExecutionSession>>,
    pending: Option<PendingBatch>,
}

pub(super) struct ModelStream {
    state: tokio::sync::Mutex<ModelStreamState>,
    closed: tokio::sync::watch::Sender<bool>,
    maximum_payload: usize,
}

struct ModelRead {
    payload: Vec<u8>,
    events: u32,
    end: bool,
}

impl ModelStream {
    fn new(session: Box<dyn ExecutionSession>, maximum_payload: usize) -> Self {
        Self {
            state: tokio::sync::Mutex::new(ModelStreamState {
                session: Some(session),
                pending: None,
            }),
            closed: tokio::sync::watch::channel(false).0,
            maximum_payload,
        }
    }

    async fn read(&self, maximum_bytes: usize) -> Result<ModelRead, PluginFault> {
        let mut state = self.state.try_lock().map_err(|_| {
            PluginFault::new(ErrorCode::Capacity, "model stream already has a reader")
        })?;
        loop {
            let is_closed = *self.closed.borrow();
            if is_closed {
                let session = state.session.take();
                state.pending.take();
                drop(state);
                if let Some(session) = session {
                    session.cancel();
                    session.detach_finalize().await;
                }
                return Err(PluginFault::new(
                    ErrorCode::Cancelled,
                    "model stream was closed",
                ));
            }
            if let Some(pending) = state.pending.take() {
                if pending.payload.len() > maximum_bytes {
                    state.pending = Some(pending);
                    return Err(PluginFault::new(
                        ErrorCode::Capacity,
                        "model event exceeds the requested read size",
                    ));
                }
                if pending.commit {
                    let result = state
                        .session
                        .as_mut()
                        .ok_or_else(denied)?
                        .commit_downstream(Some(200))
                        .await;
                    if let Err(error) = result {
                        let fault = gateway_fault(
                            gateway_core::engine::execution::gateway_error_from_engine(&error),
                        );
                        let session = state.session.take();
                        drop(state);
                        if let Some(session) = session {
                            session.cancel();
                            session.detach_finalize().await;
                        }
                        return Err(fault);
                    }
                }
                return Ok(ModelRead {
                    payload: pending.payload,
                    events: pending.events,
                    end: false,
                });
            }
            let mut closed = self.closed.subscribe();
            let next = {
                let session = state.session.as_mut().ok_or_else(denied)?;
                tokio::select! {
                    biased;
                    _ = closed.wait_for(|closed| *closed) => {
                        session.cancel();
                        None
                    }
                    result = session.next_event() => Some(result),
                }
            };
            let Some(next) = next else {
                let session = state.session.take();
                drop(state);
                if let Some(session) = session {
                    session.cancel();
                    session.detach_finalize().await;
                }
                return Err(PluginFault::new(
                    ErrorCode::Cancelled,
                    "model stream was closed",
                ));
            };
            match next {
                Ok(Some(event)) => {
                    let commit =
                        event.commit_requirement() == CommitRequirement::CommitBeforeDelivery;
                    let (payload, events) = match encode_events(event.into_provider_events()) {
                        Ok(encoded) => encoded,
                        Err(error) => {
                            let session = state.session.take();
                            drop(state);
                            if let Some(session) = session {
                                session.cancel();
                                session.detach_finalize().await;
                            }
                            return Err(error);
                        }
                    };
                    if events == 0 {
                        // 首次未提交批次即使只含 Core 内部事实，也必须释放交付屏障；
                        // 已提交批次没有待交付状态，过滤后可直接继续读取下一帧。
                        if commit {
                            let result = state
                                .session
                                .as_mut()
                                .ok_or_else(denied)?
                                .discard_pending_delivery();
                            if result.is_err() {
                                let session = state.session.take();
                                drop(state);
                                if let Some(session) = session {
                                    session.cancel();
                                    session.detach_finalize().await;
                                }
                                return Err(PluginFault::new(
                                    ErrorCode::Fault,
                                    "model event cannot be discarded",
                                ));
                            }
                        }
                        continue;
                    }
                    if payload.len() > self.maximum_payload {
                        let session = state.session.take();
                        drop(state);
                        if let Some(session) = session {
                            session.cancel();
                            session.detach_finalize().await;
                        }
                        return Err(PluginFault::new(
                            ErrorCode::Capacity,
                            "model event exceeds the stream payload limit",
                        ));
                    }
                    state.pending = Some(PendingBatch {
                        payload,
                        events,
                        commit,
                    });
                }
                Ok(None) => {
                    let finalized = state
                        .session
                        .as_ref()
                        .is_some_and(|session| session.is_finalized());
                    let session = state.session.take();
                    drop(state);
                    if !finalized {
                        if let Some(session) = session {
                            session.cancel();
                            session.detach_finalize().await;
                        }
                        return Err(PluginFault::new(
                            ErrorCode::Fault,
                            "model stream ended before finalization",
                        ));
                    }
                    return Ok(ModelRead {
                        payload: Vec::new(),
                        events: 0,
                        end: true,
                    });
                }
                Err(error) => {
                    let fault = gateway_fault(
                        gateway_core::engine::execution::gateway_error_from_engine(&error),
                    );
                    let session = state.session.take();
                    drop(state);
                    if let Some(session) = session {
                        session.cancel();
                        session.detach_finalize().await;
                    }
                    return Err(fault);
                }
            }
        }
    }

    pub(super) fn close(self: &Arc<Self>) {
        self.closed.send_replace(true);
        let stream = Arc::clone(self);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            drop(handle.spawn(async move {
                let session = stream.state.lock().await.session.take();
                if let Some(session) = session {
                    session.cancel();
                    session.detach_finalize().await;
                }
            }));
        }
    }
}

fn encode_events(events: Vec<ProviderEvent>) -> Result<(Vec<u8>, u32), PluginFault> {
    let events = events
        .into_iter()
        .filter_map(|event| project_event(event).transpose())
        .collect::<Result<Vec<_>, _>>()?;
    let count = u32::try_from(events.len()).map_err(|_| invalid())?;
    if events.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let payload = ModelEventBatch { events }
        .encode()
        .map_err(|_| PluginFault::new(ErrorCode::Capacity, "model event batch is too large"))?;
    Ok((payload, count))
}

fn project_event(event: ProviderEvent) -> Result<Option<ExecutionEvent>, PluginFault> {
    let (facts, wire) = event.into_parts();
    let facts = facts
        .into_iter()
        .filter_map(project_fact)
        .collect::<Result<Vec<_>, _>>()?;
    let wire = wire.map(project_wire).transpose()?;
    if facts.is_empty() && wire.is_none() {
        return Ok(None);
    }
    Ok(Some(ExecutionEvent {
        facts,
        wire,
        observation: None,
        session_update: None,
        failure: None,
    }))
}

fn project_fact(event: GatewayEvent) -> Option<Result<CanonicalEvent, PluginFault>> {
    Some(Ok(match event {
        GatewayEvent::Started(meta) => CanonicalEvent::Started {
            id: meta.response_id().to_owned(),
            model: meta.model().map(str::to_owned),
        },
        GatewayEvent::ContentAdded(item) => CanonicalEvent::ContentAdded {
            index: item.index(),
            kind: match item.kind() {
                ContentKind::Text => WireContentKind::Text,
                ContentKind::Reasoning => WireContentKind::Reasoning,
                ContentKind::ToolCall => WireContentKind::ToolCall,
                ContentKind::Image => WireContentKind::Image,
                ContentKind::Audio => WireContentKind::Audio,
                _ => {
                    return Some(Err(PluginFault::new(
                        ErrorCode::Unsupported,
                        "unknown content kind",
                    )));
                }
            },
        },
        GatewayEvent::TextDelta(delta) => CanonicalEvent::TextDelta {
            index: delta.content_index,
            text: delta.text,
        },
        GatewayEvent::ReasoningDelta(delta) => CanonicalEvent::ReasoningDelta {
            index: delta.content_index,
            text: delta.text,
        },
        GatewayEvent::ToolCallDelta(delta) => CanonicalEvent::ToolCallDelta {
            index: delta.content_index,
            id: delta.call_id,
            name: delta.name,
            arguments: delta.arguments_delta,
        },
        GatewayEvent::Usage(usage) => CanonicalEvent::Usage {
            usage: WireUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cached_tokens: usage.cached_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
                image_input_tokens: usage.image_input_tokens,
                image_output_tokens: usage.image_output_tokens,
                total_tokens: usage.total_tokens,
            },
        },
        // 费用已经由子请求的 Core 结算；不把它重新作为父插件可上报的用量输入。
        GatewayEvent::CalculatedCost(_) | GatewayEvent::ProviderCost(_) => return None,
        GatewayEvent::Completed(meta) => CanonicalEvent::Completed {
            id: meta.response_id().to_owned(),
            model: meta.model().map(str::to_owned),
            reason: match meta.finish_reason().unwrap_or(FinishReason::Other) {
                FinishReason::Stop => WireFinishReason::Stop,
                FinishReason::Length => WireFinishReason::Length,
                FinishReason::ToolCall => WireFinishReason::ToolCall,
                FinishReason::ContentFilter => WireFinishReason::ContentFilter,
                FinishReason::Other => WireFinishReason::Other,
                _ => WireFinishReason::Other,
            },
        },
        _ => {
            return Some(Err(PluginFault::new(
                ErrorCode::Unsupported,
                "unknown model event",
            )));
        }
    }))
}

fn project_wire(wire: ProtocolWireEvent) -> Result<WireEvent, PluginFault> {
    let protocol = wire.protocol().to_owned();
    let payload = if let Some(body) = wire.raw_http_body_bytes() {
        WirePayload::RawBody {
            body: body.to_vec(),
        }
    } else if let Some(body) = wire.raw_json_body() {
        WirePayload::RawJson {
            body: body.to_vec(),
        }
    } else if wire.has_json_data() {
        WirePayload::Json {
            event: wire.event_type().map(str::to_owned),
            data: wire.data().clone(),
            id: wire.sse_id().map(str::to_owned),
            retry: wire.sse_retry(),
            raw_sse: wire.raw_sse_frame().map(|frame| frame.to_vec()),
        }
    } else if let Some(frame) = wire.raw_sse_frame() {
        WirePayload::RawSse {
            frame: frame.to_vec(),
        }
    } else {
        return Err(PluginFault::new(
            ErrorCode::Fault,
            "model wire event has no payload",
        ));
    };
    Ok(WireEvent { protocol, payload })
}

pub(super) fn gateway_fault(error: GatewayError) -> PluginFault {
    let code = match error.kind() {
        GatewayErrorKind::InvalidRequest => ErrorCode::InvalidInput,
        GatewayErrorKind::Unsupported | GatewayErrorKind::ModelNotFound => ErrorCode::Unsupported,
        GatewayErrorKind::Unauthorized => ErrorCode::PermissionDenied,
        GatewayErrorKind::PolicyDenied => ErrorCode::Rejected,
        GatewayErrorKind::AccountCapacityUnavailable
        | GatewayErrorKind::ConcurrencyQueueFull
        | GatewayErrorKind::ConcurrencyQueueTimeout
        | GatewayErrorKind::RateLimited
        | GatewayErrorKind::NoAvailableProvider => ErrorCode::Capacity,
        GatewayErrorKind::Timeout => ErrorCode::Timeout,
        GatewayErrorKind::Cancelled => ErrorCode::Cancelled,
        GatewayErrorKind::UpstreamUnavailable
        | GatewayErrorKind::ProviderInfrastructureUnavailable => ErrorCode::Upstream,
        GatewayErrorKind::Internal => ErrorCode::Fault,
        _ => ErrorCode::Fault,
    };
    PluginFault::new(code, "nested model execution failed")
}

fn encode_result(value: impl serde::Serialize) -> Result<RpcReply, PluginFault> {
    Ok(RpcReply {
        result: serde_json::to_value(value).map_err(|_| invalid())?,
        payload: Vec::new(),
    })
}
