//! Driver task behind the TUI.
//!
//! The TUI only sends user intent; this task owns the conversation, queue,
//! attachments, model/mode changes, and journal writes. Agent events are
//! forwarded by a separate task so streaming never blocks the screen.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use t8ful_core::agent::{TurnFinish, TurnOutcome};
use t8ful_core::content::ImageInput;
use t8ful_core::event::{
    AgentEvent, EventEmitter, HistoryEntry, HistoryRole, SequencedEvent, UserMessageSource,
};
use t8ful_core::journal::UsageRecord;
use t8ful_core::message::Message;
use t8ful_core::notice::{Notice, ProcessNotice};
use t8ful_core::policy::Mode;
use t8ful_core::tool::ToolStatus;
use t8ful_tools::{ManagedProcesses, ProcessExit, ProcessState};
use t8ful_tui::bridge::{
    AuthSnapshot, CatalogSnapshot, ContextSnapshot, McpSnapshot, ProcessSnapshot, ProcessView,
    ProjectFilesSnapshot, ReasoningSnapshot, SessionSnapshot, SessionSummary, SkillView,
    SkillsSnapshot, TuiCommand, TuiEndpoints,
};
use t8ful_tui::clipboard::{ClipboardImageSource, SystemClipboard};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::compose::{McpStartup, Session};
use crate::images;
use crate::providers::Catalog;
use crate::session::{QueuedInput, SessionId, SessionJournal};
use crate::state::StatePaths;
use crate::usage::UsageLedger;

/// Catalog failure text shown while no credential is configured.
const NOT_CONNECTED: &str = "not connected; run /login";

pub struct DriverInit {
    /// The one event stream this session writes to. The spawner shares it, so
    /// child events reach the same forwarder as the root's.
    pub emitter: EventEmitter,
    pub event_rx: mpsc::UnboundedReceiver<SequencedEvent>,
    pub paths: StatePaths,
    pub ledger: Arc<UsageLedger>,
    pub project_root: PathBuf,
    pub queued: Vec<QueuedInput>,
    pub queue_paused: bool,
    pub attachments: Vec<ImageInput>,
    pub session_id: String,
    pub max_image_encoded_bytes: usize,
    /// Wake an idle agent when a background process exits.
    pub notify_on_exit: bool,
    /// The OpenRouter credential in effect, so a Go login keeps it.
    pub openrouter_api_key: Option<String>,
    /// The OpenCode Go credential in effect.
    pub opencode_go_api_key: Option<String>,
    /// Where each credential came from at startup.
    pub sources: crate::AuthSources,
}

struct Runtime {
    paths: StatePaths,
    ledger: Arc<UsageLedger>,
    project_root: PathBuf,
    /// Canonical form of `project_root`, shared by every session comparison.
    canonical_root: PathBuf,
    sessions: watch::Sender<SessionSnapshot>,
    skills: watch::Sender<SkillsSnapshot>,
    files: watch::Sender<ProjectFilesSnapshot>,
    processes: watch::Sender<ProcessSnapshot>,
    context: watch::Sender<ContextSnapshot>,
    /// Guards against overlapping project walks when turns finish quickly.
    file_scan: Arc<AtomicBool>,
    max_image_encoded_bytes: usize,
    queue: VecDeque<QueuedInput>,
    queue_paused: bool,
    pending_images: Vec<ImageInput>,
    pending_model: Option<t8ful_tui::bridge::ModelRow>,
    pending_mode: Option<Mode>,
    /// A new session requested mid-turn, applied at the boundary.
    pending_new_session: Option<(Option<Mode>, Option<String>)>,
    /// Exits of background processes that still need a notice.
    process_notices: Vec<ProcessNotice>,
    notify_on_exit: bool,
    quit: bool,
    /// The credentials in effect, so one provider's login keeps the other's.
    openrouter_api_key: Option<String>,
    opencode_go_api_key: Option<String>,
    /// Where each active credential came from, seeded at startup.
    sources: crate::AuthSources,
    /// Set when the user cancelled a turn.
    ///
    /// A cancel must leave the session idle until the user speaks again. Without
    /// this, the cancelled children still report, their notices wake the driver,
    /// and a fresh turn starts by itself — so a cancel looks like it did nothing
    /// while quietly spending money.
    cancelled_by_user: bool,
}

impl Runtime {
    /// The credential state both providers are in.
    fn auth_snapshot(&self) -> AuthSnapshot {
        AuthSnapshot {
            openrouter: auth_kind(self.sources.openrouter),
            opencode_go: auth_kind(self.sources.opencode_go),
        }
    }

    fn queue_snapshot(&self) -> Vec<String> {
        self.queue.iter().map(|item| item.text.clone()).collect()
    }

    fn emit_queue(&self, events: &EventEmitter) {
        events.send(AgentEvent::QueueUpdated {
            items: self.queue_snapshot(),
            paused: self.queue_paused,
        });
    }

    fn next_queued(&mut self) -> Option<QueuedInput> {
        if self.queue_paused {
            return None;
        }

        self.queue.pop_front()
    }

    async fn queue_add(
        &mut self,
        text: String,
        journal: &Option<Arc<SessionJournal>>,
        events: &EventEmitter,
    ) {
        let id = format!("queue-{}", crate::session::now_millis());
        let item = QueuedInput {
            id: id.clone(),
            text,
        };

        self.queue.push_back(item);

        if let Some(journal) = journal
            && let Err(error) = journal
                .queue_add(id, self.queue.back().unwrap().text.clone())
                .await
        {
            events.send(AgentEvent::Error {
                message: format!("could not record the queued follow-up: {error}"),
            });
        }

        self.emit_queue(events);
    }

    /// Forget every queued follow-up: the TUI already moved their text back
    /// into the composer, so nothing is lost.
    async fn queue_clear(&mut self, journal: &Option<Arc<SessionJournal>>, events: &EventEmitter) {
        self.queue.clear();
        self.queue_paused = false;

        if let Some(journal) = journal {
            let _ = journal.queue_clear().await;
            let _ = journal.queue_paused(false).await;
        }

        self.emit_queue(events);
    }

    async fn attach(
        &mut self,
        path: Option<String>,
        bytes: Option<Vec<u8>>,
        media_type: Option<String>,
        journal: &Option<Arc<SessionJournal>>,
        events: &EventEmitter,
    ) {
        let Some(journal) = journal else {
            events.send(AgentEvent::Warning {
                message: "attachments require a saved session".to_owned(),
            });

            return;
        };

        let result = match (path, bytes) {
            (Some(path), _) => images::attach_path(
                &self.paths,
                journal.id(),
                std::path::Path::new(&path),
                self.max_image_encoded_bytes,
            ),
            (None, Some(bytes)) => images::attach_bytes(
                &self.paths,
                journal.id(),
                &bytes,
                media_type.as_deref(),
                self.max_image_encoded_bytes,
            ),
            (None, None) => Err("no image data was provided".to_owned()),
        };

        match result {
            Ok(image) => {
                if let Err(error) = journal.record_attachment(image.clone()).await {
                    events.send(AgentEvent::Error {
                        message: format!("could not record the attachment: {error}"),
                    });

                    return;
                }

                events.send(AgentEvent::AttachmentAdded {
                    path: image.path.display().to_string(),
                    media_type: image.media_type.clone(),
                    width: image.width,
                    height: image.height,
                    bytes: image.bytes,
                });
                self.pending_images.push(image);
            }
            Err(error) => {
                events.send(AgentEvent::Warning { message: error });
            }
        }
    }
}

/// Map one credential source onto the snapshot the TUI renders.
fn auth_kind(source: crate::auth::AuthSource) -> t8ful_tui::AuthKind {
    match source {
        crate::auth::AuthSource::Environment => t8ful_tui::AuthKind::Environment,
        crate::auth::AuthSource::Stored => t8ful_tui::AuthKind::Stored,
        crate::auth::AuthSource::Missing => t8ful_tui::AuthKind::Missing,
    }
}

/// The catalog fetch currently in flight, plus the channels that publish it.
///
/// A credential change replaces the whole thing: aborting the old task and
/// dropping its receiver guarantees a stale result from the previous provider
/// can never be published against the new one.
struct CatalogRun {
    pending: bool,
    generation: u64,
    task: Option<tokio::task::JoinHandle<()>>,
    result: Option<oneshot::Receiver<Result<Catalog, String>>>,
    snapshot_tx: watch::Sender<CatalogSnapshot>,
    reasoning_tx: watch::Sender<ReasoningSnapshot>,
    auth_tx: watch::Sender<AuthSnapshot>,
    context_tx: watch::Sender<ContextSnapshot>,
}

impl CatalogRun {
    fn new(
        snapshot_tx: watch::Sender<CatalogSnapshot>,
        reasoning_tx: watch::Sender<ReasoningSnapshot>,
        auth_tx: watch::Sender<AuthSnapshot>,
        context_tx: watch::Sender<ContextSnapshot>,
    ) -> Self {
        Self {
            pending: false,
            generation: 0,
            task: None,
            result: None,
            snapshot_tx,
            reasoning_tx,
            auth_tx,
            context_tx,
        }
    }

    /// Abort the fetch in flight, if any, and forget its result channel.
    fn abort(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }

        self.result = None;
        self.pending = false;
        self.generation += 1;
    }

    /// Fetch the catalog for the session's current providers.
    ///
    /// The routing handle is shared, so the task does not borrow the session;
    /// the result arrives on a one-shot channel the loop can poll alongside
    /// commands.
    fn restart(&mut self, session: &Session) {
        self.abort();

        let (result_tx, result_rx) = oneshot::channel();
        let routing = session.routing.clone();
        let provider = Arc::clone(&session.provider);
        let catalog_timeout = session.catalog_timeout;

        self.pending = true;
        self.result = Some(result_rx);
        self.task = Some(tokio::spawn(async move {
            let result = match routing {
                Some(routing) => {
                    match tokio::time::timeout(catalog_timeout, routing.catalog(catalog_timeout))
                        .await
                    {
                        Ok(catalog) => crate::compose::settle_catalog(catalog),
                        Err(_) => Err(format!(
                            "model catalog request timed out after {} ms",
                            catalog_timeout.as_millis()
                        )),
                    }
                }
                None => crate::compose::fetch_catalog(provider, catalog_timeout).await,
            };

            let _ = result_tx.send(result);
        }));
    }

    fn finish(
        &mut self,
        session: &mut Session,
        result: Result<Catalog, String>,
        events: &EventEmitter,
    ) {
        self.result = None;
        self.pending = false;
        self.task = None;
        self.generation += 1;

        publish_catalog(
            session,
            result,
            &self.snapshot_tx,
            &self.reasoning_tx,
            events,
        );
        publish_context_tx(&self.context_tx, session);
    }

    /// Report a session that cannot fetch a catalog at all.
    fn publish_unauthenticated(&mut self, session: &mut Session) {
        self.abort();
        let _ = session.apply_catalog(Err(NOT_CONNECTED.to_owned()));
        let _ = self.snapshot_tx.send(CatalogSnapshot::Failed {
            message: NOT_CONNECTED.to_owned(),
        });
        publish_context_tx(&self.context_tx, session);
    }
}

/// The MCP startup currently in flight, plus the channel that publishes it.
///
/// Startup is a background task for the same reason the catalog is: a remote
/// handshake must not delay the first frame. Nothing is installed until it
/// settles, and turns are gated meanwhile so no request sees a partial tool
/// list.
struct McpRun {
    pending: bool,
    generation: u64,
    task: Option<tokio::task::JoinHandle<()>>,
    result: Option<oneshot::Receiver<(t8ful_mcp::McpHub, Vec<String>)>>,
    snapshot_tx: watch::Sender<McpSnapshot>,
}

impl McpRun {
    fn new(snapshot_tx: watch::Sender<McpSnapshot>) -> Self {
        Self {
            pending: false,
            generation: 0,
            task: None,
            result: None,
            snapshot_tx,
        }
    }

    /// Abort the startup in flight, if any, and forget its result channel.
    fn abort(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }

        self.result = None;
        self.pending = false;
        self.generation += 1;
    }

    /// Start the configured servers off the main loop.
    fn restart(&mut self, startup: McpStartup) {
        self.abort();

        let (result_tx, result_rx) = oneshot::channel();

        self.pending = true;
        self.result = Some(result_rx);
        let _ = self.snapshot_tx.send(McpSnapshot::Pending);
        self.task = Some(tokio::spawn(async move {
            let McpStartup {
                configs,
                context,
                timeout,
            } = startup;
            let mut warnings = Vec::new();
            let hub = t8ful_mcp::McpHub::start(configs, &context, timeout, &mut warnings).await;

            let _ = result_tx.send((hub, warnings));
        }));
    }

    /// Record a settled startup and publish what it contributed.
    fn finish(&mut self, servers: usize, tools: usize) {
        self.result = None;
        self.pending = false;
        self.task = None;
        self.generation += 1;
        let _ = self.snapshot_tx.send(McpSnapshot::Ready { servers, tools });
    }
}

/// What a prompt submitted now is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Catalog,
    Mcp,
    Both,
}

impl Pending {
    fn message(self) -> &'static str {
        match self {
            Self::Catalog => "waiting for model metadata before running this prompt",
            Self::Mcp => "waiting for MCP servers before running this prompt",
            Self::Both => "waiting for model metadata and MCP servers before running this prompt",
        }
    }
}

/// The answer to one wait on background discovery.
type McpResult = Result<(t8ful_mcp::McpHub, Vec<String>), oneshot::error::RecvError>;

enum DiscoveryEvent {
    Catalog(Result<Result<Catalog, String>, oneshot::error::RecvError>),
    Mcp(McpResult),
    Command(Option<TuiCommand>),
}

/// Catalog and MCP discovery, which both run off the main loop.
struct Discovery {
    catalog: CatalogRun,
    mcp: McpRun,
}

impl Discovery {
    fn pending(&self) -> Option<Pending> {
        match (self.catalog.pending, self.mcp.pending) {
            (true, true) => Some(Pending::Both),
            (true, false) => Some(Pending::Catalog),
            (false, true) => Some(Pending::Mcp),
            (false, false) => None,
        }
    }

    /// Abandon a pending MCP startup and publish an empty hub.
    ///
    /// A session switch stops the previous session's servers, so the gate must
    /// lift even if the startup never settled.
    fn abort_mcp(&mut self) {
        self.mcp.abort();
        let _ = self.mcp.snapshot_tx.send(McpSnapshot::Ready {
            servers: 0,
            tools: 0,
        });
    }

    /// Wait for whichever discovery result is in flight, or for a command.
    ///
    /// A receiver goes back only when its run is still the one that was
    /// pending: a credential change restarts the catalog and a session switch
    /// aborts MCP, so a stale result must never be delivered against either.
    async fn wait(
        &mut self,
        command_rx: &mut mpsc::UnboundedReceiver<TuiCommand>,
    ) -> DiscoveryEvent {
        let catalog_generation = self.catalog.generation;
        let mcp_generation = self.mcp.generation;
        let mut catalog_rx = self.catalog.result.take();
        let mut mcp_rx = self.mcp.result.take();

        debug_assert!(catalog_rx.is_some() || mcp_rx.is_some());

        let event = tokio::select! {
            result = async { catalog_rx.as_mut().expect("a pending catalog has a receiver").await },
                if catalog_rx.is_some() => DiscoveryEvent::Catalog(result),
            result = async { mcp_rx.as_mut().expect("a pending MCP startup has a receiver").await },
                if mcp_rx.is_some() => DiscoveryEvent::Mcp(result),
            command = command_rx.recv() => DiscoveryEvent::Command(command),
        };

        if self.catalog.generation == catalog_generation {
            self.catalog.result = catalog_rx;
        }

        if self.mcp.generation == mcp_generation {
            self.mcp.result = mcp_rx;
        }

        event
    }
}

