//! Owns child agents: building them, running them, and reporting when they stop.
//!
//! This is the one place that knows how to turn a [`SpawnRequest`] into a running
//! [`Agent`], because it is the only place with the provider, the tool registry,
//! the sandbox, and the session's state directory all in scope.
//!
//! Two invariants are enforced structurally:
//!
//! * A child never gains a tool its parent lacks. The registry handed to a child
//!   is filtered by the role *and* by what the parent actually has.
//! * A child inherits the `Policy` and sandbox of its parent. A role's tool list
//!   is a convenience, not a security boundary: a role with `bash` can write
//!   whatever bash can.

use std::sync::Arc;

use async_trait::async_trait;
use t8ful_core::agent::{Agent, AgentConfig};
use t8ful_core::event::{AgentEvent, EventEmitter};
use t8ful_core::message::Conversation;
use t8ful_core::policy::{Mode, Policy};
use t8ful_core::provider::{Provider, ReasoningProfile};
use t8ful_core::subagent::{
    AgentId, AgentRegistry, AgentReport, ChildAgent, ChildFactory, ChildStatus, RoleProfile,
    SpawnError, SpawnRequest,
};
use t8ful_core::tool::{ToolContext, ToolRegistry};
use tokio_util::sync::CancellationToken;

use crate::child_journal::ChildJournal;
use crate::config::Settings;
use crate::state::StatePaths;
use crate::usage::UsageLedger;

/// Everything a child needs that the root session already holds.
///
/// Cheap to clone: every field is shared, so spawning does not copy a provider
/// or a tool registry.
#[derive(Clone)]
pub struct ChildRuntime {
    pub provider: Arc<dyn Provider>,
    /// The parent's tools and prompts. A child gets a filtered subset of exactly
    /// the tools in this view.
    pub root: Arc<std::sync::RwLock<RootView>>,
    /// The root's tool context: project root, sandbox, limits, question router.
    pub context: ToolContext,
    pub max_output_tokens: u32,
    pub supports_images: Option<bool>,
    pub supports_reasoning: Option<bool>,
    pub reasoning_profile: ReasoningProfile,
    pub roles: std::sync::Arc<std::collections::BTreeMap<String, RoleProfile>>,
    pub settings: std::sync::Arc<Settings>,
    pub paths: StatePaths,
    /// The session every child journal, and the root journal metadata, belongs
    /// to. Shared so a `/resume` or `/new-session` switch is visible to the
    /// spawn path without rebuilding the tools that hold this runtime.
    pub binding: Arc<std::sync::Mutex<SessionBinding>>,
    pub ledger: Option<Arc<UsageLedger>>,
    /// The session's child registry, used to publish live progress.
    pub registry: AgentRegistry,
    /// Shared with the driver so a finished child can wake an idle parent.
    pub events: EventEmitter,
}

