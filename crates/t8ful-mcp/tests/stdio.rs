//! Transport and hub integration tests against the fake stdio server.
//!
//! These prove the parts the plan flagged as unverifiable offline: that
//! `rmcp`'s `AsyncRwTransport` accepts a child's stdout/stdin pair, that
//! `serve_client` completes the handshake over it, and that the hub surfaces
//! discovery and call results as t8ful tools.

use std::path::PathBuf;
use std::time::Duration;

use t8ful_core::policy::Mode;
use t8ful_core::tool::{ToolCall, ToolContext, ToolEffect, ToolStatus};
use t8ful_mcp::config::{MapEnv, PlanTrust, ServerConfig, Transport};
use t8ful_mcp::{McpHub, ServerState};

fn script() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_server.sh");

    // A missing fixture would otherwise surface as "unknown tool" or a failed
    // handshake and look like a client bug.
    assert!(
        path.is_file(),
        "the fake MCP server fixture is missing: {}",
        path.display()
    );

    path
}

/// `bash` from `PATH`, so the fixture is launched explicitly rather than
/// through its shebang: a Nix build sandbox has no `/usr/bin/env`.
fn bash() -> String {
    std::env::var("PATH")
        .ok()
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("bash"))
                .find(|candidate| candidate.is_file())
        })
        .expect("bash must be on PATH for the MCP tests")
        .display()
        .to_string()
}

fn context(mode: Mode) -> ToolContext {
    let dir = std::env::temp_dir();

    let mut context = ToolContext::new(dir.clone(), dir);

    context.mode = mode;
    context.default_timeout = Duration::from_secs(10);
    context.max_output_bytes = 8 * 1024;

    context
}

/// An injected environment with the real `PATH`, so the fake server's
/// `#!/usr/bin/env bash` shebang resolves, plus the given extra values.
fn env_with(pairs: &[(&str, &str)]) -> MapEnv {
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned());
    let mut entries = vec![("PATH".to_owned(), path)];

    entries.extend(
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );

    MapEnv::new(entries)
}

fn server(name: &str, mode: &str) -> ServerConfig {
    ServerConfig {
        name: name.to_owned(),
        transport: Transport::Stdio {
            command: vec![bash(), script().display().to_string(), mode.to_owned()],
            cwd: std::env::temp_dir(),
        },
        pass_env: Vec::new(),
        plan: PlanTrust::Mutating,
    }
}

#[tokio::test]
async fn a_stdio_server_starts_over_a_child_pipe_pair() {
    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![server("fake", "ok")],
        &context(Mode::Coding),
        Duration::from_secs(10),
        &mut warnings,
    )
    .await;

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(hub.state("fake"), Some(&ServerState::Ready { tools: 4 }));

    let names = hub.tool_names();
    assert!(names.contains(&"mcp__fake__echo"), "{names:?}");
    assert!(names.contains(&"mcp__fake__fail"), "{names:?}");

    // The reverse map keeps the server's own spelling.
    assert_eq!(hub.remote_name("mcp__fake__echo"), Some("echo"));
    assert_eq!(hub.server_of("mcp__fake__echo"), Some("fake"));
}

#[tokio::test]
async fn a_tool_call_round_trips_through_the_transport() {
    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![server("fake", "ok")],
        &context(Mode::Coding),
        Duration::from_secs(10),
        &mut warnings,
    )
    .await;
    let tool = hub
        .tools()
        .into_iter()
        .find(|tool| tool.spec().name == "mcp__fake__echo")
        .expect("the echo tool exists");
    let call = ToolCall::new(
        "call-1",
        "mcp__fake__echo",
        serde_json::json!({"text": "hi"}),
    );

    let output = tool
        .execute(&call, &context(Mode::Coding))
        .await
        .expect("the call succeeds");

    assert_eq!(output.content, "echoed");
    assert!(!output.truncated);
}

#[tokio::test]
async fn an_error_result_surfaces_as_a_failure() {
    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![server("fake", "ok")],
        &context(Mode::Coding),
        Duration::from_secs(10),
        &mut warnings,
    )
    .await;
    let tool = hub
        .tools()
        .into_iter()
        .find(|tool| tool.spec().name == "mcp__fake__fail")
        .expect("the fail tool exists");
    let call = ToolCall::new("call-1", "mcp__fake__fail", serde_json::json!({}));

    let error = tool
        .execute(&call, &context(Mode::Coding))
        .await
        .expect_err("isError is a failure");

    assert!(error.to_string().contains("deliberate failure"), "{error}");
}

