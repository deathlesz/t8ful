//! End-to-end tests for child agents, the task graph, and worktree isolation.
//!
//! These drive the real binary against the loopback SSE server, so a child is a
//! genuine second agent loop with its own context and journal. Nothing here
//! needs the network or a real API key.

mod support;

use assert_cmd::Command;
use serde_json::{Value, json};
use support::{FakeServer, Reply, chunked, text_stream, text_stream_with_usage, tool_stream};
use tempfile::TempDir;

struct Project {
    dir: TempDir,
    home: TempDir,
    base_url: String,
}

impl Project {
    fn new(server: &FakeServer) -> Self {
        Self::with_config(server, "[provider]\nmodel = \"vendor/test-model\"\n")
    }

    fn with_config(server: &FakeServer, config: &str) -> Self {
        let dir = TempDir::new().expect("project dir");
        let home = TempDir::new().expect("home dir");

        std::fs::create_dir_all(dir.path().join(".t8ful")).expect("config dir");
        std::fs::write(dir.path().join(".t8ful/config.toml"), config).expect("write config");

        Self {
            dir,
            home,
            base_url: server.base_url(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("t8ful").expect("t8ful binary");

        command
            .current_dir(self.dir.path())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_STATE_HOME", self.home.path().join("state"))
            .env("XDG_CONFIG_DIRS", self.home.path().join("xdg"))
            .env("OPENROUTER_API_KEY", "test-key")
            .env("T8FUL_BASE_URL", &self.base_url)
            .env("T8FUL_LOG", "off")
            .env_remove("T8FUL_MODEL")
            .env_remove("T8FUL_MODE")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");

        command
    }

    /// A plain process command with the same environment, for tests that must
    /// gate the child before reading its output.
    fn raw_command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("t8ful"));

        command
            .current_dir(self.dir.path())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_STATE_HOME", self.home.path().join("state"))
            .env("XDG_CONFIG_DIRS", self.home.path().join("xdg"))
            .env("OPENROUTER_API_KEY", "test-key")
            .env("T8FUL_BASE_URL", &self.base_url)
            .env("T8FUL_LOG", "off")
            .env_remove("T8FUL_MODEL")
            .env_remove("T8FUL_MODE")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");

        command
    }

    fn path(&self, relative: &str) -> std::path::PathBuf {
        self.dir.path().join(relative)
    }

    fn write(&self, relative: &str, contents: &str) {
        std::fs::write(self.path(relative), contents).expect("write file");
    }

    fn state(&self) -> std::path::PathBuf {
        self.home.path().join("state/t8ful")
    }
}

fn json_lines(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|error| panic!("stdout line is not JSON ({error}): {line}"))
        })
        .collect()
}

fn events_of<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["event"]["type"] == json!(kind))
        .collect()
}

