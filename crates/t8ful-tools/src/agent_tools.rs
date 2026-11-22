//! Tools that start and inspect child agents.
//!
//! Spawning is non-blocking: `spawn_agents` validates the batch, starts each
//! child as an independent task, and returns immediately with the ids. The parent
//! keeps working. When a child stops, its report is pushed onto the parent's
//! notice queue and also becomes readable through `agent_status` and
//! `agent_wait`.
//!
//! The tools never construct an agent themselves. They call an [`AgentSpawner`],
//! which the composition root implements because it owns the provider, the tool
//! registry, and the sandbox.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use t8ful_core::subagent::{
    AgentId, AgentRegistry, AgentReport, ChildStatus, Isolation, RoleProfile, SpawnRequest,
    Worktree,
};
use t8ful_core::task::TaskId;
use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec};
use t8ful_core::worktree::Worktrees;

/// Starts one child. Implemented by the composition root.
///
/// `start` must return as soon as the child is registered and its task is
/// running; it must not wait for the child to finish.
#[async_trait]
pub trait AgentSpawner: Send + Sync {
    /// The profile for a role name, or `None` when the role is unknown.
    fn role(&self, name: &str) -> Option<RoleProfile>;

    /// The registry every child in this session belongs to.
    fn registry(&self) -> AgentRegistry;

    /// The worktree source, or `None` when isolation cannot be provided.
    fn worktrees(&self) -> Arc<dyn Worktrees>;

    /// Start one child from an already validated request.
    async fn start(&self, request: SpawnRequest) -> Result<(), String>;

    /// Record a task for a child and return its id.
    ///
    /// Separate from the task tools because a spawn creates its task with the
    /// role and prompt already known.
    fn task_for(&self, title: &str) -> Option<TaskId>;

    /// Whether an existing task id is present in the session graph.
    fn task_exists(&self, id: &TaskId) -> bool;

    /// Mark a task cancelled after its spawn or worktree could not start.
    fn cancel_task(&self, id: &TaskId);
}

#[derive(Debug, Deserialize)]
struct SpawnArgs {
    agents: Vec<SpawnSpec>,
}

#[derive(Debug, Deserialize)]
struct SpawnSpec {
    #[serde(default = "default_role")]
    role: String,
    prompt: String,
    #[serde(default)]
    isolation: Option<String>,
    /// An existing task to work on instead of creating one.
    #[serde(default)]
    task: Option<String>,
}

fn default_role() -> String {
    "general".to_owned()
}

/// Start one or more child agents without waiting for them.
pub struct SpawnAgentsTool {
    spawner: Arc<dyn AgentSpawner>,
}

impl std::fmt::Debug for SpawnAgentsTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnAgentsTool").finish_non_exhaustive()
    }
}

impl SpawnAgentsTool {
    pub const MAX_BATCH: usize = 4;

    pub fn new(spawner: Arc<dyn AgentSpawner>) -> Self {
        Self { spawner }
    }
}

