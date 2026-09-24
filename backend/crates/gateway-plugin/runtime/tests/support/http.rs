use std::time::Duration;

use serde_json::Value;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    sync::oneshot,
    task::JoinHandle,
};

/// 用明确的响应屏障验证并发顺序，不依赖数据库操作恰好快于固定延迟。
pub struct GatedResponse {
    uri: String,
    received: Option<oneshot::Receiver<()>>,
    release: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl GatedResponse {
    pub async fn start(body: Value) -> Self {
        Self::start_batch(body, 1).await
    }

    pub async fn start_batch(body: Value, requests: usize) -> Self {
        assert!((1..=16).contains(&requests));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("http://{}", listener.local_addr().unwrap());
        let (received_tx, received) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let sockets = futures::future::join_all((0..requests).map(|_| async {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0, "测试请求未发送完整请求头");
                    request.extend_from_slice(&buffer[..count]);
                    assert!(request.len() <= 16 * 1024, "测试请求头超出预算");
                }
                socket
            }))
            .await;
            received_tx.send(()).unwrap();
            release_rx.await.unwrap();
            let body = serde_json::to_vec(&body).unwrap();
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            for mut socket in sockets {
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        Self {
            uri,
            received: Some(received),
            release: Some(release),
            task: Some(task),
        }
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub async fn received(&mut self) {
        tokio::time::timeout(Duration::from_secs(5), self.received.take().unwrap())
            .await
            .expect("上游应收到请求后再进行并发变更")
            .unwrap();
    }

    pub async fn respond(&mut self) {
        self.release.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.task.take().unwrap())
            .await
            .expect("上游应在屏障解除后发送响应")
            .unwrap();
    }
}

impl Drop for GatedResponse {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
