//! The bridge between the spawn tools and running children.
//!
//! [`AgentSpawner`] is the trait the tools call. This module implements it for a
//! session: it validates against the live registry, records the spawn in the
//! root journal, starts the child as an independent task so the parent's turn is
//! never blocked, and publishes the report when the child stops.

use std::sync::Arc;

use async_trait::async_trait;
use t8ful_core::event::{AgentEvent, EventEmitter};
use t8ful_core::subagent::{
    AgentId, AgentRegistry, AgentReport, ChildStatus, RoleProfile, SpawnRequest,
};
use t8ful_core::task::{TaskGraph, TaskRecord};
use t8ful_core::worktree::{NoWorktrees, Worktrees};
use t8ful_tools::agent_tools::AgentSpawner;
use t8ful_tools::task_tools::TaskSink;

use crate::children::{ChildOutcome, ChildRuntime, run_child};
use crate::session::SessionJournal;
use crate::worktrees::GitWorktrees;

/// Shared state the spawn tools and the driver both read.
pub struct SpawnerState {
    pub registry: AgentRegistry,
    /// The one task graph for the session, shared with the `task_*` tools.
    pub tasks: Arc<t8ful_tools::task_tools::SharedTasks>,
    /// The child factory state, whose binding names the session records go to.
    pub runtime: ChildRuntime,
    pub worktrees: Arc<dyn Worktrees>,
    pub events: EventEmitter,
    pub roles: std::collections::BTreeMap<String, RoleProfile>,
    /// Whether a finished child starts a turn on an idle parent.
    pub wake_on_finish: bool,
    /// Shared with the driver's idle wake, so a finish can wake a sleeping parent.
    pub wake: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
}

impl SpawnerState {
    /// Point the spawner and its children at another session.
    ///
    /// The registered `spawn_agents` and `task_*` tools hold clones of this state
    /// for the life of the process, so a `/resume` or `/new-session` switch
    /// re-points the shared binding and replaces the task graph in place instead
    /// of rebuilding them.
    pub fn rebind(&self, journal: Option<Arc<SessionJournal>>, records: &[TaskRecord]) {
        let id = journal
            .as_ref()
            .map(|journal| journal.id().to_string())
            .unwrap_or_else(|| "run".to_owned());

        // The previous session's children must not outlive the switch: they
        // would mark the new session's positional task ids and journal those
        // changes here. Cancelling is best effort, so the completion path also
        // checks the session a child was pinned to before touching the graph.
        self.registry.abandon_all();

        {
            let mut binding = self
                .runtime
                .binding
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());

            binding.id = id;
            binding.journal = journal;
        }

