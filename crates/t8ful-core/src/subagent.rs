//! Subagent identity, limits, registry, and the notice queue.
//!
//! A child agent is an ordinary [`crate::agent::Agent`] with its own model
//! context, its own conversation, and its own journal. Nothing here changes how
//! a turn runs; this module only decides who may be spawned, hands out ids,
//! tracks what is running, and delivers a completion notice to the parent.
//!
//! Two properties matter and are deliberately structural rather than
//! documented-only:
//!
//! * **Spawning is non-blocking.** [`AgentRegistry::register`] records a child
//!   the moment it starts so the parent's `spawn` tool can return immediately.
//!   No caller holds a concurrency slot while awaiting a child, so the root's
//!   own work can never be starved by its children.
//! * **A completion never interrupts.** Notices travel on their own queue, not
//!   through steering. Steering never cancels an in-flight tool call either;
//!   it is delivered only after the parent's current step finishes, so a child
//!   finishing must never disturb that work.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::journal::AgentJournal;
use crate::message::Conversation;
use crate::policy::Mode;
use crate::provider::ReasoningEffort;
use crate::task::TaskId;
use crate::usage::Usage;

/// Identifier of one agent in a session tree.
///
/// The root is always `root`; children are `a1`, `a2`, … numbered per session so
/// a resumed session keeps referring to the same agents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(String);

impl AgentId {
    pub fn root() -> Self {
        Self("root".to_owned())
    }

    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0 == "root"
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::root()
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AgentId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for AgentId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Whether a child shares the parent's working tree or gets its own worktree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// Edit the real tree. The default: a read-only child gains nothing from a
    /// worktree and loses the warm build cache.
    #[default]
    Shared,
    /// A Git worktree based on `HEAD`, so concurrent writers cannot corrupt the
    /// parent's tree.
    Worktree,
}

impl Isolation {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "shared" | "none" => Some(Self::Shared),
            "worktree" | "isolated" => Some(Self::Worktree),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Worktree => "worktree",
        }
    }
}

/// A Git worktree handed to an isolated child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// Absolute path of the working tree.
    pub path: std::path::PathBuf,
    /// Branch created for this child, or `None` for a detached checkout.
    #[serde(default)]
    pub branch: Option<String>,
    /// Commit the worktree started from.
    #[serde(default)]
    pub base_commit: Option<String>,
    /// Parent paths Git ignores, such as build output, that this worktree does
    /// not contain either. Reported, never a reason to refuse the spawn.
    #[serde(default)]
    pub ignored_paths: Vec<String>,
}

impl Worktree {
    /// The command a person or the model can run to see the work.
    pub fn inspect_command(&self) -> String {
        format!("git -C {} diff", self.path.display())
    }
}

/// A reusable description of a kind of child.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleProfile {
    /// Prepended to the child's system prompt.
    #[serde(default)]
    pub instructions: String,
    /// Tools the role may use. Empty means every tool the parent has.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Model for the role, or the parent's model when unset.
    #[serde(default)]
    pub model: Option<String>,
    /// Default isolation for this role.
    #[serde(default)]
    pub isolation: Option<Isolation>,
}

impl RoleProfile {
    /// Whether the role may use `tool`.
    ///
    /// An empty tool list means "whatever the parent has", not "none": a role
    /// that names no tools is a general-purpose child. That keeps an ad hoc
    /// role useful without configuration.
    pub fn allows(&self, tool: &str) -> bool {
        self.tools.is_empty() || self.tools.iter().any(|allowed| allowed == tool)
    }
}

/// Everything needed to start one child, after validation.
///
/// The mode, model, window, and reasoning level are the parent's values at the
/// moment the spawn was requested, not the values the session started with. A
/// child snapshots them at launch: switching the parent's mode or model later
/// never rewrites a child already running, and a child started after a switch
/// gets the new settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    pub id: AgentId,
    pub role: String,
    pub prompt: String,
    pub task: Option<TaskId>,
    pub isolation: Isolation,
    /// The parent's model; a role may override it.
    pub parent_model: String,
    /// The parent's mode, which bounds the child whatever its role says.
    pub mode: Mode,
    /// The parent's effective context window.
    pub context_limit: u64,
    /// The output budget that goes with that window.
    pub max_output_tokens: Option<u32>,
    /// The parent's reasoning level at spawn time.
    pub reasoning_effort: ReasoningEffort,
    /// The session whose journal, task graph, and child directory this spawn
    /// belongs to. Pinned by the spawner before the child task starts, so a
    /// session switch cannot split the spawn record from the child's own journal.
    pub session_id: String,
    /// Set when the child runs in its own worktree.
    pub worktree: Option<Worktree>,
}

