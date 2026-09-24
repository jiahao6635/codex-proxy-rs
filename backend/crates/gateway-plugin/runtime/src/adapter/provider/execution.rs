use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use super::{PluginProvider, events};
use crate::{RpcError, RpcSession, RpcStream};
use futures::{StreamExt as _, stream};
use gateway_core::{
    engine::{
        AttemptContext,
        provider::{
            Provider, ProviderCallMetadata, ProviderMiddlewareTerminal, ProviderRequest,
            ProviderStream,
        },
    },
    error::{ProviderError, ProviderErrorKind},
    operation::{Operation, ProviderHttpMethod as CoreProviderHttpMethod},
    upstream::{UpstreamSendState, UpstreamTransport},
};
use gateway_plugin_sdk::{
    CallContext, Stage,
    call::provider::{
        ExecutePrepared, ExecutionInput, OperationKind, PrepareExecution, PreparedExecution,
        ProviderHttpHeader, ProviderHttpMethod, ProviderHttpRequest,
    },
};

impl PluginProvider {
    pub(super) async fn prepare_execution(
        self: Arc<Self>,
        request: ProviderRequest,
        attempt: AttemptContext,
    ) -> Result<ProviderStream, ProviderError> {
        if attempt.extension_scope().contains(&self.instance_id) {
            return Err(ProviderError::new(
                ProviderErrorKind::PermissionDenied,
                UpstreamSendState::NotSent,
            ));
        }
        if let Operation::ProviderHttp(http) = request.operation()
            && !self
                .http_endpoints
                .get(http.endpoint())
                .is_some_and(|methods| methods.contains(&wire_http_method(http.method())))
        {
            return Err(ProviderError::new(
                ProviderErrorKind::Unsupported,
                UpstreamSendState::NotSent,
            ));
        }
        let continuation_state = request
            .operation()
            .provider_session_state(self.kind.as_str())
            .cloned();
        match self
            .continuation
            .interpreter_target(continuation_state.as_ref())?
        {
            super::continuation::InterpreterTarget::Current { previous_owner } => {
                if let (Some(owner), Some(drain)) =
                    (previous_owner, self.continuation_drain.upgrade())
                {
                    drain.supersede(&owner);
                }
            }
            super::continuation::InterpreterTarget::Retired { owner } => {
                let retired = self
                    .continuation_drain
                    .upgrade()
                    .and_then(|drain| drain.resolve(&owner))
                    .filter(|provider| {
                        provider.instance_id == self.instance_id
                            && provider.kind == self.kind
                            && provider.continuation.matches_authority(&self.continuation)
                    })
                    .ok_or_else(super::continuation::invalid_checkpoint)?;
                return Provider::execute(retired, request, attempt).await;
            }
        }
        let model = request
            .candidate()
            .upstream_model()
            .map(|model| model.as_str().to_owned());
        let billing = super::billing::BillingPlan::new(
            self.pricing.as_ref(),
            model.as_deref(),
            attempt.pricing().get(self.kind.as_str()),
        );
        let (credential, lease) = self
            .select_account(
                model.as_deref(),
                &request.operation().capability_requirements(),
                &attempt,
            )
            .await?;
        let middleware_operation = request.operation().clone();
        let middleware_provider = self.kind.clone();
        let middleware_model = model.clone();
        let middleware_account = credential.account.id().clone();
        let provider = Arc::clone(&self);
        let terminal_attempt = attempt.clone();
        let terminal: ProviderMiddlewareTerminal = Box::new(move |processed, headers| {
            Box::pin(async move {
                let headers = headers
                    .into_iter()
                    .map(|header| {
                        let (name, value) = header.into_parts();
                        ProviderHttpHeader {
                            name,
                            value: value.to_vec(),
                        }
                    })
                    .collect();
                let (protocol, operation, body, context, http_request) = match &processed {
                    Operation::Generate(generate) => {
                        let payload = generate.protocol_payload();
                        (
                            payload.protocol(),
                            OperationKind::Generate,
                            serde_json::to_vec(payload.body()).map_err(|_| protocol_error())?,
                            payload.context(),
                            None,
                        )
                    }
                    Operation::GenerateImage(image) => {
                        let payload = image.payload();
                        (
                            payload.protocol(),
                            OperationKind::GenerateImage,
                            payload.body().to_vec(),
                            payload.context(),
                            None,
                        )
                    }
                    Operation::Search(search) => {
                        let payload = search.payload();
                        (
                            payload.protocol(),
                            OperationKind::Search,
                            payload.body().to_vec(),
                            payload.context(),
                            None,
                        )
                    }
                    Operation::CountTokens(count) => {
                        let payload = count.payload();
                        (
                            payload.protocol(),
                            OperationKind::CountTokens,
                            payload.body().to_vec(),
                            payload.context(),
                            None,
                        )
                    }
                    Operation::ProviderHttp(http) => {
                        let payload = http.payload();
                        (
                            payload.protocol(),
                            OperationKind::ProviderHttp,
                            payload.body().to_vec(),
                            payload.context(),
                            Some(ProviderHttpRequest {
                                endpoint: http.endpoint().to_owned(),
                                method: wire_http_method(http.method()),
                                query: http.query().map(str::to_owned),
                                headers: http
                                    .headers()
                                    .iter()
                                    .map(|header| ProviderHttpHeader {
                                        name: header.name().to_owned(),
                                        value: header.value().to_vec(),
                                    })
                                    .collect(),
                            }),
                        )
                    }
                    _ => {
                        return Err(ProviderError::new(
                            ProviderErrorKind::Unsupported,
                            UpstreamSendState::NotSent,
                        ));
                    }
                };
                if !provider
                    .input_formats
                    .iter()
                    .any(|format| format == protocol)
                {
                    return Err(ProviderError::new(
                        ProviderErrorKind::Unsupported,
                        UpstreamSendState::NotSent,
                    ));
                }
                let binding = provider.continuation.bind_account(
                    credential.account.id().as_str().into(),
                    credential.account.revision().get(),
                );
                let provider_session_state = binding.restore(continuation_state.as_ref())?;
                let mut context_call = provider
                    .session
                    .context(Stage::Attempt, remaining(&terminal_attempt)?);
                context_call.request_id = Some(terminal_attempt.request_id().as_str().into());
                context_call.attempt_id = Some(format!(
                    "{}:{}",
                    terminal_attempt.request_id(),
                    terminal_attempt.attempt_index()
                ));
                context_call.account_id = Some(credential.account.id().as_str().into());
                context_call.credential_revision = Some(credential.account.revision().get());
                let scope = provider
                    .callbacks
                    .prepare_execution(&context_call, credential.account.outbound_proxy().cloned())
                    .map_err(|_| protocol_error())?;
                let params = PrepareExecution {
                    protocol: protocol.into(),
                    operation,
                    model: model.clone(),
                    account_id: credential.account.id().as_str().into(),
                    credential_revision: credential.account.revision().get(),
                    credential: credential.credential.expose_to_provider().clone(),
                    context: context.clone(),
                    request_profile: terminal_attempt.request_profile().map(|profile| {
                        serde_json::Value::Object(profile.expose_to_provider().clone())
                    }),
                    headers,
                    provider_session_state,
                    http_request,
                };
                let reply = provider
                    .session
                    .call(
                        "provider.prepare",
                        context_call.clone(),
                        serde_json::json!({}),
                        ExecutionInput {
                            request: params,
                            body,
                        }
                        .encode()
                        .map_err(|_| protocol_error())?,
                    )
                    .await
                    .map_err(|error| rpc_error(error, UpstreamSendState::NotSent))?;
                let prepared: PreparedExecution =
                    serde_json::from_value(reply.result).map_err(|_| protocol_error())?;
                if !reply.payload.is_empty()
                    || prepared.token.is_empty()
                    || prepared.token.len() > 256
                {
                    return Err(protocol_error());
                }
                let prepared_call = PreparedCall {
                    session: Arc::clone(&provider.session),
                    context: context_call.clone(),
                    token: prepared.token.clone(),
                };
                let transport =
                    UpstreamTransport::new(prepared.transport).map_err(|_| protocol_error())?;
                let metadata = match request.candidate().upstream_model() {
                    Some(model) => ProviderCallMetadata::new(
                        provider.kind.clone(),
                        model.clone(),
                        credential.account.id().clone(),
                        transport.clone(),
                    ),
                    None => ProviderCallMetadata::for_provider_endpoint(
                        provider.kind.clone(),
                        credential.account.id().clone(),
                        transport.clone(),
                    ),
                };
                let session = Arc::clone(&provider.session);
                let output_formats = provider.output_formats.clone();
                let execution_scope = scope.clone();
                // 只在 Core 记录 attempt 后首次 poll 时激活；准备调用不具备 HTTP/模型回调权限。
                let open = stream::once(async move {
                    context_call.stage = Stage::Execution;
                    context_call.timeout_ms = remaining(&terminal_attempt)?
                        .as_millis()
                        .min(u128::from(u64::MAX))
                        as u64;
                    let params = serde_json::to_value(ExecutePrepared {
                        token: prepared.token,
                    })
                    .map_err(|_| protocol_error())?;
                    execution_scope.activate();
                    let stream = session
                        .call_stream("provider.execute", context_call, params, vec![])
                        .await
                        .map_err(|error| rpc_error(error, execution_scope.send_state()))?;
                    if stream.initial.result != serde_json::json!({"stream":true})
                        || !stream.initial.payload.is_empty()
                    {
                        return Err(ProviderError::new(
                            ProviderErrorKind::Protocol,
                            execution_scope.send_state(),
                        ));
                    }
                    Ok((stream, billing))
                })
                .map(move |opened| {
                    let formats = output_formats.clone();
                    let scope = scope.clone();
                    let transport = transport.clone();
                    let binding = binding.clone();
                    stream::try_unfold(
                        (opened, formats, scope, transport, binding),
                        |(opened, formats, scope, transport, binding)| async move {
                            let (mut stream, billing): (RpcStream, super::billing::BillingPlan) =
                                opened?;
                            match stream
                                .next()
                                .await
                                .map_err(|error| rpc_error(error, scope.send_state()))?
                            {
                                Some(bytes) => Ok(Some((
                                    events::decode(
                                        &bytes,
                                        &formats,
                                        &billing,
                                        &transport,
                                        &binding,
                                        scope.send_state(),
                                    )?,
                                    (Ok((stream, billing)), formats, scope, transport, binding),
                                ))),
                                None => Ok(None),
                            }
                        },
                    )
                })
                .flatten();
                Ok(ProviderStream::new(metadata, open, (lease, prepared_call))
                    .with_account_feedback(provider.ports.account_feedback()))
            })
        });
        attempt
            .execute_middleware(
                middleware_operation,
                middleware_provider,
                middleware_model,
                middleware_account,
                terminal,
            )
            .await
    }
}

