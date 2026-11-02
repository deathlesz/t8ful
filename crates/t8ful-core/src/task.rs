//! The session task graph.
//!
//! A task is a checklist entry the model and the frontend can both see: an id, a
//! title, a state, the tasks it waits on, and an optional owner. The graph never
//! schedules or dispatches anything. Its only job is to make progress visible and
//! to make a blocked task obvious instead of implicit.
//!
//! Ownership is recorded, not enforced: a task names the agent working on it, and
//! nothing here prevents a second agent from touching the same files.
//! Coordination across concurrently writing agents is the caller's problem; this
//! type only records what was claimed.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Identifier of one task inside a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(String);

impl TaskId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for TaskId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for TaskId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    InProgress,
    Completed,
    /// The work stopped without completing cleanly.
    Cancelled,
}

impl TaskState {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "pending" => Some(Self::Pending),
            "in_progress" | "inprogress" | "active" => Some(Self::InProgress),
            "completed" | "complete" | "done" => Some(Self::Completed),
            "cancelled" | "canceled" | "abandoned" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Only a completed task unblocks its dependents.
    pub fn is_done(self) -> bool {
        self == Self::Completed
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub state: TaskState,
    /// Tasks that must complete before this one can start.
    pub deps: Vec<TaskId>,
    /// The agent that claimed the task, when one has.
    pub owner: Option<String>,
}

/// One rendered row, as the dashboard and the `task_list` tool need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub id: TaskId,
    pub title: String,
    pub state: TaskState,
    pub deps: Vec<TaskId>,
    pub owner: Option<String>,
    /// At least one dependency is not complete.
    pub blocked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskError {
    #[error("unknown task `{0}`")]
    Unknown(String),

    #[error("task `{task}` depends on `{dep}`, which would create a cycle")]
    Cycle { task: String, dep: String },
}

/// The session task graph.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskGraph {
    tasks: Vec<Task>,
    next: u64,
}