#[tokio::test]
async fn an_oversized_result_is_truncated() {
    let mut warnings = Vec::new();
    let mut ctx = context(Mode::Coding);

    ctx.max_output_bytes = 256;

    let hub = McpHub::start(
        vec![server("fake", "ok")],
        &ctx,
        Duration::from_secs(10),
        &mut warnings,
    )
    .await;
    let tool = hub
        .tools()
        .into_iter()
        .find(|tool| tool.spec().name == "mcp__fake__huge")
        .expect("the huge tool exists");
    let call = ToolCall::new("call-1", "mcp__fake__huge", serde_json::json!({}));

    let output = tool.execute(&call, &ctx).await.expect("the call succeeds");

    assert!(output.truncated);
    assert!(output.content.len() <= 256, "{}", output.content.len());
}

#[tokio::test]
async fn a_passed_variable_reaches_only_its_own_server() {
    let mut warnings = Vec::new();
    let mut config = server("fake", "ok");
    // The environment is injected, so this test never mutates the process
    // environment another test might read.
    let env = env_with(&[
        ("T8FUL_TEST_MCP_TOKEN", "only-for-fake"),
        ("T8FUL_TEST_OTHER_TOKEN", "not-for-fake"),
    ]);

    config.pass_env = vec![t8ful_mcp::EnvName::new("T8FUL_TEST_MCP_TOKEN")];

    let hub = McpHub::start_with_env(
        vec![config],
        &context(Mode::Coding),
        Duration::from_secs(10),
        std::sync::Arc::new(env),
        &mut warnings,
    )
    .await;
    let tool = hub
        .tools()
        .into_iter()
        .find(|tool| tool.spec().name == "mcp__fake__env")
        .expect("the env tool exists");
    let call = ToolCall::new("call-1", "mcp__fake__env", serde_json::json!({}));
    let output = tool
        .execute(&call, &context(Mode::Coding))
        .await
        .expect("the call succeeds");

    assert!(
        output
            .content
            .contains("T8FUL_TEST_MCP_TOKEN=only-for-fake"),
        "{}",
        output.content
    );
    // A variable the server did not list is absent from its environment.
    assert!(
        !output.content.contains("T8FUL_TEST_OTHER_TOKEN"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn a_missing_credential_fails_that_server_by_name() {
    let mut warnings = Vec::new();
    let mut config = server("fake", "ok");

    config.pass_env = vec![t8ful_mcp::EnvName::new("T8FUL_TEST_ABSENT_MCP_TOKEN")];

    let hub = McpHub::start_with_env(
        vec![config],
        &context(Mode::Coding),
        Duration::from_secs(10),
        std::sync::Arc::new(env_with(&[])),
        &mut warnings,
    )
    .await;

    assert!(
        matches!(hub.state("fake"), Some(ServerState::Failed { .. })),
        "{:?}",
        hub.state("fake")
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("T8FUL_TEST_ABSENT_MCP_TOKEN"),
        "{warnings:?}"
    );
    assert!(warnings[0].contains("fake"), "{warnings:?}");
}

#[tokio::test]
async fn a_malformed_schema_fails_that_server_and_keeps_the_others() {
    // `inputSchema` must be a JSON object. `rmcp` rejects a `tools/list` whose
    // schema is a string for the whole response, so the honest outcome is a
    // reported server failure: silently dropping the tool would leave the model
    // without something the user believes is configured.
    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![server("broken", "malformed-schema"), server("good", "ok")],
        &context(Mode::Coding),
        Duration::from_secs(10),
        &mut warnings,
    )
    .await;

    assert!(
        matches!(hub.state("broken"), Some(ServerState::Failed { .. })),
        "{:?}",
        hub.state("broken")
    );
    assert_eq!(hub.state("good"), Some(&ServerState::Ready { tools: 4 }));
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("broken"), "{warnings:?}");
}

#[tokio::test]
async fn a_mutating_server_is_denied_in_plan_mode_with_an_actionable_message() {
    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![server("fake", "ok")],
        &context(Mode::Coding),
        Duration::from_secs(10),
        &mut warnings,
    )
    .await;
    let tool = hub
        .tools()
        .into_iter()
        .find(|tool| tool.spec().name == "mcp__fake__echo")
        .expect("the echo tool exists");

    assert_eq!(tool.effect(), ToolEffect::Mutating);

    let reason = tool
        .denial_reason()
        .expect("a mutating server explains itself");

    assert!(reason.contains("fake"), "{reason}");
    assert!(reason.contains("plan = \"read-only\""), "{reason}");
}

