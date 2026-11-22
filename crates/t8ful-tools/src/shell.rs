use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use t8ful_core::policy::Mode;
use t8ful_core::sandbox::SandboxedCommand;
use t8ful_core::tool::{
    ProgressReporter, Tool, ToolCall, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec,
    Utf8Progress,
};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use crate::env::EnvFilter;
use crate::paths;
use crate::process::{
    GroupSignal, ProcessManager, READER_GRACE, REAP_TIMEOUT, TERM_GRACE, kill_group,
};

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    cwd: Option<String>,
}

/// Runs one fresh Bash process per call.
///
/// Nothing persists between calls: a `cd` or `export` inside the command affects
/// only that command, while the launch directory, `PATH`, and the Nix dev-shell
/// environment are inherited (minus denied credential variables).
#[derive(Debug)]
pub struct BashTool {
    processes: Arc<ProcessManager>,
}

impl BashTool {
    pub fn new(processes: Arc<ProcessManager>) -> Self {
        Self { processes }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".to_owned(),
            description: "Run a Bash command in a fresh process. Output is bounded and \
                          truncation is reported. For a command that may run long, set \
                          background=true: t8ful manages it, this call returns a process id \
                          immediately, and process_status, process_output, and process_stop \
                          inspect or stop it later; a stop signals the process group, so a \
                          descendant that called setsid may survive. process_wait blocks until \
                          it exits or a timeout passes. In plan mode the \
                          command runs in the local sandbox instead: the host is readable and \
                          read-only, writes fail except private /tmp, which is discarded, and \
                          there is no network or background process."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Bash command line" },
                    "timeout_ms": { "type": "integer", "minimum": 1, "description": "Foreground timeout in milliseconds" },
                    "background": { "type": "boolean", "description": "Start the command in the background (default false)" },
                    "cwd": { "type": "string", "description": "Working directory; defaults to the launch directory" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Process
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: BashArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;

        if ctx.mode == Mode::Plan {
            return run_plan(call, ctx, &args).await;
        }

        let command = build(ctx, &args.command, args.cwd.as_deref())?;

        if args.background {
            let label = args.command.clone();
            let id = self.processes.spawn_background(command, label).await?;

            return Ok(ToolOutput {
                content: format!(
                    "started {id} in the background; use process_status, process_output, and \
                     process_stop with this id"
                ),
                truncated: false,
                metadata: Some(json!({ "process_id": id, "background": true })),
                images: Vec::new(),
            });
        }

        let timeout = args
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(ctx.default_timeout);
        let outcome = self
            .processes
            .run_foreground_with_progress(command, timeout, ctx.progress.clone())
            .await?;
        let line = status_line(
            &output_status(
                outcome.timed_out,
                outcome.cancelled,
                outcome.exit_code,
                outcome.signal,
            ),
            outcome.elapsed,
        );
        let mut content = format!("{line}\n{}", outcome.output);

        if outcome.truncated {
            content.push_str(&format!(
                "\n[output truncated: kept the first {} of {} bytes]\n",
                self.processes.capacity(),
                outcome.total_bytes
            ));
        }

        if outcome.output_incomplete {
            content
                .push_str("\n[output may be incomplete: a descendant kept the output pipe open]\n");
        }

        if let Some(error) = &outcome.cleanup_error {
            content.push_str(&format!(
                "\n[cleanup error: {error}; the process outcome is indeterminate]\n"
            ));
        }

        Ok(ToolOutput {
            content,
            truncated: outcome.truncated,
            metadata: Some(json!({
                "exit_code": outcome.exit_code,
                "signal": outcome.signal,
                "timed_out": outcome.timed_out,
                "cancelled": outcome.cancelled,
                "total_bytes": outcome.total_bytes,
                "elapsed_ms": outcome.elapsed.as_millis(),
                "output_incomplete": outcome.output_incomplete,
                "cleanup_error": outcome.cleanup_error,
                "sandboxed": false,
            })),
            images: Vec::new(),
        })
    }
}

async fn run_plan(
    _call: &ToolCall,
    ctx: &ToolContext,
    args: &BashArgs,
) -> Result<ToolOutput, ToolError> {
    if args.background {
        return Err(ToolError::Failed(
            "background managed processes are not available in plan mode; run a bounded \
             foreground command instead"
                .to_owned(),
        ));
    }

    let sandbox = ctx.sandbox.as_ref().ok_or_else(|| {
        ToolError::Failed(
            "plan-mode shell execution is unavailable because the local sandbox is not \
             configured; refusing to run the command on the host"
                .to_owned(),
        )
    })?;

    let directory = plan_cwd(ctx, args.cwd.as_deref())?;
    let mounts = plan_mounts(ctx)?;
    let env = plan_env_for(ctx);
    let program = find_program("bash").ok_or_else(|| {
        ToolError::Failed(
            "bash is not available in the planning environment; install it as a runtime tool"
                .to_owned(),
        )
    })?;

    let command = SandboxedCommand {
        program: program.display().to_string(),
        args: vec!["-c".to_owned(), args.command.clone()],
        cwd: directory,
        env,
        read_only_paths: mounts,
    };

    let mut process = sandbox.spawn(command).await.map_err(|error| {
        ToolError::Failed(format!(
            "the planning sandbox could not start the command ({error}); no command was run on \
             the host"
        ))
    })?;

    let timeout = args
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(ctx.default_timeout);
    execute_sandboxed(
        &mut process.child,
        timeout,
        ctx.max_output_bytes,
        args.command.clone(),
        ctx.progress.clone(),
    )
    .await
}

fn plan_cwd(ctx: &ToolContext, requested: Option<&str>) -> Result<PathBuf, ToolError> {
    let dir = requested
        .map(|requested| paths::resolve(&ctx.launch_dir, requested))
        .unwrap_or_else(|| ctx.launch_dir.clone());
    let canonical = std::fs::canonicalize(&dir).map_err(|error| {
        ToolError::Failed(format!(
            "the planning working directory {} cannot be resolved: {error}",
            dir.display()
        ))
    })?;
    let project =
        std::fs::canonicalize(&ctx.project_root).unwrap_or_else(|_| ctx.project_root.clone());
    let covered = canonical.starts_with(&project)
        || ctx
            .read_scope
            .as_ref()
            .is_some_and(|scope| scope.roots().iter().any(|root| canonical.starts_with(root)));

    if !covered {
        return Err(ToolError::Failed(format!(
            "the planning working directory {} is outside the plan-mode read boundary",
            canonical.display()
        )));
    }

    Ok(canonical)
}

fn plan_mounts(ctx: &ToolContext) -> Result<Vec<PathBuf>, ToolError> {
    // The sandbox binds the whole host read-only, so only the roots that must
    // stay visible *after* the scratch tmpfs are listed here: a project under
    // /tmp or /var/tmp would otherwise be masked by the writable scratch mount.
    let mut mounts = vec![ctx.project_root.clone()];

    if let Some(scope) = &ctx.read_scope {
        mounts.extend(scope.roots().iter().cloned());
    }

    let mut unique = Vec::new();

    for mount in mounts {
        let canonical = std::fs::canonicalize(&mount).map_err(|error| {
            ToolError::Failed(format!(
                "cannot resolve the planning mount {}: {error}",
                mount.display()
            ))
        })?;

        if !unique.contains(&canonical) {
            unique.push(canonical);
        }
    }

    Ok(unique)
}

/// The environment a plan-mode command may receive.
///
/// Plan mode runs on behalf of a reader, so it does not inherit the ambient
/// credential set. Only locale, shell lookup, and timezone survive, plus the
/// context's deliberate extras; the deny list applies to both.
fn plan_env_for(ctx: &ToolContext) -> BTreeMap<String, String> {
    plan_env_from(ctx, std::env::vars_os())
}

fn plan_env_from(
    ctx: &ToolContext,
    ambient: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> BTreeMap<String, String> {
    let filter = EnvFilter::new(&ctx.deny_env);
    let mut env = BTreeMap::new();

    for (name, value) in ambient {
        let Some(name) = name.to_str() else {
            continue;
        };

        if !plan_allows(name) || filter.denies(std::ffi::OsStr::new(name)) {
            continue;
        }

        if let Ok(value) = value.into_string() {
            env.insert(name.to_owned(), value);
        }
    }

    for (name, value) in &ctx.extra_env {
        if !filter.denies(std::ffi::OsStr::new(name)) {
            env.insert(name.clone(), value.clone());
        }
    }

    env
}

fn plan_allows(name: &str) -> bool {
    matches!(name, "PATH" | "HOME" | "LANG" | "TZ") || name.starts_with("LC_")
}

/// The environment a coding-mode shell command may receive.
///
/// The deny list applies to inherited variables and to the context's extras
/// alike: an extra can add a variable, never reintroduce one that was withheld.
fn env_for(ctx: &ToolContext) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = EnvFilter::new(&ctx.deny_env)
        .environment()
        .into_iter()
        .filter_map(|(name, value)| {
            let name = name.into_string().ok()?;
            let value = value.into_string().ok()?;

            Some((name, value))
        })
        .collect();

    let filter = EnvFilter::new(&ctx.deny_env);

    for (name, value) in &ctx.extra_env {
        if !filter.denies(std::ffi::OsStr::new(name)) {
            env.insert(name.clone(), value.clone());
        }
    }

    env
}

fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;

    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);

        if candidate.is_file() {
            return Some(candidate);
        }
    }

    None
}

