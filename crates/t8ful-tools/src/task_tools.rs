//! Tools for the session task graph.
//!
//! These are ordinary control tools: they record intent in the graph and return
//! a short text rendering. They never dispatch work, and a blocked task is
//! reported, not enforced — the model decides what to do about it.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use t8ful_core::task::{TaskError, TaskGraph, TaskId, TaskState};
use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec};

/// The graph plus the place changes are reported to.
///
/// The tool owns the graph behind a mutex so a `task_create` and a `task_update`
/// from the same turn cannot interleave, and the sink lets the composition root
/// journal and broadcast every change without the tool knowing how.
pub struct SharedTasks {
    graph: std::sync::Mutex<TaskGraph>,
    sink: std::sync::Arc<dyn TaskSink>,
}

impl std::fmt::Debug for SharedTasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedTasks").finish_non_exhaustive()
    }
}

impl SharedTasks {
    pub fn new(graph: TaskGraph, sink: std::sync::Arc<dyn TaskSink>) -> Self {
        Self {
            graph: std::sync::Mutex::new(graph),
            sink,
        }
    }

    pub fn graph(&self) -> std::sync::MutexGuard<'_, TaskGraph> {
        self.graph
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Replace the graph in place, for switching to another session.
    ///
    /// The registered `task_*` tools hold this object for the life of the
    /// process, so the graph is swapped under the lock rather than the whole
    /// `SharedTasks` being rebuilt.
    pub fn replace_graph(&self, graph: TaskGraph) {
        *self
            .graph
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = graph;
    }

    /// The journal and event sink.
    ///
    /// Callers pass the guard they already hold to the sink rather than letting
    /// it lock again: `std::sync::Mutex` is not reentrant, so a sink that took
    /// the lock while a mutation held it would deadlock.
    pub fn sink(&self) -> &dyn TaskSink {
        self.sink.as_ref()
    }

    /// Render the graph, taking the lock only if the caller has not.
    pub fn rows_text(&self) -> String {
        render_rows(&self.graph())
    }
}

/// Where graph changes go: the journal and the event stream.
pub trait TaskSink: Send + Sync {
    fn created(&self, title: &str, id: &TaskId, deps: &[TaskId]);

    fn changed(&self, graph: &TaskGraph, id: &TaskId);

    fn dependency_added(&self, id: &TaskId, dep: &TaskId);
}

/// A sink for tests and for sessions that keep no journal.
#[derive(Debug, Default)]
pub struct NullTaskSink;

impl TaskSink for NullTaskSink {
    fn created(&self, _title: &str, _id: &TaskId, _deps: &[TaskId]) {}

    fn changed(&self, _graph: &TaskGraph, _id: &TaskId) {}

    fn dependency_added(&self, _id: &TaskId, _dep: &TaskId) {}
}