        self.tasks
            .replace_graph(TaskGraph::restore(records.iter().cloned()));
    }

    /// Attach the driver's wake. Called once, when the driver starts.
    pub fn set_idle_wake(&self, wake: Arc<tokio::sync::Notify>) {
        *self
            .wake
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(wake);
    }

    /// Whether a finished child should start a turn on an idle parent.
    pub fn wake_on_finish(&self) -> bool {
        self.wake_on_finish
    }

    fn idle_wake(&self) -> Option<Arc<tokio::sync::Notify>> {
        self.wake
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

impl std::fmt::Debug for SpawnerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnerState")
            .field("roles", &self.roles.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// The session's implementation of the spawn-tool contract.
#[derive(Debug)]
pub struct SessionSpawner {
    state: Arc<SpawnerState>,
}

impl SessionSpawner {
    pub fn new(state: Arc<SpawnerState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl AgentSpawner for SessionSpawner {
    fn role(&self, name: &str) -> Option<RoleProfile> {
        self.state.roles.get(name).cloned()
    }

    fn registry(&self) -> AgentRegistry {
        self.state.registry.clone()
    }

    fn worktrees(&self) -> Arc<dyn Worktrees> {
        Arc::clone(&self.state.worktrees)
    }

    fn task_exists(&self, id: &t8ful_core::task::TaskId) -> bool {
        self.state.tasks.graph().get(id).is_some()
    }

    fn cancel_task(&self, id: &t8ful_core::task::TaskId) {
        let shared = &self.state.tasks;
        let mut graph = shared.graph();

        if graph
            .set_state(id, t8ful_core::task::TaskState::Cancelled)
            .is_ok()
        {
            shared.sink().changed(&graph, id);
        }
    }

    fn task_for(&self, title: &str) -> Option<t8ful_core::task::TaskId> {
        let shared = Arc::clone(&self.state.tasks);
        let id = {
            let mut graph = shared.graph();
            let id = graph.create(title, []).ok()?;

            // Announce through the same sink the task tools use, so the journal
            // and the dashboard see one record per change.
            shared.sink().created(title, &id, &[]);
            shared.sink().changed(&graph, &id);
            id
        };

        Some(id)
    }

    async fn start(&self, request: SpawnRequest) -> Result<(), String> {
        // Pin the session once, before the child task exists: the spawn record,
        // the child's journal directory, and its request namespace all name this
        // id even if a switch lands before the task is first polled.
        let state = Arc::clone(&self.state);
        let journal = state.runtime.journal();
        let mut request = request;

        request.session_id = state.runtime.session_id();

        // Register and spawn with no await between them: the slot is held and
        // the task exists before the tool returns, so no caller can observe a
        // registered child that has not started.
        let cancel = state.registry.register(request.clone())?;
        // Children are single level: every child reports to the root.
        let parent = AgentId::root();
        let started = std::time::Instant::now();

        tokio::spawn(async move {
            // The root journal is the session's record, but a failure to append
            // must not strand the child: its own journal is authoritative.
            if let Some(journal) = &journal
                && let Err(error) = journal
                    .append(crate::session::JournalRecord::AgentSpawned {
                        agent: request.id.to_string(),
                        role: request.role.clone(),
                        task: request.task.as_ref().map(ToString::to_string),
                        isolation: request.isolation.as_str().to_owned(),
                        worktree: request
                            .worktree
                            .as_ref()
                            .map(|tree| tree.path.display().to_string()),
                        branch: request
                            .worktree
                            .as_ref()
                            .and_then(|tree| tree.branch.clone()),
                    })
                    .await
            {
                tracing::warn!(
                    target: "t8ful::agents",
                    agent = %request.id,
                    "cannot journal the spawn: {error}"
                );
            }

            state.events.send(AgentEvent::AgentSpawned {
                agent: request.id.to_string(),
                role: request.role.clone(),
                task: request.task.as_ref().map(ToString::to_string),
                isolation: request.isolation.as_str().to_owned(),
                worktree: request
                    .worktree
                    .as_ref()
                    .map(|tree| tree.path.display().to_string()),
            });

            // The child runs in its own task so a panic or an abort becomes a
            // report instead of a permanently running registry entry.
            let runtime = state.runtime.clone();
            let child_request = request.clone();
            let inner =
                tokio::spawn(
                    async move { run_child(Arc::new(runtime), child_request, cancel).await },
                );

            let outcome = match inner.await {
                Ok(outcome) => outcome,
                Err(error) if error.is_cancelled() => ChildOutcome {
                    report: AgentReport {
                        agent: request.id.clone(),
                        role: request.role.clone(),
                        status: ChildStatus::Cancelled,
                        summary: "the agent was cancelled before it reported".to_owned(),
                        steps: 0,
                        usage: t8ful_core::usage::Usage::default(),
                        task: request.task.clone(),
                        worktree: request.worktree.clone(),
                        elapsed_ms: started.elapsed().as_millis(),
                    },
                },
                Err(error) => ChildOutcome {
                    report: AgentReport {
                        agent: request.id.clone(),
                        role: request.role.clone(),
                        status: ChildStatus::Failed,
                        summary: format!("the agent task stopped unexpectedly: {error}"),
                        steps: 0,
                        usage: t8ful_core::usage::Usage::default(),
                        task: request.task.clone(),
                        worktree: request.worktree.clone(),
                        elapsed_ms: started.elapsed().as_millis(),
                    },
                },
            };
            let report = outcome.report;

            state.events.send(AgentEvent::AgentFinished {
                agent: report.agent.to_string(),
                role: report.role.clone(),
                status: report.status.as_str().to_owned(),
                summary: report.summary.clone(),
                steps: report.steps,
                task: report.task.as_ref().map(ToString::to_string),
                worktree: report
                    .worktree
                    .as_ref()
                    .map(|tree| tree.path.display().to_string()),
                branch: report
                    .worktree
                    .as_ref()
                    .and_then(|tree| tree.branch.clone()),
                elapsed_ms: u64::try_from(report.elapsed_ms).unwrap_or(u64::MAX),
            });

            // A child abandoned by a session switch must not mark the new
            // session's task graph, journal into it, or wake it.
            if request.session_id != state.runtime.session_id() {
                return;
            }

            finish_task(&state, &report);
            state.registry.finish(report.clone(), &parent);

            if let Some(wake) = state.idle_wake() {
                wake.notify_one();
            }
        });

        Ok(())
    }
}

/// Mark the child's task complete or cancelled, and journal it.
fn finish_task(state: &SpawnerState, report: &AgentReport) {
    let Some(task) = &report.task else {
        return;
    };

    // A cancelled child records `cancelled` rather than leaving the task
    // untouched: the work stopped, and a task that still looks pending would
    // suggest it is still going to happen.
    let graph_state = match report.status {
        ChildStatus::Completed => t8ful_core::task::TaskState::Completed,
        ChildStatus::Failed => t8ful_core::task::TaskState::Pending,
        ChildStatus::Cancelled => t8ful_core::task::TaskState::Cancelled,
        ChildStatus::Running => return,
    };

    let shared = &state.tasks;

    {
        let mut graph = shared.graph();

        if graph.set_state(task, graph_state).is_err() {
            return;
        }

        // The sink journals and broadcasts in one place, so a spawn's task
        // update looks exactly like a model-driven one.
        shared.sink().changed(&graph, task);
    }
}

/// Records task changes in the root journal and broadcasts them.
///
/// Journal appends go through one ordered channel drained by a single writer
/// task. Spawning a writer per change would race, and a record that landed out
/// of order would shift positional task ids on resume.
pub struct JournalTaskSink {
    pub records: tokio::sync::mpsc::UnboundedSender<crate::session::JournalRecord>,
    pub events: EventEmitter,
}

impl std::fmt::Debug for JournalTaskSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalTaskSink").finish_non_exhaustive()
    }
}

impl TaskSink for JournalTaskSink {
    fn created(
        &self,
        title: &str,
        id: &t8ful_core::task::TaskId,
        deps: &[t8ful_core::task::TaskId],
    ) {
        let _ = self
            .records
            .send(crate::session::JournalRecord::Task(TaskRecord::Created {
                title: title.to_owned(),
                deps: deps.to_vec(),
            }));

        self.events.send(AgentEvent::TaskChanged {
            id: id.to_string(),
            title: title.to_owned(),
            state: t8ful_core::task::TaskState::Pending.as_str().to_owned(),
            blocked: false,
            owner: None,
        });
    }

    fn changed(&self, graph: &TaskGraph, id: &t8ful_core::task::TaskId) {
        let Some(task) = graph.get(id).cloned() else {
            return;
        };

        let _ = self
            .records
            .send(crate::session::JournalRecord::Task(TaskRecord::Updated {
                id: task.id.clone(),
                state: task.state,
                owner: task.owner.clone(),
            }));

        self.events.send(AgentEvent::TaskChanged {
            id: task.id.to_string(),
            title: task.title.clone(),
            state: task.state.as_str().to_owned(),
            blocked: graph.blocked(&task.id),
            owner: task.owner,
        });
    }

    fn dependency_added(&self, id: &t8ful_core::task::TaskId, dep: &t8ful_core::task::TaskId) {
        let _ = self.records.send(crate::session::JournalRecord::Task(
            TaskRecord::DependencyAdded {
                id: id.clone(),
                dep: dep.clone(),
            },
        ));
    }
}

/// Discover the worktree source for a session, or `NoWorktrees`.
///
/// A project outside Git is not an error here: read-only children can still run
/// shared. Only a child that asks for `worktree` isolation is refused, and it is
/// refused with an error rather than silently sharing the tree.
/// Discover the worktree source for a session.
///
/// Runs on the blocking pool: `discover` executes `git rev-parse`, and doing that
/// inline in an async function would block a runtime worker. The pty tests run on
/// a two-worker runtime, where blocking both is enough to stall the interface.
pub async fn discover_worktrees(
    settings: &crate::config::Settings,
    paths: &crate::state::StatePaths,
    session: &str,
) -> Arc<dyn Worktrees> {
    let state_root = settings
        .agents
        .worktree_root
        .clone()
        .unwrap_or_else(|| paths.root.clone());
    let project = settings.root.path.clone();
    let identity = session.to_owned();
    let discovered = tokio::task::spawn_blocking(move || {
        GitWorktrees::discover(&project, &state_root, &identity).map(Arc::new)
    })
    .await;

    let discovered = match discovered {
        Ok(result) => result,
        Err(error) => {
            tracing::debug!(
                target: "t8ful::agents",
                "worktree discovery task failed: {error}"
            );

            return Arc::new(NoWorktrees);
        }
    };

    match discovered {
        Ok(trees) => trees as Arc<dyn Worktrees>,
        Err(error) => {
            tracing::debug!(
                target: "t8ful::agents",
                "worktree isolation is unavailable: {error}"
            );

            Arc::new(NoWorktrees)
        }
    }
}