pub async fn drive(mut session: Session, endpoints: TuiEndpoints, init: DriverInit) {
    let TuiEndpoints {
        mut command_rx,
        event_tx,
        catalog_tx,
        reasoning_tx,
        auth_tx,
        sessions_tx,
        skills_tx,
        files_tx,
        process_tx,
        agents_tx,
        tasks_tx,
        context_tx,
        mcp_tx,
    } = endpoints;
    let DriverInit {
        emitter,
        mut event_rx,
        paths,
        ledger,
        project_root,
        queued,
        queue_paused,
        attachments,
        session_id,
        max_image_encoded_bytes,
        notify_on_exit,
        openrouter_api_key,
        opencode_go_api_key,
        sources,
    } = init;

    // The forwarder also keeps the agent and task snapshots live. It sees every
    // event, so it is the one place that can refresh them without the turn loop
    // polling, and it owns the senders after this task starts.
    let snapshot_agents = session.agents.clone();
    let snapshot_tasks = session.tasks.clone();

    let forwarder = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            let relevant = matches!(
                event.event,
                AgentEvent::AgentSpawned { .. }
                    | AgentEvent::AgentFinished { .. }
                    | AgentEvent::TaskChanged { .. }
                    // A session switch replaces the task graph and the rows
                    // describing it, so the dashboard is refreshed here too.
                    | AgentEvent::SessionChanged { .. }
            );

            if event_tx.send(event).is_err() {
                break;
            }

            if relevant {
                publish_agent_state(&snapshot_agents, &snapshot_tasks, &agents_tx, &tasks_tx);
            }
        }
    });
    let canonical_root =
        std::fs::canonicalize(&project_root).unwrap_or_else(|_| project_root.clone());
    let mut runtime = Runtime {
        paths,
        ledger,
        project_root,
        canonical_root,
        sessions: sessions_tx,
        skills: skills_tx,
        files: files_tx,
        processes: process_tx,
        context: context_tx,
        file_scan: Arc::new(AtomicBool::new(false)),
        max_image_encoded_bytes,
        queue: queued.into(),
        queue_paused,
        pending_images: Vec::new(),
        pending_model: None,
        pending_mode: None,
        pending_new_session: None,
        process_notices: Vec::new(),
        openrouter_api_key,
        opencode_go_api_key,
        sources,
        notify_on_exit,
        quit: false,
        cancelled_by_user: false,
    };

    emitter.send(AgentEvent::SessionChanged { session_id });
    emitter.send(AgentEvent::ModeChanged {
        mode: session.mode,
        previous: None,
    });
    emitter.send(AgentEvent::ModelChanged {
        model: session.agent.model().to_owned(),
        supports_reasoning: session.agent.supports_reasoning(),
    });
    emitter.send(AgentEvent::ReasoningEffortChanged {
        effort: session.agent.reasoning_effort(),
    });
    emitter.send(AgentEvent::HistoryRestored {
        entries: history_entries(&session.conversation),
    });

    for attachment in &attachments {
        emit_attachment(&emitter, attachment);
    }

    // A child that finishes while the parent is idle wakes the driver, which
    // starts a turn to deliver the notice. The wake is created once and handed
    // to the spawner so a finished child never has to know about the loop.
    let wake = Arc::new(tokio::sync::Notify::new());
    // A notice nobody reads is not a notification, so an idle parent is woken by
    // default. It spends money with nobody typing, so it can be turned off; then
    // reports are delivered with the user's next message instead.
    let wake_enabled = session.agents.is_some()
        && session
            .agents
            .as_ref()
            .is_some_and(|state| state.wake_on_finish());

    if let Some(agents) = &session.agents {
        // The registry is the only writer of notices, so the wake is triggered
        // by the spawner's finish path; share it through the state.
        attach_idle_wake(agents, Arc::clone(&wake));
    }

    runtime.emit_queue(&emitter);
    emit_processes(&session.processes, &runtime.processes, &emitter);
    publish_context(&runtime, &session);
    publish_sessions(&runtime);
    publish_skills(&session, &runtime.skills);

    // Index project files for `@` completion off the main loop; the first frame
    // must not wait for a large repository walk.
    refresh_project_files(&runtime);

    // Start discovery without making the first frame wait for it. The results
    // are kept in one-shot channels so the loop can keep handling input
    // meanwhile.
    let mut discovery = Discovery {
        catalog: CatalogRun::new(
            catalog_tx,
            reasoning_tx.clone(),
            auth_tx,
            runtime.context.clone(),
        ),
        mcp: McpRun::new(mcp_tx),
    };

    if session.has_credentials {
        if session.catalog_pending() {
            discovery.catalog.restart(&session);
        }
    } else {
        discovery.catalog.publish_unauthenticated(&mut session);
    }

    // A remote MCP handshake can take seconds, so the startup the build handed
    // over runs here rather than before the first frame. With nothing to start,
    // the gate lifts immediately.
    match session.deferred_mcp.take() {
        Some(startup) => discovery.mcp.restart(startup),
        None => {
            let servers = session
                .mcp
                .states()
                .values()
                .filter(|state| matches!(state, t8ful_mcp::ServerState::Ready { .. }))
                .count();

            let _ = discovery.mcp.snapshot_tx.send(McpSnapshot::Ready {
                servers,
                tools: session.mcp.len(),
            });
        }
    }

    let mut process_exits = session.processes.subscribe_exits();
    let progress_task = tokio::spawn(forward_process_progress(
        Arc::clone(&session.processes),
        emitter.clone(),
    ));

    while !runtime.quit {
        // Exits that happened while the driver was busy are noticed here, not
        // in the idle wait: only the latest state matters to the next turn.
        while let Ok(exit) = process_exits.try_recv() {
            queue_process_exit(&mut runtime, &session.processes, &emitter, exit);
        }

        if runtime.notify_on_exit
            && !runtime.cancelled_by_user
            && !runtime.process_notices.is_empty()
            && deliver_pending_notices(
                &mut session,
                &mut runtime,
                &mut discovery,
                &emitter,
                &mut command_rx,
                true,
            )
            .await
        {
            continue;
        }

        if discovery.pending().is_none()
            && session_has_model(&session)
            && let Some(item) = runtime.next_queued()
        {
            let journal = session.journal.clone();

            if let Some(journal) = &journal {
                let _ = journal.queue_delivered(item.id.clone()).await;
            }

            runtime.emit_queue(&emitter);

            if !submit_prompt(
                &mut session,
                &mut runtime,
                item.text,
                UserMessageSource::Queued,
                &emitter,
            )
            .await
            {
                break;
            }

            let result = execute_turn(&mut session, &mut runtime, &mut command_rx, &emitter).await;

            handle_turn_end(
                &mut session,
                &mut runtime,
                &mut discovery,
                &emitter,
                &result,
            )
            .await;
            continue;
        }

        if discovery.pending().is_some() {
            match discovery.wait(&mut command_rx).await {
                DiscoveryEvent::Command(command) => {
                    let Some(command) = command else {
                        break;
                    };

                    if !handle_command(
                        &mut session,
                        &mut runtime,
                        &mut command_rx,
                        &mut discovery,
                        command,
                        &emitter,
                    )
                    .await
                    {
                        break;
                    }
                }
                DiscoveryEvent::Catalog(result) => {
                    let result = match result {
                        Ok(result) => result,
                        Err(_) => Err("the model catalog task stopped unexpectedly".to_owned()),
                    };

                    discovery.catalog.finish(&mut session, result, &emitter);
                }
                DiscoveryEvent::Mcp(result) => {
                    let (hub, warnings) = match result {
                        Ok(value) => value,
                        Err(_) => (
                            t8ful_mcp::McpHub::new(),
                            vec!["the MCP startup task stopped unexpectedly".to_owned()],
                        ),
                    };
                    let servers = hub
                        .states()
                        .values()
                        .filter(|state| matches!(state, t8ful_mcp::ServerState::Ready { .. }))
                        .count();
                    let tools = hub.len();

                    session.install_mcp(hub, &warnings, Some(&emitter));
                    discovery.mcp.finish(servers, tools);
                }
            }

            continue;
        }

        // Idle: wait for user intent, or for a child to finish and wake us.
        let command = tokio::select! {
            command = command_rx.recv() => command,
            () = wake.notified(),
                if wake_enabled && !runtime.cancelled_by_user => None,
            // Always receive an exit so it is queued even when the wake is
            // disabled; the delivery decision is made below.
            exit = process_exits.recv(),
                if !runtime.cancelled_by_user => {
                    if let Ok(exit) = exit {
                        queue_process_exit(&mut runtime, &session.processes, &emitter, exit);
                    }

                    None
                }
        };

        let Some(command) = command else {
            // Either the channel closed or a child finished. A finish without a
            // command means the parent must be woken with a turn that delivers
            // the notice; a closed channel means the interface is gone.
            if command_rx.is_closed() {
                break;
            }

            let include_process = runtime.notify_on_exit;

            if deliver_pending_notices(
                &mut session,
                &mut runtime,
                &mut discovery,
                &emitter,
                &mut command_rx,
                include_process,
            )
            .await
            {
                continue;
            }

            if runtime.quit {
                break;
            }

            continue;
        };

        // Any real user intent ends the cancelled state, so notices waiting from
        // before the cancel are delivered with the next prompt.
        if !matches!(command, TuiCommand::Cancel) {
            runtime.cancelled_by_user = false;
        }

        if !handle_command(
            &mut session,
            &mut runtime,
            &mut command_rx,
            &mut discovery,
            command,
            &emitter,
        )
        .await
        {
            break;
        }
    }

    discovery.catalog.abort();
    discovery.mcp.abort();

    drop(emitter);

    // The session's spawner holds its own emitter clone for the whole run, so
    // the forwarder's channel never closes on its own. It is a display task with
    // nothing left to display once the interface is gone, so it is aborted rather
    // than awaited: awaiting would hang shutdown whenever agents are enabled.
    forwarder.abort();
    progress_task.abort();
    session.processes.shutdown().await;
}

/// Display updates are coalesced so a chatty build cannot flood the event
/// stream: one event per process per tick is at most twenty per second.
const PROCESS_PROGRESS_FLUSH: std::time::Duration = std::time::Duration::from_millis(50);
/// Pending bytes per process before the tick; the final result stays
/// authoritative, so excess display bytes are dropped rather than buffered.
const PROCESS_PROGRESS_PENDING_CAP: usize = 64 * 1024;

#[derive(Default)]
struct PendingProgress {
    text: String,
    truncated: bool,
}

