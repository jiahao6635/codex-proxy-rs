use std::sync::Arc;

use gateway_plugin_sdk::{
    ErrorCode, Frame, Message, Permission, PluginFault, Stage,
    client::{read_frame, write_frame},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot},
};

use super::session::{CallbackHandler, RpcError, RpcLimits, RpcReply, Shared};

#[expect(
    clippy::too_many_arguments,
    reason = "双向资源在一个入口交给独立读写任务，不引入重复连接状态"
)]
pub(super) fn start<W, R>(
    writer: W,
    reader: R,
    data: mpsc::Receiver<Frame>,
    control: mpsc::Receiver<Frame>,
    shared: Arc<Shared>,
    callbacks: Arc<dyn CallbackHandler>,
    permissions: Vec<Permission>,
    limits: RpcLimits,
) where
    W: AsyncWrite + Unpin + Send + 'static,
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(write_loop(
        writer,
        data,
        control,
        Arc::clone(&shared),
        limits.maximum_frame_bytes,
    ));
    tokio::spawn(read_loop(reader, shared, callbacks, permissions, limits));
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut data: mpsc::Receiver<Frame>,
    mut control: mpsc::Receiver<Frame>,
    shared: Arc<Shared>,
    maximum: usize,
) {
    let mut stopped = shared.stopped.subscribe();
    loop {
        if *stopped.borrow() {
            return;
        }
        let frame = tokio::select! {
            biased;
            _ = stopped.changed() => return,
            frame = control.recv() => frame,
            frame = data.recv() => frame,
        };
        let Some(frame) = frame else {
            shared.fail(RpcError::Closed);
            return;
        };
        // 撤销尚未发送的调用时丢弃排队帧；已开始写入的 Call 必须先于 Cancel。
        if let Message::Call { id, .. } = &frame.message {
            if !shared.mark_transmitted(*id) {
                continue;
            }
        } else if let Message::Credit { id, .. } = &frame.message
            && shared.context(*id).is_none()
        {
            continue;
        }
        let written = tokio::select! {
            biased;
            _ = stopped.changed() => return,
            result = write_frame(&mut writer, &frame, maximum) => result,
        };
        if written.is_err() {
            shared.fail(RpcError::Closed);
            return;
        }
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    shared: Arc<Shared>,
    callbacks: Arc<dyn CallbackHandler>,
    permissions: Vec<Permission>,
    limits: RpcLimits,
) {
    let mut stopped = shared.stopped.subscribe();
    let mut last_callback = 0;
    loop {
        if *stopped.borrow() {
            return;
        }
        let frame = tokio::select! {
            biased;
            _ = stopped.changed() => return,
            frame = read_frame(&mut reader, limits.maximum_frame_bytes) => frame,
        };
        let Ok(frame) = frame else {
            shared.fail(RpcError::Closed);
            return;
        };
        let result = match frame.message {
            Message::Cancelled { id } if frame.payload.is_empty() => {
                shared.acknowledge_cancellation(id)
            }
            Message::Stream { id, sequence } => shared.stream_chunk(id, sequence, frame.payload),
            Message::End { id, error } if frame.payload.is_empty() => shared.stream_end(id, error),
            Message::Result { id, result } => shared.finish(
                id,
                Ok(RpcReply {
                    result,
                    payload: frame.payload,
                }),
            ),
            Message::Error { id, error } if frame.payload.is_empty() => {
                shared.finish(id, Err(RpcError::Remote(error)))
            }
            Message::Callback {
                id,
                parent_id,
                method,
                params,
            } => {
                if id == 0 || id % 2 != 0 || id <= last_callback {
                    Err(RpcError::Protocol)
                } else if let Some(context) = shared.context(parent_id) {
                    last_callback = id;
                    if !callback_allowed(&method, context.stage, &permissions) {
                        shared.send_control(Frame::control(Message::Error {
                            id,
                            error: PluginFault::new(
                                ErrorCode::PermissionDenied,
                                "callback is not authorized in this stage",
                            ),
                        }));
                    } else if let Some(permit) = shared.try_callback_slot() {
                        let handler = Arc::clone(&callbacks);
                        let response = Arc::clone(&shared);
                        let (ready, started) = oneshot::channel();
                        let task = tokio::spawn(async move {
                            // 先把任务归属登记到父调用，再允许执行宿主操作。
                            if started.await.is_err() {
                                return;
                            }
                            let result = handler.call(context, method, params, frame.payload).await;
                            let reply = match result {
                                Ok(reply) => Frame {
                                    message: Message::Result {
                                        id,
                                        result: reply.result,
                                    },
                                    payload: reply.payload,
                                },
                                Err(error) => Frame::control(Message::Error { id, error }),
                            };
                            response.send_control(reply);
                            drop(permit);
                        });
                        if shared.track_callback(parent_id, task.abort_handle()) {
                            let _ = ready.send(());
                        }
                    } else {
                        shared.send_control(Frame::control(Message::Error {
                            id,
                            error: PluginFault::new(
                                ErrorCode::Capacity,
                                "callback capacity is exhausted",
                            ),
                        }));
                    }
                    Ok(())
                } else if shared.is_cancelling(parent_id) {
                    last_callback = id;
                    shared.send_control(Frame::control(Message::Error {
                        id,
                        error: PluginFault::new(ErrorCode::Cancelled, "parent call was cancelled"),
                    }));
                    Ok(())
                } else {
                    Err(RpcError::Protocol)
                }
            }
            _ => Err(RpcError::Protocol),
        };
        if let Err(error) = result {
            shared.fail(error);
            return;
        }
    }
}

fn callback_allowed(method: &str, stage: Stage, permissions: &[Permission]) -> bool {
    // 未登录端点只服务声明的公开内容，不能借宿主回调取得管理资源。
    if stage == Stage::PublicManagement {
        return false;
    }
    if method == "host.log" {
        return true;
    }
    if matches!(stage, Stage::Registration | Stage::Configuration) {
        return false;
    }
    if matches!(
        method,
        "host.state.get" | "host.state.put" | "host.state.delete"
    ) {
        return true;
    }
    if matches!(
        method,
        gateway_plugin_sdk::call::middleware::NEXT_METHOD
            | gateway_plugin_sdk::call::middleware::BODY_READ_METHOD
            | gateway_plugin_sdk::call::middleware::BODY_CLOSE_METHOD
    ) {
        return matches!(stage, Stage::Request | Stage::Attempt);
    }
    let permission = match method {
        "host.http.do"
        | "host.http.do_stream"
        | "host.http.stream_read"
        | "host.http.stream_close" => Permission::Network,
        "host.model.execute"
        | "host.model.execute_stream"
        | "host.model.stream_read"
        | "host.model.stream_close"
        | "host.models.list"
        | "host.keys.list" => Permission::Models,
        "host.auth.list" | "host.auth.get_runtime" | "host.auth.get" | "host.auth.save" => {
            Permission::Accounts
        }
        "host.affinity.lookup" => Permission::Requests,
        _ => return false,
    };
    permissions.contains(&permission)
}
