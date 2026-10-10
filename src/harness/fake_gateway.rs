//! A loopback OpenAI-compatible gateway for wire-level tests.
//!
//! Test-only, and only under `harness`: it exists so the real model adapter can
//! be driven end to end — request body built, sent over HTTP, response parsed —
//! without a network or a provider bill. It speaks just enough HTTP/1.1 for one
//! JSON `POST` per connection, which is all an OpenAI-compatible client sends.
//!
//! Replies are scripted in order, and every request body is recorded, so a test
//! asserts both halves of the wire: what the adapter sent and what it made of
//! the answer.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One scripted reply: an HTTP status and a JSON body.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
}

impl Reply {
    /// A `200` chat completion whose message content is `content`.
    pub fn completion(model: &str, content: &str, finish_reason: &str, usage: Value) -> Self {
        Self {
            status: 200,
            body: serde_json::json!({
                "id": "gen-test",
                "object": "chat.completion",
                "model": model,
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": content},
                    "finish_reason": finish_reason
                }],
                "usage": usage
            }),
        }
    }

    /// A non-success status with an OpenAI-shaped error body.
    pub fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            body: serde_json::json!({"error": {"message": message, "code": status}}),
        }
    }
}

/// A running fake gateway. Dropping it stops accepting connections.
pub struct FakeGateway {
    /// `http://127.0.0.1:<port>/v1`, ready to be a `models.base_url`.
    pub base_url: String,
    requests: Arc<Mutex<Vec<(usize, Value)>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeGateway {
    /// Start a gateway that assigns each accepted connection the next reply in
    /// `script`, and with a `500` once the script runs out.
    pub async fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let base_url = format!("http://{}/v1", listener.local_addr().expect("addr"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut script = std::collections::VecDeque::from(script);
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            let mut next_sequence = 0;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                // Assign before reading a body so a delayed first sender cannot
                // steal a later request's scripted reply or reorder the log.
                let sequence = next_sequence;
                next_sequence += 1;
                let reply = script
                    .pop_front()
                    .unwrap_or_else(|| Reply::error(500, "script exhausted"));
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let Some(body) = read_request_body(&mut stream).await else {
                        return;
                    };
                    recorded.lock().unwrap().push((sequence, body));
                    let payload = reply.body.to_string();
                    let response = format!(
                        "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        reply.status,
                        payload.len(),
                        payload
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self {
            base_url,
            requests,
            task,
        }
    }

    /// Every request body received so far, in arrival order.
    pub fn requests(&self) -> Vec<Value> {
        let mut received = self.requests.lock().unwrap().clone();
        received.sort_by_key(|(sequence, _)| *sequence);
        received.into_iter().map(|(_, body)| body).collect()
    }
}

/// Read one HTTP/1.1 request and return its JSON body.
async fn read_request_body(stream: &mut tokio::net::TcpStream) -> Option<Value> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())?;
    while buffer.len() < header_end + length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    serde_json::from_slice(&buffer[header_end..header_end + length]).ok()
}

#[tokio::test]
async fn scripted_replies_follow_connection_order_even_when_the_first_body_is_delayed() {
    use tokio::net::TcpStream;
    let gateway = FakeGateway::start(vec![
        Reply::error(401, "first"),
        Reply::error(402, "second"),
    ])
    .await;
    let address = gateway
        .base_url
        .trim_start_matches("http://")
        .trim_end_matches("/v1");
    let mut first = TcpStream::connect(address).await.unwrap();
    first
        .write_all(b"POST /v1 HTTP/1.1\r\ncontent-length: 8\r\n\r\n")
        .await
        .unwrap();
    let mut second = TcpStream::connect(address).await.unwrap();
    second
        .write_all(b"POST /v1 HTTP/1.1\r\ncontent-length: 8\r\n\r\n{\"id\":2}")
        .await
        .unwrap();
    let mut reply = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        second.read_to_string(&mut reply),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.starts_with("HTTP/1.1 402"), "{reply}");
    first.write_all(b"{\"id\":1}").await.unwrap();
    reply.clear();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        first.read_to_string(&mut reply),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.starts_with("HTTP/1.1 401"), "{reply}");
    assert_eq!(
        gateway.requests(),
        vec![serde_json::json!({"id":1}), serde_json::json!({"id":2})]
    );
}