impl TaskGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a task and return its generated id.
    ///
    /// Every dependency must already exist. A new task cannot close a cycle on
    /// creation because nothing can depend on it yet, so only `add_dependency`
    /// has to reject one.
    pub fn create(
        &mut self,
        title: impl Into<String>,
        deps: impl IntoIterator<Item = TaskId>,
    ) -> Result<TaskId, TaskError> {
        let deps: Vec<TaskId> = deps.into_iter().collect();

        for dep in &deps {
            if self.get(dep).is_none() {
                return Err(TaskError::Unknown(dep.to_string()));
            }
        }

        self.next += 1;

        let id = TaskId::new(format!("task-{}", self.next));

        self.tasks.push(Task {
            id: id.clone(),
            title: title.into(),
            state: TaskState::Pending,
            deps: dedup(deps),
            owner: None,
        });

        Ok(id)
    }

    pub fn get(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.iter().find(|task| &task.id == id)
    }

    pub fn get_mut(&mut self, id: &TaskId) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|task| &task.id == id)
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn completed(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.state.is_done())
            .count()
    }

    /// Whether a task waits on a dependency that is not complete.
    ///
    /// A dependency naming a task that no longer exists counts as incomplete: a
    /// dangling reference must never make a task look ready.
    pub fn blocked(&self, id: &TaskId) -> bool {
        let Some(task) = self.get(id) else {
            return false;
        };

        task.deps.iter().any(|dep| {
            self.get(dep)
                .is_none_or(|dependency| !dependency.state.is_done())
        })
    }

    pub fn set_state(&mut self, id: &TaskId, state: TaskState) -> Result<(), TaskError> {
        let task = self
            .get_mut(id)
            .ok_or_else(|| TaskError::Unknown(id.to_string()))?;

        task.state = state;

        Ok(())
    }

    pub fn set_owner(&mut self, id: &TaskId, owner: Option<String>) -> Result<(), TaskError> {
        let task = self
            .get_mut(id)
            .ok_or_else(|| TaskError::Unknown(id.to_string()))?;

        task.owner = owner;

        Ok(())
    }

    /// Check a dependency without adding it.
    ///
    /// A caller that changes several things at once validates every edge first,
    /// so a rejected update leaves the graph exactly as it was. Edges only ever
    /// leave `id`, so a cycle among a batch of new dependencies is already
    /// visible here: an edge added later in the batch cannot create a new path
    /// that reaches `id`.
    pub fn check_dependency(&self, id: &TaskId, dep: &TaskId) -> Result<(), TaskError> {
        if self.get(id).is_none() {
            return Err(TaskError::Unknown(id.to_string()));
        }

        if self.get(dep).is_none() {
            return Err(TaskError::Unknown(dep.to_string()));
        }

        if id == dep || self.reaches(dep, id) {
            return Err(TaskError::Cycle {
                task: id.to_string(),
                dep: dep.to_string(),
            });
        }

        Ok(())
    }

    /// Add a dependency, rejecting an unknown id or one that closes a cycle.
    pub fn add_dependency(&mut self, id: &TaskId, dep: &TaskId) -> Result<(), TaskError> {
        self.check_dependency(id, dep)?;

        let task = self.get_mut(id).expect("checked above");

        if !task.deps.contains(dep) {
            task.deps.push(dep.clone());
        }

        Ok(())
    }

    /// Whether `from` can reach `target` by following dependencies.
    ///
    /// Iterative on purpose: a long chain must not overflow the stack, and the
    /// caller may present an arbitrary number of tasks.
    fn reaches(&self, from: &TaskId, target: &TaskId) -> bool {
        let mut stack = vec![from.clone()];
        let mut seen = BTreeSet::new();

        while let Some(current) = stack.pop() {
            if &current == target {
                return true;
            }

            if !seen.insert(current.clone()) {
                continue;
            }

            if let Some(task) = self.get(&current) {
                stack.extend(task.deps.iter().cloned());
            }
        }

        false
    }

    /// Rebuild the graph from journal records in append order.
    ///
    /// Ids are positional, exactly as `create` assigns them, so a journal that
    /// recorded every creation in order restores the ids the transcript already
    /// refers to. A dependency naming a task that never existed is kept: it
    /// counts as incomplete, so a dangling reference cannot make a task look
    /// ready.
    pub fn restore(records: impl IntoIterator<Item = TaskRecord>) -> Self {
        let mut graph = Self::new();

        for record in records {
            match record {
                TaskRecord::Created { title, deps } => {
                    graph.next += 1;

                    graph.tasks.push(Task {
                        id: TaskId::new(format!("task-{}", graph.next)),
                        title,
                        state: TaskState::Pending,
                        deps: dedup(deps),
                        owner: None,
                    });
                }
                TaskRecord::Updated { id, state, owner } => {
                    if let Some(task) = graph.get_mut(&id) {
                        task.state = state;
                        task.owner = owner;
                    }
                }
                TaskRecord::DependencyAdded { id, dep } => {
                    let _ = graph.add_dependency(&id, &dep);
                }
            }
        }

        graph
    }

    /// Rows for a dashboard or the `task_list` tool, in creation order.
    pub fn rows(&self) -> Vec<TaskRow> {
        self.tasks
            .iter()
            .map(|task| TaskRow {
                id: task.id.clone(),
                title: task.title.clone(),
                state: task.state,
                deps: task.deps.clone(),
                owner: task.owner.clone(),
                blocked: self.blocked(&task.id),
            })
            .collect()
    }

    /// A short summary line such as `tasks 3/7`, or `None` when there are none.
    pub fn summary(&self) -> Option<String> {
        if self.tasks.is_empty() {
            return None;
        }

        Some(format!("tasks {}/{}", self.completed(), self.len()))
    }
}

fn dedup(deps: Vec<TaskId>) -> Vec<TaskId> {
    let mut unique = BTreeSet::new();

    unique.extend(deps);
    unique.into_iter().collect()
}

/// One journal-visible change to the graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum TaskRecord {
    Created {
        title: String,
        #[serde(default)]
        deps: Vec<TaskId>,
    },
    Updated {
        id: TaskId,
        state: TaskState,
        #[serde(default)]
        owner: Option<String>,
    },
    DependencyAdded {
        id: TaskId,
        dep: TaskId,
    },
}