/// Limits that bound the whole tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentLimits {
    /// Children running at once across the session.
    pub max_concurrent: usize,
    /// Children one root turn may start in total.
    pub max_spawns_per_turn: u32,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            max_spawns_per_turn: 16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildStatus {
    Running,
    /// The child's turn ended on its own.
    Completed,
    /// The child reported an error, or its turn failed.
    Failed,
    /// The child was cancelled before it finished.
    Cancelled,
}

impl ChildStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_running(self) -> bool {
        self == Self::Running
    }
}

/// Escape free text so it cannot forge the closing tag of a notice envelope.
pub fn escape_inline(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());

    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            other => escaped.push(other),
        }
    }

    escaped
}

/// Inverse of [`escape_inline`]: turn a rendered notice field back into the
/// text a person should read.
///
/// `&amp;` is expanded last so `&amp;lt;` — an escaped ampersand followed by the
/// letters `lt` — comes back as `&lt;` instead of being read as `<`.
pub fn unescape_inline(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }

    let mut unescaped = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(index) = rest.find('&') {
        unescaped.push_str(&rest[..index]);

        let tail = &rest[index..];

        if let Some(remainder) = tail.strip_prefix("&lt;") {
            unescaped.push('<');
            rest = remainder;
        } else if let Some(remainder) = tail.strip_prefix("&gt;") {
            unescaped.push('>');
            rest = remainder;
        } else if let Some(remainder) = tail.strip_prefix("&amp;") {
            unescaped.push('&');
            rest = remainder;
        } else {
            unescaped.push('&');
            rest = &tail[1..];
        }
    }

    unescaped.push_str(rest);
    unescaped
}

/// Longest child summary carried inside an agent notice, in bytes.
pub const AGENT_NOTICE_SUMMARY_BYTES: usize = 8_000;

/// What a finished child hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReport {
    pub agent: AgentId,
    pub role: String,
    pub status: ChildStatus,
    /// The child's final assistant text, trimmed.
    pub summary: String,
    pub steps: u32,
    pub usage: Usage,
    pub task: Option<TaskId>,
    pub worktree: Option<Worktree>,
    /// Elapsed milliseconds from start to finish.
    pub elapsed_ms: u128,
}

impl AgentReport {
    /// The block a parent or a frontend prints for one finished child.
    pub fn render(&self) -> String {
        let mut lines = vec![format!(
            "{} ({}) {} after {} step{} in {} ms",
            self.agent,
            escape_inline(&self.role),
            self.status.as_str(),
            self.steps,
            if self.steps == 1 { "" } else { "s" },
            self.elapsed_ms,
        )];

        if let Some(task) = &self.task {
            lines.push(format!("task: {}", escape_inline(&task.to_string())));
        }

        if let Some(worktree) = &self.worktree {
            let branch = escape_inline(worktree.branch.as_deref().unwrap_or("(detached)"));

            lines.push(format!(
                "worktree: {} on branch {branch}",
                escape_inline(&worktree.path.display().to_string())
            ));
            lines.push(format!(
                "inspect: {}",
                escape_inline(&worktree.inspect_command())
            ));
        }

        if !self.usage.is_empty() {
            let input = self.usage.input_tokens.unwrap_or(0);
            let output = self.usage.output_tokens.unwrap_or(0);
            let mut line = format!("usage: {input} in / {output} out");

            if let Some(cost) = &self.usage.cost_usd {
                line.push_str(&format!(" • ${cost}"));
            }

            lines.push(line);
        }

        if !self.summary.is_empty() {
            lines.push(String::new());
            lines.push(escape_inline(&self.summary));
        }

        lines.join("\n")
    }
}

/// A notice the parent receives when a child stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentNotice {
    pub report: AgentReport,
}

impl AgentNotice {
    /// The conversation text carrying this notice.
    ///
    /// The marker is plain text because it travels through the provider like any
    /// other user message; resume parsing reads it back the same way it reads a
    /// skill load.
    pub fn render(&self) -> String {
        let mut report = self.report.clone();

        if report.summary.len() > AGENT_NOTICE_SUMMARY_BYTES {
            let mut limit = AGENT_NOTICE_SUMMARY_BYTES;

            while limit > 0 && !report.summary.is_char_boundary(limit) {
                limit -= 1;
            }

            report.summary.truncate(limit);
            report
                .summary
                .push_str("\n\n[summary truncated at 8000 bytes]");
        }

        format!(
            "<agent_notice agent=\"{}\" status=\"{}\">\n{}\n</agent_notice>\nThe agent above \
             stopped on its own. Nothing it produced was applied to your working tree.",
            self.report.agent,
            self.report.status.as_str(),
            report.render(),
        )
    }

    /// Whether a conversation message is an agent notice, and which agent it
    /// names.
    pub fn parse(text: &str) -> Option<AgentId> {
        let rest = text.strip_prefix("<agent_notice agent=\"")?;
        let (agent, _) = rest.split_once('"')?;

        Some(AgentId::new(agent))
    }
}