/// Forward managed-process output to the frontend while the agent is active or
/// idle. This task never touches the conversation or the journal.
async fn forward_process_progress(processes: Arc<ManagedProcesses>, events: EventEmitter) {
    let mut receiver = processes.subscribe_progress();
    let mut pending: BTreeMap<String, PendingProgress> = BTreeMap::new();
    let mut ticker = tokio::time::interval(PROCESS_PROGRESS_FLUSH);

    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            received = receiver.recv() => match received {
                Ok(chunk) => {
                    push_pending(&mut pending, &chunk.id, &chunk.chunk, chunk.truncated);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // The frontend may have missed bytes entirely; replace its
                    // buffer with the authoritative bounded snapshot.
                    let recently_active = pending.keys().cloned().collect::<Vec<_>>();

                    pending.clear();

                    for snapshot in processes.list() {
                        let relevant = snapshot.state == ProcessState::Running
                            || recently_active.contains(&snapshot.id);

                        if !relevant {
                            continue;
                        }

                        if let Ok((text, truncated, _)) = processes.output(&snapshot.id)
                            && !text.is_empty()
                        {
                            let _ = events.send(AgentEvent::ProcessProgress {
                                process_id: snapshot.id,
                                chunk: text,
                                truncated,
                                reset: true,
                            });
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = ticker.tick() => flush_pending(&mut pending, &events),
        }
    }

    flush_pending(&mut pending, &events);
}

fn push_pending(
    pending: &mut BTreeMap<String, PendingProgress>,
    id: &str,
    chunk: &str,
    truncated: bool,
) {
    let entry = pending.entry(id.to_owned()).or_default();

    entry.truncated |= truncated;

    let room = PROCESS_PROGRESS_PENDING_CAP.saturating_sub(entry.text.len());

    if room == 0 {
        entry.truncated = true;

        return;
    }

    let mut take = chunk.len().min(room);

    while take > 0 && !chunk.is_char_boundary(take) {
        take -= 1;
    }

    entry.text.push_str(&chunk[..take]);

    if take < chunk.len() {
        entry.truncated = true;
    }
}

fn flush_pending(pending: &mut BTreeMap<String, PendingProgress>, events: &EventEmitter) {
    for (process_id, entry) in std::mem::take(pending) {
        if entry.text.is_empty() && !entry.truncated {
            continue;
        }

        let _ = events.send(AgentEvent::ProcessProgress {
            process_id,
            chunk: entry.text,
            truncated: entry.truncated,
            reset: false,
        });
    }
}

/// Apply a command that arrived while the driver was idle.
///
/// Returns `false` when the driver should stop. While discovery is pending,
/// prompts are queued rather than sent with an incomplete tool list or limits.
async fn handle_command(
    session: &mut Session,
    runtime: &mut Runtime,
    command_rx: &mut mpsc::UnboundedReceiver<TuiCommand>,
    discovery: &mut Discovery,
    command: TuiCommand,
    events: &EventEmitter,
) -> bool {
    let pending = discovery.pending();

    match command {
        TuiCommand::Quit => {
            runtime.quit = true;

            false
        }
        // A cancel with nothing running is already satisfied. It stays a no-op
        // rather than an error, because a user pressing ctrl+c twice is normal.
        TuiCommand::Cancel => true,
        TuiCommand::Submit(prompt) | TuiCommand::Steer(prompt) => {
            if prompt.trim().is_empty() {
                return true;
            }

            if !session_has_model(session) {
                // Keep the prompt so it runs as soon as `/model` applies one.
                runtime.queue_add(prompt, &session.journal, events).await;
                events.send(AgentEvent::Warning {
                    message: "no model is selected: choose one with /model before sending a \
                              prompt"
                        .to_owned(),
                });

                return true;
            }

            if let Some(pending) = pending {
                runtime.queue_add(prompt, &session.journal, events).await;
                events.send(AgentEvent::Warning {
                    message: pending.message().to_owned(),
                });

                return true;
            }

            if !submit_prompt(
                session,
                runtime,
                prompt,
                UserMessageSource::Submitted,
                events,
            )
            .await
            {
                return false;
            }

            let result = execute_turn(session, runtime, command_rx, events).await;

            handle_turn_end(session, runtime, discovery, events, &result).await;

            true
        }
        TuiCommand::Queue(text) => {
            runtime.queue_add(text, &session.journal, events).await;

            true
        }
        TuiCommand::Dequeue => {
            runtime.queue_clear(&session.journal, events).await;
            // The TUI already moved these into the composer; draining here is
            // what stops the agent from delivering a copy as well. A steer
            // that reached a step boundary first has already been journaled and
            // cannot be un-sent.
            let _ = session.agent.take_pending_steering();

            true
        }
        TuiCommand::Login { provider, key } => {
            login(
                session,
                runtime,
                &mut discovery.catalog,
                &provider,
                key,
                events,
            )
            .await;

            true
        }
        TuiCommand::Logout { provider } => {
            logout(session, runtime, &mut discovery.catalog, &provider, events).await;

            true
        }
        TuiCommand::SelectModel(model) => {
            apply_model(
                session,
                &model,
                events,
                &discovery.catalog.reasoning_tx,
                &runtime.context,
            )
            .await;

            true
        }
        TuiCommand::SetReasoningEffort(effort) => {
            apply_reasoning(session, effort, events).await;

            true
        }
        TuiCommand::SetMode(mode) => {
            if mode == Mode::Plan && running_processes(&session.processes) {
                events.send(AgentEvent::Warning {
                    message: "stop the managed coding process(es) before entering plan mode"
                        .to_owned(),
                });
            } else {
                apply_mode(session, mode, events).await;
            }

            true
        }
        TuiCommand::Attach {
            path,
            bytes,
            media_type,
        } => {
            runtime
                .attach(path, bytes, media_type, &session.journal, events)
                .await;

            true
        }
        TuiCommand::PasteClipboard => {
            paste_clipboard(runtime, &session.journal, events).await;

            true
        }
        TuiCommand::NewSession { mode, seed } => {
            new_session(session, runtime, discovery, events, mode, seed).await;

            true
        }
        TuiCommand::ResumeSession(id) => {
            resume_session(session, runtime, discovery, &id, events).await;

            true
        }
        TuiCommand::ListSessions => {
            publish_sessions(runtime);

            true
        }
        TuiCommand::ProcessList => {
            emit_processes(&session.processes, &runtime.processes, events);

            true
        }
        TuiCommand::ProcessView(id) => {
            match session.processes.output(&id) {
                Ok((chunk, truncated, _total)) => {
                    events.send(AgentEvent::ProcessOutput {
                        process_id: id,
                        chunk,
                        truncated,
                    });
                }
                Err(error) => {
                    events.send(AgentEvent::Warning {
                        message: error.to_string(),
                    });
                }
            }

            true
        }
        TuiCommand::ProcessStop(id) => {
            match session.processes.stop(&id).await {
                Ok(snapshot) => {
                    events.send(AgentEvent::ProcessStatus {
                        process_id: snapshot.id,
                        state: snapshot.state.as_str().to_owned(),
                        exit_code: snapshot.exit_code,
                    });
                }
                Err(error) => {
                    events.send(AgentEvent::Warning {
                        message: error.to_string(),
                    });
                }
            }

            emit_processes(&session.processes, &runtime.processes, events);

            true
        }
        TuiCommand::ActivateSkill { name, prompt } => {
            if !activate_skill(session, &name, events).await {
                return true;
            }

            let Some(prompt) = prompt.filter(|prompt| !prompt.trim().is_empty()) else {
                return true;
            };

            if let Some(pending) = pending {
                runtime.queue_add(prompt, &session.journal, events).await;
                events.send(AgentEvent::Warning {
                    message: pending.message().to_owned(),
                });

                return true;
            }

            if !submit_prompt(
                session,
                runtime,
                prompt,
                UserMessageSource::Submitted,
                events,
            )
            .await
            {
                return false;
            }

            let result = execute_turn(session, runtime, command_rx, events).await;

            handle_turn_end(session, runtime, discovery, events, &result).await;

            true
        }
    }
}

/// The environment override for one provider, which always wins over a stored
/// key.
fn environment_key(provider: &str) -> Option<String> {
    let variable = crate::auth::environment_variable(provider)?;

    std::env::var(variable)
        .ok()
        .filter(|key| !key.trim().is_empty())
}

/// Save a key, then make it active if the environment does not override it.
///
/// The key is stored for the named provider only; the other provider's client
/// and stored key are untouched.
async fn login(
    session: &mut Session,
    runtime: &mut Runtime,
    catalog: &mut CatalogRun,
    provider: &str,
    key: t8ful_tui::bridge::Secret,
    events: &EventEmitter,
) {
    if !crate::auth::is_known_provider(provider) {
        events.send(AgentEvent::Error {
            message: format!("unknown provider `{provider}`; use `openrouter` or `opencode-go`"),
        });

        return;
    }

    let canonical = crate::state::canonical_provider(provider);
    let key = key.into_inner();

    if let Err(error) = crate::auth::save(&crate::auth::path(&runtime.paths), canonical, &key) {
        events.send(AgentEvent::Error {
            message: error.to_string(),
        });

        return;
    }

    let environment = environment_key(canonical);
    let active = environment.clone().or(Some(key));
    let route = crate::providers::Route::from_id(canonical).unwrap_or_default();

    apply_credentials(session, runtime, catalog, route, active, events).await;

    if environment.is_some() {
        let variable = crate::auth::environment_variable(canonical).unwrap_or_default();

        events.send(AgentEvent::Warning {
            message: format!("{variable} is set, so the saved key is stored but not used"),
        });
    }
}

/// Forget one provider's stored key; an environment key keeps it connected.
async fn logout(
    session: &mut Session,
    runtime: &mut Runtime,
    catalog: &mut CatalogRun,
    provider: &str,
    events: &EventEmitter,
) {
    if !crate::auth::is_known_provider(provider) {
        events.send(AgentEvent::Error {
            message: format!("unknown provider `{provider}`; use `openrouter` or `opencode-go`"),
        });

        return;
    }

    let canonical = crate::state::canonical_provider(provider);

    if let Err(error) = crate::auth::remove(&crate::auth::path(&runtime.paths), canonical) {
        events.send(AgentEvent::Error {
            message: error.to_string(),
        });

        return;
    }

    let environment = environment_key(canonical);
    let route = crate::providers::Route::from_id(canonical).unwrap_or_default();

    apply_credentials(
        session,
        runtime,
        catalog,
        route,
        environment.clone(),
        events,
    )
    .await;

    if environment.is_some() {
        let variable = crate::auth::environment_variable(canonical).unwrap_or_default();

        events.send(AgentEvent::Warning {
            message: format!("{variable} is set, so the session stays connected"),
        });
    }
}

/// Swap one provider's credential and reload the catalog it can see.
///
/// Only the affected provider's client is rebuilt; the other keeps its
/// credential and its connection, so logging into Go never disconnects
/// OpenRouter.
async fn apply_credentials(
    session: &mut Session,
    runtime: &mut Runtime,
    catalog: &mut CatalogRun,
    route: crate::providers::Route,
    api_key: Option<String>,
    events: &EventEmitter,
) {
    let mut openrouter = runtime.openrouter_api_key.clone();
    let mut opencode = runtime.opencode_go_api_key.clone();

    match route {
        crate::providers::Route::OpenRouter => openrouter = api_key.clone(),
        crate::providers::Route::OpencodeGo => opencode = api_key.clone(),
    }

    if let Err(error) = session.set_credentials(openrouter.clone(), opencode.clone()) {
        events.send(AgentEvent::Error {
            message: format!("cannot switch credentials: {error}"),
        });

        return;
    }

    runtime.openrouter_api_key = openrouter;
    runtime.opencode_go_api_key = opencode;

    // The login modal shows the source of the provider it is about to save a key
    // for, so the snapshot follows every login and logout.
    let environment = environment_key(route.id()).is_some();
    let source = match (&api_key, environment) {
        (Some(_), true) => crate::auth::AuthSource::Environment,
        (Some(_), false) => crate::auth::AuthSource::Stored,
        (None, _) => crate::auth::AuthSource::Missing,
    };

    match route {
        crate::providers::Route::OpenRouter => runtime.sources.openrouter = source,
        crate::providers::Route::OpencodeGo => runtime.sources.opencode_go = source,
    }

    let _ = catalog.auth_tx.send(runtime.auth_snapshot());
    let _ = catalog.snapshot_tx.send(CatalogSnapshot::Pending);
    runtime.pending_model = None;

    if session.has_credentials {
        catalog.restart(session);
    } else {
        catalog.publish_unauthenticated(session);
    }
}

fn publish_catalog(
    session: &mut Session,
    result: Result<Catalog, String>,
    catalog_tx: &watch::Sender<CatalogSnapshot>,
    reasoning_tx: &watch::Sender<ReasoningSnapshot>,
    events: &EventEmitter,
) {
    match result {
        Ok(catalog) => {
            let model_rows = catalog
                .models
                .iter()
                .map(|model| {
                    t8ful_tui::bridge::ModelRow::new(model.id.clone(), model.provider.clone())
                })
                .collect::<Vec<_>>();

            // A provider whose catalog failed is reported and its models are
            // simply absent; the other provider's models still work.
            for warning in &catalog.warnings {
                tracing::warn!(target: "t8ful::driver", "{warning}");
                events.send(AgentEvent::Warning {
                    message: warning.clone(),
                });
            }

            match session.apply_catalog(Ok(catalog)) {
                Ok(Some(message)) => {
                    tracing::warn!(target: "t8ful::driver", "{message}");
                    events.send(AgentEvent::Warning { message });
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        target: "t8ful::driver",
                        "model metadata was loaded but could not be applied: {error}"
                    );
                    events.send(AgentEvent::Warning {
                        message: format!(
                            "model metadata loaded, but its limits could not be applied: {error}"
                        ),
                    });
                }
            }

            // Metadata can change the selected model's capabilities and may
            // reset an unsupported reasoning effort. Publish both so the UI
            // never keeps a stale capability or effort.
            events.send(AgentEvent::ModelChanged {
                model: session.agent.model().to_owned(),
                supports_reasoning: session.agent.supports_reasoning(),
            });
            events.send(AgentEvent::ReasoningEffortChanged {
                effort: session.agent.reasoning_effort(),
            });

            let _ = catalog_tx.send(CatalogSnapshot::Loaded { models: model_rows });
            let _ = reasoning_tx.send(ReasoningSnapshot::Loaded {
                levels: session.agent.reasoning_profile().levels(),
            });
        }
        Err(message) => {
            let _ = session.apply_catalog(Err(message.clone()));
            tracing::warn!(
                target: "t8ful::driver",
                "could not load model metadata; using configured fallback limits: {message}"
            );
            let _ = catalog_tx.send(CatalogSnapshot::Failed { message });
        }
    }
}

/// Load a skill on explicit user request and journal it as a user message.
///
/// The message is replayed on resume; `history_entries` rebuilds the same
/// `Skill` card the live event produces instead of echoing the instructions as
/// prose.
async fn activate_skill(session: &mut Session, name: &str, events: &EventEmitter) -> bool {
    let document = match session.skills.load(name) {
        Ok(document) => document,
        Err(message) => {
            events.send(AgentEvent::Error { message });

            return false;
        }
    };
    let (text, truncated) = t8ful_tools::skill::format_skill(&document, session.max_output_bytes);

    if truncated {
        events.send(AgentEvent::Warning {
            message: format!("skill `{name}` was truncated to fit the output limit"),
        });
    }

    let message = Message::user_text(text.clone());

    if let Some(journal) = &session.journal
        && let Err(error) = journal
            .append(crate::session::JournalRecord::Message {
                message: message.clone(),
            })
            .await
    {
        events.send(AgentEvent::Error {
            message: format!("cannot record the skill message: {error}"),
        });

        return false;
    }

    session.conversation.push(message);

    events.send(AgentEvent::SkillLoaded {
        name: name.to_owned(),
        content: text,
        truncated,
    });

    true
}

/// Read one clipboard image and attach it.
///
/// A clipboard that simply holds no image is silent; a missing backend or a
/// helper failure still warns. Both command paths call this so their behavior
/// cannot drift.
async fn paste_clipboard(
    runtime: &mut Runtime,
    journal: &Option<Arc<SessionJournal>>,
    events: &EventEmitter,
) {
    match tokio::task::spawn_blocking(|| SystemClipboard.read_image()).await {
        Ok(Ok(image)) => {
            runtime
                .attach(
                    None,
                    Some(image.bytes),
                    Some(image.media_type),
                    journal,
                    events,
                )
                .await;
        }
        Ok(Err(error)) => {
            if !error.is_silent() {
                events.send(AgentEvent::Warning {
                    message: error.to_string(),
                });
            }
        }
        Err(error) => {
            events.send(AgentEvent::Warning {
                message: format!("clipboard task failed: {error}"),
            });
        }
    }
}

async fn submit_prompt(
    session: &mut Session,
    runtime: &mut Runtime,
    text: String,
    source: UserMessageSource,
    events: &EventEmitter,
) -> bool {
    // A notice queued behind a cancel, or while the idle wake was disabled, is
    // delivered with this prompt rather than never or one turn late.
    if deliver_queued_notices(session, runtime, events, true)
        .await
        .is_err()
    {
        return false;
    }

    let images = std::mem::take(&mut runtime.pending_images);
    let event_images = images.clone();
    let message = Message::user_with_images(Some(text.clone()), images);

    if let Some(journal) = &session.journal
        && let Err(error) = journal
            .append(crate::session::JournalRecord::Message {
                message: message.clone(),
            })
            .await
    {
        events.send(AgentEvent::Error {
            message: format!("the session journal stopped accepting input: {error}"),
        });

        return false;
    }

    session.conversation.push(message);
    events.send(AgentEvent::UserMessage {
        text,
        source,
        images: event_images,
    });

    true
}

async fn execute_turn(
    session: &mut Session,
    runtime: &mut Runtime,
    command_rx: &mut mpsc::UnboundedReceiver<TuiCommand>,
    events: &EventEmitter,
) -> Result<TurnOutcome, t8ful_core::AgentError> {
    // Every turn gets a fresh spawn budget: user turns and idle wake turns
    // alike, because the limit is per turn, not per session.
    if let Some(agents) = &session.agents {
        agents.registry.begin_turn();
    }

    let cancel = CancellationToken::new();
    let journal = session.journal.clone();
    let processes = Arc::clone(&session.processes);
    let mut conversation = std::mem::take(&mut session.conversation);

    let outcome = {
        let run = session.agent.run(&mut conversation, events, &cancel);

        tokio::pin!(run);

        loop {
            tokio::select! {
                // A finished run wins over a command that arrived at the same
                // instant. Otherwise a prompt sent exactly at the turn boundary
                // can be swallowed as steering for the turn that just ended, and
                // the next turn never begins (so per-turn limits never reset).
                biased;
                result = &mut run => break result,
                command = command_rx.recv() => {
                    let Some(command) = command else {
                        cancel.cancel();
                        break Err(t8ful_core::AgentError::Cancelled);
                    };

                    match command {
                        TuiCommand::Cancel => {
                            cancel.cancel();
                            // Latch before cancelling the children: their
                            // reports arrive later and must not wake a new turn.
                            runtime.cancelled_by_user = true;
                            cancel_children(session, events);
                        }
                        TuiCommand::Steer(text) => session.agent.steer(text),
                        TuiCommand::Queue(text) => {
                            runtime.queue_add(text, &journal, events).await;
                        }
                        TuiCommand::Dequeue => {
                            runtime.queue_clear(&journal, events).await;
                            let _ = session.agent.take_pending_steering();
                        }
                        TuiCommand::Login { .. } | TuiCommand::Logout { .. } => {
                            events.send(AgentEvent::Warning {
                                message: "finish the current turn before logging in/out"
                                    .to_owned(),
                            });
                        }
                        TuiCommand::SelectModel(model) => {
                            events.send(AgentEvent::Warning {
                                message: format!(
                                    "model `{}` will be applied at the next turn boundary",
                                    model.label()
                                ),
                            });
                            runtime.pending_model = Some(model.clone());
                        }
                        TuiCommand::SetReasoningEffort(effort) => {
                            apply_reasoning(session, effort, events).await;
                        }
                        TuiCommand::SetMode(mode) => {
                            if mode == Mode::Plan && running_processes(&processes) {
                                events.send(AgentEvent::Warning {
                                    message: "stop the managed coding process(es) before entering \
                                              plan mode".to_owned(),
                                });
                            } else {
                                runtime.pending_mode = Some(mode);
                                session.agent.request_mode(mode);
                            }
                        }
                        TuiCommand::Attach { path, bytes, media_type } => {
                            runtime.attach(path, bytes, media_type, &journal, events).await;
                        }
                        TuiCommand::PasteClipboard => {
                            paste_clipboard(runtime, &journal, events).await;
                        }
                        TuiCommand::Submit(text) => {
                            runtime.queue_add(text, &journal, events).await;
                        }
                        TuiCommand::NewSession { mode, seed } => {
                            runtime.pending_new_session = Some((mode, seed));
                            events.send(AgentEvent::Warning {
                                message: "the new session starts at the turn boundary".to_owned(),
                            });
                        }
                        TuiCommand::ResumeSession(_) => {
                            events.send(AgentEvent::Warning {
                                message: "finish the current turn before switching sessions"
                                    .to_owned(),
                            });
                        }
                        TuiCommand::ListSessions => publish_sessions(runtime),
                        TuiCommand::ProcessList => {
                            emit_processes(&processes, &runtime.processes, events);
                        }
                        TuiCommand::ProcessView(id) => match processes.output(&id) {
                            Ok((chunk, truncated, _total)) => {
                                events.send(AgentEvent::ProcessOutput {
                                    process_id: id,
                                    chunk,
                                    truncated,
                                });
                            }
                            Err(error) => {
                                events.send(AgentEvent::Warning {
                                    message: error.to_string(),
                                });
                            }
                        },
                        TuiCommand::ProcessStop(id) => {
                            match processes.stop(&id).await {
                                Ok(snapshot) => {
                                    events.send(AgentEvent::ProcessStatus {
                                        process_id: snapshot.id,
                                        state: snapshot.state.as_str().to_owned(),
                                        exit_code: snapshot.exit_code,
                                    });
                                }
                                Err(error) => {
                                    events.send(AgentEvent::Warning {
                                        message: error.to_string(),
                                    });
                                }
                            }

                            emit_processes(&processes, &runtime.processes, events);
                        }
                        TuiCommand::ActivateSkill { .. } => {
                            events.send(AgentEvent::Warning {
                                message: "finish the current turn before loading a skill"
                                    .to_owned(),
                            });
                        }
                        TuiCommand::Quit => {
                            runtime.quit = true;
                            session.agent.steer("");
                            cancel.cancel();
                            cancel_children(session, events);
                        }
                    }
                }
            }
        }
    };

    session.conversation = conversation;

    let late_steering = session.agent.take_pending_steering();

    for text in late_steering.into_iter().rev() {
        let id = format!("steer-{}", crate::session::now_millis());

        runtime
            .queue
            .push_front(crate::session::QueuedInput { id, text });
    }

    if !runtime.queue.is_empty() {
        runtime.emit_queue(events);
    }

    outcome
}

async fn handle_turn_end(
    session: &mut Session,
    runtime: &mut Runtime,
    discovery: &mut Discovery,
    events: &EventEmitter,
    result: &Result<TurnOutcome, t8ful_core::AgentError>,
) {
    match result {
        Ok(turn) if matches!(turn.finish, TurnFinish::Stop | TurnFinish::Length) => {
            if runtime.queue.is_empty() {
                runtime.queue_paused = false;
            }
        }
        Ok(_) | Err(_) => {
            if !runtime.queue.is_empty() {
                runtime.queue_paused = true;

                if let Some(journal) = &session.journal {
                    let _ = journal.queue_paused(true).await;
                }
            }
        }
    }

    // A model or mode chosen during the turn applies at this boundary, where
    // the next request will pick it up.
    if let Some(model) = runtime.pending_model.take() {
        apply_model(
            session,
            &model,
            events,
            &discovery.catalog.reasoning_tx,
            &runtime.context,
        )
        .await;
    }

    if let Some(mode) = runtime.pending_mode.take() {
        apply_mode(session, mode, events).await;
    }

    // A new session requested during the turn starts only now, when the old
    // one is no longer running. A seeded session queues its first prompt
    // through the same path as any follow-up.
    if let Some((mode, seed)) = runtime.pending_new_session.take() {
        new_session(session, runtime, discovery, events, mode, seed).await;
    }

    runtime.emit_queue(events);
    emit_processes(&session.processes, &runtime.processes, events);

    // The turn may have created files; keep `@` completion current without
    // making the next prompt wait for a walk.
    refresh_project_files(runtime);
}

async fn apply_model(
    session: &mut Session,
    model: &t8ful_tui::bridge::ModelRow,
    events: &EventEmitter,
    reasoning_tx: &watch::Sender<ReasoningSnapshot>,
    context_tx: &watch::Sender<ContextSnapshot>,
) {
    if let Err(error) = session.select_model_from_row(&model.id, model.provider.as_deref()) {
        events.send(AgentEvent::Error {
            message: format!("cannot select model `{}`: {error}", model.label()),
        });

        return;
    }

    if let Some(message) = session.take_selection_warning() {
        events.send(AgentEvent::Warning { message });
    }

    let selected = session.agent.model().to_owned();
    let provider = session.selected_route().map(|route| route.id().to_owned());

    if let Some(journal) = &session.journal {
        let _ = journal
            .record_settings_with_provider(
                Some(selected.clone()),
                session.mode,
                session.agent.reasoning_effort(),
                provider,
            )
            .await;
    }

    events.send(AgentEvent::ModelChanged {
        model: selected,
        supports_reasoning: session.agent.supports_reasoning(),
    });
    events.send(AgentEvent::ReasoningEffortChanged {
        effort: session.agent.reasoning_effort(),
    });

    // The new model may offer a different set of levels, so republish them
    // rather than leaving the picker on the previous model's list.
    let _ = reasoning_tx.send(ReasoningSnapshot::Loaded {
        levels: session.agent.reasoning_profile().levels(),
    });
    publish_context_tx(context_tx, session);
}

async fn apply_reasoning(
    session: &Session,
    effort: t8ful_core::provider::ReasoningEffort,
    events: &EventEmitter,
) {
    session.agent.set_reasoning_effort(effort);

    if let Some(journal) = &session.journal {
        let _ = journal.record_settings(None, session.mode, effort).await;
    }

    events.send(AgentEvent::ReasoningEffortChanged { effort });
}

async fn apply_mode(session: &mut Session, mode: Mode, events: &EventEmitter) {
    let previous = (session.mode != mode).then_some(session.mode);

    session.agent.switch_mode(mode);
    session.mode = mode;

    if let Some(journal) = &session.journal {
        let _ = journal
            .record_settings(None, mode, session.agent.reasoning_effort())
            .await;
    }

    events.send(AgentEvent::ModeChanged { mode, previous });
}

fn emit_processes(
    processes: &ManagedProcesses,
    snapshot: &watch::Sender<ProcessSnapshot>,
    events: &EventEmitter,
) {
    let listed = processes.list();
    let entries = listed
        .iter()
        .filter(|process| matches!(process.state, t8ful_tools::ProcessState::Running))
        .map(|process| {
            format!(
                "{} {} {}",
                process.id,
                process.state.as_str(),
                process.command
            )
        })
        .collect::<Vec<_>>();

    events.send(AgentEvent::ProcessSnapshot { entries });

    let views = listed
        .into_iter()
        .map(|process| ProcessView {
            id: process.id,
            command: process.command,
            state: process.state.as_str().to_owned(),
            exit_code: process.exit_code,
            signal: process.signal,
            elapsed_ms: process.elapsed_ms,
            total_bytes: process.total_bytes,
            truncated: process.truncated,
            output_incomplete: process.output_incomplete,
            cleanup_error: process.cleanup_error,
        })
        .collect();

    let _ = snapshot.send(ProcessSnapshot::Loaded { processes: views });
}

fn publish_context(runtime: &Runtime, session: &Session) {
    publish_context_tx(&runtime.context, session);
}

fn publish_context_tx(tx: &watch::Sender<ContextSnapshot>, session: &Session) {
    if let Some(limit) = session.agent.config().context_limit {
        let _ = tx.send(ContextSnapshot { limit });
    }
}

/// Remember a natural background-process exit for the next injection.
///
/// A stop the user asked for is not news, and neither is one that happened
/// during shutdown, which goes through the same stop path.
fn queue_process_exit(
    runtime: &mut Runtime,
    processes: &ManagedProcesses,
    events: &EventEmitter,
    exit: ProcessExit,
) {
    // The frontend may be showing a live card for this process; tell it the
    // process ended even when the user requested the stop. Only a natural exit
    // also becomes a conversation notice.
    events.send(AgentEvent::ProcessStatus {
        process_id: exit.snapshot.id.clone(),
        state: exit.snapshot.state.as_str().to_owned(),
        exit_code: exit.snapshot.exit_code,
    });

    if exit.requested {
        return;
    }

    let tail = processes
        .output(&exit.snapshot.id)
        .map(|(text, _truncated, _total)| ProcessNotice::tail(&text))
        .unwrap_or_default();

    runtime.process_notices.push(ProcessNotice {
        process_id: exit.snapshot.id,
        command: exit.snapshot.command,
        state: exit.snapshot.state.as_str().to_owned(),
        exit_code: exit.snapshot.exit_code,
        signal: exit.snapshot.signal,
        elapsed_ms: exit.snapshot.elapsed_ms,
        total_bytes: exit.snapshot.total_bytes,
        truncated: exit.snapshot.truncated,
        output_tail: tail,
    });
}

/// Whether the session has a model the next request can use.
///
/// The TUI may start without one so `/model` and `/login` remain reachable;
/// prompts are held in the queue until a model is applied.
fn session_has_model(session: &Session) -> bool {
    !session.agent.model().trim().is_empty()
}

fn running_processes(processes: &ManagedProcesses) -> bool {
    processes
        .list()
        .iter()
        .any(|process| matches!(process.state, t8ful_tools::ProcessState::Running))
}

fn emit_attachment(events: &EventEmitter, attachment: &ImageInput) {
    events.send(AgentEvent::AttachmentAdded {
        path: attachment.path.display().to_string(),
        media_type: attachment.media_type.clone(),
        width: attachment.width,
        height: attachment.height,
        bytes: attachment.bytes,
    });
}

/// Render the persisted conversation for a frontend that has no transcript yet.
fn history_entries(conversation: &t8ful_core::message::Conversation) -> Vec<HistoryEntry> {
    let mut entries = Vec::new();
    let mut arguments = HashMap::new();

    for message in conversation.messages() {
        match message.role {
            t8ful_core::message::Role::User => {
                let text = message.text();
                let images = message.images().cloned().collect::<Vec<_>>();

                // A child report or process notice is an informational
                // injection, not something the user typed. Resume must not
                // present it as a user prompt.
                if text.starts_with("<agent_notice ") || text.starts_with("<process_notice ") {
                    entries.push(HistoryEntry {
                        role: HistoryRole::Info,
                        text,
                        images: Vec::new(),
                        tool: None,
                        arguments: None,
                        status: None,
                        truncated: false,
                    });
                } else if let Some(name) = skill_name(&text) {
                    let name = name.to_owned();

                    entries.push(HistoryEntry {
                        role: HistoryRole::Tool,
                        text,
                        images: Vec::new(),
                        tool: Some("skill".to_owned()),
                        arguments: Some(serde_json::json!({ "name": name })),
                        status: Some(ToolStatus::Ok),
                        truncated: false,
                    });
                } else if !text.is_empty() || !images.is_empty() {
                    entries.push(HistoryEntry {
                        role: HistoryRole::User,
                        text,
                        images,
                        tool: None,
                        arguments: None,
                        status: None,
                        truncated: false,
                    });
                }
            }
            t8ful_core::message::Role::Assistant => {
                for call in message.tool_calls() {
                    arguments.insert(call.id.as_str().to_owned(), call.arguments.clone());
                }

                let text = message.text();

                if !text.is_empty() {
                    entries.push(HistoryEntry {
                        role: HistoryRole::Assistant,
                        text,
                        images: Vec::new(),
                        tool: None,
                        arguments: None,
                        status: None,
                        truncated: false,
                    });
                }
            }
            t8ful_core::message::Role::Tool => {
                for result in message.results() {
                    entries.push(HistoryEntry {
                        role: HistoryRole::Tool,
                        text: result.content.clone(),
                        images: result.images.clone(),
                        tool: Some(result.name.clone()),
                        arguments: arguments.get(result.call_id.as_str()).cloned(),
                        status: Some(result.status),
                        truncated: result.truncated,
                    });
                }
            }
            t8ful_core::message::Role::System => {}
        }
    }

    entries
}

/// The skill name at the start of a journaled skill message.
fn skill_name(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("<skill name=\"")?;
    let end = rest.find('"')?;

    Some(&rest[..end])
}

/// Rebuild the `@` file index without blocking the driver.
///
/// A scan already in flight keeps the newer result; a turn can finish faster
/// than a large repository walk.
fn refresh_project_files(runtime: &Runtime) {
    if runtime.file_scan.swap(true, Ordering::AcqRel) {
        return;
    }

    let sender = runtime.files.clone();
    let root = runtime.project_root.clone();
    let scanning = Arc::clone(&runtime.file_scan);

    tokio::task::spawn_blocking(move || {
        let files = crate::files::project_files(&root);
        let _ = sender.send(ProjectFilesSnapshot {
            loaded: true,
            files,
        });
        scanning.store(false, Ordering::Release);
    });
}

/// Publish the discovered skills once per driver run. Skills are discovered at
/// session build time; there is no hot reload.
fn publish_skills(session: &Session, tx: &watch::Sender<SkillsSnapshot>) {
    let skills = session
        .skills
        .skills()
        .iter()
        .map(|skill| SkillView {
            name: skill.name.clone(),
            description: skill.description.clone(),
            source: skill.source.label().to_owned(),
            commit: skill.managed.as_ref().map(|managed| managed.commit.clone()),
        })
        .collect();

    let _ = tx.send(SkillsSnapshot {
        loaded: true,
        skills,
    });
}

/// Rescan saved sessions and publish the TUI's list and picker rows.
fn publish_sessions(runtime: &Runtime) {
    let sessions = crate::session::summaries(&runtime.paths)
        .into_iter()
        .map(|listing| SessionSummary {
            id: listing.id.to_string(),
            title: listing.title,
            last_message_at: listing.last_message_at,
            project: listing
                .project_root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| listing.project_root.display().to_string()),
            resumable: listing.project_root == runtime.canonical_root,
        })
        .collect();

    let _ = runtime.sessions.send(SessionSnapshot::Loaded { sessions });
}