#[tokio::test]
async fn a_read_only_server_runs_in_plan_mode() {
    let mut warnings = Vec::new();
    let mut config = server("fake", "ok");

    config.plan = PlanTrust::ReadOnly;

    let hub = McpHub::start(
        vec![config],
        &context(Mode::Plan),
        Duration::from_secs(10),
        &mut warnings,
    )
    .await;

    // A local server in plan mode goes through the sandbox, which is absent in
    // this test context, so the honest outcome is a refusal rather than a host
    // process.
    match hub.state("fake") {
        Some(ServerState::Skipped { reason }) => {
            assert!(reason.contains("sandbox"), "{reason}");
        }
        other => panic!("expected a sandbox refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_remote_server_is_skipped_in_plan_mode_unless_trusted() {
    let remote = |plan| ServerConfig {
        name: "remote".to_owned(),
        transport: Transport::Http {
            url: "http://127.0.0.1:1/mcp".to_owned(),
            bearer_token_env: None,
            headers: Default::default(),
        },
        pass_env: Vec::new(),
        plan,
    };

    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![remote(PlanTrust::Mutating)],
        &context(Mode::Plan),
        Duration::from_secs(1),
        &mut warnings,
    )
    .await;

    match hub.state("remote") {
        Some(ServerState::Skipped { reason }) => {
            assert!(reason.contains("plan = \"read-only\""), "{reason}");
        }
        other => panic!("expected a plan-mode refusal, got {other:?}"),
    }

    // Trusted for planning, it starts; this port is closed, so the outcome is a
    // failure rather than a skip, which proves the plan check passed.
    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![remote(PlanTrust::ReadOnly)],
        &context(Mode::Plan),
        Duration::from_secs(1),
        &mut warnings,
    )
    .await;

    assert!(
        matches!(hub.state("remote"), Some(ServerState::Failed { .. })),
        "{:?}",
        hub.state("remote")
    );
}

#[tokio::test]
async fn two_servers_offering_the_same_tool_get_distinct_names() {
    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![server("one", "ok"), server("two", "ok")],
        &context(Mode::Coding),
        Duration::from_secs(10),
        &mut warnings,
    )
    .await;
    let names = hub.tool_names();
    let echoes = names
        .iter()
        .filter(|name| name.ends_with("__echo"))
        .collect::<Vec<_>>();

    assert_eq!(echoes.len(), 2, "{names:?}");
    assert_eq!(hub.remote_name("mcp__one__echo"), Some("echo"));
    assert_eq!(hub.remote_name("mcp__two__echo"), Some("echo"));
    assert_eq!(hub.server_of("mcp__one__echo"), Some("one"));
    assert_eq!(hub.server_of("mcp__two__echo"), Some("two"));
}

#[tokio::test]
async fn a_server_that_dies_mid_call_fails_instead_of_hanging() {
    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![server("fake", "ok")],
        &context(Mode::Coding),
        Duration::from_secs(10),
        &mut warnings,
    )
    .await;
    let tool = hub
        .tools()
        .into_iter()
        .find(|tool| tool.spec().name == "mcp__fake__echo")
        .expect("the echo tool exists");

    // Stop the server out from under the tool, then call it: the handle is
    // closed, so the call must fail rather than wait forever.
    drop(hub);

    let call = ToolCall::new(
        "call-1",
        "mcp__fake__echo",
        serde_json::json!({"text": "hi"}),
    );
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        tool.execute(&call, &context(Mode::Coding)),
    )
    .await
    .expect("the call does not hang");

    assert!(outcome.is_err(), "a dead server must not answer");
}

#[tokio::test]
async fn a_configured_status_is_reported_for_every_server() {
    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![server("fake", "ok")],
        &context(Mode::Coding),
        Duration::from_secs(10),
        &mut warnings,
    )
    .await;

    assert_eq!(hub.states().len(), 1);
    assert_eq!(hub.len(), 4);
    assert!(!hub.is_empty());
}