/// Per-agent completion queue, drained by that agent's loop at a safe boundary.
///
/// Cloning shares the queue. The registry holds one clone per parent; the parent
/// agent holds another.
#[derive(Debug, Clone, Default)]
pub struct NoticeQueue {
    inner: Arc<NoticeQueueInner>,
}

#[derive(Debug, Default)]
struct NoticeQueueInner {
    notices: Mutex<Vec<AgentNotice>>,
}

impl NoticeQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, notice: AgentNotice) {
        self.inner
            .notices
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(notice);
    }

    pub fn drain(&self) -> Vec<AgentNotice> {
        std::mem::take(
            &mut *self
                .inner
                .notices
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.inner
            .notices
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_empty()
    }
}

/// One running or finished child, as a frontend or a sibling tool sees it.
#[derive(Debug, Clone)]
pub struct ChildSnapshot {
    pub id: AgentId,
    pub role: String,
    pub task: Option<TaskId>,
    pub isolation: Isolation,
    pub worktree: Option<Worktree>,
    pub status: ChildStatus,
    pub elapsed: Duration,
    /// Steps the child has taken so far, from its own turn progress.
    pub steps: u32,
}

impl ChildSnapshot {
    /// One display line, stable enough for a status-line notice.
    pub fn line(&self) -> String {
        let elapsed = self.elapsed.as_secs();

        format!(
            "{} {}{} {}s",
            self.id,
            self.role,
            if self.worktree.is_some() {
                " (worktree)"
            } else {
                ""
            },
            elapsed,
        )
    }
}

#[derive(Debug)]
struct ChildEntry {
    request: SpawnRequest,
    status: ChildStatus,
    started: Instant,
    finished: Option<Instant>,
    steps: u32,
    cancel: CancellationToken,
    report: Option<AgentReport>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    next: u64,
    children: BTreeMap<AgentId, ChildEntry>,
    spawns_this_turn: u32,
}

/// Session-wide tracking of child agents.
///
/// Cheap to clone: every clone shares one registry. The driver keeps one, the
/// spawning tool another, and a frontend reads snapshots from the same place.
#[derive(Debug, Clone)]
pub struct AgentRegistry {
    inner: Arc<Mutex<RegistryInner>>,
    limits: AgentLimits,
    notices: Arc<Mutex<BTreeMap<AgentId, NoticeQueue>>>,
    /// Bumped whenever a child finishes so waiters can wake without polling.
    version_tx: watch::Sender<u64>,
}

impl AgentRegistry {
    pub fn new(limits: AgentLimits) -> Self {
        let (version_tx, _) = watch::channel(0);

        Self {
            inner: Arc::new(Mutex::new(RegistryInner::default())),
            limits,
            notices: Arc::new(Mutex::new(BTreeMap::new())),
            version_tx,
        }
    }

    pub fn limits(&self) -> AgentLimits {
        self.limits
    }

    /// The completion queue for one agent, created on first use.
    pub fn notices_for(&self, agent: &AgentId) -> NoticeQueue {
        let mut notices = self
            .notices
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        notices.entry(agent.clone()).or_default().clone()
    }

    /// Allocate the next child id.
    pub fn allocate(&self) -> AgentId {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        inner.next += 1;

        AgentId::new(format!("a{}", inner.next))
    }