async fn new_session(
    session: &mut Session,
    runtime: &mut Runtime,
    discovery: &mut Discovery,
    events: &EventEmitter,
    mode: Option<Mode>,
    seed: Option<String>,
) {
    if let Some(mode) = mode {
        session.agent.set_mode(mode);
        session.mode = mode;
    }

    // A session switch abandons the previous session's MCP servers: a startup
    // still pending is aborted, the started ones are stopped, and the registry
    // and prompts are rebuilt from the pre-MCP tools, so the new session never
    // advertises a tool whose server is gone. Leaving the processes running
    // would outlive the work they were started for.
    discovery.abort_mcp();
    session.mcp.stop_all().await;
    session.install_mcp(t8ful_mcp::McpHub::new(), &[], Some(events));

    let mode = session.mode;
    let model = session.agent.model().to_owned();
    let effort = session.agent.reasoning_effort();

    let provider = session.selected_route().map(|route| route.id().to_owned());

    match SessionJournal::pending(
        &runtime.paths,
        &runtime.project_root,
        Some(model.clone()),
        mode,
        effort,
        provider,
        Some(Arc::clone(&runtime.ledger)),
    ) {
        Ok(journal) => {
            let journal = Arc::new(journal);
            let id = journal.id().to_string();

            session
                .agent
                .set_journal(Arc::clone(&journal) as Arc<dyn t8ful_core::journal::AgentJournal>);
            session.agent.set_projection(None);
            session.conversation.clear();
            // Nothing in a fresh session has happened yet, so the spawner and
            // the task tools move to the new journal and an empty graph.
            if let Some(state) = &session.agents {
                state.rebind(Some(Arc::clone(&journal)), &[]);
            }

            session.journal = Some(journal);
            session.set_current_session(Some(id.clone()));
            session.agent.set_session_id(id.clone());
            runtime.queue.clear();
            runtime.pending_images.clear();
            runtime.process_notices.clear();
            runtime.queue_paused = false;

            events.send(AgentEvent::SessionChanged { session_id: id });
            events.send(AgentEvent::HistoryRestored {
                entries: Vec::new(),
            });
            events.send(AgentEvent::ModeChanged {
                mode,
                previous: None,
            });
            events.send(AgentEvent::ModelChanged {
                model,
                supports_reasoning: session.agent.supports_reasoning(),
            });
            events.send(AgentEvent::ReasoningEffortChanged { effort });
            runtime.emit_queue(events);
            publish_sessions(runtime);
            publish_context(runtime, session);

            if let Some(seed) = seed {
                runtime.queue_add(seed, &session.journal, events).await;
            }
        }
        Err(error) => {
            events.send(AgentEvent::Error {
                message: format!("cannot create a new session: {error}"),
            });
        }
    }
}

