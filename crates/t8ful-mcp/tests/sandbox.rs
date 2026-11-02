//! Plan-mode sandbox tests for local MCP servers.
//!
//! A local server started in plan mode must run inside the planning sandbox,
//! never on the host. These tests use a sentinel write: the sandbox makes the
//! host read-only, so a server that writes outside its private `/tmp` fails,
//! and a server that reports the write's success proves it ran unsandboxed.
//!
//! Set `T8FUL_SANDBOX_TESTS=required` to turn a host that cannot create the
//! needed namespaces into a failure instead of a skip.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use t8ful_core::policy::Mode;
use t8ful_core::sandbox::{Sandbox, SandboxedCommand};
use t8ful_core::tool::{ToolCall, ToolContext};
use t8ful_mcp::config::{PlanTrust, ServerConfig, Transport};
use t8ful_mcp::{McpHub, ServerState};
use tempfile::TempDir;

fn required() -> bool {
    std::env::var("T8FUL_SANDBOX_TESTS").ok().as_deref() == Some("required")
        || std::env::var("T8FUL_TEST_SANDBOX_REQUIRED").ok().as_deref() == Some("required")
}

/// The sandbox backend, or `None` when this host cannot run one.
async fn backend() -> Option<Box<dyn Sandbox>> {
    let backend = match t8ful_sandbox::Bubblewrap::discover() {
        Ok(backend) => backend,
        Err(error) => {
            assert!(
                !required(),
                "sandbox tests are required but unavailable: {error}"
            );

            return None;
        }
    };

    // `discover` only proves the binary exists; a build sandbox can still
    // forbid user namespaces, so probe with a trivial command. The mount list
    // must be a real directory the backend can pin, and the command runs through
    // `bash` so the probe matches how a sandboxed server is actually launched.
    let probe_root = TempDir::new().expect("probe dir");
    let bash = std::env::var("PATH").ok().and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join("bash"))
            .find(|candidate| candidate.is_file())
    });

    let Some(bash) = bash else {
        assert!(
            !required(),
            "sandbox tests are required but bash is not on PATH"
        );

        return None;
    };

    let probe = SandboxedCommand {
        program: bash.display().to_string(),
        args: vec!["-c".to_owned(), "true".to_owned()],
        cwd: probe_root.path().to_path_buf(),
        env: BTreeMap::from([("PATH".to_owned(), std::env::var("PATH").unwrap_or_default())]),
        read_only_paths: vec![probe_root.path().to_path_buf()],
    };

    match backend.spawn(probe).await {
        Ok(mut process) => match process.child.wait().await {
            Ok(status) if status.success() => Some(Box::new(backend)),
            _ => {
                assert!(!required(), "sandbox tests are required but a probe failed");

                None
            }
        },
        Err(error) => {
            assert!(
                !required(),
                "sandbox tests are required but a probe failed: {error}"
            );

            None
        }
    }
}

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_server.sh")
}

/// `bash` from `PATH`, so the fixture is launched explicitly instead of through
/// its shebang: a Nix build sandbox has no `/usr/bin/env`.
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

fn plan_context(sandbox: Box<dyn Sandbox>, project: PathBuf) -> ToolContext {
    let mut context = ToolContext::new(project.clone(), project);

    context.mode = Mode::Plan;
    context.default_timeout = Duration::from_secs(20);
    context.sandbox = Some(std::sync::Arc::from(sandbox));
    context.read_scope = Some(t8ful_core::readscope::ReadScope::new(
        [std::env::temp_dir()],
    ));

    context
}