    /// Reset the per-turn spawn budget. Called by the driver when a root turn
    /// starts, because the limit is per turn, not per session.
    pub fn begin_turn(&self) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        inner.spawns_this_turn = 0;
    }

    pub fn spawns_this_turn(&self) -> u32 {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .spawns_this_turn
    }

    pub fn running(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .children
            .values()
            .filter(|entry| entry.status.is_running())
            .count()
    }

    /// Why one more child cannot start now, or `None` when it can.
    ///
    /// Separate from [`Self::register`] so the tool can refuse a whole batch
    /// before starting any of it.
    pub fn check_capacity(&self, additional: usize) -> Result<(), String> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let running = inner
            .children
            .values()
            .filter(|entry| entry.status.is_running())
            .count();
        let room = self.limits.max_concurrent.saturating_sub(running);

        if additional > room {
            return Err(format!(
                "{} child agent(s) already running; {} of {} slots are free and {} were requested. \
                 Wait with agent_wait for some to finish",
                running, room, self.limits.max_concurrent, additional
            ));
        }

        let used = inner.spawns_this_turn;

        if used + additional as u32 > self.limits.max_spawns_per_turn {
            return Err(format!(
                "this turn has started {used} of {} allowed child agents; {} more were requested. \
                 The budget resets at the next turn",
                self.limits.max_spawns_per_turn, additional
            ));
        }

        Ok(())
    }

    /// Record a child that is about to run and return its cancel token.
    pub fn register(&self, request: SpawnRequest) -> Result<CancellationToken, String> {
        self.check_capacity(1)?;

        let cancel = CancellationToken::new();
        let id = request.id.clone();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        inner.spawns_this_turn += 1;
        inner.children.insert(
            id,
            ChildEntry {
                request,
                status: ChildStatus::Running,
                started: Instant::now(),
                finished: None,
                steps: 0,
                cancel: cancel.clone(),
                report: None,
            },
        );

        Ok(cancel)
    }

    /// Cancel every child and forget it.
    ///
    /// Called when the session changes. The registry is shared for the life of
    /// the process, so entries left behind would keep publishing status into the
    /// new session and let a finished child reach its graph.
    pub fn abandon_all(&self) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        for entry in inner.children.values() {
            entry.cancel.cancel();
        }

        inner.children.clear();

        // The notice queues are shared with the abandoned agents, so draining
        // and dropping the registry's clones is the only way to prevent a stale
        // report from being journaled into the next session.
        let mut notices = self
            .notices
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        for queue in notices.values() {
            queue.drain();
        }

        notices.clear();
    }

    /// Note how many steps a child has taken, for status display.
    pub fn set_steps(&self, agent: &AgentId, steps: u32) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        if let Some(entry) = inner.children.get_mut(agent) {
            entry.steps = steps;
        }
    }

    /// The cancel token of one child, for a frontend that stops it directly.
    pub fn cancel_token(&self, agent: &AgentId) -> Option<CancellationToken> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .children
            .get(agent)
            .map(|entry| entry.cancel.clone())
    }

    /// Cancel every running child.
    ///
    /// The token is cancelled immediately; a status stays `Running` until the
    /// child's own turn observes the cancellation and reports. Returns the
    /// number of children that were still running.
    pub fn cancel_all(&self) -> usize {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut cancelled = 0;

        for entry in inner.children.values() {
            if entry.status.is_running() && !entry.cancel.is_cancelled() {
                entry.cancel.cancel();
                cancelled += 1;
            }
        }

        cancelled
    }

    /// Mark a child finished, publish its report, and notify its parent.
    ///
    /// Returns the rendered notice text so a caller that also journals the
    /// parent's conversation can stay in step with what the loop will deliver.
    pub fn finish(&self, report: AgentReport, parent: &AgentId) -> AgentNotice {
        let notice = AgentNotice {
            report: report.clone(),
        };

        {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());

            if let Some(entry) = inner.children.get_mut(&report.agent) {
                if let Some(existing) = &entry.report {
                    return AgentNotice {
                        report: existing.clone(),
                    };
                }

                entry.status = report.status;
                entry.finished = Some(Instant::now());
                entry.steps = report.steps;
                entry.report = Some(report.clone());

                // Published and queued under the same guard: an `agent_wait`
                // that sees the finished report must not be able to drain an
                // empty notice queue and then be handed the same report again
                // by the wake turn. Lock order stays inner -> notices.
                self.notices_for(parent).push(notice.clone());

                // Wake anyone in `agent_wait` without polling. An abandoned
                // child is no longer registered and must not repopulate the
                // queue or wake the new session.
                self.version_tx.send_modify(|version| *version += 1);
            }
        }

        notice
    }

    /// Drop the notices whose reports were returned to the model, keeping the
    /// rest queued for the next turn.
    ///
    /// `agent_wait` calls this after returning reports: the model has the
    /// content, so a notice for the same report must not spend another turn.
    pub fn consume_notices(&self, parent: &AgentId, returned: &[AgentId]) {
        let queue = self.notices_for(parent);
        let notices = queue.drain();
        let mut kept = Vec::new();

        for notice in notices {
            if !returned.contains(&notice.report.agent) {
                kept.push(notice);
            }
        }

        for notice in kept {
            queue.push(notice);
        }
    }

    /// The report of a finished child.
    pub fn report(&self, agent: &AgentId) -> Option<AgentReport> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .children
            .get(agent)
            .and_then(|entry| entry.report.clone())
    }

    /// Reports for `agents`, or for every finished child when `agents` is empty.
    pub fn reports(&self, agents: &[AgentId]) -> Vec<AgentReport> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        inner
            .children
            .values()
            .filter(|entry| {
                agents.is_empty() || agents.iter().any(|wanted| wanted == &entry.request.id)
            })
            .filter_map(|entry| entry.report.clone())
            .collect()
    }

    /// Ids that are still running, out of `agents` (or all children when empty).
    pub fn still_running(&self, agents: &[AgentId]) -> Vec<AgentId> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        inner
            .children
            .values()
            .filter(|entry| {
                agents.is_empty() || agents.iter().any(|wanted| wanted == &entry.request.id)
            })
            .filter(|entry| entry.status.is_running())
            .map(|entry| entry.request.id.clone())
            .collect()
    }

    pub fn snapshots(&self) -> Vec<ChildSnapshot> {
        self.snapshots_of(&[])
    }

    pub fn snapshots_of(&self, agents: &[AgentId]) -> Vec<ChildSnapshot> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let now = Instant::now();

        inner
            .children
            .values()
            .filter(|entry| {
                agents.is_empty() || agents.iter().any(|wanted| wanted == &entry.request.id)
            })
            .map(|entry| ChildSnapshot {
                id: entry.request.id.clone(),
                role: entry.request.role.clone(),
                task: entry.request.task.clone(),
                isolation: entry.request.isolation,
                worktree: entry.request.worktree.clone(),
                status: entry.status,
                elapsed: entry
                    .finished
                    .unwrap_or(now)
                    .saturating_duration_since(entry.started),
                steps: entry.steps,
            })
            .collect()
    }

    /// Whether no child has ever been started in this session.
    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .children
            .is_empty()
    }

    /// Whether the current turn still has spawn budget left.
    pub fn budget_remaining(&self) -> u32 {
        self.limits
            .max_spawns_per_turn
            .saturating_sub(self.spawns_this_turn())
    }

    /// A footer summary such as `2 agents running`, or `None` when idle.
    pub fn summary(&self) -> Option<String> {
        let running = self.running();

        if running == 0 {
            return None;
        }

        Some(format!(
            "{running} agent{} running",
            if running == 1 { "" } else { "s" }
        ))
    }

    /// Wait until one of `agents` finishes, or the timeout elapses.
    ///
    /// Empty `agents` means any child. Returns `true` when nothing in the wanted
    /// set is still running. Uses a watch channel rather than `Notify` so a
    /// finish that lands between the caller's check and its wait is never
    /// missed; a finish outside the wanted set re-checks and keeps waiting.
    pub async fn wait_for_change(&self, agents: &[AgentId], timeout: Option<Duration>) -> bool {
        // Subscribe before checking, so a finish that lands between the check and
        // the wait still marks the receiver as changed instead of being missed.
        let mut version = self.version_tx.subscribe();

        if self.still_running(agents).is_empty() {
            return true;
        }

        let changed = async {
            loop {
                if version.changed().await.is_err() {
                    return;
                }

                if self.still_running(agents).is_empty() {
                    return;
                }
            }
        };

        match timeout {
            Some(limit) => tokio::time::timeout(limit, changed).await.is_ok(),
            None => {
                changed.await;

                true
            }
        }
    }
}