#[async_trait]
impl Tool for SpawnAgentsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "spawn_agents".to_owned(),
            description: "Start one or more child agents to work in parallel. This returns \
                          immediately with their ids; it does not wait for them. Each child has its \
                          own context, so verbose work it does never enters your conversation. When \
                          a child stops, you receive a notice with its report. Use agent_status to \
                          see what they are doing and agent_wait to block until they finish. \
                          Use isolation \"worktree\" for a child that writes files, so it cannot \
                          disturb your working tree; its work stays in its own worktree and branch \
                          and is never merged for you. Use \"shared\" for a child that only reads or \
                          runs tests."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agents": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": SpawnAgentsTool::MAX_BATCH,
                        "description": "The children to start, in order",
                        "items": {
                            "type": "object",
                            "properties": {
                                "role": {
                                    "type": "string",
                                    "description": "Configured role name, such as researcher or implementer. Defaults to general."
                                },
                                "prompt": {
                                    "type": "string",
                                    "description": "The complete task for the child. It does not see your conversation, so inline everything it needs."
                                },
                                "isolation": {
                                    "type": "string",
                                    "enum": ["shared", "worktree"],
                                    "description": "shared (default) edits the real tree; worktree gives the child its own checkout based on HEAD"
                                },
                                "task": {
                                    "type": "string",
                                    "description": "An existing task id for this child instead of creating one"
                                }
                            },
                            "required": ["prompt"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["agents"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Control
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: SpawnArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;

        if args.agents.is_empty() {
            return Err(ToolError::InvalidArguments(
                "spawn_agents needs at least one agent".to_owned(),
            ));
        }

        if args.agents.len() > Self::MAX_BATCH {
            return Err(ToolError::InvalidArguments(format!(
                "spawn_agents accepts at most {} agents at once; {} were given",
                Self::MAX_BATCH,
                args.agents.len()
            )));
        }

        // Validate the whole batch before any side effect: a refused spec must
        // not leave a worktree, a task, or a half-started batch behind.
        let mut prepared = Vec::with_capacity(args.agents.len());

        for spec in &args.agents {
            if spec.prompt.trim().is_empty() {
                return Err(ToolError::InvalidArguments(
                    "every spawned agent needs a prompt".to_owned(),
                ));
            }

            let role = self.spawner.role(&spec.role).ok_or_else(|| {
                ToolError::InvalidArguments(format!(
                    "unknown role `{}`; configure it under [agents.roles.{}] or omit role to \
                     use `general`",
                    spec.role, spec.role
                ))
            })?;

            let isolation = match spec.isolation.as_deref() {
                Some(raw) => Isolation::parse(raw).ok_or_else(|| {
                    ToolError::InvalidArguments(format!(
                        "`{raw}` is not an isolation mode; use shared or worktree"
                    ))
                })?,
                None => role.isolation.unwrap_or_default(),
            };

            if isolation == Isolation::Worktree && ctx.mode == t8ful_core::policy::Mode::Plan {
                return Err(ToolError::Failed(format!(
                    "plan mode cannot create a worktree for `{}`: `git worktree add` mutates the \
                     repository; use isolation \"shared\" or leave plan mode",
                    spec.role
                )));
            }

            let task = match &spec.task {
                Some(raw) => {
                    let id = TaskId::new(raw.trim());

                    if id.as_str().is_empty() {
                        return Err(ToolError::InvalidArguments(
                            "an existing task id cannot be empty".to_owned(),
                        ));
                    }

                    if !self.spawner.task_exists(&id) {
                        return Err(ToolError::Failed(format!(
                            "unknown task `{id}`; create it with task_create or omit `task` to \
                             create one"
                        )));
                    }

                    Some(id)
                }
                None => None,
            };

            prepared.push((role, isolation, task));
        }

        let registry = self.spawner.registry();

        // Capacity is checked for the whole batch first, so a batch that cannot
        // fit starts nothing instead of half-starting.
        registry
            .check_capacity(prepared.len())
            .map_err(ToolError::Failed)?;

        let mut started: Vec<(AgentId, Option<TaskId>, Option<Worktree>, Isolation, String)> =
            Vec::new();
        let mut failure = None;

        for (spec, (role, isolation, existing_task)) in args.agents.iter().zip(prepared) {
            let id = registry.allocate();
            let worktree = match isolation {
                Isolation::Shared => None,
                Isolation::Worktree => match self.spawner.worktrees().create(&id).await {
                    Ok(tree) => Some(tree),
                    Err(error) => {
                        failure = Some(format!("could not start {id}: {error}"));
                        break;
                    }
                },
            };
            let task = match existing_task {
                Some(task) => Some(task),
                None => self.spawner.task_for(&task_title(&spec.prompt)),
            };
            // The dispatch context carries the parent's live settings, so a
            // child starts with the mode, model, window, and reasoning level the
            // parent has now. A role's own model still overrides the model.
            let request = SpawnRequest {
                id: id.clone(),
                role: spec.role.clone(),
                prompt: spec.prompt.clone(),
                task: task.clone(),
                isolation,
                parent_model: role.model.clone().unwrap_or_else(|| ctx.model.clone()),
                mode: ctx.mode,
                context_limit: ctx
                    .context_limit
                    .unwrap_or(t8ful_core::DEFAULT_CONTEXT_LIMIT_FALLBACK),
                max_output_tokens: ctx.max_output_tokens,
                reasoning_effort: ctx.reasoning_effort,
                // The spawner pins the session before the child task starts; a
                // tool has no view of the session binding.
                session_id: String::new(),
                worktree: worktree.clone(),
            };

            if let Err(message) = self.spawner.start(request).await {
                if let Some(task) = &task {
                    self.spawner.cancel_task(task);
                }

                failure = Some(format!("could not start {id}: {message}"));
                break;
            }

            started.push((id, task, worktree, isolation, spec.role.clone()));
        }

        if started.is_empty()
            && let Some(failure) = failure
        {
            return Err(ToolError::Failed(failure));
        }

        let lines = started
            .iter()
            .map(|(id, task, worktree, isolation, role)| {
                let mut line = format!(
                    "{id} started: role {role}, isolation {}",
                    isolation.as_str()
                );

                if let Some(task) = task {
                    line.push_str(&format!(", task {task}"));
                } else {
                    line.push_str(", no task");
                }

                if let Some(worktree) = worktree {
                    if worktree.branch.is_some() {
                        line.push_str(&format!(
                            ", worktree {} on branch {}",
                            worktree.path.display(),
                            worktree.branch.as_deref().unwrap_or_default()
                        ));
                    }

                    if !worktree.ignored_paths.is_empty() {
                        line.push_str(&format!(
                            ", ignored paths not copied: {}",
                            summarize(&worktree.ignored_paths, 5)
                        ));
                    }
                }

                line
            })
            .collect::<Vec<_>>()
            .join("\n");
        let failure = failure
            .map(|failure| {
                format!("\n\n{failure}\nThe batch stopped after the agents above started.")
            })
            .unwrap_or_default();

        let ids = started
            .iter()
            .map(|(id, _, _, _, _)| id.as_str().to_owned())
            .collect::<Vec<_>>();

        Ok(ToolOutput {
            content: format!(
                "{lines}{failure}\n\nThey are working now. Keep working, or use agent_wait to \
                 block until some finish. Their reports arrive as notices when they stop."
            ),
            truncated: false,
            metadata: Some(json!({ "agents": ids })),
            images: Vec::new(),
        })
    }
}

/// A bounded list of path names for one report line.
fn summarize(paths: &[String], limit: usize) -> String {
    let shown = paths
        .iter()
        .take(limit)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let more = paths.len().saturating_sub(limit);

    match more {
        0 => shown,
        extra => format!("{shown} and {extra} more"),
    }
}

/// The first line of a prompt, short enough to be a task title.
fn task_title(prompt: &str) -> String {
    let first = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("delegated work");

    if first.chars().count() <= 80 {
        return first.to_owned();
    }

    let cut = first.chars().take(79).collect::<String>();

    format!("{cut}…")
}

#[derive(Debug, Deserialize)]
struct AgentsArgs {
    #[serde(default)]
    agents: Vec<String>,
    /// Milliseconds to wait.
    ///
    /// Absent uses a bounded default; zero waits indefinitely. An unbounded
    /// default would leave a turn with no way back except cancelling it.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn render_report(report: &AgentReport) -> String {
    report.render()
}

/// Block until the named children stop.
pub struct AgentWaitTool {
    spawner: Arc<dyn AgentSpawner>,
}

impl std::fmt::Debug for AgentWaitTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentWaitTool").finish_non_exhaustive()
    }
}