fn remaining(attempt: &AttemptContext) -> Result<Duration, ProviderError> {
    attempt
        .deadline()
        .duration_since(SystemTime::now())
        .ok()
        .filter(|duration| !duration.is_zero())
        .map(|duration| duration.min(Duration::from_secs(120)))
        .ok_or_else(|| ProviderError::new(ProviderErrorKind::Timeout, UpstreamSendState::NotSent))
}

const fn wire_http_method(method: CoreProviderHttpMethod) -> ProviderHttpMethod {
    match method {
        CoreProviderHttpMethod::Get => ProviderHttpMethod::Get,
        CoreProviderHttpMethod::Post => ProviderHttpMethod::Post,
    }
}

pub(super) fn protocol_error() -> ProviderError {
    ProviderError::new(ProviderErrorKind::Protocol, UpstreamSendState::NotSent)
}

pub(super) fn stream_protocol_error() -> ProviderError {
    ProviderError::new(ProviderErrorKind::Protocol, UpstreamSendState::Ambiguous)
}

struct PreparedCall {
    session: Arc<RpcSession>,
    context: CallContext,
    token: String,
}

impl Drop for PreparedCall {
    fn drop(&mut self) {
        // 冷流可能从未激活；显式释放对端准备句柄，账号租约由外层独立归还。
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let session = self.session.clone();
            let mut context = self.context.clone();
            context.timeout_ms = 1_000;
            let params = serde_json::json!({"token":self.token});
            runtime.spawn(async move {
                let _ = session
                    .call("provider.discard", context, params, vec![])
                    .await;
            });
        }
    }
}

