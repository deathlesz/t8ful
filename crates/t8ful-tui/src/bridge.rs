//! Channels connecting the TUI to the agent driver task.

use std::sync::Arc;

use t8ful_core::event::SequencedEvent;
use tokio::sync::{mpsc, watch};

use t8ful_core::policy::Mode;
use t8ful_core::provider::ReasoningEffort;

use crate::question::{PendingQuestion, TuiQuestionRouter};

/// A credential in transit.
///
/// The key only ever exists in this process: `Debug` is redacted so a log line
/// or a panic message cannot print it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Commands the TUI sends to the agent driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiCommand {
    Submit(String),
    /// Deliver this text at the next safe boundary of the current turn.
    Steer(String),
    /// Queue this text as a follow-up for after a successful turn.
    Queue(String),
    /// Forget the queued follow-ups; the TUI already copied their text back
    /// into the composer.
    Dequeue,
    /// Save and apply an API key for a provider.
    Login {
        provider: String,
        key: Secret,
    },
    /// Remove the stored API key for one provider.
    Logout {
        provider: String,
    },
    Cancel,
    /// Apply a model from the merged catalog. The chosen provider travels with
    /// the id because the same id can exist in both catalogs.
    SelectModel(ModelRow),
    SetReasoningEffort(ReasoningEffort),
    SetMode(Mode),
    /// Attach an image from a path or clipboard bytes.
    Attach {
        path: Option<String>,
        bytes: Option<Vec<u8>>,
        media_type: Option<String>,
    },
    PasteClipboard,
    /// Start a fresh session, optionally switching mode and seeding the first
    /// prompt. A seed runs one turn through the normal queue path.
    NewSession {
        mode: Option<Mode>,
        seed: Option<String>,
    },
    ResumeSession(String),
    /// Refresh the saved-session snapshot the TUI lists and completes from.
    ListSessions,
    /// Load a discovered skill's instructions into the conversation, then
    /// submit this prompt in the same turn when one is attached.
    ActivateSkill {
        name: String,
        prompt: Option<String>,
    },
    /// Refresh the TUI-only background-process snapshot.
    ProcessList,
    /// Print the collected output of one managed process.
    ProcessView(String),
    /// Terminate one managed process.
    ProcessStop(String),
    Quit,
}

/// One background process row for the `/process` report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessView {
    pub id: String,
    pub command: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub elapsed_ms: u128,
    pub total_bytes: u64,
    pub truncated: bool,
    pub output_incomplete: bool,
    pub cleanup_error: Option<String>,
}

/// TUI-only agent rows published by the driver.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentsSnapshot {
    pub agents: Vec<AgentView>,
}

/// One child agent as the dashboard shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentView {
    pub id: String,
    pub role: String,
    /// `running`, `completed`, `failed`, or `cancelled`.
    pub status: String,
    pub elapsed_ms: u128,
    pub steps: u32,
    pub task: Option<String>,
    /// Worktree path when the agent is isolated.
    pub worktree: Option<String>,
    pub branch: Option<String>,
    /// The child's report text once it stops.
    pub report: Option<String>,
}

/// TUI-only task rows published by the driver.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TasksSnapshot {
    pub tasks: Vec<TaskView>,
}

/// One task-graph row as the dashboard shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskView {
    pub id: String,
    pub title: String,
    pub state: String,
    pub blocked: bool,
    pub owner: Option<String>,
    pub deps: Vec<String>,
}

/// TUI-only background-process state. Kept out of
/// [`t8ful_core::event::AgentEvent`] so the headless schema does not change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ProcessSnapshot {
    /// The snapshot has not been published yet.
    #[default]
    Pending,
    Loaded {
        processes: Vec<ProcessView>,
    },
}

/// TUI-only context-window state for the selected model. Kept out of
/// [`t8ful_core::event::AgentEvent`] so the headless schema does not change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextSnapshot {
    pub limit: u64,
}

/// TUI-only reasoning-capability state for the selected model.
///
/// Deliberately separate from [`t8ful_core::event::AgentEvent`] so the headless
/// event schema and journal stay unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ReasoningSnapshot {
    /// The driver has not published capabilities yet.
    #[default]
    Pending,
    Loaded {
        levels: Vec<ReasoningEffort>,
    },
}