impl AgentWaitTool {
    /// How long an unspecified wait lasts before reporting progress.
    ///
    /// Bounded on purpose: a wait with no deadline is a turn that cannot end
    /// except by cancellation, which is the worst state to put a person in.
    pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

    pub fn new(spawner: Arc<dyn AgentSpawner>) -> Self {
        Self { spawner }
    }
}

#[async_trait]
impl Tool for AgentWaitTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agent_wait".to_owned(),
            description: "Wait until child agents stop, then return their reports. Use it when \
                          your next step depends on what they produced. Waiting for children that \
                          are already done returns immediately. A timeout returns whatever finished \
                          plus which agents are still running, so you can go back to your own work."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agents": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Agent ids to wait for. Omit to wait for every child."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Give up after this many milliseconds and report what is still running. Defaults to 120000; 0 waits indefinitely"
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, call: &ToolCall, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: AgentsArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let registry = self.spawner.registry();
        let wanted = args
            .agents
            .iter()
            .map(|id| AgentId::new(id.trim()))
            .collect::<Vec<_>>();

        for id in &wanted {
            if registry.report(id).is_none() && registry.cancel_token(id).is_none() {
                return Err(ToolError::Failed(format!("unknown agent `{id}`")));
            }
        }

        let timeout = match args.timeout_ms {
            Some(0) => None,
            Some(ms) => Some(std::time::Duration::from_millis(ms)),
            None => Some(AgentWaitTool::DEFAULT_TIMEOUT),
        };

        let deadline = timeout.map(|limit| std::time::Instant::now() + limit);

        loop {
            if registry.still_running(&wanted).is_empty() {
                break;
            }

            let remaining = match deadline {
                Some(deadline) => {
                    let now = std::time::Instant::now();

                    if now >= deadline {
                        break;
                    }

                    Some(deadline - now)
                }
                None => None,
            };

            if !registry.wait_for_change(&wanted, remaining).await {
                break;
            }
        }

        let reports = registry.reports(&wanted);
        let running = registry.still_running(&wanted);

        // The reports below are the model's copy. Drop their notices so the
        // same content cannot also start a paid turn at the next boundary.
        let returned = reports
            .iter()
            .map(|report| report.agent.clone())
            .collect::<Vec<_>>();

        registry.consume_notices(&AgentId::root(), &returned);

        if reports.is_empty() && running.is_empty() {
            return Ok(ToolOutput::text(match args.agents.is_empty() {
                true => "no child agents have been started in this session",
                false => "those agents have not produced a report",
            }));
        }

        let mut sections = reports.iter().map(render_report).collect::<Vec<_>>();

        if !running.is_empty() {
            sections.push(format!(
                "still running: {}",
                running
                    .iter()
                    .map(AgentId::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        let truncated = sections.iter().any(|section| section.len() > 8_000);

        Ok(ToolOutput {
            content: sections.join("\n\n"),
            truncated,
            metadata: Some(json!({
                "finished": reports.len(),
                "running": running.iter().map(AgentId::as_str).collect::<Vec<_>>(),
            })),
            images: Vec::new(),
        })
    }
}

/// Report what children are doing without waiting.
pub struct AgentStatusTool {
    spawner: Arc<dyn AgentSpawner>,
}

impl std::fmt::Debug for AgentStatusTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentStatusTool").finish_non_exhaustive()
    }
}