/// Every request body the model received, for asserting on context isolation.
fn system_prompts(requests: &[support::RequestRecord]) -> Vec<String> {
    requests
        .iter()
        .filter_map(|request| request.body["messages"][0]["content"].as_str())
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_agent_runs_and_its_report_reaches_the_parent() {
    // Root: spawn a child, then wait for it. Child: report. Root: summarise.
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-spawn",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "count the files in this project" }] }),
            ),
            5,
        ))
        .for_parent(),
        Reply::sse(chunked(text_stream("I found three files."), 4)).for_child_agent(),
        Reply::sse(chunked(
            tool_stream("call-wait", "agent_wait", &json!({})),
            6,
        ))
        .for_parent(),
        Reply::sse(chunked(text_stream("The child reported three files."), 5)),
    ])
    .await;

    let project = Project::new(&server);
    project.write("a.txt", "a\n");
    project.write("b.txt", "b\n");

    let output = project
        .command()
        .args(["run", "delegate this", "--format", "json"])
        .output()
        .expect("run t8ful");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = json_lines(&output.stdout);

    let spawned = events_of(&events, "agent_spawned");

    assert_eq!(spawned.len(), 1, "one child was announced");
    assert_eq!(spawned[0]["event"]["agent"], json!("a1"));
    assert_eq!(spawned[0]["event"]["isolation"], json!("shared"));

    let finished = events_of(&events, "agent_finished");

    assert_eq!(finished.len(), 1, "the child's report was published");
    assert_eq!(finished[0]["event"]["status"], json!("completed"));
    assert!(
        finished[0]["event"]["summary"]
            .as_str()
            .expect("summary")
            .contains("three files"),
        "the child's own words are the report: {:?}",
        finished[0]["event"]["summary"]
    );

    // The parent's context carries the report, whether it arrived through
    // `agent_wait` or as an injected notice.
    let requests = server.requests();
    let last = requests.last().expect("a final request");
    let context = last.body["messages"].to_string();

    assert!(
        context.contains("I found three files."),
        "the child's report reached the parent's context: {context}"
    );
    assert!(
        context.contains("a1 (general) completed"),
        "the report names the agent and its status: {context}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_has_its_own_context_and_is_told_it_is_a_child() {
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-spawn",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "look at the parser" }] }),
            ),
            5,
        )),
        Reply::sse(chunked(text_stream("nothing notable"), 4)).for_child_agent(),
        Reply::sse(chunked(
            tool_stream("call-wait", "agent_wait", &json!({})),
            6,
        ))
        .for_parent(),
        Reply::sse(chunked(text_stream("ok"), 5)),
    ])
    .await;

    let project = Project::new(&server);

    let output = project
        .command()
        .args(["run", "delegate", "--format", "json"])
        .output()
        .expect("run t8ful");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = server.requests();
    let prompts = system_prompts(&requests);
    let child_prompt = prompts
        .iter()
        .find(|prompt| prompt.contains("<subagent"))
        .expect("the child got a subagent system prompt");

    assert!(child_prompt.contains("agent=\"a1\""), "{child_prompt}");
    assert!(
        !child_prompt.contains("depth="),
        "children are single level: {child_prompt}"
    );
    assert!(
        child_prompt.contains("only your final message reaches the agent"),
        "the child knows its output is summarised: {child_prompt}"
    );
    assert!(
        child_prompt.contains("When your task is done, stop"),
        "the child knows to stop: {child_prompt}"
    );

    // The child's conversation starts from its prompt, not the parent's history.
    let child_request = requests
        .iter()
        .find(|request| {
            request.body["messages"][0]["content"]
                .as_str()
                .is_some_and(|prompt| prompt.contains("<subagent"))
        })
        .expect("a child request");

    let messages = child_request.body["messages"].as_array().expect("messages");

    assert_eq!(messages.len(), 2, "system plus the prompt: {messages:?}");
    assert_eq!(messages[1]["content"], json!("look at the parser"));
    assert!(
        !child_request.body["messages"]
            .to_string()
            .contains("delegate"),
        "the parent's prompt did not leak into the child"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_journals_to_its_own_file() {
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-spawn",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "do a small thing" }] }),
            ),
            5,
        )),
        Reply::sse(chunked(text_stream("small thing done"), 4)).for_child_agent(),
        Reply::sse(chunked(
            tool_stream("call-wait", "agent_wait", &json!({})),
            6,
        ))
        .for_parent(),
        Reply::sse(chunked(text_stream("ok"), 5)),
    ])
    .await;

    let project = Project::new(&server);

    let output = project
        .command()
        .args(["run", "delegate", "--format", "json"])
        .output()
        .expect("run t8ful");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let agents = project.state().join("sessions");
    let mut child_journals = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&agents) {
        for entry in entries.flatten() {
            let path = entry.path().join("agents");

            if let Ok(kids) = std::fs::read_dir(&path) {
                for kid in kids.flatten() {
                    let journal = kid.path().join("journal.jsonl");

                    if journal.exists() {
                        child_journals.push(journal);
                    }
                }
            }
        }
    }

    assert_eq!(
        child_journals.len(),
        1,
        "one child journal: {child_journals:?}"
    );

    let text = std::fs::read_to_string(&child_journals[0]).expect("read child journal");

    assert!(text.contains("\"kind\":\"message\""), "{text}");
    assert!(
        text.contains("small thing done"),
        "the child's work is recorded"
    );
    assert!(
        text.contains("\"agent\":\"a1\""),
        "usage is attributed to the child: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_task_graph_is_recorded_and_visible() {
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-create",
                "task_create",
                &json!({ "title": "write the parser" }),
            ),
            5,
        )),
        Reply::sse(chunked(
            tool_stream(
                "call-second",
                "task_create",
                &json!({ "title": "test the parser", "deps": ["task-1"] }),
            ),
            5,
        )),
        Reply::sse(chunked(
            tool_stream(
                "call-done",
                "task_update",
                &json!({ "id": "task-1", "state": "completed" }),
            ),
            5,
        )),
        Reply::sse(chunked(text_stream("planned"), 4)),
    ])
    .await;

    let project = Project::new(&server);

    let output = project
        .command()
        .args(["run", "plan this", "--format", "json"])
        .output()
        .expect("run t8ful");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = json_lines(&output.stdout);
    let changes = events_of(&events, "task_changed");

    assert!(changes.len() >= 3, "every change is announced: {changes:?}");
    assert!(
        changes
            .iter()
            .any(|change| change["event"]["id"] == json!("task-2")
                && change["event"]["blocked"] == json!(true)),
        "a dependent task starts blocked: {changes:?}"
    );
    assert!(
        changes
            .iter()
            .any(|change| change["event"]["id"] == json!("task-1")
                && change["event"]["state"] == json!("completed")),
        "{changes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_dependency_cycle_is_reported_to_the_model() {
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream("call-a", "task_create", &json!({ "title": "a" })),
            5,
        )),
        Reply::sse(chunked(
            tool_stream(
                "call-b",
                "task_create",
                &json!({ "title": "b", "deps": ["task-1"] }),
            ),
            5,
        )),
        Reply::sse(chunked(
            tool_stream(
                "call-cycle",
                "task_update",
                &json!({ "id": "task-1", "add_deps": ["task-2"] }),
            ),
            5,
        )),
        Reply::sse(chunked(text_stream("understood"), 4)),
    ])
    .await;

    let project = Project::new(&server);

    let output = project
        .command()
        .args(["run", "make a cycle", "--format", "json"])
        .output()
        .expect("run t8ful");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = json_lines(&output.stdout);
    let results = events_of(&events, "tool_result");
    let cycle = results
        .iter()
        .find(|result| result["event"]["call_id"] == json!("call-cycle"))
        .expect("the cycle attempt has a result");

    assert_eq!(cycle["event"]["status"], json!("error"));
    assert!(
        cycle["event"]["content"]
            .as_str()
            .expect("content")
            .contains("cycle"),
        "{:?}",
        cycle["event"]["content"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_can_be_disabled_by_configuration() {
    // With agents off the tool is not registered, so the call is denied as
    // unknown rather than starting anything.
    let server_tools = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-spawn",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "x" }] }),
            ),
            5,
        )),
        Reply::sse(chunked(text_stream("understood"), 4)),
    ])
    .await;

    let disabled = Project::with_config(
        &server_tools,
        "[provider]\nmodel = \"vendor/test-model\"\n\n[agents]\nenabled = false\n",
    );

    let output = disabled
        .command()
        .args(["run", "try to delegate", "--format", "json"])
        .output()
        .expect("run t8ful");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = json_lines(&output.stdout);

    assert!(
        events_of(&events, "agent_spawned").is_empty(),
        "nothing was spawned"
    );

    let results = events_of(&events, "tool_result");
    let denied = results
        .iter()
        .find(|result| result["event"]["call_id"] == json!("call-spawn"))
        .expect("the spawn has a result");

    assert_eq!(denied["event"]["status"], json!("denied"));
    assert!(
        denied["event"]["content"]
            .as_str()
            .expect("content")
            .contains("unknown tool"),
        "{:?}",
        denied["event"]["content"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_role_is_refused_with_a_useful_message() {
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-spawn",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "x", "role": "wizard" }] }),
            ),
            5,
        )),
        Reply::sse(chunked(text_stream("sorry"), 4)),
    ])
    .await;

    let project = Project::new(&server);

    let output = project
        .command()
        .args(["run", "delegate badly", "--format", "json"])
        .output()
        .expect("run t8ful");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = json_lines(&output.stdout);
    let results = events_of(&events, "tool_result");
    let failed = results
        .iter()
        .find(|result| result["event"]["call_id"] == json!("call-spawn"))
        .expect("a result");

    assert_eq!(failed["event"]["status"], json!("error"));
    assert!(
        failed["event"]["content"]
            .as_str()
            .expect("content")
            .contains("agents.roles.wizard"),
        "{:?}",
        failed["event"]["content"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_concurrency_limit_refuses_a_second_batch() {
    let config = "[provider]\nmodel = \"vendor/test-model\"\n\n[agents]\nmax_concurrent = 1\n";
    // The child is held open so it is still running when the parent asks for a
    // second one. Without the gate the child finishes in microseconds and the
    // slot really is free, which is not the case being tested.
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-first",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "slow work" }] }),
            ),
            5,
        ))
        .for_parent(),
        Reply::sse(chunked(
            tool_stream(
                "call-second",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "more work" }] }),
            ),
            5,
        ))
        .for_parent(),
        Reply::sse(chunked(text_stream("the child finished"), 4))
            .for_child_agent()
            .with_gate(std::sync::Arc::clone(&gate)),
        Reply::sse(chunked(text_stream("all done"), 4)).for_parent(),
    ])
    .await;

    let project = Project::with_config(&server, config);

    // Built directly rather than through assert_cmd, which does not expose a
    // spawn that a test can gate on.
    let mut command = project.raw_command();

    command.args(["run", "spawn twice", "--format", "json"]);

    let child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run t8ful");

    // Wait for the parent's request after the refused spawn: that is proof the
    // second spawn was decided while the gated child still held the slot. A
    // sleep here made the test race the scheduler.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);

    while server
        .requests()
        .iter()
        .filter(|request| {
            !request.body["messages"][0]["content"]
                .as_str()
                .unwrap_or_default()
                .contains("<subagent")
        })
        .count()
        < 3
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the parent never reached the refused spawn: {} requests",
            server.requests().len()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    gate.notify_one();

    let output = child.wait_with_output().expect("wait");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = json_lines(&output.stdout);
    let results = events_of(&events, "tool_result");
    let refused = results
        .iter()
        .find(|result| result["event"]["call_id"] == json!("call-second"))
        .expect("the second spawn has a result");

    assert_eq!(refused["event"]["status"], json!("error"));
    assert!(
        refused["event"]["content"]
            .as_str()
            .expect("content")
            .contains("slots are free"),
        "{:?}",
        refused["event"]["content"]
    );
}

