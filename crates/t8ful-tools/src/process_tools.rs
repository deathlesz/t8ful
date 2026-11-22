//! Tools for inspecting, waiting for, and stopping managed background processes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use t8ful_core::notice::ProcessNotice;
use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec};

use crate::process::{ProcessManager, ProcessSnapshot, ProcessState};

/// Printed whenever a managed process has stopped.
///
/// Cleanup signals the process group. A descendant that called `setsid` is in a
/// new group, so a clean stop is not proof the whole tree is gone.
const DETACHED_NOTE: &str =
    "[a descendant that called setsid left the process group and may survive]";

/// Seconds `process_wait` waits before it reports a process still running.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// One day; keeps `Instant + Duration` far from overflow.
const MAX_WAIT_SECS: u64 = 86_400;

#[derive(Debug, Deserialize)]
struct ProcessIdArgs {
    process_id: String,
}

#[derive(Debug, Deserialize)]
struct ProcessWaitArgs {
    process_id: String,
    #[serde(default)]
    timeout_s: Option<u64>,
}

/// The status line `process_status` and `process_wait` share.
fn status_line(snapshot: &ProcessSnapshot) -> String {
    format!(
        "{id}: {state}, exit code {code}, signal {signal}, {elapsed} ms, {bytes} bytes \
         (truncated: {truncated})",
        id = snapshot.id,
        state = snapshot.state.as_str(),
        code = snapshot
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "none".to_owned()),
        signal = snapshot
            .signal
            .map(|signal| signal.to_string())
            .unwrap_or_else(|| "none".to_owned()),
        elapsed = snapshot.elapsed_ms,
        bytes = snapshot.total_bytes,
        truncated = snapshot.truncated,
    )
}

#[derive(Debug)]
pub struct ProcessOutputTool {
    processes: Arc<ProcessManager>,
}

impl ProcessOutputTool {
    pub fn new(processes: Arc<ProcessManager>) -> Self {
        Self { processes }
    }
}

#[async_trait]
impl Tool for ProcessOutputTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "process_output".to_owned(),
            description: "Show the output collected so far for a managed background process."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "process_id": { "type": "string", "description": "Id returned by a background bash call" }
                },
                "required": ["process_id"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, call: &ToolCall, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ProcessIdArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let snapshot = self.processes.snapshot(&args.process_id)?;
        let (output, truncated, total_bytes) = self.processes.output(&args.process_id)?;

        let mut content = format!(
            "[{} • {} • {total_bytes} bytes]\n{output}",
            snapshot.state.as_str(),
            snapshot.elapsed_ms
        );

        if truncated {
            content.push_str("\n[output truncated: older bytes were dropped]\n");
        }

        if snapshot.output_incomplete {
            content
                .push_str("\n[output may be incomplete: a descendant kept the output pipe open]\n");
        }

        if let Some(error) = &snapshot.cleanup_error {
            content.push_str(&format!(
                "\n[cleanup error: {error}; the process outcome is indeterminate]\n"
            ));
        }

        Ok(ToolOutput {
            content,
            truncated,
            metadata: Some(self.processes.describe(&args.process_id)?),
            images: Vec::new(),
        })
    }
}

#[derive(Debug)]
pub struct ProcessStatusTool {
    processes: Arc<ProcessManager>,
}

impl ProcessStatusTool {
    pub fn new(processes: Arc<ProcessManager>) -> Self {
        Self { processes }
    }
}

#[async_trait]
impl Tool for ProcessStatusTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "process_status".to_owned(),
            description: "Report whether a managed background process is still running and its \
                          exit code or signal."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "process_id": { "type": "string", "description": "Id returned by a background bash call" }
                },
                "required": ["process_id"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, call: &ToolCall, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ProcessIdArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let snapshot = self.processes.snapshot(&args.process_id)?;
        let mut content = format!("{}\ncommand: {}", status_line(&snapshot), snapshot.command);

        if snapshot.state != ProcessState::Running {
            content.push('\n');
            content.push_str(DETACHED_NOTE);
        }

        if let Some(error) = &snapshot.cleanup_error {
            content.push_str(&format!(
                "\ncleanup error: {error}; the process outcome is indeterminate"
            ));
        }

        Ok(ToolOutput {
            content,
            truncated: false,
            metadata: Some(self.processes.describe(&args.process_id)?),
            images: Vec::new(),
        })
    }
}

#[derive(Debug)]
pub struct ProcessListTool {
    processes: Arc<ProcessManager>,
}

impl ProcessListTool {
    pub fn new(processes: Arc<ProcessManager>) -> Self {
        Self { processes }
    }
}

