#![allow(dead_code)]
//! Minimal loopback HTTP/1.1 + SSE server used by the CLI, TUI, and headless tests.
//!
//! It speaks just enough HTTP to serve scripted replies and records each request
//! so tests can assert on authentication and the JSON body. Replies are written
//! as caller-provided chunks, which lets tests split SSE frames at arbitrary
//! byte boundaries and truncate streams on purpose.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

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
    /// When set, the response headers and body wait for a notification first.
    pub gate: Option<Arc<Notify>>,
    /// When set, the reply is only used for a request whose system prompt
    /// contains this text.
    ///
    /// Subagent tests need it because a parent and its child are in flight at the
    /// same time and the scripted queue has no inherent ordering between them.
    pub only_for: Option<String>,
}

impl Reply {
    /// Stream a single pre-built body.
    pub fn body(payload: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream".to_owned(),
            chunks: vec![payload],
            delay: Duration::ZERO,
            gate: None,
            only_for: None,
        }
    }

    pub fn sse(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream".to_owned(),
            chunks,
            delay: Duration::ZERO,
            gate: None,
            only_for: None,
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
            gate: None,
            only_for: None,
        }
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn with_gate(mut self, gate: Arc<Notify>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Only answer a request whose system prompt contains `needle`.
    ///
    /// A child's prompt is the parent's prompt plus a subagent section, so this
    /// matches the parent too. Use [`Reply::for_parent`] to pin a reply to the
    /// root agent.
    pub fn for_prompt_containing(mut self, needle: impl Into<String>) -> Self {
        self.only_for = Some(needle.into());
        self
    }

    /// Only answer a request from a child agent.
    pub fn for_child_agent(mut self) -> Self {
        self.only_for = Some(CHILD_MARKER.into());
        self
    }

    /// Only answer a request that is *not* from a child agent.
    pub fn for_parent(mut self) -> Self {
        self.only_for = Some(format!("!{CHILD_MARKER}"));
        self
    }
}

pub fn sse_frame(data: &str) -> String {
    format!("data: {data}\n\n")
}