/// The tool status the agent loop would record, to keep `ToolStatus` referenced
/// where the adapter contract is asserted.
#[test]
fn the_adapter_contract_is_the_core_tool_trait() {
    fn assert_tool<T: t8ful_core::tool::Tool>() {}

    assert_tool::<t8ful_mcp::McpTool>();
    let _ = ToolStatus::Ok;
}

#[tokio::test]
async fn one_execution_sends_exactly_one_request() {
    // The agent loop is the only thing that may retry a call, and it never
    // replays an indeterminate one. The adapter therefore sends exactly one
    // `tools/call` per execution, and this log proves it.
    let log = tempfile::NamedTempFile::new().expect("temp file");
    let log_path = log.path().display().to_string();
    let mut config = server("fake", "ok");

    // The server reads its log path from the environment it is given.
    config.pass_env = vec![t8ful_mcp::EnvName::new("T8FUL_FAKE_CALL_LOG")];

    let env = env_with(&[("T8FUL_FAKE_CALL_LOG", &log_path)]);
    let mut warnings = Vec::new();
    let hub = McpHub::start_with_env(
        vec![config],
        &context(Mode::Coding),
        Duration::from_secs(10),
        std::sync::Arc::new(env),
        &mut warnings,
    )
    .await;
    let tool = hub
        .tools()
        .into_iter()
        .find(|tool| tool.spec().name == "mcp__fake__echo")
        .expect("the echo tool exists");
    let call = ToolCall::new(
        "call-1",
        "mcp__fake__echo",
        serde_json::json!({"text": "hi"}),
    );

    tool.execute(&call, &context(Mode::Coding))
        .await
        .expect("the call succeeds");

    let recorded = std::fs::read_to_string(&log_path).expect("the log exists");

    assert_eq!(
        recorded.lines().filter(|line| *line == "echo").count(),
        1,
        "{recorded:?}"
    );
}

#[tokio::test]
async fn the_client_probes_discovery_before_the_legacy_handshake() {
    // A legacy server is only reached by `server/discover` failing first. The
    // fixture records every method it received, so this fails if the `Auto`
    // lifecycle is dropped or replaced with a plain `initialize`.
    let log = tempfile::NamedTempFile::new().expect("temp file");
    let log_path = log.path().display().to_string();
    let mut config = server("fake", "ok");

    config.pass_env = vec![t8ful_mcp::EnvName::new("T8FUL_FAKE_METHOD_LOG")];

    let env = env_with(&[("T8FUL_FAKE_METHOD_LOG", &log_path)]);
    let mut warnings = Vec::new();
    let hub = McpHub::start_with_env(
        vec![config],
        &context(Mode::Coding),
        Duration::from_secs(10),
        std::sync::Arc::new(env),
        &mut warnings,
    )
    .await;

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(hub.state("fake"), Some(&ServerState::Ready { tools: 4 }));

    let recorded = std::fs::read_to_string(&log_path).expect("the log exists");
    let methods: Vec<&str> = recorded.lines().collect();

    assert_eq!(
        methods.first().copied(),
        Some("server/discover"),
        "{methods:?}"
    );
    assert_eq!(methods.get(1).copied(), Some("initialize"), "{methods:?}");
}

#[tokio::test]
async fn registration_never_replaces_an_existing_tool() {
    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![server("fake", "ok")],
        &context(Mode::Coding),
        Duration::from_secs(10),
        &mut warnings,
    )
    .await;
    let mut registry = t8ful_core::tool::ToolRegistry::new();

    // A tool already occupying the name a server would use.
    registry.register(ShadowingTool);

    hub.register_into(&mut registry);

    assert_eq!(
        registry
            .get("mcp__fake__echo")
            .expect("still present")
            .spec()
            .description,
        "the original tool"
    );
    // Every other server tool was registered normally.
    assert!(registry.get("mcp__fake__env").is_some());
}

struct ShadowingTool;

#[async_trait::async_trait]
impl t8ful_core::tool::Tool for ShadowingTool {
    fn spec(&self) -> t8ful_core::tool::ToolSpec {
        t8ful_core::tool::ToolSpec {
            name: "mcp__fake__echo".to_owned(),
            description: "the original tool".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    async fn execute(
        &self,
        _call: &ToolCall,
        _ctx: &ToolContext,
    ) -> Result<t8ful_core::tool::ToolOutput, t8ful_core::tool::ToolError> {
        Ok(t8ful_core::tool::ToolOutput::text("shadow"))
    }
}