/// Commit everything currently in the tree, with the developer's global Git
/// configuration ignored.
fn git_commit_all(dir: &std::path::Path) {
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .status()
            .expect("git runs");

        assert!(status.success(), "git {args:?} failed");
    };

    run(&["add", "-A"]);
    run(&["commit", "-m", "config"]);
}

/// A Git repository with one commit, for the worktree tests.
fn git_repository(dir: &std::path::Path) {
    // The developer's global configuration is ignored: a signed-commit policy or
    // a commit template there would decide whether these tests pass.
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .status()
            .expect("git runs");

        assert!(status.success(), "git {args:?} failed");
    };

    run(&["init", "--initial-branch=main"]);
    run(&["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "hello\n").expect("write");
    run(&["add", "."]);
    run(&["commit", "-m", "initial"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_isolated_child_works_in_a_worktree_without_touching_the_parent() {
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-spawn",
                "spawn_agents",
                &json!({ "agents": [{ "role": "builder", "prompt": "create a file" }] }),
            ),
            5,
        )),
        Reply::sse(chunked(
            tool_stream(
                "call-write",
                "write_file",
                &json!({ "path": "child.txt", "content": "from the child\n" }),
            ),
            5,
        ))
        .for_child_agent(),
        Reply::sse(chunked(text_stream("wrote child.txt"), 4)).for_child_agent(),
        Reply::sse(chunked(
            tool_stream("call-wait", "agent_wait", &json!({})),
            6,
        ))
        .for_parent(),
        Reply::sse(chunked(text_stream("the child wrote a file"), 4)),
    ])
    .await;

    let dir = TempDir::new().expect("project dir");
    let home = TempDir::new().expect("home dir");

    git_repository(dir.path());
    std::fs::create_dir_all(dir.path().join(".t8ful")).expect("config dir");
    std::fs::write(
        dir.path().join(".t8ful/config.toml"),
        "[provider]\nmodel = \"vendor/test-model\"\n\n[agents.roles.builder]\nmodel = \
         \"vendor/role-model\"\nisolation = \"worktree\"\n",
    )
    .expect("write config");
    // The worktree is created from HEAD, so the config has to be committed for
    // the tree to count as clean.
    git_commit_all(dir.path());

    let project = Project {
        dir,
        home,
        base_url: server.base_url(),
    };

    let output = project
        .command()
        .args(["run", "delegate to a worktree", "--format", "json"])
        .output()
        .expect("run t8ful");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = json_lines(&output.stdout);
    let spawned = events_of(&events, "agent_spawned");

    assert_eq!(spawned.len(), 1, "the isolated child started");
    assert_eq!(spawned[0]["event"]["isolation"], json!("worktree"));

    let worktree = spawned[0]["event"]["worktree"]
        .as_str()
        .expect("a worktree path");

    assert!(
        std::path::Path::new(worktree).join("child.txt").exists(),
        "the child wrote into its worktree"
    );
    assert!(
        !project.path("child.txt").exists(),
        "the parent's tree is untouched"
    );

    let finished = events_of(&events, "agent_finished");

    assert_eq!(finished[0]["event"]["status"], json!("completed"));
    assert!(
        finished[0]["event"]["branch"]
            .as_str()
            .expect("branch")
            .starts_with("t8ful/"),
        "{:?}",
        finished[0]["event"]["branch"]
    );

    // The role supplied both values: the model goes into the child's request and
    // its isolation decided the worktree above.
    let child_request = server
        .requests()
        .into_iter()
        .find(|request| {
            request.body["messages"][0]["content"]
                .as_str()
                .is_some_and(|prompt| prompt.contains("<subagent"))
        })
        .expect("a child request");

    assert_eq!(child_request.body["model"], json!("vendor/role-model"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_mode_refuses_a_worktree_spawn_before_creating_one() {
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-spawn",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "x", "isolation": "worktree" }] }),
            ),
            5,
        )),
        Reply::sse(chunked(text_stream("understood"), 4)),
    ])
    .await;

    let project = Project::new(&server);

    let output = project
        .command()
        .args(["run", "delegate", "--mode", "plan", "--format", "json"])
        .output()
        .expect("run t8ful");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = json_lines(&output.stdout);
    let results = events_of(&events, "tool_result");
    let failed = results
        .iter()
        .find(|result| result["event"]["call_id"] == json!("call-spawn"))
        .expect("a result");
    let content = failed["event"]["content"].as_str().expect("content");

    assert_eq!(failed["event"]["status"], json!("error"));
    assert!(content.contains("plan mode"), "{content}");
    assert!(
        content.contains("isolation \"shared\""),
        "the error is actionable: {content}"
    );
    assert!(
        events_of(&events, "agent_spawned").is_empty(),
        "nothing started"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dirty_working_tree_refuses_a_worktree_child() {
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-spawn",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "x", "isolation": "worktree" }] }),
            ),
            5,
        )),
        Reply::sse(chunked(text_stream("understood"), 4)),
    ])
    .await;

    let dir = TempDir::new().expect("project dir");
    let home = TempDir::new().expect("home dir");

    git_repository(dir.path());
    std::fs::write(dir.path().join("README.md"), "uncommitted change\n").expect("dirty");
    std::fs::create_dir_all(dir.path().join(".t8ful")).expect("config dir");
    std::fs::write(
        dir.path().join(".t8ful/config.toml"),
        "[provider]\nmodel = \"vendor/test-model\"\n",
    )
    .expect("write config");

    let project = Project {
        dir,
        home,
        base_url: server.base_url(),
    };

    let output = project
        .command()
        .args(["run", "delegate", "--format", "json"])
        .output()
        .expect("run t8ful");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = json_lines(&output.stdout);
    let results = events_of(&events, "tool_result");
    let failed = results
        .iter()
        .find(|result| result["event"]["call_id"] == json!("call-spawn"))
        .expect("a result");
    let content = failed["event"]["content"].as_str().expect("content");

    assert_eq!(failed["event"]["status"], json!("error"));
    assert!(content.contains("README.md"), "{content}");
    assert!(content.contains("isolation \"shared\""), "{content}");
    assert!(
        events_of(&events, "agent_spawned").is_empty(),
        "nothing started"
    );
    // The parent's uncommitted work is still there and still uncommitted.
    assert_eq!(
        std::fs::read_to_string(project.path("README.md")).expect("read"),
        "uncommitted change\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_that_fails_after_starting_reports_the_started_child_and_creates_no_orphan_task() {
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-spawn",
                "spawn_agents",
                &json!({
                    "agents": [
                        { "prompt": "shared child" },
                        { "prompt": "isolated child", "isolation": "worktree" }
                    ]
                }),
            ),
            5,
        ))
        .for_parent(),
        Reply::sse(chunked(text_stream("shared child done"), 4)).for_child_agent(),
        Reply::sse(chunked(
            tool_stream("call-list", "task_list", &json!({})),
            5,
        ))
        .for_parent(),
        Reply::sse(chunked(text_stream("ok"), 4)).for_parent(),
    ])
    .await;

    let dir = TempDir::new().expect("project dir");
    let home = TempDir::new().expect("home dir");

    git_repository(dir.path());
    // The uncommitted config makes the worktree creation for the second spec
    // fail after the first shared child has already started.
    std::fs::create_dir_all(dir.path().join(".t8ful")).expect("config dir");
    std::fs::write(
        dir.path().join(".t8ful/config.toml"),
        "[provider]\nmodel = \"vendor/test-model\"\n",
    )
    .expect("write config");

    let project = Project {
        dir,
        home,
        base_url: server.base_url(),
    };

    let output = project
        .command()
        .args(["run", "spawn a mixed batch", "--format", "json"])
        .output()
        .expect("run t8ful");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = json_lines(&output.stdout);
    let results = events_of(&events, "tool_result");
    let spawned = results
        .iter()
        .find(|result| result["event"]["call_id"] == json!("call-spawn"))
        .expect("the batch has a result");
    let content = spawned["event"]["content"].as_str().expect("content");

    assert_eq!(spawned["event"]["status"], json!("ok"));
    assert!(content.contains("a1 started"), "{content}");
    assert!(content.contains("could not start a2"), "{content}");
    assert!(!content.contains("a2 started"), "{content}");

    let listed = results
        .iter()
        .find(|result| result["event"]["call_id"] == json!("call-list"))
        .expect("task_list ran");
    let tasks = listed["event"]["content"].as_str().expect("content");

    assert!(tasks.contains("task-1"), "{tasks}");
    assert!(
        !tasks.contains("task-2"),
        "the failed spec left no task behind: {tasks}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_spawned_task_is_visible_to_task_list() {
    // Regression: the spawn path and the task tools once kept separate graphs, so
    // a task created by a spawn was invisible to `task_list` and a child's
    // completion never updated the task the model could see.
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-spawn",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "count the files" }] }),
            ),
            5,
        ))
        .for_parent(),
        Reply::sse(chunked(text_stream("three files. done."), 4)).for_child_agent(),
        Reply::sse(chunked(
            tool_stream("call-list", "task_list", &json!({})),
            5,
        ))
        .for_parent(),
        Reply::sse(chunked(
            tool_stream("call-wait", "agent_wait", &json!({})),
            6,
        ))
        .for_parent(),
        Reply::sse(chunked(text_stream("done"), 4)).for_parent(),
    ])
    .await;

    let project = Project::new(&server);

    let output = project
        .command()
        .args(["run", "delegate then inspect tasks", "--format", "json"])
        .output()
        .expect("run t8ful");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = json_lines(&output.stdout);
    let results = events_of(&events, "tool_result");
    let listed = results
        .iter()
        .find(|result| result["event"]["call_id"] == json!("call-list"))
        .expect("task_list produced a result");
    let content = listed["event"]["content"].as_str().expect("content");

    assert_ne!(
        content, "no tasks",
        "a task created by a spawn must be visible to task_list: {content}"
    );
    assert!(
        content.contains("count the files"),
        "task_list shows the spawn's task: {content}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_child_marks_its_task_completed() {
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-spawn",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "do the thing" }] }),
            ),
            5,
        ))
        .for_parent(),
        Reply::sse(chunked(text_stream("thing done"), 4)).for_child_agent(),
        Reply::sse(chunked(
            tool_stream("call-wait", "agent_wait", &json!({})),
            6,
        ))
        .for_parent(),
        Reply::sse(chunked(text_stream("all done"), 4)).for_parent(),
    ])
    .await;

    let project = Project::new(&server);

    let output = project
        .command()
        .args(["run", "delegate", "--format", "json"])
        .output()
        .expect("run t8ful");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = json_lines(&output.stdout);
    let spawned = events_of(&events, "agent_spawned");

    assert_eq!(
        spawned[0]["event"]["task"],
        json!("task-1"),
        "the spawn created the task this test follows"
    );

    let changes = events_of(&events, "task_changed");

    assert!(
        changes.iter().any(|change| {
            change["event"]["id"] == json!("task-1")
                && change["event"]["state"] == json!("completed")
        }),
        "the child's completion marks its own task completed: {changes:?}"
    );
}

