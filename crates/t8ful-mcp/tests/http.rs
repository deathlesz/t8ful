//! Streamable HTTP transport tests against a loopback server.
//!
//! The fixture speaks just enough of the Streamable HTTP transport for one
//! initialize handshake, a `tools/list`, and a `tools/call`, and records the
//! headers each request carried so a test can assert on bearer and custom
//! header injection. It never reaches the network.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use t8ful_core::policy::Mode;
use t8ful_core::tool::{ToolCall, ToolContext};
use t8ful_mcp::config::{MapEnv, PlanTrust, ServerConfig, Transport};
use t8ful_mcp::{McpHub, ServerState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Headers of one recorded request, lower-cased names.
type Headers = HashMap<String, String>;

/// A loopback Streamable HTTP MCP server.
struct Fixture {
    port: u16,
    requests: Arc<Mutex<Vec<(Headers, Value)>>>,
}

impl Fixture {
    /// Headers of the first request whose JSON-RPC method matches.
    fn headers_for(&self, method: &str) -> Headers {
        self.requests
            .lock()
            .expect("the fixture lock")
            .iter()
            .find(|(_, body)| body.get("method").and_then(Value::as_str) == Some(method))
            .map(|(headers, _)| headers.clone())
            .unwrap_or_default()
    }

    /// The JSON-RPC methods received, in arrival order.
    fn methods(&self) -> Vec<String> {
        self.requests
            .lock()
            .expect("the fixture lock")
            .iter()
            .filter_map(|(_, body)| body.get("method").and_then(Value::as_str))
            .map(str::to_owned)
            .collect()
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }
}

async fn start_fixture() -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };

            let recorded = Arc::clone(&recorded);

            tokio::spawn(async move {
                let _ = serve(stream, recorded).await;
            });
        }
    });

    Fixture { port, requests }
}

async fn serve(
    mut stream: TcpStream,
    recorded: Arc<Mutex<Vec<(Headers, Value)>>>,
) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let read = stream.read(&mut chunk).await?;

        if read == 0 {
            return Ok(());
        }

        buffer.extend_from_slice(&chunk[..read]);

        // A complete request ends at the header terminator plus its body.
        let Some(head_end) = find(&buffer, b"\r\n\r\n") else {
            continue;
        };
        let headers = parse_headers(&buffer[..head_end]);
        let length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);

        if buffer.len() < head_end + 4 + length {
            continue;
        }

        let body = &buffer[head_end + 4..head_end + 4 + length];
        let request: Value = serde_json::from_slice(body).unwrap_or(Value::Null);

        recorded
            .lock()
            .expect("the fixture lock")
            .push((headers.clone(), request.clone()));

        let response = respond(&request);
        let payload = serde_json::to_vec(&response).expect("serialize");
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
            payload.len()
        );

        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&payload).await?;

        buffer.drain(..head_end + 4 + length);
    }
}

fn respond(request: &Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "fake-http", "version": "0.1.0" }
            }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [{
                    "name": "lookup",
                    "description": "Look something up",
                    "inputSchema": { "type": "object", "properties": { "query": { "type": "string" } } }
                }]
            }
        }),
        "tools/call" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": "remote answer" }],
                "isError": false
            }
        }),
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "method not found" }
        }),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_headers(raw: &[u8]) -> Headers {
    let text = String::from_utf8_lossy(raw);
    let mut headers = Headers::new();

    for line in text.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }

    headers
}

fn context() -> ToolContext {
    let dir = std::env::temp_dir();

    let mut context = ToolContext::new(dir.clone(), dir);

    context.mode = Mode::Coding;
    context.default_timeout = Duration::from_secs(10);

    context
}

fn remote(url: String, token: Option<&str>, headers: &[(&str, &str)]) -> ServerConfig {
    ServerConfig {
        name: "remote".to_owned(),
        transport: Transport::Http {
            url,
            bearer_token_env: token.map(t8ful_mcp::EnvName::new),
            headers: headers
                .iter()
                .map(|(header, variable)| {
                    ((*header).to_owned(), t8ful_mcp::EnvName::new(*variable))
                })
                .collect(),
        },
        pass_env: Vec::new(),
        plan: PlanTrust::Mutating,
    }
}