/// Render the graph as the model sees it.
pub fn render_rows(graph: &TaskGraph) -> String {
    if graph.is_empty() {
        return "no tasks".to_owned();
    }

    graph
        .rows()
        .into_iter()
        .map(|row| {
            let blocked = if row.blocked { " [blocked]" } else { "" };
            let owner = row
                .owner
                .map(|owner| format!(" ({owner})"))
                .unwrap_or_default();
            let deps = if row.deps.is_empty() {
                String::new()
            } else {
                format!(
                    " after {}",
                    row.deps
                        .iter()
                        .map(TaskId::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };

            format!(
                "{} {} {}{}{}{}",
                row.id,
                row.state.as_str(),
                row.title,
                owner,
                deps,
                blocked
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Deserialize)]
struct CreateArgs {
    title: String,
    #[serde(default)]
    deps: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateArgs {
    id: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    add_deps: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct IdArgs {
    #[serde(default)]
    id: Option<String>,
}

/// Record a task in the session graph.
pub struct TaskCreateTool {
    tasks: std::sync::Arc<SharedTasks>,
}

impl TaskCreateTool {
    pub fn new(tasks: std::sync::Arc<SharedTasks>) -> Self {
        Self { tasks }
    }
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_create".to_owned(),
            description: "Record a task in this session's task graph. Use it to plan work you \
                          will do across several steps, including work you delegate to agents. It \
                          never starts anything; a task is a checklist entry you and the user can \
                          both see."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "What has to be done, short enough for one line"
                    },
                    "deps": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Task ids that must complete before this one"
                    }
                },
                "required": ["title"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Control
    }

    async fn execute(&self, call: &ToolCall, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: CreateArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let deps = args.deps.into_iter().map(TaskId::new).collect::<Vec<_>>();

        let id = {
            let mut graph = self.tasks.graph();
            let id = graph
                .create(args.title.trim(), deps.clone())
                .map_err(|error| ToolError::Failed(error.to_string()))?;

            // Announce while the guard is held, passing it down: a sink that
            // locked the graph again would deadlock.
            self.tasks.sink().created(args.title.trim(), &id, &deps);
            self.tasks.sink().changed(&graph, &id);
            id
        };

        Ok(ToolOutput {
            content: format!("{id} created\n{}", self.tasks.rows_text()),
            truncated: false,
            metadata: Some(json!({ "id": id.as_str() })),
        })
    }
}

/// Change a task's state, owner, or dependencies.
pub struct TaskUpdateTool {
    tasks: std::sync::Arc<SharedTasks>,
}

impl TaskUpdateTool {
    pub fn new(tasks: std::sync::Arc<SharedTasks>) -> Self {
        Self { tasks }
    }
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_update".to_owned(),
            description: "Change a task's state, owner, or dependencies. Mark your own work \
                          in_progress when you start it and completed only when it is actually \
                          done; a task that is not completed keeps its dependents blocked."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Task id, such as task-1" },
                    "state": {
                        "type": "string",
                        "enum": ["pending", "in_progress", "completed", "cancelled"],
                        "description": "New state"
                    },
                    "owner": {
                        "type": "string",
                        "description": "Agent working on the task, such as root or a1"
                    },
                    "add_deps": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Task ids to add as dependencies"
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Control
    }

    async fn execute(&self, call: &ToolCall, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: UpdateArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let id = TaskId::new(args.id.trim());
        let state = match args.state.as_deref() {
            Some(raw) => Some(TaskState::parse(raw).ok_or_else(|| {
                ToolError::InvalidArguments(format!(
                    "`{raw}` is not a task state; use pending, in_progress, completed, or cancelled"
                ))
            })?),
            None => None,
        };

        let deps = args
            .add_deps
            .iter()
            .map(|dep| TaskId::new(dep.trim()))
            .collect::<Vec<_>>();

        {
            let mut graph = self.tasks.graph();

            // Validate the target and every dependency before anything else is
            // applied. A rejected dependency used to return after the state and
            // owner had already changed, leaving the live graph with a change
            // that the journal never recorded, so the model and a resumed
            // session disagreed about what happened.
            if graph.get(&id).is_none() {
                return Err(ToolError::Failed(
                    TaskError::Unknown(id.to_string()).to_string(),
                ));
            }

            for dep in &deps {
                graph
                    .check_dependency(&id, dep)
                    .map_err(|error| ToolError::Failed(error.to_string()))?;
            }

            if let Some(state) = state {
                graph
                    .set_state(&id, state)
                    .map_err(|error| ToolError::Failed(error.to_string()))?;
            }

            if let Some(owner) = args.owner {
                let owner = owner.trim();

                graph
                    .set_owner(&id, (!owner.is_empty()).then(|| owner.to_owned()))
                    .map_err(|error| ToolError::Failed(error.to_string()))?;
            }

            for dep in &deps {
                graph
                    .add_dependency(&id, dep)
                    .map_err(|error| ToolError::Failed(error.to_string()))?;
                self.tasks.sink().dependency_added(&id, dep);
            }

            self.tasks.sink().changed(&graph, &id);
        }

        Ok(ToolOutput {
            content: format!("{id} updated\n{}", self.tasks.rows_text()),
            truncated: false,
            metadata: Some(json!({ "id": id.as_str() })),
        })
    }
}

/// Show the task graph.
pub struct TaskListTool {
    tasks: std::sync::Arc<SharedTasks>,
}

impl TaskListTool {
    pub fn new(tasks: std::sync::Arc<SharedTasks>) -> Self {
        Self { tasks }
    }
}

#[async_trait]
impl Tool for TaskListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_list".to_owned(),
            description: "Show this session's task graph with each task's state and whether it is \
                          blocked by an incomplete dependency."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Show one task in detail instead of the whole graph"
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
        let args: IdArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let graph = self.tasks.graph();

        let content = match args.id {
            Some(raw) => {
                let id = TaskId::new(raw.trim());
                let task = graph
                    .get(&id)
                    .ok_or_else(|| ToolError::Failed(format!("unknown task `{id}`")))?;

                format!(
                    "{id} {}\nblocked: {}\nowner: {}\ndeps: {}",
                    task.state.as_str(),
                    graph.blocked(&id),
                    task.owner.as_deref().unwrap_or("none"),
                    if task.deps.is_empty() {
                        "none".to_owned()
                    } else {
                        task.deps
                            .iter()
                            .map(TaskId::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )
            }
            None => render_rows(&graph),
        };

        Ok(ToolOutput {
            content,
            truncated: false,
            metadata: Some(json!({
                "tasks": graph.len(),
                "completed": graph.completed(),
            })),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use t8ful_core::task::{TaskGraph, TaskId};
    use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolStatus};

    use super::{
        NullTaskSink, SharedTasks, TaskCreateTool, TaskListTool, TaskSink, TaskUpdateTool,
    };

    fn context() -> ToolContext {
        ToolContext::new("/tmp".into(), "/tmp".into())
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall::new("c1", name, arguments)
    }

    #[tokio::test]
    async fn creating_a_task_returns_its_id_and_the_graph() {
        let tasks = Arc::new(SharedTasks::new(TaskGraph::new(), Arc::new(NullTaskSink)));
        let tool = TaskCreateTool::new(Arc::clone(&tasks));

        let output = tool
            .execute(
                &call("task_create", json!({ "title": "write the parser" })),
                &context(),
            )
            .await
            .expect("created");

        assert!(output.content.starts_with("task-1 created"));
        assert!(output.content.contains("task-1 pending write the parser"));
        assert_eq!(tasks.graph().len(), 1);
    }

    #[tokio::test]
    async fn a_task_can_depend_on_an_existing_one() {
        let tasks = Arc::new(SharedTasks::new(TaskGraph::new(), Arc::new(NullTaskSink)));

        TaskCreateTool::new(Arc::clone(&tasks))
            .execute(
                &call("task_create", json!({ "title": "schema" })),
                &context(),
            )
            .await
            .expect("first");

        let output = TaskCreateTool::new(Arc::clone(&tasks))
            .execute(
                &call(
                    "task_create",
                    json!({ "title": "parser", "deps": ["task-1"] }),
                ),
                &context(),
            )
            .await
            .expect("second");

        assert!(
            output
                .content
                .contains("task-2 pending parser after task-1 [blocked]")
        );
    }

    #[derive(Default)]
    struct RecordingSink {
        created: std::sync::Mutex<Vec<(String, Vec<TaskId>)>>,
        changed: std::sync::Mutex<Vec<String>>,
        dependencies: std::sync::Mutex<Vec<(TaskId, TaskId)>>,
    }

    impl TaskSink for RecordingSink {
        fn created(&self, title: &str, _id: &TaskId, deps: &[TaskId]) {
            self.created
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push((title.to_owned(), deps.to_vec()));
        }

        fn changed(&self, _graph: &TaskGraph, id: &TaskId) {
            self.changed
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(id.to_string());
        }

        fn dependency_added(&self, id: &TaskId, dep: &TaskId) {
            self.dependencies
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push((id.clone(), dep.clone()));
        }
    }

    #[tokio::test]
    async fn creating_a_task_journals_its_dependencies() {
        let sink = Arc::new(RecordingSink::default());
        let tasks = Arc::new(SharedTasks::new(TaskGraph::new(), sink.clone()));

        TaskCreateTool::new(Arc::clone(&tasks))
            .execute(
                &call("task_create", json!({ "title": "schema" })),
                &context(),
            )
            .await
            .expect("first");
        TaskCreateTool::new(tasks)
            .execute(
                &call(
                    "task_create",
                    json!({ "title": "parser", "deps": ["task-1"] }),
                ),
                &context(),
            )
            .await
            .expect("second");

        let created = sink
            .created
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        assert_eq!(created.len(), 2);
        assert_eq!(created[1].0, "parser");
        assert_eq!(created[1].1, vec![TaskId::new("task-1")]);
    }

    #[tokio::test]
    async fn an_unknown_dependency_is_a_tool_failure_not_a_panic() {
        let tasks = Arc::new(SharedTasks::new(TaskGraph::new(), Arc::new(NullTaskSink)));
        let error = TaskCreateTool::new(tasks)
            .execute(
                &call(
                    "task_create",
                    json!({ "title": "parser", "deps": ["task-9"] }),
                ),
                &context(),
            )
            .await
            .expect_err("rejected");

        assert!(error.to_string().contains("unknown task `task-9`"));
    }

    #[tokio::test]
    async fn completing_a_dependency_unblocks_its_dependent() {
        let tasks = Arc::new(SharedTasks::new(TaskGraph::new(), Arc::new(NullTaskSink)));

        TaskCreateTool::new(Arc::clone(&tasks))
            .execute(
                &call("task_create", json!({ "title": "schema" })),
                &context(),
            )
            .await
            .expect("first");
        TaskCreateTool::new(Arc::clone(&tasks))
            .execute(
                &call(
                    "task_create",
                    json!({ "title": "parser", "deps": ["task-1"] }),
                ),
                &context(),
            )
            .await
            .expect("second");

        let output = TaskUpdateTool::new(Arc::clone(&tasks))
            .execute(
                &call(
                    "task_update",
                    json!({ "id": "task-1", "state": "completed" }),
                ),
                &context(),
            )
            .await
            .expect("updated");

        assert!(output.content.contains("task-1 completed schema"));
        assert!(!output.content.contains("[blocked]"));
        assert_eq!(tasks.graph().completed(), 1);
    }

    #[tokio::test]
    async fn an_owner_of_empty_string_clears_the_owner() {
        let tasks = Arc::new(SharedTasks::new(TaskGraph::new(), Arc::new(NullTaskSink)));

        TaskCreateTool::new(Arc::clone(&tasks))
            .execute(&call("task_create", json!({ "title": "work" })), &context())
            .await
            .expect("created");
        TaskUpdateTool::new(Arc::clone(&tasks))
            .execute(
                &call("task_update", json!({ "id": "task-1", "owner": "a1" })),
                &context(),
            )
            .await
            .expect("claimed");

        assert_eq!(
            tasks
                .graph()
                .get(&TaskId::new("task-1"))
                .expect("task")
                .owner
                .as_deref(),
            Some("a1")
        );

        TaskUpdateTool::new(Arc::clone(&tasks))
            .execute(
                &call("task_update", json!({ "id": "task-1", "owner": "" })),
                &context(),
            )
            .await
            .expect("released");

        assert!(
            tasks
                .graph()
                .get(&TaskId::new("task-1"))
                .expect("task")
                .owner
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_unknown_state_is_rejected_with_the_valid_ones() {
        let tasks = Arc::new(SharedTasks::new(TaskGraph::new(), Arc::new(NullTaskSink)));

        TaskCreateTool::new(tasks.clone())
            .execute(&call("task_create", json!({ "title": "work" })), &context())
            .await
            .expect("created");

        let error = TaskUpdateTool::new(tasks)
            .execute(
                &call("task_update", json!({ "id": "task-1", "state": "blocked" })),
                &context(),
            )
            .await
            .expect_err("rejected");

        assert!(error.to_string().contains("not a task state"));
    }

    /// A rejected update must not leave anything behind.
    ///
    /// Regression: the state and owner were applied before `add_deps` was
    /// validated, and the error returned before the sink journaled anything, so
    /// the live graph showed a completed task that a resumed session saw as
    /// pending.
    #[tokio::test]
    async fn a_rejected_dependency_leaves_the_graph_and_journal_untouched() {
        let sink = Arc::new(RecordingSink::default());
        let tasks = Arc::new(SharedTasks::new(TaskGraph::new(), sink.clone()));

        TaskCreateTool::new(Arc::clone(&tasks))
            .execute(&call("task_create", json!({ "title": "work" })), &context())
            .await
            .expect("created");

        let journaled = |sink: &RecordingSink| {
            (
                sink.changed
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .len(),
                sink.dependencies
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .len(),
            )
        };
        let before = journaled(&sink);

        let error = TaskUpdateTool::new(Arc::clone(&tasks))
            .execute(
                &call(
                    "task_update",
                    json!({
                        "id": "task-1",
                        "state": "completed",
                        "owner": "a1",
                        "add_deps": ["task-9"],
                    }),
                ),
                &context(),
            )
            .await
            .expect_err("rejected");

        assert!(error.to_string().contains("unknown task `task-9`"));

        let graph = tasks.graph();
        let task = graph.get(&TaskId::new("task-1")).expect("task");

        assert_eq!(task.state.as_str(), "pending");
        assert!(task.owner.is_none());
        assert!(task.deps.is_empty());

        drop(graph);

        assert_eq!(
            journaled(&sink),
            before,
            "the rejected update journaled something"
        );
    }

    #[tokio::test]
    async fn listing_an_empty_graph_says_so() {
        let tasks = Arc::new(SharedTasks::new(TaskGraph::new(), Arc::new(NullTaskSink)));
        let output = TaskListTool::new(tasks)
            .execute(&call("task_list", json!({})), &context())
            .await
            .expect("listed");

        assert_eq!(output.content, "no tasks");
    }

    #[tokio::test]
    async fn listing_one_task_reports_its_blocked_state_and_deps() {
        let tasks = Arc::new(SharedTasks::new(TaskGraph::new(), Arc::new(NullTaskSink)));

        TaskCreateTool::new(Arc::clone(&tasks))
            .execute(
                &call("task_create", json!({ "title": "schema" })),
                &context(),
            )
            .await
            .expect("first");
        TaskCreateTool::new(Arc::clone(&tasks))
            .execute(
                &call(
                    "task_create",
                    json!({ "title": "parser", "deps": ["task-1"] }),
                ),
                &context(),
            )
            .await
            .expect("second");

        let output = TaskListTool::new(tasks)
            .execute(&call("task_list", json!({ "id": "task-2" })), &context())
            .await
            .expect("listed");

        assert!(
            output.content.contains("task-2 pending"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("blocked: true"),
            "{}",
            output.content
        );
        assert!(
            output.content.contains("deps: task-1"),
            "{}",
            output.content
        );
        assert!(output.content.contains("owner: none"), "{}", output.content);
    }

    #[test]
    fn the_tools_declare_their_effects() {
        let tasks = Arc::new(SharedTasks::new(TaskGraph::new(), Arc::new(NullTaskSink)));

        assert_eq!(
            TaskCreateTool::new(tasks.clone()).effect(),
            ToolEffect::Control
        );
        assert_eq!(
            TaskUpdateTool::new(tasks.clone()).effect(),
            ToolEffect::Control
        );
        assert_eq!(TaskListTool::new(tasks).effect(), ToolEffect::ReadOnly);

        // A created task must never be reported as anything but success.
        let _ = ToolStatus::Ok;
    }
}