impl std::fmt::Debug for ChildRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildRuntime")
            .field("roles", &self.roles.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// The root's tools and prompts, as the child factory sees them now.
///
/// MCP discovery settles after the session is built, so the runtime reads one
/// shared view instead of holding a copy: a child spawned later in the session
/// inherits the tools and prompts the root has now, not the ones it had at
/// startup. Children can only be spawned during a turn, and no turn starts
/// before discovery settles, so the install never tears a running child.
pub struct RootView {
    pub tools: Arc<ToolRegistry>,
    pub system_coding: String,
    pub system_plan: String,
}

/// Which session records go to.
///
/// The spawner, the child factory, and the task-record writer all share one
/// binding, so switching sessions re-points a single object instead of leaving
/// components writing into the session the user left.
#[derive(Debug)]
pub struct SessionBinding {
    pub id: String,
    pub journal: Option<Arc<crate::session::SessionJournal>>,
}

impl ChildRuntime {
    /// The id of the session this runtime currently records into.
    pub fn session_id(&self) -> String {
        self.binding
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .id
            .clone()
    }

    /// The root journal this runtime currently records into.
    pub fn journal(&self) -> Option<Arc<crate::session::SessionJournal>> {
        self.binding
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .journal
            .clone()
    }
}

/// Builds children from the root session's state.
#[derive(Debug)]
pub struct SessionChildFactory {
    runtime: Arc<ChildRuntime>,
    /// The parent agent's id, so a child knows who started it.
    parent: AgentId,
}

impl SessionChildFactory {
    pub fn new(runtime: Arc<ChildRuntime>, parent: AgentId) -> Self {
        Self { runtime, parent }
    }

    /// The tools a role may use: its list, intersected with what the parent has.
    ///
    /// Delegation and user-interaction tools are excluded for every role. A
    /// child is single level by product decision, and it has no user channel, so
    /// these are structurally absent rather than filtered by a role's list.
    pub(crate) fn tools_for(&self, role: &RoleProfile) -> ToolRegistry {
        const DENIED: &[&str] = &[
            "spawn_agents",
            "agent_wait",
            "agent_status",
            "ask_user_question",
            "present_plan",
        ];

        let mut registry = ToolRegistry::new();
        let root = self
            .runtime
            .root
            .read()
            .unwrap_or_else(|poison| poison.into_inner());

        for name in root.tools.names() {
            if DENIED.contains(&name) {
                continue;
            }

            if role.allows(name)
                && let Some(tool) = root.tools.get(name)
            {
                registry.register_arc(tool);
            }
        }

        registry
    }

    /// The system prompt for one child.
    ///
    /// It reuses the parent's mode prompt — so plan-mode rules, the sandbox
    /// description, skills, and project instructions are identical — then states
    /// the child's own position: which agent it is, and that it stops when its
    /// work is done rather than waiting for anything.
    fn system_for(&self, request: &SpawnRequest, role: &RoleProfile) -> String {
        let base = {
            let root = self
                .runtime
                .root
                .read()
                .unwrap_or_else(|poison| poison.into_inner());

            match request.mode {
                Mode::Coding => root.system_coding.clone(),
                Mode::Plan => root.system_plan.clone(),
            }
        };
        let identity = format!(
            "<subagent agent=\"{}\" role=\"{}\">\nYou are a child agent started by \
             {}. Nobody sees your conversation: only your final message reaches the agent that \
             started you, and it should be a compact report of what you found or did, not a \
             transcript of your steps. You cannot ask the user questions or start another agent; if \
             something is genuinely ambiguous, state the assumption you made and continue. When \
             your task is done, stop: do not wait for further instructions, and do not start work \
             you were not asked to do.\n</subagent>",
            request.id, request.role, self.parent,
        );
        let instructions = role.instructions.trim();

        let mut system = format!("{base}\n\n{identity}");

        if !instructions.is_empty() {
            system.push_str(&format!(
                "\n\n<role name=\"{}\">\n{instructions}\n</role>",
                request.role
            ));
        }

        if let Some(worktree) = &request.worktree {
            system.push_str(&format!(
                "\n\n<worktree>\nYou are working in {}, a Git worktree on branch {}. The user's \
                 main working tree is elsewhere and is not affected by your edits. Your changes \
                 stay here; nothing is merged for you. Do not commit unless you were asked \
                 to.\n</worktree>",
                worktree.path.display(),
                worktree.branch.as_deref().unwrap_or("(detached)"),
            ));
        }

        system
    }

    fn conversation_for(request: &SpawnRequest) -> Conversation {
        let mut conversation = Conversation::new();

        conversation.push(t8ful_core::message::Message::user_text(
            request.prompt.clone(),
        ));

        conversation
    }
}

#[async_trait]
impl ChildFactory for SessionChildFactory {
    async fn build(&self, request: &SpawnRequest) -> Result<ChildAgent, SpawnError> {
        let role = self
            .runtime
            .roles
            .get(&request.role)
            .cloned()
            .unwrap_or_default();
        let tools = self.tools_for(&role);
        let system = self.system_for(request, &role);
        let (system_coding, system_plan) = {
            let root = self
                .runtime
                .root
                .read()
                .unwrap_or_else(|poison| poison.into_inner());

            (root.system_coding.clone(), root.system_plan.clone())
        };

        let mut config = AgentConfig::new(request.parent_model.clone(), system.clone());

        config.system_coding = Some(system_coding);
        config.system_plan = Some(system_plan);
        // The window and the budget come from the same model, so a child never
        // pairs a live window with the output budget of a model the parent left.
        config.max_output_tokens = request
            .max_output_tokens
            .or(Some(self.runtime.max_output_tokens));
        config.context_limit = Some(request.context_limit);
        config.supports_images = self.runtime.supports_images;
        config.supports_reasoning = self.runtime.supports_reasoning;
        config.agent_id = request.id.as_str().to_owned();
        // Request ids must stay unique across the tree, so the agent id is part
        // of the namespace rather than just the session.
        config.request_namespace = format!(
            "{}-{}-{}",
            request.session_id,
            std::process::id(),
            request.id.as_str()
        );

        // An isolated child sees its own tree as the project root, so relative
        // paths and the shell's start directory match what it is allowed to touch.
        let mut context = self.runtime.context.clone();

        // A child has no user channel. Clearing the inherited router makes that
        // structural even if a tool registry is ever built differently.
        context.questions = None;
        context.mode = request.mode;

        if let Some(worktree) = &request.worktree {
            context.project_root = worktree.path.clone();
            context.launch_dir = worktree.path.clone();
            context.read_scope = Some(t8ful_core::readscope::ReadScope::new(vec![
                worktree.path.clone(),
            ]));
        }

        // A shared target directory is the difference between N parallel writers
        // paying N cold builds and N writers serializing on one warm cache.
        if let Some(target) = &self.runtime.settings.agents.shared_target_dir {
            context
                .extra_env
                .push(("CARGO_TARGET_DIR".to_owned(), target.display().to_string()));
        }

        let mut agent = Agent::new(
            Arc::clone(&self.runtime.provider),
            Arc::new(tools),
            Policy::new(request.mode),
            config,
            context,
        );

        agent.set_reasoning_effort(request.reasoning_effort);
        agent.set_reasoning_profile(self.runtime.reasoning_profile.clone());

        // One journal instance per child: two would each keep their own file
        // handle and mutex over the same path, which is two writers to one file.
        let journal = Arc::new(ChildJournal::new(
            &self.runtime.paths.session_dir(&request.session_id),
            &request.session_id,
            request.id.as_str(),
            self.runtime.ledger.clone(),
        ));

        journal.materialize().map_err(|error| SpawnError::Refused {
            agent: request.id.to_string(),
            message: format!("cannot create the child journal: {error}"),
        })?;

        let as_trait: Arc<dyn t8ful_core::journal::AgentJournal> = journal.clone();

        agent.set_journal(as_trait);

        let conversation = Self::conversation_for(request);
        let usage = journal.usage();

        Ok(ChildAgent {
            agent,
            conversation,
            journal,
            usage,
        })
    }
}

/// What running one child produced, ready to publish.
pub struct ChildOutcome {
    pub report: AgentReport,
}

/// Run one child to completion, then publish its report.
///
/// The child's events are wrapped with [`AgentEvent::AgentEvent`] so a frontend
/// can attribute them and the root transcript shows one card instead of the
/// child's whole page. The parent is never interrupted: the report goes onto its
/// notice queue.
pub async fn run_child(
    runtime: Arc<ChildRuntime>,
    request: SpawnRequest,
    cancel: CancellationToken,
) -> ChildOutcome {
    let started = std::time::Instant::now();
    let id = request.id.clone();
    let role = request.role.clone();
    let task = request.task.clone();
    let worktree = request.worktree.clone();

    let factory = SessionChildFactory::new(Arc::clone(&runtime), AgentId::root());

    let mut child = match factory.build(&request).await {
        Ok(child) => child,
        Err(SpawnError::Refused { message, .. }) => {
            return ChildOutcome {
                report: AgentReport {
                    agent: id,
                    role,
                    status: ChildStatus::Failed,
                    summary: format!("could not start: {message}"),
                    steps: 0,
                    usage: t8ful_core::usage::Usage::default(),
                    task,
                    worktree,
                    elapsed_ms: started.elapsed().as_millis(),
                },
            };
        }
    };

    // The child's stream is nested under its id, so the parent card stays the
    // only root-level trace of the child.
    let usage = std::sync::Arc::clone(&child.usage);

    let child_events = NestedEmitter {
        inner: runtime.events.clone(),
        agent: id.clone(),
        registry: runtime.registry.clone(),
        steps: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
    };

    let cancel = cancel.child_token();
    let outcome = child
        .agent
        .run(
            &mut child.conversation,
            &child_events.event_emitter(),
            &cancel,
        )
        .await;

    let steps = match &outcome {
        Ok(turn) => turn.steps,
        Err(_) => 0,
    };
    // Any finished turn counts as completed: a child that hit its step limit or
    // presented a plan still did the work it was given and still reported. Only a
    // failed or cancelled turn is anything else.
    let (status, failure) = match &outcome {
        Ok(_) => (ChildStatus::Completed, None),
        Err(t8ful_core::AgentError::Cancelled) => (ChildStatus::Cancelled, None),
        Err(error) => (ChildStatus::Failed, Some(error.to_string())),
    };

    // The final assistant text is the report. A child that failed still reports
    // what it managed to say, which is usually the most useful part.
    let summary = child
        .conversation
        .messages()
        .iter()
        .rev()
        .find(|message| message.role == t8ful_core::message::Role::Assistant)
        .map(|message| message.text().trim().to_owned())
        .unwrap_or_default();

    // A failed child must say why. The parent cannot see the child's transcript,
    // so an empty summary would leave the failure unexplainable.
    let summary = match (summary.is_empty(), failure) {
        (false, _) => summary,
        (true, Some(error)) => format!("the agent failed: {error}"),
        (true, None) => match status {
            ChildStatus::Cancelled => "the agent was cancelled before it reported".to_owned(),
            _ => String::new(),
        },
    };

    let usage = usage
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();

    ChildOutcome {
        report: AgentReport {
            agent: id,
            role,
            status,
            summary,
            steps,
            usage,
            task,
            worktree,
            elapsed_ms: started.elapsed().as_millis(),
        },
    }
}

/// Wraps a child's events so they arrive attributed to it.
struct NestedEmitter {
    inner: EventEmitter,
    agent: AgentId,
    registry: AgentRegistry,
    /// Steps seen so far, published to the registry as the child works.
    steps: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl NestedEmitter {
    fn event_emitter(&self) -> EventEmitter {
        // The core emitter is a plain channel; nesting is done by the forwarder
        // this returns, so `Agent` keeps its single `send` path.
        let (emitter, mut receiver) = EventEmitter::channel();
        let inner = self.inner.clone();
        let agent = self.agent.clone();
        let registry = self.registry.clone();
        let steps = std::sync::Arc::clone(&self.steps);

        tokio::spawn(async move {
            while let Some(sequenced) = receiver.recv().await {
                // Each `ModelStarted` is one more step the child has taken, so
                // `agent_status` shows real progress instead of a flat zero.
                if matches!(sequenced.event, AgentEvent::ModelStarted { .. }) {
                    let count = steps.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;

                    registry.set_steps(&agent, count);
                }

                inner.send(AgentEvent::AgentEvent {
                    agent: agent.to_string(),
                    event: Box::new(sequenced.event),
                });
            }
        });

        emitter
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use t8ful_core::AgentLimits;
    use t8ful_core::ProviderError;
    use t8ful_core::event::EventEmitter;
    use t8ful_core::policy::Mode;
    use t8ful_core::provider::{
        ModelInfo, Provider, ProviderRequest, ProviderStream, ReasoningEffort, ReasoningProfile,
    };
    use t8ful_core::subagent::{AgentId, Isolation, RoleProfile, SpawnRequest, Worktree};
    use t8ful_core::tool::{
        Tool, ToolCall, ToolContext, ToolError, ToolOutput, ToolRegistry, ToolSpec,
    };

    use super::{ChildRuntime, SessionChildFactory};

    /// A provider that never streams: these tests build agents, they do not run
    /// them.
    struct NoProvider;

    #[async_trait]
    impl Provider for NoProvider {
        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }

        fn stream(&self, _request: ProviderRequest) -> ProviderStream {
            Box::pin(futures_util::stream::empty())
        }
    }

    #[derive(Debug)]
    struct Named(&'static str);

    #[async_trait]
    impl Tool for Named {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.to_owned(),
                description: "test tool".to_owned(),
                parameters: serde_json::json!({ "type": "object" }),
            }
        }

        async fn execute(
            &self,
            _call: &ToolCall,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("ok"))
        }
    }

    fn registry(names: &[&'static str]) -> ToolRegistry {
        let mut registry = ToolRegistry::new();

        for name in names {
            registry.register(Named(name));
        }

        registry
    }

    fn runtime(tools: ToolRegistry) -> Arc<ChildRuntime> {
        Arc::new(ChildRuntime {
            provider: Arc::new(NoProvider),
            root: Arc::new(std::sync::RwLock::new(super::RootView {
                tools: Arc::new(tools),
                system_coding: "coding rules".to_owned(),
                system_plan: "plan rules".to_owned(),
            })),
            context: ToolContext::new("/work".into(), "/work".into()),
            max_output_tokens: 8_192,
            supports_images: None,
            supports_reasoning: None,
            reasoning_profile: ReasoningProfile::unknown(),
            roles: Arc::new(std::collections::BTreeMap::new()),
            settings: Arc::new(crate::config::Settings {
                agents: crate::config::AgentSettings {
                    limits: AgentLimits::default(),
                    ..crate::config::AgentSettings::default()
                },
                ..test_settings()
            }),
            paths: crate::state::StatePaths {
                root: "/state/t8ful".into(),
                sessions: "/state/t8ful/sessions".into(),
                usage: "/state/t8ful/usage".into(),
            },
            binding: Arc::new(std::sync::Mutex::new(super::SessionBinding {
                id: "session-1".to_owned(),
                journal: None,
            })),
            ledger: None,
            registry: t8ful_core::AgentRegistry::new(t8ful_core::AgentLimits::default()),
            events: EventEmitter::channel().0,
        })
    }

    pub(super) fn test_settings() -> crate::config::Settings {
        crate::config::Settings {
            agents: crate::config::AgentSettings::default(),
            root: crate::root::RootInfo {
                path: "/work".into(),
                source: crate::root::RootSource::Git,
            },
            launch_dir: "/work".into(),
            model: Some("vendor/model".to_owned()),
            base_url: "http://127.0.0.1".to_owned(),
            api_key: Some("key".to_owned()),
            mode: Mode::Coding,
            reasoning_effort: ReasoningEffort::Default,
            reasoning_models: std::collections::BTreeMap::new(),
            max_steps: None,
            max_output_tokens: None,
            context_limit: None,
            context_limit_fallback: 128_000,
            max_image_encoded_bytes: 1024,
            mouse: true,
            theme: t8ful_tui::ThemeName::Pi,
            show_reasoning: false,
            notifications: true,
            notify_command: Vec::new(),
            notify_on_exit: true,
            catalog_timeout: std::time::Duration::from_secs(1),
            system_prompt_extra: String::new(),
            default_timeout: std::time::Duration::from_secs(1),
            max_output_bytes: 1024,
            deny_env: Vec::new(),
            tool_disabled: std::collections::BTreeSet::new(),
            skill_roots: Vec::new(),
            mcp_servers: Vec::new(),
            mcp_startup_timeout: t8ful_mcp::DEFAULT_STARTUP_TIMEOUT,
            config_sources: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn request(isolation: Isolation, worktree: Option<Worktree>) -> SpawnRequest {
        SpawnRequest {
            id: AgentId::new("a1"),
            role: "general".to_owned(),
            prompt: "look at the parser".to_owned(),
            task: None,
            isolation,
            parent_model: "vendor/model".to_owned(),
            mode: Mode::Coding,
            context_limit: 128_000,
            max_output_tokens: Some(8_192),
            reasoning_effort: ReasoningEffort::Default,
            session_id: "session-1".to_owned(),
            worktree,
        }
    }

    fn factory(runtime: Arc<ChildRuntime>) -> SessionChildFactory {
        SessionChildFactory::new(runtime, AgentId::root())
    }

    #[test]
    fn a_child_gets_only_the_tools_both_the_role_and_the_parent_allow() {
        let runtime = runtime(registry(&["read_file", "write_file", "bash"]));
        let factory = factory(Arc::clone(&runtime));
        let role = RoleProfile {
            tools: vec!["read_file".to_owned(), "web_search".to_owned()],
            ..RoleProfile::default()
        };

        let tools = factory.tools_for(&role);

        assert_eq!(tools.names(), vec!["read_file"]);
        assert!(
            tools.get("web_search").is_none(),
            "a role cannot name a tool the parent lacks"
        );
    }

    #[test]
    fn a_role_without_a_tool_list_gets_every_parent_tool() {
        let runtime = runtime(registry(&["read_file", "bash"]));
        let factory = factory(runtime);

        let tools = factory.tools_for(&RoleProfile::default());

        assert_eq!(tools.names(), vec!["bash", "read_file"]);
    }

    #[test]
    fn a_late_install_reaches_the_tools_and_prompts_a_child_inherits() {
        let runtime = runtime(registry(&["read_file"]));
        let factory = factory(Arc::clone(&runtime));
        let role = RoleProfile::default();

        assert_eq!(factory.tools_for(&role).names(), vec!["read_file"]);

        // MCP discovery settles after the runtime was built. The install
        // rewrites the one shared view, so a child spawned now inherits the
        // wider registry and the prompt rebuilt from it.
        {
            let mut view = runtime.root.write().expect("root view");

            view.tools = Arc::new(registry(&["read_file", "mcp__fake__echo"]));
            view.system_coding = "coding with mcp".to_owned();
            view.system_plan = "plan with mcp".to_owned();
        }

        assert_eq!(
            factory.tools_for(&role).names(),
            vec!["mcp__fake__echo", "read_file"]
        );

        let system = factory.system_for(&request(Isolation::Shared, None), &role);

        assert!(system.starts_with("coding with mcp"), "{system}");
    }

    #[test]
    fn a_child_never_gets_delegation_or_user_interaction_tools() {
        let runtime = runtime(registry(&[
            "read_file",
            "spawn_agents",
            "agent_wait",
            "agent_status",
            "ask_user_question",
            "present_plan",
            "task_list",
        ]));
        let factory = factory(runtime);
        let role = RoleProfile {
            tools: vec![
                "spawn_agents".to_owned(),
                "agent_wait".to_owned(),
                "task_list".to_owned(),
            ],
            ..RoleProfile::default()
        };

        let tools = factory.tools_for(&role);

        assert_eq!(tools.names(), vec!["task_list"]);
    }

    #[test]
    fn a_child_prompt_states_its_identity_and_the_stop_rule() {
        let runtime = runtime(registry(&["read_file"]));
        let factory = factory(runtime);

        let system = factory.system_for(&request(Isolation::Shared, None), &RoleProfile::default());

        assert!(system.starts_with("coding rules"), "{system}");
        assert!(system.contains("<subagent agent=\"a1\" role=\"general\">"));
        assert!(system.contains("started by root"));
        assert!(system.contains("only your final message reaches the agent"));
        assert!(system.contains("You cannot ask the user questions"));
        assert!(system.contains("When your task is done, stop"));
        assert!(system.contains("</subagent>"));
    }

    #[test]
    fn a_plan_mode_child_receives_the_plan_prompt() {
        let runtime = runtime(registry(&["read_file"]));
        let factory = factory(runtime);
        let mut request = request(Isolation::Shared, None);
        request.mode = Mode::Plan;

        let system = factory.system_for(&request, &RoleProfile::default());

        assert!(system.starts_with("plan rules"), "{system}");
        assert!(!system.contains("coding rules"));
    }

    #[test]
    fn a_role_adds_its_instructions_as_a_named_section() {
        let runtime = runtime(registry(&["read_file"]));
        let factory = factory(runtime);
        let role = RoleProfile {
            instructions: "Only read. Never edit.".to_owned(),
            ..RoleProfile::default()
        };

        let system = factory.system_for(&request(Isolation::Shared, None), &role);

        assert!(system.contains("<role name=\"general\">"));
        assert!(system.contains("Only read. Never edit."));
        assert!(system.contains("</role>"));
    }

    #[test]
    fn a_role_without_instructions_adds_no_section() {
        let runtime = runtime(registry(&["read_file"]));
        let factory = factory(runtime);

        let system = factory.system_for(&request(Isolation::Shared, None), &RoleProfile::default());

        assert!(!system.contains("<role name="));
    }

    #[test]
    fn an_isolated_child_is_told_where_its_worktree_is() {
        let runtime = runtime(registry(&["read_file"]));
        let factory = factory(runtime);
        let worktree = Worktree {
            path: "/state/worktrees/a1".into(),
            branch: Some("t8ful/s1/a1".to_owned()),
            base_commit: Some("abc123".to_owned()),
            ignored_paths: Vec::new(),
        };

        let system = factory.system_for(
            &request(Isolation::Worktree, Some(worktree)),
            &RoleProfile::default(),
        );

        assert!(system.contains("<worktree>"));
        assert!(system.contains("/state/worktrees/a1"));
        assert!(system.contains("branch t8ful/s1/a1"));
        assert!(system.contains("not affected by your edits"));
        assert!(system.contains("Do not commit unless you were asked"));
    }

    #[test]
    fn a_shared_child_gets_no_worktree_section() {
        let runtime = runtime(registry(&["read_file"]));
        let factory = factory(runtime);

        let system = factory.system_for(&request(Isolation::Shared, None), &RoleProfile::default());

        assert!(!system.contains("<worktree>"));
    }

    #[test]
    fn a_child_starts_from_its_prompt_alone() {
        let request = request(Isolation::Shared, None);
        let conversation = SessionChildFactory::conversation_for(&request);

        assert_eq!(conversation.len(), 1);
        assert_eq!(conversation.messages()[0].text(), "look at the parser");
    }
}

#[cfg(test)]
mod shared_target_tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use t8ful_core::ProviderError;
    use t8ful_core::event::EventEmitter;
    use t8ful_core::policy::Mode;
    use t8ful_core::provider::{
        ModelInfo, Provider, ProviderRequest, ProviderStream, ReasoningEffort, ReasoningProfile,
    };
    use t8ful_core::subagent::{AgentId, ChildFactory, Isolation, SpawnRequest};
    use t8ful_core::tool::{ToolContext, ToolRegistry};

    use super::{ChildRuntime, SessionChildFactory};

    struct NoProvider;

    #[async_trait]
    impl Provider for NoProvider {
        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }

        fn stream(&self, _request: ProviderRequest) -> ProviderStream {
            Box::pin(futures_util::stream::empty())
        }
    }

    fn runtime(
        state: &std::path::Path,
        shared_target_dir: Option<std::path::PathBuf>,
    ) -> Arc<ChildRuntime> {
        let agents = crate::config::AgentSettings {
            shared_target_dir,
            ..crate::config::AgentSettings::default()
        };

        Arc::new(ChildRuntime {
            provider: Arc::new(NoProvider),
            root: Arc::new(std::sync::RwLock::new(super::RootView {
                tools: Arc::new(ToolRegistry::new()),
                system_coding: "coding".to_owned(),
                system_plan: "plan".to_owned(),
            })),
            context: ToolContext::new("/work".into(), "/work".into()),
            max_output_tokens: 8_192,
            supports_images: None,
            supports_reasoning: None,
            reasoning_profile: ReasoningProfile::unknown(),
            roles: Arc::new(std::collections::BTreeMap::new()),
            settings: Arc::new(crate::config::Settings {
                agents,
                ..super::tests::test_settings()
            }),
            paths: crate::state::StatePaths {
                root: state.to_path_buf(),
                sessions: state.join("sessions"),
                usage: state.join("usage"),
            },
            binding: Arc::new(std::sync::Mutex::new(super::SessionBinding {
                id: "session-1".to_owned(),
                journal: None,
            })),
            ledger: None,
            registry: t8ful_core::AgentRegistry::new(t8ful_core::AgentLimits::default()),
            events: EventEmitter::channel().0,
        })
    }

    fn request() -> SpawnRequest {
        SpawnRequest {
            id: AgentId::new("a1"),
            role: "general".to_owned(),
            prompt: "build it".to_owned(),
            task: None,
            isolation: Isolation::Shared,
            parent_model: "vendor/model".to_owned(),
            mode: Mode::Coding,
            context_limit: 128_000,
            max_output_tokens: Some(8_192),
            reasoning_effort: ReasoningEffort::Default,
            session_id: "session-1".to_owned(),
            worktree: None,
        }
    }

    #[tokio::test]
    async fn a_child_inherits_the_shared_target_directory_when_configured() {
        let dir = tempfile::tempdir().expect("state dir");
        let runtime = runtime(dir.path(), Some("/shared/target".into()));
        let factory = SessionChildFactory::new(runtime, AgentId::root());

        let child = factory.build(&request()).await.expect("built");

        assert!(
            child
                .agent
                .tool_context()
                .extra_env
                .iter()
                .any(|(name, value)| name == "CARGO_TARGET_DIR" && value == "/shared/target"),
            "the child's shell inherits CARGO_TARGET_DIR"
        );
    }

    #[tokio::test]
    async fn a_child_gets_no_target_override_by_default() {
        let dir = tempfile::tempdir().expect("state dir");
        let runtime = runtime(dir.path(), None);
        let factory = SessionChildFactory::new(runtime, AgentId::root());

        let child = factory.build(&request()).await.expect("built");

        assert!(child.agent.tool_context().extra_env.is_empty());
    }
}