/// Open another session and make the TUI show it.
async fn resume_session(
    session: &mut Session,
    runtime: &mut Runtime,
    discovery: &mut Discovery,
    raw: &str,
    events: &EventEmitter,
) {
    let id = match SessionId::parse(raw) {
        Ok(id) => id,
        Err(error) => {
            events.send(AgentEvent::Warning {
                message: error.to_string(),
            });

            return;
        }
    };

    let current_id = session.journal.as_ref().map(|journal| journal.id().clone());

    if current_id.as_ref() == Some(&id) {
        events.send(AgentEvent::Warning {
            message: format!("session {id} is already open"),
        });

        return;
    }

    match SessionJournal::open(
        &runtime.paths,
        &id,
        &runtime.project_root,
        Arc::clone(&runtime.ledger),
    )
    .await
    {
        Ok((journal, loaded)) => {
            // The abandoned session's servers stop with the session that owned
            // them, whether the resume succeeds or not. A startup still pending
            // belongs to the abandoned session too, and the resumed session
            // starts without the tools that server used to offer.
            discovery.abort_mcp();
            session.mcp.stop_all().await;
            session.install_mcp(t8ful_mcp::McpHub::new(), &[], Some(events));

            let journal = Arc::new(journal);
            // A resumed journal may tighten the live mode, never widen it: an
            // active plan session stays plan until the user runs /mode coding.
            let mode = session.mode.most_restrictive(loaded.mode);

            session
                .agent
                .set_journal(Arc::clone(&journal) as Arc<dyn t8ful_core::journal::AgentJournal>);
            session.agent.set_projection(loaded.projection.clone());
            session.agent.set_mode(mode);
            session.agent.set_reasoning_effort(loaded.reasoning_effort);
            session.mode = mode;

            if let Some(model) = loaded.model.clone() {
                // A journal that recorded its provider re-selects the same copy
                // of a duplicated id; an older one falls back to the index.
                let selection = match loaded.provider.as_deref() {
                    Some(provider) => session.select_model_from_row(&model, Some(provider)),
                    None => session.select_model(&model).map(|_| ()),
                };

                match selection {
                    Ok(()) => {}
                    Err(error) => {
                        events.send(AgentEvent::Error {
                            message: format!("cannot select resumed model `{model}`: {error}"),
                        });
                    }
                }

                if let Some(message) = session.take_selection_warning() {
                    events.send(AgentEvent::Warning { message });
                }
            }

            // The resumed session's task graph and its journal replace the ones
            // the spawner and the task tools still point at.
            if let Some(state) = &session.agents {
                state.rebind(Some(Arc::clone(&journal)), &loaded.tasks);
            }

            session.conversation = loaded.conversation;
            session.journal = Some(journal);
            session.set_current_session(Some(id.to_string()));
            session.agent.set_session_id(id.to_string());
            runtime.queue = loaded.queued.into();
            runtime.queue_paused = loaded.queue_paused;
            runtime.pending_images.clear();
            runtime.process_notices.clear();

            events.send(AgentEvent::SessionChanged {
                session_id: id.to_string(),
            });
            events.send(AgentEvent::HistoryRestored {
                entries: history_entries(&session.conversation),
            });

            for record in &loaded.usage {
                events.send(usage_event(record));
            }

            events.send(AgentEvent::ModeChanged {
                mode,
                previous: None,
            });
            events.send(AgentEvent::ModelChanged {
                model: session.agent.model().to_owned(),
                supports_reasoning: session.agent.supports_reasoning(),
            });
            events.send(AgentEvent::ReasoningEffortChanged {
                effort: session.agent.reasoning_effort(),
            });

            for attachment in &loaded.attachments {
                emit_attachment(events, attachment);
            }

            runtime.emit_queue(events);
            publish_sessions(runtime);
            publish_context(runtime, session);
        }
        Err(error) => {
            events.send(AgentEvent::Error {
                message: format!("cannot resume session {id}: {error}"),
            });
        }
    }
}