impl AgentStatusTool {
    pub fn new(spawner: Arc<dyn AgentSpawner>) -> Self {
        Self { spawner }
    }
}

#[async_trait]
impl Tool for AgentStatusTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "agent_status".to_owned(),
            description:
                "Report what child agents are doing without waiting: state, role, elapsed \
                          time, steps taken, and worktree. Finished agents keep their last report, \
                          so this is also how you re-read a report you already dismissed."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agents": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Agent ids to report on. Omit to report on every child."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, call: &ToolCall, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: AgentsArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let registry = self.spawner.registry();
        let wanted = args
            .agents
            .iter()
            .map(|id| AgentId::new(id.trim()))
            .collect::<Vec<_>>();

        for id in &wanted {
            if registry.report(id).is_none() && registry.cancel_token(id).is_none() {
                return Err(ToolError::Failed(format!("unknown agent `{id}`")));
            }
        }

        let snapshots = registry.snapshots_of(&wanted);

        if snapshots.is_empty() {
            return Ok(ToolOutput::text(
                "no child agents have been started in this session",
            ));
        }

        let content = snapshots
            .iter()
            .map(|snapshot| {
                let mut line = format!(
                    "{} {} {} {:.1}s {} step{}{}",
                    snapshot.id,
                    snapshot.status.as_str(),
                    snapshot.role,
                    snapshot.elapsed.as_secs_f64(),
                    snapshot.steps,
                    if snapshot.steps == 1 { "" } else { "s" },
                    snapshot
                        .task
                        .as_ref()
                        .map(|task| format!(" task {task}"))
                        .unwrap_or_default(),
                );

                if let Some(worktree) = &snapshot.worktree {
                    line.push_str(&format!(
                        " worktree {} ({})",
                        worktree.path.display(),
                        worktree.branch.as_deref().unwrap_or("detached")
                    ));
                }

                line
            })
            .collect::<Vec<_>>()
            .join("\n");

        // A finished child's last steps and status are the useful part; the
        // report itself stays available through agent_wait.
        let content = match snapshots
            .iter()
            .find(|snapshot| snapshot.status == ChildStatus::Completed)
        {
            Some(_) => format!("{content}\n\nuse agent_wait to read the finished reports"),
            None => content,
        };

        Ok(ToolOutput {
            content,
            truncated: false,
            metadata: Some(json!({
                "running": registry.running(),
            })),
            images: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use t8ful_core::subagent::{
        AgentId, AgentLimits, AgentRegistry, AgentReport, ChildStatus, Isolation, RoleProfile,
        SpawnRequest, Worktree,
    };
    use t8ful_core::task::{TaskGraph, TaskId};
    use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolError};
    use t8ful_core::usage::Usage;
    use t8ful_core::worktree::{NoWorktrees, WorktreeError, Worktrees};

    use super::{AgentSpawner, AgentStatusTool, AgentWaitTool, SpawnAgentsTool, task_title};
    use crate::task_tools::{NullTaskSink, SharedTasks};

    struct FakeSpawner {
        registry: AgentRegistry,
        tasks: Arc<SharedTasks>,
        roles: Vec<String>,
        worktrees: Arc<dyn Worktrees>,
        next_task: std::sync::Mutex<u64>,
        started: std::sync::Mutex<Vec<SpawnRequest>>,
        /// Allow this many successful starts, then refuse; `None` never refuses.
        fail_after: std::sync::Mutex<Option<usize>>,
    }

    impl std::fmt::Debug for FakeSpawner {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FakeSpawner").finish_non_exhaustive()
        }
    }

    impl FakeSpawner {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                registry: AgentRegistry::new(AgentLimits::default()),
                tasks: Arc::new(SharedTasks::new(TaskGraph::new(), Arc::new(NullTaskSink))),
                roles: vec!["general".to_owned(), "researcher".to_owned()],
                worktrees: Arc::new(NoWorktrees),
                next_task: std::sync::Mutex::new(0),
                started: std::sync::Mutex::new(Vec::new()),
                fail_after: std::sync::Mutex::new(None),
            })
        }

        fn started(&self) -> Vec<SpawnRequest> {
            self.started
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone()
        }

        fn fail_start_after(&self, count: usize) {
            *self
                .fail_after
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(count);
        }

        fn finish_first(&self, status: ChildStatus) {
            let request = self.started().into_iter().next().expect("one started");

            self.registry.finish(
                AgentReport {
                    agent: request.id.clone(),
                    role: request.role.clone(),
                    status,
                    summary: "read the parser and found three call sites".to_owned(),
                    steps: 5,
                    usage: Usage::default(),
                    task: request.task.clone(),
                    worktree: request.worktree.clone(),
                    elapsed_ms: 900,
                },
                &AgentId::root(),
            );
        }
    }

    #[async_trait::async_trait]
    impl AgentSpawner for FakeSpawner {
        fn role(&self, name: &str) -> Option<RoleProfile> {
            self.roles
                .iter()
                .any(|role| role == name)
                .then(RoleProfile::default)
        }

        fn registry(&self) -> AgentRegistry {
            self.registry.clone()
        }

        fn worktrees(&self) -> Arc<dyn Worktrees> {
            Arc::clone(&self.worktrees)
        }

        async fn start(&self, request: SpawnRequest) -> Result<(), String> {
            let started = self
                .started
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let limit = *self
                .fail_after
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());

            if limit.is_some_and(|limit| started.len() >= limit) {
                return Err("refused by the test spawner".to_owned());
            }

            drop(started);
            self.registry.register(request.clone())?;
            self.started
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(request);

            Ok(())
        }

        fn task_exists(&self, id: &TaskId) -> bool {
            self.tasks.graph().get(id).is_some()
        }

        fn cancel_task(&self, id: &TaskId) {
            let mut graph = self.tasks.graph();

            let _ = graph.set_state(id, t8ful_core::task::TaskState::Cancelled);
        }

        fn task_for(&self, title: &str) -> Option<TaskId> {
            let mut next = self
                .next_task
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());

            *next += 1;

            self.tasks.graph().create(title, []).ok().filter(|_| true)
        }
    }

    fn context() -> ToolContext {
        ToolContext::new("/tmp".into(), "/tmp".into())
    }

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall::new("c1", "spawn_agents", arguments)
    }

    #[tokio::test]
    async fn spawning_returns_immediately_with_ids() {
        let spawner = FakeSpawner::new();
        let tool = SpawnAgentsTool::new(spawner.clone());

        let output = tool
            .execute(
                &call(json!({ "agents": [{ "prompt": "read the parser" }] })),
                &context(),
            )
            .await
            .expect("spawned");

        assert!(output.content.contains("a1 started"), "{}", output.content);
        assert!(
            output.content.contains("role general"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("isolation shared"),
            "{}",
            output.content
        );
        assert_eq!(
            spawner.registry.running(),
            1,
            "the tool did not wait for the child"
        );
        assert_eq!(spawner.started().len(), 1);
    }

    #[tokio::test]
    async fn a_batch_of_children_starts_together() {
        let spawner = FakeSpawner::new();
        let tool = SpawnAgentsTool::new(spawner.clone());

        tool.execute(
            &call(json!({
                "agents": [
                    { "prompt": "first" },
                    { "prompt": "second", "role": "researcher" },
                ]
            })),
            &context(),
        )
        .await
        .expect("spawned");

        assert_eq!(spawner.registry.running(), 2);

        let started = spawner.started();

        assert_eq!(started[0].id.as_str(), "a1");
        assert_eq!(started[1].id.as_str(), "a2");
        assert_eq!(started[1].role, "researcher");
        assert!(
            started
                .iter()
                .all(|request| request.isolation == Isolation::Shared)
        );
    }

    #[tokio::test]
    async fn an_unknown_role_is_rejected_before_anything_starts() {
        let spawner = FakeSpawner::new();
        let tool = SpawnAgentsTool::new(spawner.clone());

        let error = tool
            .execute(
                &call(json!({ "agents": [{ "prompt": "x", "role": "wizard" }] })),
                &context(),
            )
            .await
            .expect_err("rejected");

        assert!(error.to_string().contains("unknown role `wizard`"));
        assert_eq!(spawner.registry.running(), 0);
    }

    #[tokio::test]
    async fn an_empty_prompt_is_rejected() {
        let spawner = FakeSpawner::new();
        let error = SpawnAgentsTool::new(spawner)
            .execute(
                &call(json!({ "agents": [{ "prompt": "   " }] })),
                &context(),
            )
            .await
            .expect_err("rejected");

        assert!(error.to_string().contains("needs a prompt"));
    }

    #[tokio::test]
    async fn an_empty_batch_is_rejected() {
        let spawner = FakeSpawner::new();
        let error = SpawnAgentsTool::new(spawner)
            .execute(&call(json!({ "agents": [] })), &context())
            .await
            .expect_err("rejected");

        assert!(error.to_string().contains("at least one agent"));
    }

    #[tokio::test]
    async fn an_oversized_batch_is_rejected() {
        let spawner = FakeSpawner::new();
        let agents = (0..5)
            .map(|index| json!({ "prompt": format!("task {index}") }))
            .collect::<Vec<_>>();

        let error = SpawnAgentsTool::new(spawner)
            .execute(&call(json!({ "agents": agents })), &context())
            .await
            .expect_err("rejected");

        assert!(error.to_string().contains("at most 4 agents"));
    }

    #[tokio::test]
    async fn a_bad_isolation_mode_names_the_valid_ones() {
        let spawner = FakeSpawner::new();
        let error = SpawnAgentsTool::new(spawner)
            .execute(
                &call(json!({ "agents": [{ "prompt": "x", "isolation": "sandbox" }] })),
                &context(),
            )
            .await
            .expect_err("rejected");

        assert!(error.to_string().contains("use shared or worktree"));
    }

    #[tokio::test]
    async fn an_unavailable_worktree_source_refuses_instead_of_sharing() {
        let spawner = FakeSpawner::new();
        let error = SpawnAgentsTool::new(spawner.clone())
            .execute(
                &call(json!({ "agents": [{ "prompt": "x", "isolation": "worktree" }] })),
                &context(),
            )
            .await
            .expect_err("refused");

        assert!(error.to_string().contains("isolation \"shared\""));
        assert_eq!(spawner.registry.running(), 0, "nothing started");
    }

    #[derive(Debug)]
    struct StaticWorktrees;

    #[async_trait::async_trait]
    impl Worktrees for StaticWorktrees {
        async fn create(&self, agent: &AgentId) -> Result<Worktree, WorktreeError> {
            Ok(Worktree {
                path: format!("/state/worktrees/{}", agent.as_str()).into(),
                branch: Some(format!("t8ful/s1/{}", agent.as_str())),
                base_commit: Some("abc123".to_owned()),
                ignored_paths: vec!["target/".to_owned()],
            })
        }

        async fn remove(&self, _worktree: &Worktree) -> Result<(), WorktreeError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn an_isolated_child_gets_a_worktree_and_reports_it() {
        let mut spawner = FakeSpawner::new();

        // Rebuild the fixture with a worktree source that works.
        let owned = Arc::get_mut(&mut spawner).expect("sole owner");
        owned.worktrees = Arc::new(StaticWorktrees);

        let output = SpawnAgentsTool::new(spawner.clone())
            .execute(
                &call(json!({ "agents": [{ "prompt": "x", "isolation": "worktree" }] })),
                &context(),
            )
            .await
            .expect("spawned");

        assert!(
            output.content.contains("isolation worktree"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("worktree /state/worktrees/a1"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("branch t8ful/s1/a1"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("ignored paths not copied: target/"),
            "an ignored path in the parent is reported, not hidden: {}",
            output.content
        );

        let started = spawner.started();

        assert_eq!(started[0].isolation, Isolation::Worktree);
        assert_eq!(
            started[0].worktree.as_ref().map(|tree| tree.branch.clone()),
            Some(Some("t8ful/s1/a1".to_owned()))
        );
    }

    #[tokio::test]
    async fn a_spawn_creates_a_task_and_links_it() {
        let spawner = FakeSpawner::new();

        SpawnAgentsTool::new(spawner.clone())
            .execute(
                &call(json!({ "agents": [{ "prompt": "read the parser" }] })),
                &context(),
            )
            .await
            .expect("spawned");

        let started = spawner.started();

        assert_eq!(
            started[0].task.as_ref().map(TaskId::as_str),
            Some("task-1"),
            "the child is attached to a task"
        );
        assert!(!spawner.tasks.graph().is_empty());
    }

    #[tokio::test]
    async fn a_batch_that_fails_after_one_started_returns_the_started_id_and_cancels_the_task() {
        let spawner = FakeSpawner::new();

        spawner.fail_start_after(1);

        let output = SpawnAgentsTool::new(spawner.clone())
            .execute(
                &call(json!({
                    "agents": [
                        { "prompt": "read the parser" },
                        { "prompt": "read the lexer" }
                    ]
                })),
                &context(),
            )
            .await
            .expect("partial success is reported, not discarded");

        assert!(output.content.contains("a1 started"), "{}", output.content);
        assert!(
            output.content.contains("could not start a2"),
            "{}",
            output.content
        );
        assert_eq!(spawner.started().len(), 1);

        let graph = spawner.tasks.graph();

        assert_eq!(
            graph
                .get(&TaskId::new("task-2"))
                .expect("the failed spec's task exists")
                .state,
            t8ful_core::task::TaskState::Cancelled
        );
    }

    #[tokio::test]
    async fn an_unknown_existing_task_is_rejected_before_anything_starts() {
        let spawner = FakeSpawner::new();

        let error = SpawnAgentsTool::new(spawner.clone())
            .execute(
                &call(json!({
                    "agents": [
                        { "prompt": "read the parser" },
                        { "prompt": "read the lexer", "task": "task-99" }
                    ]
                })),
                &context(),
            )
            .await
            .expect_err("the second spec names a missing task");

        assert!(error.to_string().contains("unknown task `task-99`"));
        assert!(spawner.started().is_empty());
        assert!(spawner.tasks.graph().is_empty());
    }

    #[tokio::test]
    async fn plan_mode_refuses_a_worktree_before_touching_the_repository() {
        let spawner = FakeSpawner::new();
        let mut ctx = context();

        ctx.mode = t8ful_core::policy::Mode::Plan;

        let error = SpawnAgentsTool::new(spawner.clone())
            .execute(
                &call(json!({
                    "agents": [{ "prompt": "edit it", "isolation": "worktree" }]
                })),
                &ctx,
            )
            .await
            .expect_err("plan mode may not mutate the repository");

        assert!(error.to_string().contains("plan mode"), "{error}");
        assert!(spawner.started().is_empty());
        assert!(spawner.tasks.graph().is_empty());
    }

    #[tokio::test]
    async fn a_failed_worktree_creates_no_task() {
        let spawner = FakeSpawner::new();

        SpawnAgentsTool::new(spawner.clone())
            .execute(
                &call(json!({
                    "agents": [{ "prompt": "edit it", "isolation": "worktree" }]
                })),
                &context(),
            )
            .await
            .expect_err("no worktree source");

        assert!(spawner.started().is_empty());
        assert!(spawner.tasks.graph().is_empty(), "no orphan pending task");
    }

    #[tokio::test]
    async fn waiting_returns_the_report_of_a_finished_child() {
        let spawner = FakeSpawner::new();

        SpawnAgentsTool::new(spawner.clone())
            .execute(
                &call(json!({ "agents": [{ "prompt": "read the parser" }] })),
                &context(),
            )
            .await
            .expect("spawned");
        spawner.finish_first(ChildStatus::Completed);

        let output = AgentWaitTool::new(spawner.clone())
            .execute(&call(json!({})), &context())
            .await
            .expect("waited");

        assert!(
            output.content.contains("a1 (general) completed"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("three call sites"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("task: task-1"),
            "{}",
            output.content
        );
        assert!(
            spawner.registry().notices_for(&AgentId::root()).is_empty(),
            "a report read through agent_wait must not be queued again"
        );
    }

    #[tokio::test]
    async fn waiting_with_a_timeout_reports_what_is_still_running() {
        let spawner = FakeSpawner::new();

        SpawnAgentsTool::new(spawner.clone())
            .execute(
                &call(json!({ "agents": [{ "prompt": "slow work" }] })),
                &context(),
            )
            .await
            .expect("spawned");

        let started = std::time::Instant::now();
        let output = AgentWaitTool::new(spawner)
            .execute(&call(json!({ "timeout_ms": 40 })), &context())
            .await
            .expect("waited");

        assert!(started.elapsed() >= std::time::Duration::from_millis(30));
        assert!(
            output.content.contains("still running: a1"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn waiting_for_an_unknown_agent_is_an_error() {
        let spawner = FakeSpawner::new();
        let error = AgentWaitTool::new(spawner)
            .execute(&call(json!({ "agents": ["a9"] })), &context())
            .await
            .expect_err("unknown");

        assert!(error.to_string().contains("unknown agent `a9`"));
    }

    #[tokio::test]
    async fn waiting_when_nothing_was_ever_spawned_says_so() {
        let spawner = FakeSpawner::new();
        let output = AgentWaitTool::new(spawner)
            .execute(&call(json!({})), &context())
            .await
            .expect("waited");

        assert!(output.content.contains("no child agents"));
    }

    #[tokio::test]
    async fn status_shows_a_running_child_without_blocking() {
        let spawner = FakeSpawner::new();

        SpawnAgentsTool::new(spawner.clone())
            .execute(
                &call(json!({ "agents": [{ "prompt": "slow work" }] })),
                &context(),
            )
            .await
            .expect("spawned");

        let started = std::time::Instant::now();
        let output = AgentStatusTool::new(spawner)
            .execute(&call(json!({})), &context())
            .await
            .expect("status");

        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(
            output.content.contains("a1 running general"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn status_after_a_finish_points_at_agent_wait() {
        let spawner = FakeSpawner::new();

        SpawnAgentsTool::new(spawner.clone())
            .execute(
                &call(json!({ "agents": [{ "prompt": "read" }] })),
                &context(),
            )
            .await
            .expect("spawned");
        spawner.finish_first(ChildStatus::Completed);

        let output = AgentStatusTool::new(spawner)
            .execute(&call(json!({})), &context())
            .await
            .expect("status");

        assert!(
            output.content.contains("a1 completed general"),
            "{}",
            output.content
        );
        assert!(output.content.contains("agent_wait"), "{}", output.content);
    }

    #[tokio::test]
    async fn status_when_nothing_was_spawned_says_so() {
        let spawner = FakeSpawner::new();
        let output = AgentStatusTool::new(spawner)
            .execute(&call(json!({})), &context())
            .await
            .expect("status");

        assert!(output.content.contains("no child agents"));
    }

    #[test]
    fn the_tools_declare_their_effects() {
        let spawner = FakeSpawner::new();

        assert_eq!(
            SpawnAgentsTool::new(spawner.clone()).effect(),
            ToolEffect::Control
        );
        assert_eq!(
            AgentWaitTool::new(spawner.clone()).effect(),
            ToolEffect::ReadOnly
        );
        assert_eq!(AgentStatusTool::new(spawner).effect(), ToolEffect::ReadOnly);
    }

    #[test]
    fn a_task_title_is_the_first_line_and_is_bounded() {
        assert_eq!(task_title("read the parser\nand more"), "read the parser");

        let long = "x".repeat(200);

        assert!(task_title(&long).chars().count() <= 80);
        assert!(task_title(&long).ends_with('…'));
        assert_eq!(task_title("\n\n  real work  \n"), "real work");
    }

    #[tokio::test]
    async fn a_failed_start_reports_which_agent_failed() {
        #[derive(Debug)]
        struct Refusing;

        #[async_trait::async_trait]
        impl AgentSpawner for Refusing {
            fn role(&self, _name: &str) -> Option<RoleProfile> {
                Some(RoleProfile::default())
            }

            fn registry(&self) -> AgentRegistry {
                AgentRegistry::new(AgentLimits::default())
            }

            fn worktrees(&self) -> Arc<dyn Worktrees> {
                Arc::new(NoWorktrees)
            }

            fn task_exists(&self, _id: &TaskId) -> bool {
                false
            }

            fn cancel_task(&self, _id: &TaskId) {}

            async fn start(&self, _request: SpawnRequest) -> Result<(), String> {
                Err("provider is unavailable".to_owned())
            }

            fn task_for(&self, _title: &str) -> Option<TaskId> {
                None
            }
        }

        let error = SpawnAgentsTool::new(Arc::new(Refusing))
            .execute(&call(json!({ "agents": [{ "prompt": "x" }] })), &context())
            .await
            .expect_err("failed");

        assert!(matches!(error, ToolError::Failed(_)));
        assert!(error.to_string().contains("could not start a1"));
        assert!(error.to_string().contains("provider is unavailable"));
    }
}