/// One model in the merged catalog, with the provider that serves it.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModelRow {
    pub id: String,
    /// `openrouter` or `opencode-go`; `None` when the driver published no
    /// provenance (a test, or a catalog from a single-provider build).
    pub provider: Option<String>,
}

impl ModelRow {
    pub fn new(id: impl Into<String>, provider: Option<String>) -> Self {
        Self {
            id: id.into(),
            provider,
        }
    }

    /// The row as the picker renders it: the id plus the source in brackets.
    pub fn label(&self) -> String {
        match &self.provider {
            Some(provider) if !provider.is_empty() => format!("{} [{}]", self.id, provider),
            _ => self.id.clone(),
        }
    }

    /// What the fuzzy filter matches against, so `/model opencode` narrows to
    /// the Go catalog.
    pub fn filter_text(&self) -> String {
        match &self.provider {
            Some(provider) => format!("{} {}", self.id, provider),
            None => self.id.clone(),
        }
    }
}

/// TUI-only model catalog state. This is deliberately separate from
/// [`t8ful_core::event::AgentEvent`] so the headless event schema does not change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CatalogSnapshot {
    #[default]
    Pending,
    Loaded {
        models: Vec<ModelRow>,
    },
    Failed {
        message: String,
    },
}

/// TUI-only MCP startup state. Deliberately separate from
/// [`t8ful_core::event::AgentEvent`] so the headless event schema does not
/// change: startup settles after the first frame, and turns wait for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum McpSnapshot {
    /// The servers are still connecting; a prompt submitted now is queued.
    #[default]
    Pending,
    Ready {
        /// Servers that started and listed their tools.
        servers: usize,
        /// Tools they contributed to the registry.
        tools: usize,
    },
}

/// One row of the TUI-only saved-session list. Deliberately separate from
/// [`t8ful_core::event::AgentEvent`] so the headless event schema does not change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    /// Unix seconds of the newest message in the session.
    pub last_message_at: u64,
    /// File name of the project root, or its full path when it has none.
    pub project: String,
    /// Whether the session belongs to the current project root.
    pub resumable: bool,
}

/// TUI-only saved-session state. Published by the driver whenever the session
/// list may have changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SessionSnapshot {
    /// The first scan has not finished yet.
    #[default]
    Pending,
    Loaded {
        sessions: Vec<SessionSummary>,
    },
}

/// One skill row in the TUI snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillView {
    pub name: String,
    pub description: String,
    /// `project`, `user`, `system`, or `extra`.
    pub source: String,
    /// Resolved commit when t8ful installed the skill.
    pub commit: Option<String>,
}

/// TUI-only skill state, published once when the driver starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillsSnapshot {
    pub loaded: bool,
    pub skills: Vec<SkillView>,
}

/// TUI-only project file index for `@` mention completion. The driver walks
/// the project in the background so the first frame never waits for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectFilesSnapshot {
    pub loaded: bool,
    pub files: Vec<String>,
}

/// Where one provider's active credential comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AuthKind {
    /// No key is configured; run `/login`.
    #[default]
    Missing,
    /// The provider's environment variable is set and overrides any stored key.
    Environment,
    /// The key in `auth.json` is active.
    Stored,
}

impl AuthKind {
    /// The wording the login modal shows for this source.
    pub fn label(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Stored => "stored in auth.json",
            Self::Missing => "not connected",
        }
    }
}

/// TUI-only credential state, one entry per provider. Deliberately separate
/// from [`t8ful_core::event::AgentEvent`] so the headless event schema does not
/// change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuthSnapshot {
    pub openrouter: AuthKind,
    pub opencode_go: AuthKind,
}

impl AuthSnapshot {
    /// The source for one provider id.
    pub fn for_provider(&self, provider: &str) -> AuthKind {
        if provider.eq_ignore_ascii_case("opencode-go")
            || provider.eq_ignore_ascii_case("opencode")
            || provider.eq_ignore_ascii_case("opencode_go")
        {
            self.opencode_go
        } else {
            self.openrouter
        }
    }

    /// Whether any provider has a credential.
    pub fn any_connected(&self) -> bool {
        self.openrouter != AuthKind::Missing || self.opencode_go != AuthKind::Missing
    }
}