fn rpc_error(error: RpcError, observed: UpstreamSendState) -> ProviderError {
    use gateway_plugin_sdk::ErrorCode;
    match error {
        RpcError::Remote(fault) => {
            let kind = match fault.code {
                ErrorCode::Unsupported => ProviderErrorKind::Unsupported,
                ErrorCode::Rejected | ErrorCode::InvalidInput | ErrorCode::PermissionDenied => {
                    ProviderErrorKind::InvalidRequest
                }
                ErrorCode::Timeout => ProviderErrorKind::Timeout,
                ErrorCode::Cancelled => ProviderErrorKind::Cancelled,
                _ => ProviderErrorKind::Unavailable,
            };
            let state = send_watermark(observed, fault.send_state);
            let error = ProviderError::new(kind, state);
            match fault.http_status {
                Some(status) => error.with_status(status),
                None => error,
            }
        }
        RpcError::Timeout => ProviderError::new(ProviderErrorKind::Timeout, observed),
        RpcError::Protocol | RpcError::Context => {
            ProviderError::new(ProviderErrorKind::Protocol, observed)
        }
        _ => ProviderError::new(ProviderErrorKind::Unavailable, observed),
    }
}

pub(super) fn send_watermark(
    observed: UpstreamSendState,
    reported: gateway_plugin_sdk::SendState,
) -> UpstreamSendState {
    use gateway_plugin_sdk::SendState;
    // 宿主事实不可回退；控制面 fault 与执行失败封套共用同一单调上界。
    match (observed, reported) {
        (UpstreamSendState::Sent, _) | (_, SendState::Sent) => UpstreamSendState::Sent,
        (UpstreamSendState::Ambiguous, _) | (_, SendState::Ambiguous) => {
            UpstreamSendState::Ambiguous
        }
        _ => UpstreamSendState::NotSent,
    }
}