#[cfg(test)]
mod tests {
    use super::{TaskError, TaskGraph, TaskId, TaskRecord, TaskState};

    fn id(value: &str) -> TaskId {
        TaskId::new(value)
    }

    #[test]
    fn a_new_task_is_pending_and_unblocked() {
        let mut graph = TaskGraph::new();
        let task = graph.create("write the parser", []).expect("created");

        assert_eq!(task.as_str(), "task-1");
        assert_eq!(graph.get(&task).expect("task").state, TaskState::Pending);
        assert!(!graph.blocked(&task));
    }

    #[test]
    fn a_task_waits_for_every_incomplete_dependency() {
        let mut graph = TaskGraph::new();
        let first = graph.create("schema", []).expect("created");
        let second = graph.create("parser", [first.clone()]).expect("created");

        assert!(graph.blocked(&second));

        graph
            .set_state(&first, TaskState::Completed)
            .expect("state");

        assert!(!graph.blocked(&second));
    }

    #[test]
    fn a_dependency_blocks_until_it_completes_not_when_it_starts() {
        let mut graph = TaskGraph::new();
        let first = graph.create("schema", []).expect("created");
        let second = graph.create("parser", [first.clone()]).expect("created");

        graph
            .set_state(&first, TaskState::InProgress)
            .expect("state");

        assert!(graph.blocked(&second), "in-progress is not done");

        graph
            .set_state(&first, TaskState::Completed)
            .expect("state");

        assert!(!graph.blocked(&second));
    }

    #[test]
    fn a_cancelled_dependency_never_unblocks() {
        let mut graph = TaskGraph::new();
        let first = graph.create("schema", []).expect("created");
        let second = graph.create("parser", [first.clone()]).expect("created");

        graph
            .set_state(&first, TaskState::Cancelled)
            .expect("state");

        assert!(graph.blocked(&second));
    }

    #[test]
    fn an_unknown_dependency_is_rejected_on_creation() {
        let mut graph = TaskGraph::new();
        let error = graph
            .create("parser", [id("task-99")])
            .expect_err("rejected");

        assert_eq!(error, TaskError::Unknown("task-99".to_owned()));
        assert!(graph.is_empty(), "a rejected task is not added");
    }

    #[test]
    fn a_cycle_is_rejected_and_leaves_the_graph_alone() {
        let mut graph = TaskGraph::new();
        let first = graph.create("a", []).expect("created");
        let second = graph.create("b", [first.clone()]).expect("created");

        // b depends on a; making a depend on b closes the loop.
        let error = graph
            .add_dependency(&first, &second)
            .expect_err("cycle rejected");

        assert_eq!(
            error,
            TaskError::Cycle {
                task: "task-1".to_owned(),
                dep: "task-2".to_owned(),
            }
        );
        assert!(graph.get(&first).expect("task").deps.is_empty());
    }

    #[test]
    fn a_self_dependency_is_a_cycle() {
        let mut graph = TaskGraph::new();
        let task = graph.create("a", []).expect("created");

        assert!(matches!(
            graph.add_dependency(&task, &task),
            Err(TaskError::Cycle { .. })
        ));
    }

    #[test]
    fn a_longer_chain_closes_as_a_cycle() {
        let mut graph = TaskGraph::new();
        let first = graph.create("a", []).expect("created");
        let second = graph.create("b", [first.clone()]).expect("created");
        let third = graph.create("c", [second]).expect("created");

        assert!(matches!(
            graph.add_dependency(&first, &third),
            Err(TaskError::Cycle { .. })
        ));
    }

    #[test]
    fn adding_the_same_dependency_twice_keeps_one() {
        let mut graph = TaskGraph::new();
        let first = graph.create("a", []).expect("created");
        let second = graph.create("b", [first.clone()]).expect("created");

        graph.add_dependency(&second, &first).expect("added");

        assert_eq!(graph.get(&second).expect("task").deps.len(), 1);
    }