/// One journaled usage record as the event the footer already understands.
fn usage_event(record: &UsageRecord) -> AgentEvent {
    AgentEvent::UsageRecorded {
        request_id: record.request_id.clone(),
        model: record.model.clone(),
        agent: record.agent.clone(),
        kind: record.kind.clone(),
        input_tokens: record.usage.input_tokens,
        output_tokens: record.usage.output_tokens,
        cost_usd: record.usage.cost_usd.clone(),
        generation_id: record
            .usage
            .generation_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        cache_read_tokens: record.usage.cache_read_tokens,
        cache_write_tokens: record.usage.cache_write_tokens,
        reasoning_tokens: record.usage.reasoning_tokens,
    }
}

/// Cancel every child the session started, and say how many were stopped.
///
/// A child's work is meaningless once its parent stopped, and leaving one
/// running means spending money with nobody to read the result.
fn cancel_children(session: &Session, events: &EventEmitter) {
    let Some(state) = &session.agents else {
        return;
    };

    let cancelled = state.registry.cancel_all();

    if cancelled > 0 {
        events.send(AgentEvent::Warning {
            message: format!(
                "cancelled {cancelled} child agent{}; their worktrees were kept",
                if cancelled == 1 { "" } else { "s" }
            ),
        });
    }
}

/// Inject every queued notice into the root conversation.
///
/// Agent reports come first, then process exits, because a child's report is
/// usually what the user is waiting for. `Agent::deliver_notice` journals and
/// announces each message, so this is the only write path. Returns whether a
/// notice was delivered. A failed injection is reported and the notice is not
/// retried: a report stays readable through `agent_status` and `/agents`, and
/// a process stays readable through `process_output`.
async fn deliver_queued_notices(
    session: &mut Session,
    runtime: &mut Runtime,
    events: &EventEmitter,
    include_process: bool,
) -> Result<bool, ()> {
    let mut delivered = false;

    if let Some(state) = &session.agents {
        let notices = state
            .registry
            .notices_for(&t8ful_core::AgentId::root())
            .drain();

        for notice in notices {
            if let Err(error) = session
                .agent
                .deliver_notice(
                    Notice::Agent(Box::new(notice)),
                    &mut session.conversation,
                    events,
                )
                .await
            {
                events.send(AgentEvent::Error {
                    message: format!("could not deliver an agent report: {error}"),
                });

                return Err(());
            }

            delivered = true;
        }
    }

    if !include_process {
        return Ok(delivered);
    }

    for notice in std::mem::take(&mut runtime.process_notices) {
        // The model already read this exit through `process_wait`, so announcing
        // it again would say the same thing twice. Delivery is the check point,
        // not queueing, because the exit can be claimed after it is queued.
        if session.processes.notice_claimed(&notice.process_id) {
            continue;
        }

        if let Err(error) = session
            .agent
            .deliver_notice(Notice::Process(notice), &mut session.conversation, events)
            .await
        {
            events.send(AgentEvent::Error {
                message: format!("could not deliver a process notice: {error}"),
            });

            return Err(());
        }

        delivered = true;
    }

    Ok(delivered)
}