#[async_trait]
impl Tool for ProcessListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "process_list".to_owned(),
            description: "List every background process started in this session.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, _call: &ToolCall, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let processes = self.processes.list();

        if processes.is_empty() {
            return Ok(ToolOutput::text(
                "no background processes have been started",
            ));
        }

        let content = processes
            .iter()
            .map(|process| {
                format!(
                    "{id}: {state}, {elapsed} ms, {bytes} bytes — {command}",
                    id = process.id,
                    state = process.state.as_str(),
                    elapsed = process.elapsed_ms,
                    bytes = process.total_bytes,
                    command = process.command,
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(ToolOutput {
            content,
            truncated: false,
            metadata: Some(json!({
                "processes": processes
                    .iter()
                    .map(|process| process.id.clone())
                    .collect::<Vec<_>>(),
            })),
            images: Vec::new(),
        })
    }
}

#[derive(Debug)]
pub struct ProcessWaitTool {
    processes: Arc<ProcessManager>,
}

impl ProcessWaitTool {
    pub fn new(processes: Arc<ProcessManager>) -> Self {
        Self { processes }
    }
}

#[async_trait]
impl Tool for ProcessWaitTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "process_wait".to_owned(),
            description: "Wait for a managed background process to exit, or for a timeout. \
                          timeout_s is in seconds: 120 by default, 0 waits with no deadline, and \
                          one day is the maximum. An exit read here is not announced again; the \
                          full output stays in process_output. Coding mode only, because plan \
                          mode has no background processes."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "process_id": { "type": "string", "description": "Id returned by a background bash call" },
                    "timeout_s": { "type": "integer", "minimum": 0, "description": "Seconds to wait (default 120, 0 waits indefinitely)" }
                },
                "required": ["process_id"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, call: &ToolCall, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ProcessWaitArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let seconds = args
            .timeout_s
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_WAIT_SECS);
        // Zero seconds means no deadline at all.
        let limit = (seconds > 0).then(|| Duration::from_secs(seconds));
        let started = Instant::now();
        let (snapshot, exited) = self
            .processes
            .wait_for_exit(&args.process_id, limit)
            .await?;
        let waited_ms = started.elapsed().as_millis();

        let content = if exited {
            exited_report(&self.processes, &args.process_id, &snapshot)?
        } else {
            format!(
                "{} is still running after {seconds} s ({elapsed} ms, {bytes} bytes); call \
                 process_wait again or read process_output with process_id",
                snapshot.id,
                elapsed = snapshot.elapsed_ms,
                bytes = snapshot.total_bytes,
            )
        };

        let mut metadata = self.processes.describe(&args.process_id)?;

        if let Some(object) = metadata.as_object_mut() {
            object.insert("timed_out".to_owned(), json!(!exited));
            object.insert("waited_ms".to_owned(), json!(waited_ms));
        }

        Ok(ToolOutput {
            content,
            truncated: false,
            metadata: Some(metadata),
            images: Vec::new(),
        })
    }
}

/// The report for an exit this call observed.
fn exited_report(
    processes: &ProcessManager,
    id: &str,
    snapshot: &ProcessSnapshot,
) -> Result<String, ToolError> {
    let (output, _, _) = processes.output(id)?;
    let tail = ProcessNotice::tail(&output);
    let mut content = status_line(snapshot);

    content.push_str("\n\noutput tail:\n");
    content.push_str(&tail);

    if tail.len() < output.len() {
        content.push_str("\n[… earlier bytes in process_output]");
    }

    // A natural exit is not a stop, so only signalled and failed outcomes warn
    // about a descendant that left the process group.
    if matches!(
        snapshot.state,
        ProcessState::Signalled | ProcessState::Failed
    ) {
        content.push('\n');
        content.push_str(DETACHED_NOTE);
    }

    if let Some(error) = &snapshot.cleanup_error {
        content.push_str(&format!(
            "\ncleanup error: {error}; the process outcome is indeterminate"
        ));
    }

    if snapshot.output_incomplete {
        content.push_str("\n[output may be incomplete: a descendant kept the output pipe open]");
    }

    Ok(content)
}

#[derive(Debug)]
pub struct ProcessStopTool {
    processes: Arc<ProcessManager>,
}

impl ProcessStopTool {
    pub fn new(processes: Arc<ProcessManager>) -> Self {
        Self { processes }
    }
}

#[async_trait]
impl Tool for ProcessStopTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "process_stop".to_owned(),
            description: "Terminate a managed background process group, escalating to SIGKILL if \
                          it does not stop. A descendant that called setsid is outside the group \
                          and may survive."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "process_id": { "type": "string", "description": "Id returned by a background bash call" }
                },
                "required": ["process_id"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Control
    }

    async fn execute(&self, call: &ToolCall, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: ProcessIdArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let snapshot = self.processes.stop(&args.process_id).await?;

        Ok(ToolOutput {
            content: format!(
                "{} is now {} (exit code {}, signal {})\n{}",
                snapshot.id,
                snapshot.state.as_str(),
                snapshot
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "none".to_owned()),
                snapshot
                    .signal
                    .map(|signal| signal.to_string())
                    .unwrap_or_else(|| "none".to_owned()),
                DETACHED_NOTE,
            ),
            truncated: false,
            metadata: Some(self.processes.describe(&args.process_id)?),
            images: Vec::new(),
        })
    }
}