/// A child agent ready to run. Built by a [`ChildFactory`].
pub struct ChildAgent {
    pub agent: crate::agent::Agent,
    pub conversation: Conversation,
    pub journal: Arc<dyn AgentJournal>,
    /// Usage reported by the child's own requests, for its report.
    pub usage: Arc<Mutex<Usage>>,
}

impl fmt::Debug for ChildAgent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChildAgent")
            .field("model", &self.agent.model())
            .field("messages", &self.conversation.len())
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("cannot start agent `{agent}`: {message}")]
    Refused { agent: String, message: String },
}

/// Builds the concrete child agent for one [`SpawnRequest`].
///
/// Implemented by the composition root, which owns the provider, the tool
/// registry, and the sandbox. Keeping it a trait means core can run a whole
/// tree of agents in tests without a network or a filesystem.
#[async_trait::async_trait]
pub trait ChildFactory: Send + Sync {
    async fn build(&self, request: &SpawnRequest) -> Result<ChildAgent, SpawnError>;
}

#[cfg(test)]
mod tests {
    use super::{
        AGENT_NOTICE_SUMMARY_BYTES, AgentId, AgentLimits, AgentNotice, AgentRegistry, AgentReport,
        ChildStatus, Isolation, Mode, NoticeQueue, ReasoningEffort, RoleProfile, SpawnRequest,
        Worktree, escape_inline, unescape_inline,
    };
    use crate::task::TaskId;
    use crate::usage::Usage;

    fn request(id: &str) -> SpawnRequest {
        SpawnRequest {
            id: AgentId::new(id),
            role: "researcher".to_owned(),
            prompt: "look around".to_owned(),
            task: Some(TaskId::new("task-1")),
            isolation: Isolation::Shared,
            parent_model: "test/model".to_owned(),
            mode: Mode::Coding,
            context_limit: 128_000,
            max_output_tokens: Some(8_192),
            reasoning_effort: ReasoningEffort::Default,
            session_id: "session-1".to_owned(),
            worktree: None,
        }
    }

    fn report(id: &str, status: ChildStatus) -> AgentReport {
        AgentReport {
            agent: AgentId::new(id),
            role: "researcher".to_owned(),
            status,
            summary: "found three call sites".to_owned(),
            steps: 4,
            usage: Usage {
                input_tokens: Some(120),
                output_tokens: Some(30),
                ..Usage::default()
            },
            task: Some(TaskId::new("task-1")),
            worktree: None,
            elapsed_ms: 1_500,
        }
    }