/// Reported on every sandboxed result so the model does not mistake a scratch
/// write for a change to the host or the project.
const SANDBOX_NOTE: &str = "[sandbox: the host is readable and read-only; writes fail except private /tmp, which is \
     discarded; no network]";

async fn execute_sandboxed(
    child: &mut Child,
    timeout: Duration,
    limit: usize,
    label: String,
    progress: Option<ProgressReporter>,
) -> Result<ToolOutput, ToolError> {
    let started = Instant::now();
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::Failed("the sandboxed command has no stdout".to_owned()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Failed("the sandboxed command has no stderr".to_owned()))?;

    let stdout_output = Arc::new(Mutex::new(Collected::default()));
    let stderr_output = Arc::new(Mutex::new(Collected::default()));
    let stdout_progress = progress.clone();
    let stdout_shared = Arc::clone(&stdout_output);
    let mut stdout_task = tokio::spawn(async move {
        read_bounded(&mut stdout, limit, stdout_progress, stdout_shared).await;
    });
    let stderr_shared = Arc::clone(&stderr_output);
    let mut stderr_task = tokio::spawn(async move {
        read_bounded(&mut stderr, limit, progress, stderr_shared).await;
    });

    let waited = tokio::time::timeout(timeout, child.wait()).await;
    let timed_out = waited.is_err();

    let status = match waited {
        Ok(status) => status.map_err(|error| {
            ToolError::Failed(format!("cannot wait for the sandboxed command: {error}"))
        })?,
        Err(_) => terminate_sandboxed(child).await?,
    };

    // A descendant that inherited the pipes can outlive the command, so the
    // readers are bounded the way the managed-process path bounds them.
    let deadline = Instant::now() + READER_GRACE;
    let mut output_incomplete = drain_reader(&mut stdout_task, deadline).await;

    output_incomplete |= drain_reader(&mut stderr_task, deadline).await;

    let (out, out_total, out_error) = take_collected(&stdout_output);
    let (err, err_total, err_error) = take_collected(&stderr_output);

    if let Some(error) = out_error.or(err_error) {
        return Err(ToolError::Failed(error));
    }

    let total = out_total + err_total;
    let truncated = out.len() < out_total || err.len() < err_total;
    let elapsed = started.elapsed();
    let (kind, value) = status_parts(&status);
    let (exit_code, signal) = match kind {
        "signal" => (None, value),
        _ => (value, None),
    };
    let line = status_line(&output_status(timed_out, false, exit_code, signal), elapsed);
    let mut content = format!("{line}\n{SANDBOX_NOTE}\n{}", String::from_utf8_lossy(&out));
    let err_text = String::from_utf8_lossy(&err);

    if !err.is_empty() {
        content.push_str(&err_text);
    }

    // The two failures a read-only host makes likely: a toolchain that is not
    // visible at the inherited PATH, and a command that tried to write.
    if exit_code == Some(127) {
        content.push_str("\n[plan mode: the command is not on PATH]\n");
    }

    if err_text.contains("Read-only file system") || err_text.contains("read-only file system") {
        content.push_str("\n[plan mode: mounts are read-only]\n");
    }

    if truncated {
        content.push_str(&format!(
            "\n[output truncated: kept at most {limit} bytes of {total}]\n"
        ));
    }

    if output_incomplete {
        content.push_str("\n[output may be incomplete: a descendant kept the output pipe open]\n");
    }

    Ok(ToolOutput {
        content,
        truncated,
        metadata: Some(json!({
            "sandboxed": true,
            "timed_out": timed_out,
            "exit_code": exit_code,
            "signal": signal,
            "total_bytes": total,
            "elapsed_ms": elapsed.as_millis(),
            "output_incomplete": output_incomplete,
            "command": label,
        })),
        images: Vec::new(),
    })
}

