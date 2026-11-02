//! Headless rendering.
//!
//! Text output is for people; JSON output is a versioned, one-object-per-line
//! event stream on stdout. All diagnostics go to stderr so a consumer can pipe
//! stdout straight into `jq`.

use std::io::{self, BufWriter, Write};

use serde_json::json;
use t8ful_core::event::SequencedEvent;
use t8ful_core::tool::ToolStatus;

use crate::cli::OutputFormat;
use crate::text::{terminal_safe, terminal_safe_line};

/// Bump only with an incompatible change to the JSONL envelope or event schema.
///
/// t8ful removed the `tool_approval_requested` and `tool_approval_resolved` events
/// when coding mode became autonomous. Consumers that matched those variants
/// must tolerate their absence; the envelope and every remaining event field
/// stay compatible, so the version is unchanged.
pub const JSONL_SCHEMA_VERSION: u64 = 1;

pub struct Renderer {
    format: OutputFormat,
    out: BufWriter<io::Stdout>,
    open_text: bool,
}

impl Renderer {
    pub fn new(format: OutputFormat) -> Self {
        Self {
            format,
            out: BufWriter::new(io::stdout()),
            open_text: false,
        }
    }

    pub fn write(&mut self, event: &SequencedEvent) -> io::Result<()> {
        match self.format {
            OutputFormat::Json => self.write_json(event),
            OutputFormat::Text => self.write_text(event),
        }
    }

    fn write_json(&mut self, event: &SequencedEvent) -> io::Result<()> {
        let line = json!({
            "v": JSONL_SCHEMA_VERSION,
            "seq": event.seq,
            "event": event.event,
        });

        writeln!(self.out, "{line}")?;
        self.out.flush()
    }

    fn write_text(&mut self, event: &SequencedEvent) -> io::Result<()> {
        match &event.event {
            t8ful_core::event::AgentEvent::TextDelta { text } => {
                write!(self.out, "{}", terminal_safe(text))?;
                self.open_text = true;
            }
            t8ful_core::event::AgentEvent::TextCompleted { .. } => {
                if self.open_text {
                    writeln!(self.out)?;
                    self.open_text = false;
                }
            }
            t8ful_core::event::AgentEvent::ToolCallStarted {
                tool, arguments, ..
            } => {
                self.close_text()?;

                writeln!(
                    self.out,
                    "→ {} {}",
                    terminal_safe_line(tool),
                    terminal_safe(&arguments.to_string())
                )?;
            }
            t8ful_core::event::AgentEvent::ToolResult {
                tool,
                status,
                content,
                truncated,
                ..
            } => {
                self.close_text()?;

                let marker = if *truncated { ", truncated" } else { "" };

                writeln!(
                    self.out,
                    "[{}: {}{marker}]",
                    terminal_safe_line(tool),
                    status_label(*status)
                )?;
                writeln!(self.out, "{}", terminal_safe(content))?;
            }
            t8ful_core::event::AgentEvent::Error { message } => {
                self.close_text()?;

                writeln!(self.out, "error: {}", terminal_safe(message))?;
            }
            t8ful_core::event::AgentEvent::Warning { message } => {
                self.close_text()?;

                writeln!(self.out, "warning: {}", terminal_safe(message))?;
            }
            t8ful_core::event::AgentEvent::InputRequired { kind, detail } => {
                self.close_text()?;

                writeln!(
                    self.out,
                    "input required ({}): {}",
                    terminal_safe_line(kind),
                    terminal_safe(detail)
                )?;
            }
            t8ful_core::event::AgentEvent::ProcessStatus {
                process_id,
                state,
                exit_code,
            } => {
                writeln!(
                    self.out,
                    "[{}: {}, exit {exit_code:?}]",
                    terminal_safe_line(process_id),
                    terminal_safe_line(state)
                )?;
            }
            t8ful_core::event::AgentEvent::AgentSpawned {
                agent,
                role,
                isolation,
                worktree,
                ..
            } => {
                self.close_text()?;

                let where_ = worktree
                    .as_deref()
                    .map(|path| format!(", worktree {}", terminal_safe_line(path)))
                    .unwrap_or_default();

                writeln!(
                    self.out,
                    "[{} started: role {}, {}{where_}]",
                    terminal_safe_line(agent),
                    terminal_safe_line(role),
                    terminal_safe_line(isolation)
                )?;
            }
            t8ful_core::event::AgentEvent::AgentFinished {
                agent,
                status,
                summary,
                steps,
                elapsed_ms,
                worktree,
                branch,
                ..
            } => {
                self.close_text()?;

                writeln!(
                    self.out,
                    "[{} {} after {steps} step(s) in {elapsed_ms} ms]",
                    terminal_safe_line(agent),
                    terminal_safe_line(status)
                )?;

                if let Some(path) = worktree {
                    let path = terminal_safe_line(path);
                    let branch_label =
                        terminal_safe_line(branch.as_deref().unwrap_or("(detached)"));
                    let branch_ref = terminal_safe_line(branch.as_deref().unwrap_or("HEAD"));

                    writeln!(
                        self.out,
                        "[{} worktree {path} on branch {branch_label}]",
                        terminal_safe_line(agent)
                    )?;
                    writeln!(self.out, "[inspect: git -C {path} diff {branch_ref}]")?;
                }

                if !summary.trim().is_empty() {
                    writeln!(self.out, "{}", terminal_safe(summary))?;
                }
            }
            t8ful_core::event::AgentEvent::TaskChanged {
                id,
                state,
                title,
                blocked,
                ..
            } => {
                self.close_text()?;

                let blocked = if *blocked { ", blocked" } else { "" };

                writeln!(
                    self.out,
                    "[{} {}: {}{blocked}]",
                    terminal_safe_line(id),
                    terminal_safe_line(state),
                    terminal_safe_line(title)
                )?;
            }
            _ => {}
        }

        self.out.flush()
    }

    fn close_text(&mut self) -> io::Result<()> {
        if self.open_text {
            writeln!(self.out)?;
            self.open_text = false;
        }

        Ok(())
    }

    pub fn finish(&mut self) -> io::Result<()> {
        if self.open_text {
            writeln!(self.out)?;
            self.open_text = false;
        }

        self.out.flush()
    }
}

fn status_label(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Ok => "ok",
        ToolStatus::Error => "error",
        ToolStatus::Denied => "denied",
        ToolStatus::Indeterminate => "indeterminate",
        ToolStatus::NotExecuted => "not executed",
        ToolStatus::InputRequired => "input required",
    }
}