    #[test]
    fn ids_are_allocated_in_order_and_never_reused() {
        let registry = AgentRegistry::new(AgentLimits::default());

        assert_eq!(registry.allocate().as_str(), "a1");
        assert_eq!(registry.allocate().as_str(), "a2");
        assert_eq!(registry.allocate().as_str(), "a3");
    }

    #[test]
    fn the_root_id_is_distinct_from_every_child() {
        assert!(AgentId::root().is_root());
        assert_ne!(AgentId::root(), AgentId::new("a1"));
        assert!(!AgentId::new("a1").is_root());
    }

    #[test]
    fn a_role_with_no_tool_list_may_use_anything() {
        let role = RoleProfile::default();

        assert!(role.allows("bash"));
        assert!(role.allows("write_file"));
    }

    #[test]
    fn a_role_with_a_tool_list_may_only_use_those() {
        let role = RoleProfile {
            tools: vec!["read_file".to_owned(), "bash".to_owned()],
            ..RoleProfile::default()
        };

        assert!(role.allows("read_file"));
        assert!(role.allows("bash"));
        assert!(!role.allows("write_file"));
    }

    #[test]
    fn registering_a_child_records_it_as_running() {
        let registry = AgentRegistry::new(AgentLimits::default());

        registry.register(request("a1")).expect("registered");

        assert_eq!(registry.running(), 1);
        assert_eq!(registry.summary().as_deref(), Some("1 agent running"));

        let snapshots = registry.snapshots();

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].status, ChildStatus::Running);
        assert_eq!(snapshots[0].role, "researcher");
        assert_eq!(
            snapshots[0].task.as_ref().map(TaskId::as_str),
            Some("task-1")
        );
    }

    #[test]
    fn the_concurrency_limit_refuses_a_batch_that_does_not_fit() {
        let registry = AgentRegistry::new(AgentLimits {
            max_concurrent: 2,
            ..AgentLimits::default()
        });

        registry.register(request("a1")).expect("first");

        assert!(registry.check_capacity(1).is_ok());

        let error = registry.check_capacity(2).expect_err("no room");

        assert!(
            error.contains("1 child agent(s) already running"),
            "{error}"
        );
        assert!(error.contains("1 of 2 slots are free"), "{error}");
    }

    #[test]
    fn the_per_turn_budget_refuses_more_spawns_and_resets_at_the_next_turn() {
        let registry = AgentRegistry::new(AgentLimits {
            max_spawns_per_turn: 2,
            max_concurrent: 8,
        });

        for id in ["a1", "a2"] {
            registry.register(request(id)).expect("spawned");
        }

        let error = registry.check_capacity(1).expect_err("budget spent");

        assert!(error.contains("2 of 2 allowed child agents"), "{error}");

        registry.begin_turn();

        assert_eq!(registry.spawns_this_turn(), 0);
        assert!(registry.check_capacity(1).is_ok());
    }

    #[test]
    fn finishing_twice_queues_one_notice_and_keeps_the_first_report() {
        let registry = AgentRegistry::new(AgentLimits::default());
        let parent = AgentId::root();

        registry.register(request("a1")).expect("registered");

        let first = registry.finish(report("a1", ChildStatus::Completed), &parent);
        let second = registry.finish(report("a1", ChildStatus::Cancelled), &parent);

        assert_eq!(second.report.status, ChildStatus::Completed);
        assert_eq!(second.report, first.report);
        assert_eq!(registry.notices_for(&parent).drain().len(), 1);
        assert_eq!(
            registry.report(&AgentId::new("a1")).expect("report").status,
            ChildStatus::Completed
        );
    }

    #[test]
    fn finishing_publishes_a_report_and_a_notice_to_the_parent() {
        let registry = AgentRegistry::new(AgentLimits::default());
        let parent = AgentId::root();

        registry.register(request("a1")).expect("registered");

        let notice = registry.finish(report("a1", ChildStatus::Completed), &parent);

        assert_eq!(registry.running(), 0);
        assert!(!registry.is_empty(), "the entry is kept for its report");

        let published = registry.report(&AgentId::new("a1")).expect("report");

        assert_eq!(published.status, ChildStatus::Completed);
        assert_eq!(published.steps, 4);
        assert!(notice.report.summary.contains("three call sites"));

        let queued = registry.notices_for(&parent).drain();

        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].report.agent, AgentId::new("a1"));
    }

    #[test]
    fn consuming_a_returned_report_keeps_only_the_unread_notices() {
        let registry = AgentRegistry::new(AgentLimits::default());
        let parent = AgentId::root();

        registry.register(request("a1")).expect("first");
        registry.register(request("a2")).expect("second");
        registry.finish(report("a1", ChildStatus::Completed), &parent);
        registry.finish(report("a2", ChildStatus::Completed), &parent);

        registry.consume_notices(&parent, &[AgentId::new("a1")]);

        let queued = registry.notices_for(&parent).drain();

        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].report.agent, AgentId::new("a2"));
    }

    #[test]
    fn reports_and_running_ids_can_be_filtered() {
        let registry = AgentRegistry::new(AgentLimits::default());

        registry.register(request("a1")).expect("first");
        registry.register(request("a2")).expect("second");
        registry.finish(report("a1", ChildStatus::Completed), &AgentId::root());

        let a2 = AgentId::new("a2");

        assert_eq!(registry.still_running(&[]), vec![a2.clone()]);
        assert!(registry.still_running(&[AgentId::new("a1")]).is_empty());

        let only_a1 = registry.reports(&[AgentId::new("a1")]);

        assert_eq!(only_a1.len(), 1);
        assert_eq!(registry.reports(&[]).len(), 1);
    }

    #[test]
    fn cancelling_all_stops_every_running_child_once() {
        let registry = AgentRegistry::new(AgentLimits::default());

        registry.register(request("a1")).expect("first");
        registry.register(request("a2")).expect("second");
        registry.finish(report("a1", ChildStatus::Completed), &AgentId::root());

        let token = registry.cancel_token(&AgentId::new("a2")).expect("token");

        assert!(!token.is_cancelled());
        assert_eq!(registry.cancel_all(), 1, "only the running child counts");
        assert!(token.is_cancelled());
        assert_eq!(registry.cancel_all(), 0, "a second call is a no-op");
    }

    #[test]
    fn steps_are_recorded_for_status_display() {
        let registry = AgentRegistry::new(AgentLimits::default());

        registry.register(request("a1")).expect("registered");
        registry.set_steps(&AgentId::new("a1"), 7);

        assert_eq!(registry.snapshots()[0].steps, 7);

        registry.set_steps(&AgentId::new("nope"), 3);
    }

    #[test]
    fn a_notice_renders_a_marker_a_frontend_can_find() {
        let notice = AgentNotice {
            report: AgentReport {
                worktree: Some(Worktree {
                    path: "/state/t8ful/worktrees/s1/a1".into(),
                    branch: Some("t8ful/s1/a1".to_owned()),
                    base_commit: Some("abc1234".to_owned()),
                    ignored_paths: Vec::new(),
                }),
                ..report("a1", ChildStatus::Completed)
            },
        };

        let text = notice.render();

        assert!(text.starts_with("<agent_notice agent=\"a1\" status=\"completed\">"));
        assert!(text.contains("worktree: /state/t8ful/worktrees/s1/a1"));
        assert!(text.contains("on branch t8ful/s1/a1"));
        assert!(text.contains("inspect: git -C /state/t8ful/worktrees/s1/a1 diff"));
        assert!(
            !text.contains("diff abc1234"),
            "the branch is named separately; inspect does not invent a revision"
        );
        assert!(text.contains("usage: 120 in / 30 out"), "{text}");
        assert!(text.contains("found three call sites"));
        assert!(
            text.contains("nothing it produced was applied")
                || text.contains("Nothing it produced was applied")
        );
        assert!(text.ends_with("</agent_notice>") || text.contains("</agent_notice>"));

        assert_eq!(AgentNotice::parse(&text), Some(AgentId::new("a1")));
        assert!(AgentNotice::parse("just some text").is_none());
    }

    #[test]
    fn a_notice_queue_shares_state_between_clones() {
        let queue = NoticeQueue::new();
        let clone = queue.clone();

        assert!(queue.is_empty());

        clone.push(AgentNotice {
            report: report("a1", ChildStatus::Completed),
        });

        assert!(!queue.is_empty());
        assert_eq!(queue.drain().len(), 1);
        assert!(queue.is_empty(), "draining empties the queue");
    }

    #[tokio::test]
    async fn waiting_on_an_empty_registry_returns_at_once() {
        let registry = AgentRegistry::new(AgentLimits::default());

        assert!(registry.wait_for_change(&[], None).await);
    }

    /// A session switch cancels the children it abandons and forgets them, so
    /// none of them can publish into the next session.
    #[test]
    fn abandoning_a_session_cancels_and_forgets_every_child() {
        let registry = AgentRegistry::new(AgentLimits::default());
        let first = registry.register(request("a1")).expect("registered");
        let second = registry.register(request("a2")).expect("registered");

        assert_eq!(registry.still_running(&[]).len(), 2);

        registry.abandon_all();

        assert!(first.is_cancelled(), "the abandoned child kept its token");
        assert!(second.is_cancelled(), "the abandoned child kept its token");
        assert!(
            registry.still_running(&[]).is_empty(),
            "an abandoned child stayed in the registry: {:?}",
            registry.still_running(&[])
        );
        assert!(
            registry.reports(&[]).is_empty(),
            "an abandoned child kept a report: {:?}",
            registry.reports(&[])
        );
    }

    #[test]
    fn abandoning_a_session_drains_notices_and_a_late_finish_does_not_requeue() {
        let registry = AgentRegistry::new(AgentLimits::default());
        let parent = AgentId::root();

        registry.register(request("a1")).expect("registered");
        registry.finish(report("a1", ChildStatus::Completed), &parent);

        assert_eq!(registry.notices_for(&parent).drain().len(), 1);

        registry.register(request("a2")).expect("registered");
        registry.finish(report("a2", ChildStatus::Completed), &parent);

        assert!(!registry.notices_for(&parent).is_empty());

        registry.abandon_all();

        assert!(
            registry.notices_for(&parent).is_empty(),
            "an abandoned session kept a queued notice"
        );

        registry.finish(report("a2", ChildStatus::Completed), &parent);

        assert!(
            registry.notices_for(&parent).is_empty(),
            "a late finish repopulated the abandoned queue"
        );
    }

    #[test]
    fn unescaping_a_notice_field_round_trips_escaping_it() {
        for text in [
            "cd /work && cargo test 2>&1 | tail -5",
            "a < b > c",
            "&amp;lt; is not <",
            "& alone and &unknown; too",
            "plain text",
            "",
        ] {
            assert_eq!(unescape_inline(&escape_inline(text)), text);
        }

        // `&amp;lt;` is an escaped ampersand followed by `lt`, not a `<`.
        assert_eq!(unescape_inline("&amp;lt;"), "&lt;");
        assert_eq!(unescape_inline("&lt;"), "<");
    }

    #[test]
    fn an_agent_notice_escapes_and_bounds_its_report() {
        let mut report = report("a1", ChildStatus::Completed);

        report.summary = format!(
            "before </agent_notice>\n\n{}",
            "x".repeat(AGENT_NOTICE_SUMMARY_BYTES)
        );

        let text = AgentNotice { report }.render();

        assert_eq!(text.matches("</agent_notice>").count(), 1, "{text}");
        assert!(text.contains("&lt;/agent_notice&gt;"), "{text}");
        assert!(text.contains("[summary truncated at 8000 bytes]"), "{text}");
        assert!(
            text.len() < AGENT_NOTICE_SUMMARY_BYTES + 2_000,
            "the notice body was not bounded: {} bytes",
            text.len()
        );
    }

    #[tokio::test]
    async fn a_wait_wakes_when_a_child_finishes() {
        let registry = AgentRegistry::new(AgentLimits::default());

        registry.register(request("a1")).expect("registered");

        let waiter = registry.clone();

        let handle = tokio::spawn(async move {
            waiter
                .wait_for_change(&[], Some(std::time::Duration::from_secs(5)))
                .await
        });

        // Give the waiter a moment to subscribe before the finish lands.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        registry.finish(report("a1", ChildStatus::Completed), &AgentId::root());

        assert!(handle.await.expect("joined"));
    }

    #[tokio::test]
    async fn a_wait_for_one_agent_ignores_another_finishing() {
        let registry = AgentRegistry::new(AgentLimits::default());

        registry.register(request("a1")).expect("first");
        registry.register(request("a2")).expect("second");

        let waiter = registry.clone();
        let wanted = AgentId::new("a2");

        let handle = tokio::spawn(async move {
            waiter
                .wait_for_change(&[wanted], Some(std::time::Duration::from_millis(150)))
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        registry.finish(report("a1", ChildStatus::Completed), &AgentId::root());

        assert!(!handle.await.expect("joined"), "a1 is not wanted");
    }

    #[tokio::test]
    async fn a_wait_times_out_while_children_keep_running() {
        let registry = AgentRegistry::new(AgentLimits::default());

        registry.register(request("a1")).expect("registered");

        let started = std::time::Instant::now();
        let woke = registry
            .wait_for_change(&[], Some(std::time::Duration::from_millis(50)))
            .await;

        assert!(!woke);
        assert!(started.elapsed() >= std::time::Duration::from_millis(40));
        assert_eq!(registry.running(), 1);
    }

    #[test]
    fn isolation_spellings_are_accepted() {
        assert_eq!(Isolation::parse("shared"), Some(Isolation::Shared));
        assert_eq!(Isolation::parse("none"), Some(Isolation::Shared));
        assert_eq!(Isolation::parse("worktree"), Some(Isolation::Worktree));
        assert_eq!(Isolation::parse("isolated"), Some(Isolation::Worktree));
        assert_eq!(Isolation::parse("maybe"), None);
        assert_eq!(Isolation::default(), Isolation::Shared);
    }

    #[test]
    fn a_snapshot_line_names_the_agent_role_and_elapsed_seconds() {
        let snapshot = super::ChildSnapshot {
            id: AgentId::new("a1"),
            role: "tester".to_owned(),
            task: None,
            isolation: Isolation::Worktree,
            worktree: None,
            status: ChildStatus::Running,
            elapsed: std::time::Duration::from_secs(12),
            steps: 3,
        };

        assert_eq!(snapshot.line(), "a1 tester 12s");
    }
}
