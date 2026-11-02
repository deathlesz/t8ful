#![allow(dead_code)]
//! Minimal loopback HTTP/1.1 + SSE server used by provider tests.
//!
//! It speaks just enough HTTP to serve scripted replies and records each request
//! so tests can assert on authentication and the JSON body. Replies are written
//! as caller-provided chunks, which lets tests split SSE frames at arbitrary
//! byte boundaries and truncate streams on purpose.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone)]
pub struct RequestRecord {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub body: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub content_type: String,
    pub chunks: Vec<Vec<u8>>,
    pub delay: Duration,
}

impl Reply {
    pub fn sse(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream".to_owned(),
            chunks,
            delay: Duration::ZERO,
        }
    }

    pub fn json(body: impl Into<String>) -> Self {
        Self::status(200, body)
    }

    pub fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "application/json".to_owned(),
            chunks: vec![body.into().into_bytes()],
            delay: Duration::ZERO,
        }
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

pub fn sse_frame(data: &str) -> String {
    format!("data: {data}\n\n")
}

/// Split a payload into small chunks to exercise frame reassembly.
pub fn chunked(payload: Vec<u8>, size: usize) -> Vec<Vec<u8>> {
    payload.chunks(size).map(<[u8]>::to_vec).collect()
}

pub struct FakeServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RequestRecord>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl FakeServer {
    pub async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake server");
        let addr = listener.local_addr().expect("fake server address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(replies)));
        let recorded = Arc::clone(&requests);

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };

                let reply = queue.lock().expect("reply queue").pop_front();
                let recorded = Arc::clone(&recorded);

                let Some(reply) = reply else {
                    continue;
                };

                tokio::spawn(async move {
                    let _ = serve(&mut stream, reply, recorded).await;
                });
            }
        });

        Self {
            addr,
            requests,
            handle,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn requests(&self) -> Vec<RequestRecord> {
        self.requests.lock().expect("request log").clone()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve(
    stream: &mut TcpStream,
    reply: Reply,
    recorded: Arc<Mutex<Vec<RequestRecord>>>,
) -> std::io::Result<()> {
    let (method, path, headers, body) = read_request(stream).await?;

    let parsed_body = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);

    recorded.lock().expect("request log").push(RequestRecord {
        method,
        path,
        authorization: headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.clone()),
        body: parsed_body,
    });

    let reason = match reply.status {
        200 => "OK",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Status",
    };

    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        reply.status, reason, reply.content_type
    );

    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    for chunk in reply.chunks {
        stream.write_all(&chunk).await?;
        stream.flush().await?;

        if !reply.delay.is_zero() {
            tokio::time::sleep(reply.delay).await;
        }
    }

    stream.shutdown().await
}

type Headers = Vec<(String, String)>;

async fn read_request(
    stream: &mut TcpStream,
) -> std::io::Result<(String, String, Headers, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];

    let head_end = loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position + 4;
        }

        let read = stream.read(&mut chunk).await?;

        if read == 0 {
            return Ok((String::new(), String::new(), Vec::new(), String::new()));
        }

        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");

    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();

    let mut headers = Headers::new();

    for line in lines {
        if line.is_empty() {
            continue;
        }

        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }

    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = buffer[head_end..].to_vec();

    while body.len() < content_length {
        let read = stream.read(&mut chunk).await?;

        if read == 0 {
            break;
        }

        body.extend_from_slice(&chunk[..read]);
    }

    Ok((
        method,
        path,
        headers,
        String::from_utf8_lossy(&body).into_owned(),
    ))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