/// Deliver queued child reports and run a turn so the model can act on them.
///
/// This is the idle path: children finished after the parent's turn ended, and
/// the notices are the only reason to spend another request. Returns `false`
/// when there was nothing to deliver or the turn did not run.
async fn deliver_pending_notices(
    session: &mut Session,
    runtime: &mut Runtime,
    discovery: &mut Discovery,
    events: &EventEmitter,
    command_rx: &mut mpsc::UnboundedReceiver<TuiCommand>,
    include_process: bool,
) -> bool {
    // A wake turn is a turn too: it must not start with half the tools
    // registered. The notice stays queued and is delivered once discovery
    // settles, which is the same reason a submitted prompt waits.
    if discovery.pending().is_some() {
        return false;
    }

    // An unrelated wake (or a notice already read through `agent_wait`) costs
    // one loop iteration, not a request.
    match deliver_queued_notices(session, runtime, events, include_process).await {
        Ok(true) => {}
        Ok(false) | Err(()) => return false,
    }

    // The wake turn reads the live command channel, so Ctrl-C, Esc, and quit end
    // it exactly as they end a user turn. A turn nobody can cancel is a turn
    // nobody can escape, and this one spends money unattended.
    let result = execute_turn(session, runtime, command_rx, events).await;

    handle_turn_end(session, runtime, discovery, events, &result).await;

    true
}

/// Give the session's spawner the wake a finished child uses.
///
/// The spawner state is created before the driver exists, so the wake is
/// attached here rather than passed in at construction.
fn attach_idle_wake(state: &Arc<crate::spawner::SpawnerState>, wake: Arc<tokio::sync::Notify>) {
    state.set_idle_wake(wake);
}

/// Publish agent and task rows from the shared state.
fn publish_agent_state(
    agents_state: &Option<Arc<crate::spawner::SpawnerState>>,
    tasks_state: &Option<Arc<t8ful_tools::task_tools::SharedTasks>>,
    agents_tx: &watch::Sender<t8ful_tui::bridge::AgentsSnapshot>,
    tasks_tx: &watch::Sender<t8ful_tui::bridge::TasksSnapshot>,
) {
    publish_agent_rows(agents_state, agents_tx);
    publish_task_rows(tasks_state, tasks_tx);
}

fn publish_agent_rows(
    state: &Option<Arc<crate::spawner::SpawnerState>>,
    tx: &watch::Sender<t8ful_tui::bridge::AgentsSnapshot>,
) {
    let Some(state) = state else {
        return;
    };

    let agents = state
        .registry
        .snapshots()
        .into_iter()
        .map(|snapshot| {
            let report = state
                .registry
                .report(&snapshot.id)
                .map(|report| report.render());

            t8ful_tui::bridge::AgentView {
                id: snapshot.id.to_string(),
                role: snapshot.role,
                status: snapshot.status.as_str().to_owned(),
                elapsed_ms: snapshot.elapsed.as_millis(),
                steps: snapshot.steps,
                task: snapshot.task.as_ref().map(ToString::to_string),
                worktree: snapshot
                    .worktree
                    .as_ref()
                    .map(|tree| tree.path.display().to_string()),
                branch: snapshot
                    .worktree
                    .as_ref()
                    .and_then(|tree| tree.branch.clone()),
                report,
            }
        })
        .collect();

    let _ = tx.send(t8ful_tui::bridge::AgentsSnapshot { agents });
}

fn publish_task_rows(
    tasks: &Option<Arc<t8ful_tools::task_tools::SharedTasks>>,
    tx: &watch::Sender<t8ful_tui::bridge::TasksSnapshot>,
) {
    let Some(tasks) = tasks else {
        return;
    };

    let graph = tasks.graph();
    let rows = graph
        .rows()
        .into_iter()
        .map(|row| t8ful_tui::bridge::TaskView {
            id: row.id.to_string(),
            title: row.title,
            state: row.state.as_str().to_owned(),
            blocked: row.blocked,
            owner: row.owner,
            deps: row.deps.iter().map(ToString::to_string).collect(),
        })
        .collect();

    let _ = tx.send(t8ful_tui::bridge::TasksSnapshot { tasks: rows });
}

#[cfg(test)]
mod tests {
    use t8ful_core::event::HistoryRole;
    use t8ful_core::message::{Conversation, Message};
    use t8ful_core::tool::ToolStatus;

    use super::{PROCESS_PROGRESS_PENDING_CAP, history_entries, push_pending, skill_name};

    fn skill_message() -> Message {
        let document = t8ful_tools::SkillDocument {
            name: "review".to_owned(),
            directory: std::path::PathBuf::from("/skills/review"),
            body: "SKILL-BODY-SENTINEL".to_owned(),
            files: Vec::new(),
            truncated: false,
        };
        let (text, _) = t8ful_tools::skill::format_skill(&document, 4096);

        Message::user_text(text)
    }

    #[test]
    fn a_journaled_skill_message_renders_as_a_skill_tool_card() {
        let mut conversation = Conversation::new();

        conversation.push(skill_message());
        conversation.push(Message::user_text("hello"));

        let entries = history_entries(&conversation);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, HistoryRole::Tool);
        assert_eq!(entries[0].tool.as_deref(), Some("skill"));
        assert_eq!(
            entries[0]
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get("name"))
                .and_then(serde_json::Value::as_str),
            Some("review")
        );
        assert_eq!(entries[0].status, Some(ToolStatus::Ok));
        assert!(entries[0].text.contains("SKILL-BODY-SENTINEL"));
        assert!(!entries[0].truncated);
        assert_eq!(entries[1].role, HistoryRole::User);
        assert_eq!(entries[1].text, "hello");
    }

    #[test]
    fn journaled_notices_render_as_information_not_user_prompts() {
        let mut conversation = Conversation::new();

        conversation.push(Message::user_text(
            "<agent_notice agent=\"a1\" status=\"completed\">\nreport\n</agent_notice>\n",
        ));
        conversation.push(Message::user_text(
            "<process_notice id=\"process-1\" state=\"exited\">\noutput\n</process_notice>",
        ));
        conversation.push(Message::user_text("hello"));

        let entries = history_entries(&conversation);

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].role, HistoryRole::Info);
        assert_eq!(entries[1].role, HistoryRole::Info);
        assert_eq!(entries[2].role, HistoryRole::User);
        // The envelope reaches the frontend intact: the TUI decodes it back
        // into a notice card, and a reworded copy here would defeat that.
        assert!(entries[0].text.starts_with("<agent_notice agent=\"a1\""));
        assert!(
            entries[1]
                .text
                .starts_with("<process_notice id=\"process-1\"")
        );
        assert!(entries[1].text.contains("</process_notice>"));
    }

    #[test]
    fn pending_process_progress_is_bounded_and_keeps_character_boundaries() {
        let mut pending = std::collections::BTreeMap::new();
        let chunk = "\u{e9}".repeat(PROCESS_PROGRESS_PENDING_CAP);

        push_pending(&mut pending, "process-1", &chunk, false);

        let entry = pending.get("process-1").expect("pending entry");

        assert!(entry.text.len() <= PROCESS_PROGRESS_PENDING_CAP);
        assert!(entry.text.is_char_boundary(entry.text.len()));
        assert!(entry.truncated);
    }

    #[test]
    fn skill_name_requires_the_exact_wrapper_prefix() {
        assert_eq!(
            skill_name("<skill name=\"review\" root=\"/skills/review\">\nbody"),
            Some("review")
        );
        assert_eq!(skill_name("earlier text <skill name=\"review\">"), None);
        assert_eq!(skill_name("plain prompt"), None);
    }
}