/// Stop a sandboxed command that overran its timeout.
///
/// `start_kill` alone reaches only the direct child, and bubblewrap's child is
/// the whole command: one descendant that inherited the output pipe keeps the
/// readers alive forever. The sandbox starts the command in its own process
/// group, so the group gets SIGTERM, a short grace period, then SIGKILL, before
/// the leader is reaped. A backend that did not make the child a group leader
/// falls back to killing the direct child, because signalling the inherited
/// group would reach t8ful itself.
async fn terminate_sandboxed(child: &mut Child) -> Result<std::process::ExitStatus, ToolError> {
    let group = child.id().filter(|pid| leads_process_group(*pid));

    if let Some(pid) = group {
        let _ = kill_group(pid, GroupSignal::Term);

        // The leader is still unreaped, so the group id stays allocated for
        // this sleep and the SIGKILL below cannot reach a recycled process.
        tokio::time::sleep(TERM_GRACE).await;
        let _ = kill_group(pid, GroupSignal::Kill);
    } else {
        let _ = child.start_kill();
    }

    match tokio::time::timeout(REAP_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(error)) => Err(ToolError::Failed(format!(
            "cannot reap the sandboxed command: {error}"
        ))),
        Err(_) => Err(ToolError::Failed(
            "timed out while reaping the sandboxed command".to_owned(),
        )),
    }
}