/// The TUI end of the connection plus the question router for the agent.
pub struct TuiBridge {
    pub command_tx: mpsc::UnboundedSender<TuiCommand>,
    pub event_rx: mpsc::UnboundedReceiver<SequencedEvent>,
    pub question_rx: mpsc::UnboundedReceiver<PendingQuestion>,
    pub question_router: Arc<TuiQuestionRouter>,
    pub catalog_rx: watch::Receiver<CatalogSnapshot>,
    pub mcp_rx: watch::Receiver<McpSnapshot>,
    pub reasoning_rx: watch::Receiver<ReasoningSnapshot>,
    pub auth_rx: watch::Receiver<AuthSnapshot>,
    pub sessions_rx: watch::Receiver<SessionSnapshot>,
    pub skills_rx: watch::Receiver<SkillsSnapshot>,
    pub files_rx: watch::Receiver<ProjectFilesSnapshot>,
    pub process_rx: watch::Receiver<ProcessSnapshot>,
    pub agents_rx: watch::Receiver<AgentsSnapshot>,
    pub tasks_rx: watch::Receiver<TasksSnapshot>,
    pub context_rx: watch::Receiver<ContextSnapshot>,
}

/// Build the channels shared by the TUI and the agent task.
pub fn bridge() -> (TuiBridge, TuiEndpoints) {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (question_tx, question_rx) = mpsc::unbounded_channel();
    let question_router = Arc::new(TuiQuestionRouter::new(question_tx));
    let (catalog_tx, catalog_rx) = watch::channel(CatalogSnapshot::Pending);
    let (mcp_tx, mcp_rx) = watch::channel(McpSnapshot::Pending);
    let (reasoning_tx, reasoning_rx) = watch::channel(ReasoningSnapshot::Pending);
    let (auth_tx, auth_rx) = watch::channel(AuthSnapshot::default());
    let (sessions_tx, sessions_rx) = watch::channel(SessionSnapshot::Pending);
    let (skills_tx, skills_rx) = watch::channel(SkillsSnapshot::default());
    let (files_tx, files_rx) = watch::channel(ProjectFilesSnapshot::default());
    let (process_tx, process_rx) = watch::channel(ProcessSnapshot::Pending);
    let (agents_tx, agents_rx) = watch::channel(AgentsSnapshot::default());
    let (tasks_tx, tasks_rx) = watch::channel(TasksSnapshot::default());
    let (context_tx, context_rx) = watch::channel(ContextSnapshot::default());

    (
        TuiBridge {
            command_tx,
            event_rx,
            question_rx,
            question_router,
            catalog_rx,
            mcp_rx,
            reasoning_rx,
            auth_rx,
            sessions_rx,
            skills_rx,
            files_rx,
            process_rx,
            agents_rx,
            tasks_rx,
            context_rx,
        },
        TuiEndpoints {
            command_rx,
            event_tx,
            catalog_tx,
            mcp_tx,
            reasoning_tx,
            auth_tx,
            sessions_tx,
            skills_tx,
            files_tx,
            process_tx,
            agents_tx,
            tasks_tx,
            context_tx,
        },
    )
}

/// The driver-side ends of the same channels.
pub struct TuiEndpoints {
    pub command_rx: mpsc::UnboundedReceiver<TuiCommand>,
    pub event_tx: mpsc::UnboundedSender<SequencedEvent>,
    pub catalog_tx: watch::Sender<CatalogSnapshot>,
    pub mcp_tx: watch::Sender<McpSnapshot>,
    pub reasoning_tx: watch::Sender<ReasoningSnapshot>,
    pub auth_tx: watch::Sender<AuthSnapshot>,
    pub sessions_tx: watch::Sender<SessionSnapshot>,
    pub skills_tx: watch::Sender<SkillsSnapshot>,
    pub files_tx: watch::Sender<ProjectFilesSnapshot>,
    pub process_tx: watch::Sender<ProcessSnapshot>,
    pub agents_tx: watch::Sender<AgentsSnapshot>,
    pub tasks_tx: watch::Sender<TasksSnapshot>,
    pub context_tx: watch::Sender<ContextSnapshot>,
}
