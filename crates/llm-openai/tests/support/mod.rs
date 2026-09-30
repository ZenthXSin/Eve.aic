//! 仅绑定随机 loopback 端口的 HTTP 验收夹具，不需要外网或真实凭据。
use serde_json::Value;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
    task::{JoinHandle, JoinSet},
};

pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
    pub chunked: bool,
    pub body_delay: Duration,
    pub declared_length: Option<usize>,
}
impl Reply {
    pub fn json(value: Value) -> Self {
        Self {
            status: 200,
            body: serde_json::to_vec(&value).unwrap(),
            headers: vec![],
            chunked: false,
            body_delay: Duration::ZERO,
            declared_length: None,
        }
    }
}
pub struct Captured {
    pub headers: String,
    pub body: Value,
}
pub struct Server {
    pub url: String,
    pub requests: mpsc::UnboundedReceiver<Captured>,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    pub async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let (tx, requests) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut replies = std::collections::VecDeque::from(replies);
            let mut workers = JoinSet::new();
            loop {
                let reply = replies.pop_front().unwrap_or_else(|| {
                    let mut reply = Reply::json(serde_json::json!({}));
                    reply.status = 503;
                    reply
                });
                let (mut socket, _) = listener.accept().await.unwrap();
                let tx = tx.clone();
                workers.spawn(async move {
                    let request = tokio::time::timeout(Duration::from_secs(5), async {
                        let mut bytes = Vec::new();
                        let header_end = loop {
                            let mut chunk = [0; 4096];
                            let count = socket.read(&mut chunk).await.unwrap();
                            assert!(count > 0);
                            bytes.extend_from_slice(&chunk[..count]);
                            assert!(bytes.len() < 1024 * 1024);
                            if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                                break i + 4;
                            }
                        };
                        let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                        let len: usize = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|s| s.trim().parse().unwrap())
                            })
                            .unwrap();
                        while bytes.len() < header_end + len {
                            let mut chunk = [0; 4096];
                            let count = socket.read(&mut chunk).await.unwrap();
                            assert!(count > 0);
                            bytes.extend_from_slice(&chunk[..count]);
                        }
                        Captured {
                            headers,
                            body: serde_json::from_slice(&bytes[header_end..header_end + len])
                                .unwrap(),
                        }
                    })
                    .await
                    .unwrap();
                    tx.send(request).unwrap();
                    let size = if reply.chunked {
                        "Transfer-Encoding: chunked".into()
                    } else {
                        format!("Content-Length: {}", reply.declared_length.unwrap_or(reply.body.len()))
                    };
                    let content_type = reply.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                        .map(|(_, v)| v.as_str()).unwrap_or("application/json");
                    let mut headers = format!(
                        "HTTP/1.1 {} Fixture\r\nConnection: close\r\nContent-Type: {content_type}\r\n{size}\r\n",
                        reply.status
                    );
                    for (key, value) in reply.headers {
                        if key.eq_ignore_ascii_case("content-type") { continue; }
                        headers.push_str(&format!("{key}: {value}\r\n"));
                    }
                    headers.push_str("\r\n");
                    if socket.write_all(headers.as_bytes()).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(reply.body_delay).await;
                    if reply.chunked {
                        for chunk in reply.body.chunks(7) {
                            let framed = format!("{:x}\r\n", chunk.len());
                            if socket.write_all(framed.as_bytes()).await.is_err() {
                                return;
                            }
                            if socket.write_all(chunk).await.is_err() {
                                return;
                            }
                            if socket.write_all(b"\r\n").await.is_err() {
                                return;
                            }
                        }
                        let _ = socket.write_all(b"0\r\n\r\n").await;
                    } else {
                        let _ = socket.write_all(&reply.body).await;
                    }
                });
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
    pub async fn next(&mut self) -> Captured {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
}
pub fn final_response(text: &str) -> Value {
    serde_json::json!({"status":"completed", "error":null, "output":[
        {"type":"message", "role":"assistant", "status":"completed", "content":[
            {"type":"output_text", "text":text, "annotations":[]}
        ]}
    ]})
}