/// The one session directory in a fresh project.
fn session_id(project: &Project) -> String {
    let entries = std::fs::read_dir(project.state().join("sessions")).expect("sessions");
    let mut dirs = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();

    dirs.sort();

    assert_eq!(dirs.len(), 1, "expected one session: {dirs:?}");

    dirs[0]
        .file_name()
        .and_then(|name| name.to_str())
        .expect("session id")
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tasks_and_dependencies_survive_resume() {
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream("call-a", "task_create", &json!({ "title": "schema" })),
            5,
        )),
        Reply::sse(chunked(
            tool_stream(
                "call-b",
                "task_create",
                &json!({ "title": "parser", "deps": ["task-1"] }),
            ),
            5,
        )),
        Reply::sse(chunked(text_stream("planned"), 4)),
    ])
    .await;

    let project = Project::new(&server);

    let output = project
        .command()
        .args(["run", "plan", "--format", "json"])
        .output()
        .expect("first run");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let id = session_id(&project);
    let resumed_server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream("call-list", "task_list", &json!({})),
            5,
        )),
        Reply::sse(chunked(text_stream("listed"), 4)),
    ])
    .await;
    let mut project = project;

    project.base_url = resumed_server.base_url();

    let output = project
        .command()
        .args(["run", "--resume", &id, "what is left", "--format", "json"])
        .output()
        .expect("resume");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = json_lines(&output.stdout);
    let results = events_of(&events, "tool_result");
    let listed = results
        .iter()
        .find(|result| result["event"]["call_id"] == json!("call-list"))
        .expect("task_list ran");
    let content = listed["event"]["content"].as_str().expect("content");

    assert!(content.contains("task-1 pending schema"), "{content}");
    assert!(
        content.contains("task-2 pending parser after task-1 [blocked]"),
        "dependencies survive too: {content}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_childs_usage_is_attributed_and_survives_a_rebuild() {
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-spawn",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "do a small thing" }] }),
            ),
            5,
        ))
        .for_parent(),
        Reply::sse(chunked(
            text_stream_with_usage("child done", 100, 20, "0.5", "gen-child"),
            5,
        ))
        .for_child_agent(),
        Reply::sse(chunked(
            tool_stream("call-wait", "agent_wait", &json!({})),
            5,
        ))
        .for_parent(),
        Reply::sse(chunked(
            text_stream_with_usage("parent done", 10, 5, "0.1", "gen-parent"),
            5,
        ))
        .for_parent(),
    ])
    .await;

    let project = Project::new(&server);

    let output = project
        .command()
        .args(["run", "delegate", "--format", "json"])
        .output()
        .expect("run");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let ledger = project.home.path().join("state/t8ful/usage/ledger.jsonl");
    let written = std::fs::read_to_string(&ledger).expect("ledger");
    let records = written
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("ledger record"))
        .collect::<Vec<_>>();

    assert!(
        records
            .iter()
            .any(|record| record["agent"] == json!("a1") && record["input_tokens"] == json!(100)),
        "the child's cost is in the ledger: {written}"
    );

    // Rebuild from journals only: the child journal must contribute its usage.
    std::fs::remove_file(&ledger).expect("remove ledger");

    let rebuilt = project
        .command()
        .args(["usage", "--rebuild", "--json"])
        .output()
        .expect("usage rebuild");

    assert!(
        rebuilt.status.success(),
        "{}",
        String::from_utf8_lossy(&rebuilt.stderr)
    );

    let usage: Value = serde_json::from_slice(&rebuilt.stdout).expect("usage json");
    let child = usage["by_agent"]
        .as_array()
        .expect("by_agent")
        .iter()
        .find(|agent| agent["agent"] == json!("a1"))
        .expect("the child is attributed after a rebuild");

    assert_eq!(child["input"], json!("100"));
    assert_eq!(child["output"], json!("20"));
    assert_eq!(child["cost"], json!("0.5"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_cannot_use_delegation_or_user_interaction_tools() {
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-spawn",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "try to delegate" }] }),
            ),
            5,
        ))
        .for_parent(),
        Reply::sse(chunked(
            tool_stream(
                "call-nested",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "nested" }] }),
            ),
            5,
        ))
        .for_child_agent(),
        Reply::sse(chunked(text_stream("cannot delegate"), 4)).for_child_agent(),
        Reply::sse(chunked(
            tool_stream("call-wait", "agent_wait", &json!({})),
            5,
        ))
        .for_parent(),
        Reply::sse(chunked(text_stream("the child could not delegate"), 4)).for_parent(),
    ])
    .await;

    let project = Project::new(&server);

    let output = project
        .command()
        .args(["run", "delegate", "--format", "json"])
        .output()
        .expect("run");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let child_request = server
        .requests()
        .into_iter()
        .find(|request| {
            request.body["messages"][0]["content"]
                .as_str()
                .is_some_and(|prompt| prompt.contains("<subagent"))
        })
        .expect("a child request");
    let tools = child_request.body["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect::<Vec<_>>();

    for denied in [
        "spawn_agents",
        "agent_wait",
        "agent_status",
        "ask_user_question",
        "present_plan",
    ] {
        assert!(
            !tools.contains(&denied),
            "child tools contain {denied}: {tools:?}"
        );
    }

    let events = json_lines(&output.stdout);
    // A child's events are nested under `agent_event` so one card is the only
    // root-level trace of it.
    let denied = events
        .iter()
        .find(|event| {
            event["event"]["type"] == json!("agent_event")
                && event["event"]["event"]["type"] == json!("tool_result")
                && event["event"]["event"]["call_id"] == json!("call-nested")
        })
        .expect("the nested spawn has a result");

    assert_eq!(denied["event"]["event"]["status"], json!("denied"));
    assert!(
        denied["event"]["event"]["content"]
            .as_str()
            .expect("content")
            .contains("unknown tool"),
        "{:?}",
        denied["event"]["event"]["content"]
    );
}