pub struct FakeServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RequestRecord>>>,
    models_requests: Arc<AtomicUsize>,
    /// Authorization headers seen on catalog requests, oldest first.
    models_authorizations: Arc<Mutex<Vec<Option<String>>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl FakeServer {
    pub async fn start(replies: Vec<Reply>) -> Self {
        Self::start_with_models(replies, &default_models()).await
    }

    pub async fn start_with_models(replies: Vec<Reply>, models: &str) -> Self {
        Self::start_with_models_reply(replies, Reply::json(models)).await
    }

    pub async fn start_with_models_reply(replies: Vec<Reply>, models: Reply) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fake server");
        let addr = listener.local_addr().expect("fake server address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let models_requests = Arc::new(AtomicUsize::new(0));
        let models_authorizations = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(replies)));
        let recorded = Arc::clone(&requests);
        let counted = Arc::clone(&models_requests);
        let catalog_auth = Arc::clone(&models_authorizations);
        let models = Arc::new(models);

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };

                let recorded = Arc::clone(&recorded);
                let counted = Arc::clone(&counted);
                let catalog_auth = Arc::clone(&catalog_auth);
                let queue = Arc::clone(&queue);
                let models = Arc::clone(&models);

                tokio::spawn(async move {
                    let _ =
                        serve(&mut stream, queue, recorded, counted, catalog_auth, &models).await;
                });
            }
        });

        Self {
            addr,
            requests,
            models_requests,
            models_authorizations,
            handle,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn requests(&self) -> Vec<RequestRecord> {
        self.requests.lock().expect("request log").clone()
    }

    pub fn models_requests(&self) -> usize {
        self.models_requests.load(Ordering::SeqCst)
    }

    /// Authorization headers sent with catalog requests.
    pub fn models_authorizations(&self) -> Vec<Option<String>> {
        self.models_authorizations
            .lock()
            .expect("catalog authorization log")
            .clone()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// The section the child factory appends to a child's system prompt.
const CHILD_MARKER: &str = "<subagent";

/// Take the first reply this request may use.
///
/// A reply pinned to another prompt is skipped and left in place for the request
/// it belongs to; an unpinned reply matches anything. This is what lets a parent
/// and a child be in flight at once without consuming each other's scripted
/// answers.
fn take_matching(queue: &mut VecDeque<Reply>, system: &str) -> Option<Reply> {
    let index = queue.iter().position(|reply| {
        let Some(needle) = reply.only_for.as_deref() else {
            return true;
        };

        match needle.strip_prefix('!') {
            Some(negated) => !system.contains(negated),
            None => system.contains(needle),
        }
    })?;

    queue.remove(index)
}

async fn serve(
    stream: &mut TcpStream,
    queue: Arc<Mutex<VecDeque<Reply>>>,
    recorded: Arc<Mutex<Vec<RequestRecord>>>,
    models_requests: Arc<AtomicUsize>,
    models_authorizations: Arc<Mutex<Vec<Option<String>>>>,
    models: &Reply,
) -> std::io::Result<()> {
    let (method, path, headers, body) = read_request(stream).await?;

    if path.ends_with("/models") {
        models_requests.fetch_add(1, Ordering::SeqCst);
        models_authorizations
            .lock()
            .expect("catalog authorization log")
            .push(
                headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .map(|(_, value)| value.clone()),
            );

        return write_response(stream, models.clone()).await;
    }

    let parsed_body = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let system = parsed_body["messages"]
        .as_array()
        .and_then(|messages| messages.first())
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned();

    let reply = {
        let mut queue = queue.lock().expect("reply queue");

        take_matching(&mut queue, &system)
    };

    let Some(reply) = reply else {
        return Ok(());
    };

    recorded.lock().expect("request log").push(RequestRecord {
        method,
        path,
        authorization: headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.clone()),
        body: parsed_body,
    });

    write_response(stream, reply).await
}

async fn write_response(stream: &mut TcpStream, reply: Reply) -> std::io::Result<()> {
    if let Some(gate) = &reply.gate {
        gate.notified().await;
    }

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

fn default_models() -> String {
    serde_json::json!({
        "data": [{
            "id": "vendor/test-model",
            "name": "Test Model",
            "context_length": 128000,
            "top_provider": { "max_completion_tokens": 16384 },
            "architecture": { "input_modalities": ["text", "image"] },
            "pricing": { "prompt": "0", "completion": "0" }
        }]
    })
    .to_string()
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

/// Build a complete SSE body that streams `text` and finishes.
pub fn text_stream(text: &str) -> Vec<u8> {
    let delta = json!({
        "id": "gen-1",
        "choices": [{ "index": 0, "delta": { "content": text } }]
    });
    let finish = json!({
        "id": "gen-1",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
    });

    format!(
        "{}{}{}",
        sse_frame(&delta.to_string()),
        sse_frame(&finish.to_string()),
        sse_frame("[DONE]")
    )
    .into_bytes()
}

/// Build a complete SSE body with provider reasoning before the answer.
pub fn reasoning_stream(reasoning: &str, text: &str) -> Vec<u8> {
    let reasoning_frame = json!({
        "id": "gen-1",
        "choices": [{ "index": 0, "delta": { "reasoning": reasoning } }]
    });
    let text_frame = json!({
        "id": "gen-1",
        "choices": [{ "index": 0, "delta": { "content": text } }]
    });
    let finish = json!({
        "id": "gen-1",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
    });

    format!(
        "{}{}{}{}",
        sse_frame(&reasoning_frame.to_string()),
        sse_frame(&text_frame.to_string()),
        sse_frame(&finish.to_string()),
        sse_frame("[DONE]")
    )
    .into_bytes()
}

/// Build a complete SSE body requesting one tool call.
pub fn tool_stream(call_id: &str, name: &str, arguments: &Value) -> Vec<u8> {
    let call = json!({
        "id": "gen-1",
        "choices": [{ "index": 0, "delta": { "tool_calls": [{
            "index": 0,
            "id": call_id,
            "function": { "name": name, "arguments": arguments.to_string() }
        }] } }]
    });
    let finish = json!({
        "id": "gen-1",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
    });

    format!(
        "{}{}{}",
        sse_frame(&call.to_string()),
        sse_frame(&finish.to_string()),
        sse_frame("[DONE]")
    )
    .into_bytes()
}

/// Build a complete SSE body requesting one tool call with no provider id.
///
/// OpenRouter does not always send an id for a call; the decoder then has to
/// synthesize one.
pub fn tool_stream_without_id(name: &str, arguments: &Value) -> Vec<u8> {
    let call = json!({
        "id": "gen-1",
        "choices": [{ "index": 0, "delta": { "tool_calls": [{
            "index": 0,
            "function": { "name": name, "arguments": arguments.to_string() }
        }] } }]
    });
    let finish = json!({
        "id": "gen-1",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
    });

    format!(
        "{}{}{}",
        sse_frame(&call.to_string()),
        sse_frame(&finish.to_string()),
        sse_frame("[DONE]")
    )
    .into_bytes()
}

/// Build a complete SSE body with provider-reported usage.
pub fn text_stream_with_usage(
    text: &str,
    input: u64,
    output: u64,
    cost: &str,
    generation: &str,
) -> Vec<u8> {
    let delta = json!({
        "id": generation,
        "choices": [{ "index": 0, "delta": { "text": "" } }]
    });
    let finish = json!({
        "id": generation,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
        "usage": {
            "prompt_tokens": input,
            "completion_tokens": output,
            "total_cost": cost.parse::<f64>().unwrap_or_default(),
            "generation_id": generation
        }
    });
    let text_frame = json!({
        "id": generation,
        "choices": [{ "index": 0, "delta": { "content": text } }]
    });

    format!(
        "{}{}{}{}",
        sse_frame(&delta.to_string()),
        sse_frame(&text_frame.to_string()),
        sse_frame(&finish.to_string()),
        sse_frame("[DONE]")
    )
    .into_bytes()
}

/// Split a payload into small chunks to exercise frame reassembly.
pub fn chunked(payload: Vec<u8>, size: usize) -> Vec<Vec<u8>> {
    payload.chunks(size).map(<[u8]>::to_vec).collect()
}