/// Whether `pid` leads its own process group, so a group signal reaches the
/// command's descendants instead of the group t8ful itself belongs to.
fn leads_process_group(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        use nix::unistd::{Pid, getpgid};

        getpgid(Some(Pid::from_raw(pid as i32)))
            .map(|group| group.as_raw() == pid as i32)
            .unwrap_or(false)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;

        false
    }
}

/// Wait a bounded time for one reader to reach EOF, then abort it.
///
async fn drain_reader(task: &mut tokio::task::JoinHandle<()>, deadline: Instant) -> bool {
    let remaining = deadline.saturating_duration_since(Instant::now());

    if !remaining.is_zero() && tokio::time::timeout(remaining, &mut *task).await.is_ok() {
        return false;
    }

    task.abort();
    let _ = task.await;

    true
}

/// Output one reader task collected, shared so aborting the task does not
/// discard what it already read.
#[derive(Debug, Default)]
struct Collected {
    kept: Vec<u8>,
    total: usize,
    error: Option<String>,
}

fn take_collected(collected: &Mutex<Collected>) -> (Vec<u8>, usize, Option<String>) {
    let mut collected = collected
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    (
        std::mem::take(&mut collected.kept),
        collected.total,
        collected.error.take(),
    )
}

async fn read_bounded<R>(
    reader: &mut R,
    limit: usize,
    progress: Option<ProgressReporter>,
    collected: Arc<Mutex<Collected>>,
) where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut buffer = [0_u8; 8192];
    let mut progress = progress.map(Utf8Progress::new);

    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => {
                collected
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .error = Some(format!("cannot read process output: {error}"));

                break;
            }
        };

        let mut collected = collected
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        collected.total += read;

        if collected.kept.len() < limit {
            let take = (limit - collected.kept.len()).min(read);

            collected.kept.extend_from_slice(&buffer[..take]);
        }

        drop(collected);

        if let Some(progress) = &mut progress {
            progress.report(&buffer[..read]);
        }
    }

    if let Some(progress) = &mut progress {
        progress.finish();
    }
}

fn status_parts(status: &std::process::ExitStatus) -> (&'static str, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if let Some(signal) = status.signal() {
            return ("signal", Some(signal));
        }
    }

    if status.success() {
        ("exit", Some(0))
    } else {
        ("exit", status.code())
    }
}