    #[test]
    fn unknown_tasks_are_rejected_by_every_mutation() {
        let mut graph = TaskGraph::new();
        let real = graph.create("a", []).expect("created");

        assert!(matches!(
            graph.set_state(&id("nope"), TaskState::Completed),
            Err(TaskError::Unknown(_))
        ));
        assert!(matches!(
            graph.set_owner(&id("nope"), Some("a1".to_owned())),
            Err(TaskError::Unknown(_))
        ));
        assert!(matches!(
            graph.add_dependency(&id("nope"), &real),
            Err(TaskError::Unknown(_))
        ));
        assert!(matches!(
            graph.add_dependency(&real, &id("nope")),
            Err(TaskError::Unknown(_))
        ));
    }

    #[test]
    fn a_dangling_dependency_counts_as_incomplete() {
        let mut graph = TaskGraph::new();
        let first = graph.create("a", []).expect("created");
        let second = graph.create("b", [first]).expect("created");

        graph.tasks.retain(|task| task.id != id("task-1"));

        assert!(graph.blocked(&second), "a missing dependency is not done");
    }

    #[test]
    fn rows_report_blocked_state_and_owner() {
        let mut graph = TaskGraph::new();
        let first = graph.create("schema", []).expect("created");
        let second = graph.create("parser", [first]).expect("created");

        graph
            .set_owner(&second, Some("a1".to_owned()))
            .expect("owner");

        let rows = graph.rows();

        assert_eq!(rows.len(), 2);
        assert!(!rows[0].blocked);
        assert_eq!(rows[0].title, "schema");
        assert!(rows[1].blocked);
        assert_eq!(rows[1].owner.as_deref(), Some("a1"));
        assert_eq!(graph.completed(), 0);
    }

    #[test]
    fn a_graph_restores_from_its_records() {
        let mut graph = TaskGraph::new();
        let first = graph.create("schema", []).expect("created");
        let second = graph.create("parser", [first.clone()]).expect("created");

        graph
            .set_state(&first, TaskState::Completed)
            .expect("state");
        graph
            .set_owner(&second, Some("a1".to_owned()))
            .expect("owner");

        let restored = TaskGraph::restore(vec![
            TaskRecord::Created {
                title: "schema".to_owned(),
                deps: Vec::new(),
            },
            TaskRecord::Created {
                title: "parser".to_owned(),
                deps: vec![id("task-1")],
            },
            TaskRecord::Updated {
                id: first.clone(),
                state: TaskState::Completed,
                owner: None,
            },
            TaskRecord::Updated {
                id: second.clone(),
                state: TaskState::InProgress,
                owner: Some("a1".to_owned()),
            },
        ]);

        assert_eq!(restored.len(), 2);
        assert_eq!(restored.completed(), 1);
        assert_eq!(
            restored.get(&second).expect("task").state,
            TaskState::InProgress
        );
        assert_eq!(
            restored.get(&second).expect("task").owner.as_deref(),
            Some("a1")
        );
        assert!(!restored.blocked(&second));
    }

    #[test]
    fn restoring_keeps_a_dependency_on_a_missing_task() {
        let restored = TaskGraph::restore(vec![TaskRecord::Created {
            title: "parser".to_owned(),
            deps: vec![id("task-42")],
        }]);

        assert_eq!(restored.len(), 1);
        assert!(
            restored.blocked(&id("task-1")),
            "a missing dependency is incomplete"
        );
        assert_eq!(
            restored.get(&id("task-1")).expect("task").deps,
            vec![id("task-42")]
        );
    }

    #[test]
    fn state_spellings_the_model_may_send_are_accepted() {
        assert_eq!(TaskState::parse("done"), Some(TaskState::Completed));
        assert_eq!(TaskState::parse("in-progress"), Some(TaskState::InProgress));
        assert_eq!(TaskState::parse("PENDING"), Some(TaskState::Pending));
        assert_eq!(TaskState::parse("blocked"), None);
    }

    #[test]
    fn the_summary_line_counts_completed_tasks() {
        let mut graph = TaskGraph::new();

        assert!(graph.summary().is_none());

        let first = graph.create("a", []).expect("created");

        graph.create("b", []).expect("created");
        graph
            .set_state(&first, TaskState::Completed)
            .expect("state");

        assert_eq!(graph.summary().as_deref(), Some("tasks 1/2"));
    }
}