#[tokio::test]
async fn a_plan_mode_local_server_runs_in_the_sandbox() {
    let Some(sandbox) = backend().await else {
        return;
    };

    // The sentinel lives outside the sandbox's private scratch, so a sandboxed
    // server cannot create it while a host process could.
    let scratch = TempDir::new().expect("temp dir");
    let sentinel = scratch.path().join("escaped.txt");
    let project = TempDir::new().expect("project");
    let context = plan_context(sandbox, project.path().to_path_buf());
    let mut config = ServerConfig {
        name: "fake".to_owned(),
        transport: Transport::Stdio {
            command: vec![
                bash(),
                script().display().to_string(),
                "ok".to_owned(),
                sentinel.display().to_string(),
            ],
            cwd: project.path().to_path_buf(),
        },
        pass_env: Vec::new(),
        plan: PlanTrust::ReadOnly,
    };

    config.plan = PlanTrust::ReadOnly;

    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![config],
        &context,
        Duration::from_secs(20),
        &mut warnings,
    )
    .await;

    // Whether the server started or not, the sentinel must not exist: the
    // sandbox refused the write, which is the property under test.
    assert!(
        !sentinel.exists(),
        "a plan-mode server wrote outside the sandbox: {}",
        sentinel.display()
    );

    // The fake server is a plain shell script, so it does start inside the
    // sandbox. Asserting that keeps this test from passing vacuously through a
    // startup failure: the point is that a *running* plan-mode server cannot
    // write to the host.
    assert!(
        matches!(hub.state("fake"), Some(ServerState::Ready { .. })),
        "the sandboxed server did not start: {:?}",
        hub.state("fake")
    );
}

#[tokio::test]
async fn a_plan_mode_local_server_cannot_read_a_withheld_credential() {
    let Some(sandbox) = backend().await else {
        return;
    };

    let project = TempDir::new().expect("project");
    let context = plan_context(sandbox, project.path().to_path_buf());
    // The variable is set for the *hub*, but a plan-sandboxed server must not
    // receive it: D07 withholds known credentials from local planning servers.
    let env = t8ful_mcp::config::MapEnv::from_pairs(&[
        ("T8FUL_TEST_MCP_TOKEN", "must-not-cross"),
        ("PATH", &std::env::var("PATH").unwrap_or_default()),
    ]);
    let config = ServerConfig {
        name: "fake".to_owned(),
        transport: Transport::Stdio {
            command: vec![bash(), script().display().to_string(), "ok".to_owned()],
            cwd: project.path().to_path_buf(),
        },
        pass_env: vec![t8ful_mcp::EnvName::new("T8FUL_TEST_MCP_TOKEN")],
        plan: PlanTrust::ReadOnly,
    };

    let mut warnings = Vec::new();
    let hub = McpHub::start_with_env(
        vec![config],
        &context,
        Duration::from_secs(20),
        std::sync::Arc::new(env),
        &mut warnings,
    )
    .await;

    // Only a started server can be asked; a failed one proves nothing about the
    // environment, and the refusal itself is acceptable behavior.
    let Some(ServerState::Ready { .. }) = hub.state("fake") else {
        return;
    };

    let tool = hub
        .tools()
        .into_iter()
        .find(|tool| tool.spec().name == "mcp__fake__env")
        .expect("the env tool exists");
    let call = ToolCall::new("call-1", "mcp__fake__env", serde_json::json!({}));
    let output = tool
        .execute(&call, &context)
        .await
        .expect("the call succeeds");

    assert!(
        !output.content.contains("must-not-cross"),
        "a credential crossed the plan sandbox boundary: {}",
        output.content
    );
}

#[tokio::test]
async fn plan_mode_refuses_a_local_server_without_a_sandbox() {
    let project = TempDir::new().expect("project");
    let mut context = ToolContext::new(project.path().to_path_buf(), project.path().to_path_buf());

    context.mode = Mode::Plan;
    // No sandbox: plan mode must refuse rather than run on the host.
    context.sandbox = None;

    let config = ServerConfig {
        name: "fake".to_owned(),
        transport: Transport::Stdio {
            command: vec![bash(), script().display().to_string(), "ok".to_owned()],
            cwd: project.path().to_path_buf(),
        },
        pass_env: Vec::new(),
        plan: PlanTrust::ReadOnly,
    };

    let mut warnings = Vec::new();
    let hub = McpHub::start(
        vec![config],
        &context,
        Duration::from_secs(10),
        &mut warnings,
    )
    .await;

    match hub.state("fake") {
        Some(ServerState::Skipped { reason }) => {
            assert!(reason.contains("sandbox"), "{reason}");
        }
        other => panic!("expected a refusal without a sandbox, got {other:?}"),
    }
}