fn output_status(
    timed_out: bool,
    cancelled: bool,
    exit_code: Option<i32>,
    signal: Option<i32>,
) -> (&'static str, Option<i32>) {
    if timed_out {
        ("timeout", None)
    } else if cancelled {
        ("cancelled", None)
    } else if let Some(signal) = signal {
        ("signal", Some(signal))
    } else {
        ("exit", exit_code)
    }
}

fn status_line(status: &(&'static str, Option<i32>), elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64();

    match status {
        ("timeout", _) => format!("[timed out after {seconds:.1}s]"),
        ("cancelled", _) => format!("[cancelled, {seconds:.1}s]"),
        ("signal", Some(signal)) => format!("[terminated by signal {signal}, {seconds:.1}s]"),
        ("exit", Some(code)) => format!("[exit code {code}, {seconds:.1}s]"),
        _ => format!("[finished, {seconds:.1}s]"),
    }
}

/// Build a Bash command with the filtered environment and resolved working
/// directory. Shared with the managed-process tools for consistency.
pub(crate) fn build(
    ctx: &ToolContext,
    command_text: &str,
    cwd: Option<&str>,
) -> Result<Command, ToolError> {
    let dir = match cwd {
        Some(cwd) => paths::resolve(&ctx.launch_dir, cwd),
        None => ctx.launch_dir.clone(),
    };

    if !dir.is_dir() {
        return Err(ToolError::Failed(format!(
            "working directory {} does not exist or is not a directory",
            dir.display()
        )));
    }

    let mut command = Command::new("bash");

    command.arg("-c").arg(command_text);
    command.current_dir(&dir);
    command.env_clear();
    command.envs(env_for(ctx));

    Ok(command)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};

    use t8ful_core::tool::ToolContext;

    use super::{build, plan_env_from};

    #[test]
    fn a_denied_extra_is_not_reintroduced_into_a_command() {
        let dir = tempfile::tempdir().expect("dir");
        let mut ctx = ToolContext::new(dir.path().into(), dir.path().into());

        ctx.deny_env = vec!["T8FUL_TEST_SECRET".to_owned()];
        ctx.extra_env = vec![
            ("T8FUL_TEST_SECRET".to_owned(), "leak".to_owned()),
            ("T8FUL_TEST_ALLOWED".to_owned(), "ok".to_owned()),
        ];

        let command = build(&ctx, "true", None).expect("built");
        let command = command.as_std();
        let env = command.get_envs().collect::<Vec<_>>();

        assert!(
            !env.iter()
                .any(|(name, _)| *name == OsStr::new("T8FUL_TEST_SECRET")),
            "the deny list must win over an extra"
        );
        assert!(
            env.iter()
                .any(|(name, value)| *name == OsStr::new("T8FUL_TEST_ALLOWED")
                    && *value == Some(OsStr::new("ok")))
        );
    }

    #[test]
    fn plan_mode_keeps_only_the_environment_allowlist() {
        let dir = tempfile::tempdir().expect("dir");
        let mut ctx = ToolContext::new(dir.path().into(), dir.path().into());

        ctx.deny_env = vec!["EXTRA_SECRET".to_owned()];
        ctx.extra_env = vec![
            ("EXTRA_SECRET".to_owned(), "leak".to_owned()),
            ("EXTRA_ALLOWED".to_owned(), "ok".to_owned()),
        ];

        let ambient = [
            ("PATH", "/bin"),
            ("HOME", "/home/u"),
            ("LANG", "C.UTF-8"),
            ("LC_ALL", "C.UTF-8"),
            ("LC_CTYPE", "C.UTF-8"),
            ("TZ", "UTC"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("OPENROUTER_API_KEY", "sk-or-secret"),
            ("OPENCODE_GO_API_KEY", "oc_sk-secret"),
            ("TERM", "xterm"),
        ]
        .into_iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));

        let env = plan_env_from(&ctx, ambient);

        assert_eq!(
            env,
            BTreeMap::from([
                ("PATH".to_owned(), "/bin".to_owned()),
                ("HOME".to_owned(), "/home/u".to_owned()),
                ("LANG".to_owned(), "C.UTF-8".to_owned()),
                ("LC_ALL".to_owned(), "C.UTF-8".to_owned()),
                ("LC_CTYPE".to_owned(), "C.UTF-8".to_owned()),
                ("TZ".to_owned(), "UTC".to_owned()),
                ("EXTRA_ALLOWED".to_owned(), "ok".to_owned()),
            ])
        );
    }
}