#[tokio::test]
async fn a_remote_server_discovers_and_calls_a_tool() {
    let fixture = start_fixture().await;
    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![remote(fixture.url(), None, &[])],
        &context(),
        Duration::from_secs(10),
        &mut warnings,
    )
    .await;

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(hub.state("remote"), Some(&ServerState::Ready { tools: 1 }));

    // The fixture answers `server/discover` with -32601, so the session is
    // reached through the legacy fallback. Discovery must come first.
    let methods = fixture.methods();
    let discover = methods
        .iter()
        .position(|method| method == "server/discover")
        .expect("discovery was probed");
    let initialize = methods
        .iter()
        .position(|method| method == "initialize")
        .expect("the legacy handshake ran");

    assert!(discover < initialize, "{methods:?}");

    let tool = hub
        .tools()
        .into_iter()
        .find(|tool| tool.spec().name == "mcp__remote__lookup")
        .expect("the lookup tool exists");
    let call = ToolCall::new(
        "call-1",
        "mcp__remote__lookup",
        json!({ "query": "anything" }),
    );
    let output = tool
        .execute(&call, &context())
        .await
        .expect("the call succeeds");

    assert_eq!(output.content, "remote answer");
    assert_eq!(hub.remote_name("mcp__remote__lookup"), Some("lookup"));
}

#[tokio::test]
async fn a_configured_bearer_token_and_headers_reach_the_server() {
    let fixture = start_fixture().await;
    let env = MapEnv::from_pairs(&[
        ("T8FUL_TEST_HTTP_TOKEN", "token-value"),
        ("T8FUL_TEST_HTTP_ORG", "org-value"),
    ]);
    let mut warnings = Vec::new();
    let hub = McpHub::start_with_env(
        vec![remote(
            fixture.url(),
            Some("T8FUL_TEST_HTTP_TOKEN"),
            &[("X-Org", "T8FUL_TEST_HTTP_ORG")],
        )],
        &context(),
        Duration::from_secs(10),
        Arc::new(env),
        &mut warnings,
    )
    .await;

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(hub.state("remote"), Some(&ServerState::Ready { tools: 1 }));

    let headers = fixture.headers_for("initialize");

    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer token-value")
    );
    assert_eq!(headers.get("x-org").map(String::as_str), Some("org-value"));
}

#[tokio::test]
async fn an_absent_token_fails_the_server_and_sends_no_authorization_header() {
    let fixture = start_fixture().await;
    let mut warnings = Vec::new();
    let hub = McpHub::start_with_env(
        vec![remote(
            fixture.url(),
            Some("T8FUL_TEST_ABSENT_HTTP_TOKEN"),
            &[],
        )],
        &context(),
        Duration::from_secs(10),
        Arc::new(MapEnv::default()),
        &mut warnings,
    )
    .await;

    assert!(
        matches!(hub.state("remote"), Some(ServerState::Failed { .. })),
        "{:?}",
        hub.state("remote")
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("T8FUL_TEST_ABSENT_HTTP_TOKEN"),
        "{warnings:?}"
    );
    // No request was made at all: a missing token is caught before connecting.
    assert!(fixture.headers_for("initialize").is_empty());
}

#[tokio::test]
async fn no_token_configured_sends_no_authorization_header() {
    let fixture = start_fixture().await;
    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![remote(fixture.url(), None, &[])],
        &context(),
        Duration::from_secs(10),
        &mut warnings,
    )
    .await;

    assert_eq!(hub.state("remote"), Some(&ServerState::Ready { tools: 1 }));

    let headers = fixture.headers_for("initialize");

    assert!(!headers.contains_key("authorization"), "{headers:?}");
}

#[tokio::test]
async fn a_trusted_remote_server_starts_in_plan_mode() {
    let fixture = start_fixture().await;
    let mut config = remote(fixture.url(), None, &[]);

    config.plan = PlanTrust::ReadOnly;

    let mut warnings = Vec::new();
    let mut plan = context();

    plan.mode = Mode::Plan;

    let hub = McpHub::start(vec![config], &plan, Duration::from_secs(10), &mut warnings).await;

    assert_eq!(hub.state("remote"), Some(&ServerState::Ready { tools: 1 }));

    let tool = hub
        .tools()
        .into_iter()
        .find(|tool| tool.spec().name == "mcp__remote__lookup")
        .expect("the lookup tool exists");

    assert_eq!(tool.effect(), t8ful_core::tool::ToolEffect::ReadOnly);
    assert!(tool.denial_reason().is_none());
}

#[tokio::test]
async fn an_unreachable_remote_server_fails_without_blocking_startup() {
    // Port 1 is closed on any normal host.
    let mut warnings = Vec::new();
    let started = std::time::Instant::now();
    let hub = McpHub::start(
        vec![remote("http://127.0.0.1:1/mcp".to_owned(), None, &[])],
        &context(),
        Duration::from_secs(2),
        &mut warnings,
    )
    .await;

    assert!(
        matches!(hub.state("remote"), Some(ServerState::Failed { .. })),
        "{:?}",
        hub.state("remote")
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "startup must be bounded by the configured timeout"
    );
}