/// A role that names a task tool must actually receive it.
///
/// The child's tool set is its role list intersected with the parent's registry,
/// so the runtime has to be built after the `task_*` tools are registered; a
/// snapshot taken before them admits the role silently and yields nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_role_that_names_a_task_tool_receives_it() {
    let server = FakeServer::start(vec![
        Reply::sse(chunked(
            tool_stream(
                "call-spawn",
                "spawn_agents",
                &json!({ "agents": [{ "prompt": "look at the board", "role": "planner" }] }),
            ),
            5,
        )),
        Reply::sse(chunked(text_stream("the child saw the board"), 4)).for_child_agent(),
        Reply::sse(chunked(text_stream("done"), 4)),
    ])
    .await;

    let project = Project::with_config(
        &server,
        "[provider]\nmodel = \"vendor/test-model\"\n\n[agents.roles.planner]\ntools = \
         [\"task_list\"]\n",
    );

    let output = project
        .command()
        .args(["run", "delegate to the planner", "--format", "json"])
        .output()
        .expect("run t8ful");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let child_request = server
        .requests()
        .into_iter()
        .find(|request| {
            request.body["messages"][0]["content"]
                .as_str()
                .is_some_and(|prompt| prompt.contains("<subagent"))
        })
        .expect("a child request");

    let tools = child_request.body["tools"]
        .as_array()
        .expect("the child request carries tools");

    assert!(
        tools
            .iter()
            .any(|tool| tool["function"]["name"] == json!("task_list")),
        "the role named task_list but the child did not receive it: {tools:?}"
    );
}
