//! The `t8ful` binary: CLI parsing, state, and process entry points.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use clap::Parser;

use t8ful_core::agent::TurnFinish;
use t8ful_core::message::Message;
use t8ful_tui::{Header, TuiConfig, UsageTotals};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

use crate::cli::{
    Cli, Command, ModelsArgs, RunArgs, SessionsArgs, SkillsArgs, SkillsCommand, SkillsInstallArgs,
    SkillsUpdateArgs, TrustArgs, TrustCommand, UsageArgs,
};
use crate::compose::Session;
use crate::config::{CliOverrides, ConfigError, ConfigPaths, EnvSource, Settings, resolve};
use crate::headless::Renderer;
use crate::session::{LoadedSession, SessionId, SessionJournal};
use crate::state::StatePaths;
use crate::text::terminal_safe_line;
use crate::trust::ProjectTrust;
use crate::usage::UsageLedger;
use t8ful_core::question::{Question, QuestionOption, QuestionRequest};

mod attachments;
mod auth;
mod child_journal;
mod children;
mod cli;
mod compaction;
mod compose;
mod config;
mod driver;
mod exit;
mod files;
mod headless;
mod images;
mod instructions;
mod providers;
mod recall;
mod root;
mod session;
mod skills;
mod spawner;
mod state;
mod text;
mod trust;
mod usage;
mod worktrees;

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    init_tracing(cli.log.as_deref(), cli.command.is_none());

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("t8ful: cannot start the async runtime: {error}");

            return std::process::ExitCode::from(exit::FAILURE as u8);
        }
    };

    let code = runtime.block_on(run(cli));

    std::process::ExitCode::from(code as u8)
}

async fn run(cli: Cli) -> i32 {
    match &cli.command {
        None => run_tui(&cli).await,
        Some(Command::Run(args)) => run_headless(&cli, args).await,
        Some(Command::Models(args)) => run_models(&cli, args).await,
        Some(Command::Sessions(args)) => run_sessions(&cli, args),
        Some(Command::Usage(args)) => run_usage(&cli, args),
        Some(Command::Skills(args)) => run_skills(&cli, args),
        Some(Command::Trust(args)) => run_trust(&cli, args),
    }
}

fn init_tracing(level: Option<&str>, full_screen: bool) {
    let filter = match level {
        Some(level) => EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("warn")),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
    };

    if full_screen {
        let path = state_paths().root.join("logs").join("t8ful.log");

        install_diagnostics(DashboardLog::new(path), filter, Some(false));
    } else {
        // Keep the subscriber's own color decision, including `NO_COLOR`.
        install_diagnostics(std::io::stderr, filter, None);
    }
}

fn install_diagnostics<W>(writer: W, filter: EnvFilter, ansi: Option<bool>)
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let builder = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_env_filter(filter)
        .with_target(false);
    let builder = match ansi {
        Some(ansi) => builder.with_ansi(ansi),
        None => builder,
    };

    let _ = builder.try_init();
}

/// Diagnostic sink for the full-screen interface.
///
/// While the dashboard owns the terminal, stderr is the screen: a log record
/// would land where the cursor sits, usually the composer. Records go to a
/// private state-directory file instead, and the file is created only when
/// something is actually logged, so an untouched run leaves nothing behind.
struct DashboardLog {
    path: PathBuf,
    file: OnceLock<Option<std::fs::File>>,
}

impl DashboardLog {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: OnceLock::new(),
        }
    }
}

impl<'writer> MakeWriter<'writer> for DashboardLog {
    type Writer = DashboardLogWriter<'writer>;

    fn make_writer(&'writer self) -> Self::Writer {
        match self
            .file
            .get_or_init(|| crate::state::open_private_append(&self.path).ok())
        {
            Some(file) => DashboardLogWriter::File(file),
            None => DashboardLogWriter::Discard,
        }
    }
}

/// One record's destination, or a silent fallback when the file cannot open.
enum DashboardLogWriter<'writer> {
    File(&'writer std::fs::File),
    Discard,
}

impl std::io::Write for DashboardLogWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::File(file) => std::io::Write::write(file, buffer),
            Self::Discard => Ok(buffer.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::File(file) => std::io::Write::flush(file),
            Self::Discard => Ok(()),
        }
    }
}

/// Everything needed to resolve settings, discovered once per run.
struct Startup {
    launch_dir: PathBuf,
    root: root::RootInfo,
    config: ConfigPaths,
}

impl Startup {
    fn discover(cli: &Cli) -> Result<Self, i32> {
        let launch_dir = std::env::current_dir().map_err(|error| {
            eprintln!("t8ful: cannot determine the current directory: {error}");
            exit::FAILURE
        })?;

        let root = root::discover(&launch_dir).map_err(|error| {
            eprintln!("t8ful: {error}");
            exit::FAILURE
        })?;
        let config = ConfigPaths::discover(&root.path, cli.config.clone(), &EnvSource::Process);

        Ok(Self {
            launch_dir,
            root,
            config,
        })
    }

    /// The project configuration file this run would read.
    fn project_config(&self) -> &Path {
        &self.config.project
    }

    /// The authority-bearing keys the project configuration asks for.
    ///
    /// Parsed separately from resolution so the prompt can list what the user
    /// would be trusting without granting any of it first.
    fn project_keys(&self) -> Result<Vec<String>, ConfigError> {
        let path = self.project_config();

        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        let mut warnings = Vec::new();
        let parsed = crate::config::parse_layer(path, &text, &mut warnings)?;

        Ok(parsed.requires_trust())
    }

    fn resolve(
        &self,
        overrides: CliOverrides,
        require_model: bool,
        project: ProjectTrust,
    ) -> Result<Settings, i32> {
        self.resolve_quietly(overrides, require_model, project, true)
    }

    /// Resolve settings, optionally reporting warnings.
    ///
    /// The trust prompt probes the restricted resolution to learn what the
    /// project would set, before the user has decided. Reporting "ignoring …"
    /// warnings at that point would be wrong twice over: they would appear
    /// before the answer, and they would be false if the user then trusts the
    /// project. The probe is silent and the final resolution speaks.
    fn resolve_quietly(
        &self,
        overrides: CliOverrides,
        require_model: bool,
        project: ProjectTrust,
        report: bool,
    ) -> Result<Settings, i32> {
        let env = EnvSource::Process;
        let settings = resolve(
            self.root.clone(),
            self.launch_dir.clone(),
            &self.config,
            &overrides,
            &env,
            require_model,
            project,
        )
        .map_err(|error| {
            eprintln!("t8ful: {error}");
            exit::USAGE
        })?;

        if report {
            for warning in &settings.warnings {
                eprintln!("t8ful: warning: {warning}");
            }
        }

        for source in &settings.config_sources {
            tracing::debug!(target: "t8ful::config", path = %source.display(), "loaded configuration");
        }

        Ok(settings)
    }
}

/// Resolve settings shared by every command.
fn settings_for(cli: &Cli, overrides: CliOverrides, require_model: bool) -> Result<Settings, i32> {
    let startup = Startup::discover(cli)?;
    let project = if cli.trust_project {
        ProjectTrust::Trusted
    } else {
        stored_trust(&startup)
    };

    startup.resolve(overrides, require_model, project)
}

/// The trust decision recorded for this project, if it is still current.
///
/// A malformed trust file is a startup error rather than a silent "untrusted":
/// the user made decisions that are now unreadable, and asking again without
/// saying why would hide a real problem. Headless runs surface it as a warning
/// and continue, because a CI script must not block on a prompt.
fn stored_trust(startup: &Startup) -> ProjectTrust {
    let paths = state_paths();
    let hash = trust::config_hash(startup.project_config());

    match trust::state(&paths, &startup.root.path, hash.as_deref()) {
        Ok(trust::TrustState::Trusted) => ProjectTrust::Trusted,
        Ok(_) => ProjectTrust::Restricted,
        Err(error) => {
            eprintln!("t8ful: warning: {error}; project configuration is restricted");

            ProjectTrust::Restricted
        }
    }
}

fn state_paths() -> StatePaths {
    StatePaths::discover(&EnvSource::Process)
}

/// Apply stored credentials to the resolved settings.
///
/// Each provider's environment variable always wins; `auth.json` only fills in
/// when that variable is absent. A malformed file is a startup error, not a
/// silent fallback, because the user asked for those keys to be used.
fn apply_auth(settings: &mut Settings, paths: &StatePaths) -> Result<AuthSources, i32> {
    let openrouter = match auth::resolve(
        paths,
        auth::OPENROUTER,
        settings.openrouter_api_key.as_deref(),
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("t8ful: {error}");

            return Err(exit::FAILURE);
        }
    };
    let opencode = match auth::resolve(
        paths,
        auth::OPENCODE_GO,
        settings.opencode_go_api_key.as_deref(),
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("t8ful: {error}");

            return Err(exit::FAILURE);
        }
    };

    settings.openrouter_api_key = openrouter.key.clone();
    settings.opencode_go_api_key = opencode.key.clone();

    Ok(AuthSources {
        openrouter: openrouter.source,
        opencode_go: opencode.source,
    })
}

/// Where each provider's active credential came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AuthSources {
    pub(crate) openrouter: auth::AuthSource,
    pub(crate) opencode_go: auth::AuthSource,
}

impl AuthSources {
    fn snapshot(self) -> t8ful_tui::AuthSnapshot {
        fn kind(source: auth::AuthSource) -> t8ful_tui::AuthKind {
            match source {
                auth::AuthSource::Environment => t8ful_tui::AuthKind::Environment,
                auth::AuthSource::Stored => t8ful_tui::AuthKind::Stored,
                auth::AuthSource::Missing => t8ful_tui::AuthKind::Missing,
            }
        }

        t8ful_tui::AuthSnapshot {
            openrouter: kind(self.openrouter),
            opencode_go: kind(self.opencode_go),
        }
    }
}

fn ledger(paths: &StatePaths) -> Arc<UsageLedger> {
    Arc::new(UsageLedger::open(paths))
}

/// The session-recall backend shared by the terminal and headless entry points.
///
/// It reads the journals this process already writes; it keeps no index and
/// makes no provider request, so constructing it costs nothing.
fn recall(paths: &StatePaths) -> Arc<dyn t8ful_tools::recall::SessionRecall> {
    Arc::new(crate::recall::JournalRecall::new(
        paths.clone(),
        crate::recall::RecallLimits::default(),
    ))
}

async fn prepare_journal(
    settings: &mut Settings,
    cli: &Cli,
    paths: &StatePaths,
    ledger: Arc<UsageLedger>,
    explicit_model: bool,
    lazy: bool,
) -> Result<(Arc<SessionJournal>, Option<LoadedSession>), i32> {
    if let Some(raw) = &cli.resume {
        let id = SessionId::parse(raw).map_err(|error| {
            eprintln!("t8ful: {error}");
            exit::USAGE
        })?;

        let (journal, loaded) = SessionJournal::open(paths, &id, &settings.root.path, ledger)
            .await
            .map_err(|error| {
                eprintln!("t8ful: cannot resume session {id}: {error}");
                exit::FAILURE
            })?;

        if !explicit_model && let Some(model) = &loaded.model {
            settings.model = Some(model.clone());
        }

        // An explicit `--mode` already won during resolution. Otherwise the
        // journaled mode is a ceiling: it may restore a stricter plan mode but
        // may not widen one configured for this invocation through the
        // environment or a configuration file.
        if cli.mode.is_none() {
            settings.mode = settings.mode.most_restrictive(loaded.mode);
        }

        settings.reasoning_effort = loaded.reasoning_effort;

        Ok((Arc::new(journal), Some(loaded)))
    } else if lazy {
        // The terminal interface writes nothing until the user does something,
        // so an unused session leaves no directory behind.
        let journal = SessionJournal::pending(
            paths,
            &settings.root.path,
            settings.model.clone(),
            settings.mode,
            settings.reasoning_effort,
            None,
            Some(ledger),
        )
        .map_err(|error| {
            eprintln!("t8ful: cannot allocate a session: {error}");
            exit::FAILURE
        })?;

        Ok((Arc::new(journal), None))
    } else {
        let journal = SessionJournal::create(
            paths,
            &settings.root.path,
            settings.model.clone(),
            settings.mode,
            settings.reasoning_effort,
            ledger,
        )
        .await
        .map_err(|error| {
            eprintln!("t8ful: cannot create a session: {error}");
            exit::FAILURE
        })?;

        Ok((Arc::new(journal), None))
    }
}

async fn run_tui(cli: &Cli) -> i32 {
    if !std::io::stdout().is_terminal() {
        eprintln!(
            "t8ful: the full-screen interface needs a terminal; use `t8ful run` for headless execution"
        );

        return exit::FAILURE;
    }

    // The trust decision has to be made before settings are resolved for real,
    // because it decides whether the project layer applies in full.
    let startup = match Startup::discover(cli) {
        Ok(startup) => startup,
        Err(code) => return code,
    };
    let paths = state_paths();
    let bridge_start = t8ful_tui::bridge();
    let mut settings = match trust_gate(cli, &startup, &paths, &bridge_start.1).await {
        Ok(settings) => settings,
        Err(code) => return code,
    };

    // Start the interface with a model instead of an empty id, which the catalog
    // cannot resolve. Headless runs keep requiring a configured one.
    if settings.model.is_none() {
        settings.model = Some(config::DEFAULT_MODEL.to_owned());
    }

    let instructions = match instructions::load(&settings.root.path) {
        Ok(instructions) => instructions,
        Err(error) => {
            eprintln!("t8ful: {error}");

            return exit::FAILURE;
        }
    };

    if let Some(warning) = &instructions.warning {
        eprintln!("t8ful: warning: {warning}");
    }

    let recall = recall(&paths);
    let auth_sources = match apply_auth(&mut settings, &paths) {
        Ok(source) => source,
        Err(code) => return code,
    };
    let ledger = ledger(&paths);
    let (journal, loaded) =
        match prepare_journal(&mut settings, cli, &paths, Arc::clone(&ledger), false, true).await {
            Ok(value) => value,
            Err(code) => return code,
        };

    let (bridge, endpoints) = bridge_start;

    let mode = settings.mode;

    let (conversation, projection, queued, queue_paused, attachments) = match &loaded {
        Some(loaded) => (
            loaded.conversation.clone(),
            loaded.projection.clone(),
            loaded.queued.clone(),
            loaded.queue_paused,
            loaded.attachments.clone(),
        ),
        None => (
            t8ful_core::message::Conversation::new(),
            None,
            Vec::new(),
            false,
            Vec::new(),
        ),
    };

    let (emitter, event_rx) = t8ful_core::event::EventEmitter::channel();
    let agent_env = compose::AgentEnv {
        paths: paths.clone(),
        ledger: Some(Arc::clone(&ledger)),
        events: emitter.clone(),
        tasks: loaded
            .as_ref()
            .map(|loaded| loaded.tasks.clone())
            .unwrap_or_default(),
    };

    let session = match compose::build_interactive(
        &settings,
        &instructions,
        Some(bridge.question_router.clone()),
        settings.model.clone(),
        mode,
        conversation,
        projection,
        Some(journal.clone()),
        Some(recall),
        Some(agent_env),
    )
    .await
    {
        Ok(session) => session,
        Err(error) => {
            eprintln!("t8ful: {error}");

            return exit::FAILURE;
        }
    };

    if !session.sandbox_available {
        eprintln!(
            "t8ful: warning: the planning sandbox is unavailable; plan-mode shell commands will fail \
             closed"
        );
    }

    report_skill_warnings(&session);

    let session_id = journal.id().to_string();
    let header = Header {
        model: settings.model.clone().unwrap_or_default(),
        mode,
        session_id: session.id_string(),
        project_root: settings.root.path.clone(),
        launch_dir: settings.launch_dir.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        branch: root::branch(&settings.launch_dir),
    };
    let models = settings
        .model
        .clone()
        .into_iter()
        .map(|id| t8ful_tui::ModelRow::new(id, None))
        .collect::<Vec<_>>();

    let init = driver::DriverInit {
        paths,
        ledger: Arc::clone(&ledger),
        project_root: settings.root.path.clone(),
        queued,
        queue_paused,
        attachments,
        session_id,
        max_image_encoded_bytes: settings.max_image_encoded_bytes,
        notify_on_exit: settings.notify_on_exit,
        emitter,
        event_rx,
        openrouter_api_key: settings.openrouter_api_key.clone(),
        opencode_go_api_key: settings.opencode_go_api_key.clone(),
        sources: auth_sources,
    };
    let reasoning_effort = session.agent.reasoning_effort();
    // The journal's records are the footer's starting point; the driver only
    // replays them when a resume switches sessions at runtime.
    let usage = loaded
        .as_ref()
        .map(|loaded| UsageTotals::from_records(&loaded.usage))
        .unwrap_or_default();
    let _ = endpoints.auth_tx.send(auth_sources.snapshot());

    let driver = tokio::spawn(driver::drive(session, endpoints, init));

    let result = t8ful_tui::run_tui(
        TuiConfig {
            header,
            theme: t8ful_tui::Theme::for_name(settings.theme),
            usage,
            show_reasoning: settings.show_reasoning,
            reasoning_effort,
            models,
            max_image_encoded_bytes: settings.max_image_encoded_bytes,
            mouse: settings.mouse,
            notifications: settings.notifications,
            notify_command: settings.notify_command.clone(),
        },
        bridge,
    )
    .await;

    let _ = driver.await;

    match result {
        Ok(()) => exit::SUCCESS,
        Err(error) => {
            eprintln!("t8ful: terminal interface failed: {error}");

            exit::FAILURE
        }
    }
}

async fn run_headless(cli: &Cli, args: &RunArgs) -> i32 {
    let prompt = match read_prompt(args.prompt.as_deref()) {
        Ok(prompt) => prompt,
        Err(message) => {
            eprintln!("t8ful: {message}");

            return exit::USAGE;
        }
    };

    let overrides = CliOverrides {
        model: args.model.clone(),
        max_steps: args.max_steps,
        mode: cli.mode.map(Into::into),
    };

    let mut settings = match settings_for(cli, overrides, false) {
        Ok(settings) => settings,
        Err(code) => return code,
    };

    let instructions = match instructions::load(&settings.root.path) {
        Ok(instructions) => instructions,
        Err(error) => {
            eprintln!("t8ful: {error}");

            return exit::FAILURE;
        }
    };

    if let Some(warning) = &instructions.warning {
        eprintln!("t8ful: warning: {warning}");
    }

    let paths = state_paths();

    if let Err(code) = apply_auth(&mut settings, &paths) {
        return code;
    }

    let ledger = ledger(&paths);

    // Resolve every requested skill before creating a journal or contacting the
    // provider, so a typo leaves no partial session behind.
    let mut skill_messages = Vec::new();
    let catalog = skills::SkillCatalog::discover(&settings.skill_roots);

    for warning in catalog.warnings() {
        eprintln!("t8ful: warning: {warning}");
    }

    for name in &args.skill {
        match catalog.load(name) {
            Ok(document) => {
                let (text, truncated) =
                    t8ful_tools::skill::format_skill(&document, settings.max_output_bytes);

                if truncated {
                    eprintln!(
                        "t8ful: warning: skill `{name}` was truncated to fit the output limit"
                    );
                }

                skill_messages.push(Message::user_text(text));
            }
            Err(message) => {
                eprintln!("t8ful: {message}");

                return exit::USAGE;
            }
        }
    }

    let (journal, loaded) = match prepare_journal(
        &mut settings,
        cli,
        &paths,
        Arc::clone(&ledger),
        args.model.is_some(),
        false,
    )
    .await
    {
        Ok(value) => value,
        Err(code) => return code,
    };

    if let Err(error) = settings.require_model() {
        eprintln!("t8ful: {error}");

        return exit::USAGE;
    }

    let mode = settings.mode;

    let recall = recall(&paths);

    let (conversation, projection, queue_paused) = match &loaded {
        Some(loaded) => (
            loaded.conversation.clone(),
            loaded.projection.clone(),
            loaded.queue_paused,
        ),
        None => (t8ful_core::message::Conversation::new(), None, false),
    };

    // The headless run shares one event stream with its children, so a spawned
    // agent's events reach the same renderer as the root's.
    let (emitter, mut events) = t8ful_core::event::EventEmitter::channel();
    let agent_env = compose::AgentEnv {
        paths: paths.clone(),
        ledger: Some(Arc::clone(&ledger)),
        events: emitter.clone(),
        tasks: loaded
            .as_ref()
            .map(|loaded| loaded.tasks.clone())
            .unwrap_or_default(),
    };

    let mut session: Session = match compose::build(
        &settings,
        &instructions,
        None,
        settings.model.clone(),
        mode,
        conversation,
        projection,
        Some(journal.clone()),
        Some(recall),
        Some(agent_env),
    )
    .await
    {
        Ok(session) => session,
        Err(error) => {
            eprintln!("t8ful: {error}");

            return exit::FAILURE;
        }
    };

    match session.load_catalog().await {
        Ok(Some(warning)) => eprintln!("t8ful: warning: {warning}"),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                target: "t8ful::compose",
                "could not load model metadata; using the configured context fallback: {error}"
            );
            eprintln!(
                "t8ful: warning: could not load model metadata ({error}); using the configured \
                 context fallback"
            );
        }
    }

    if queue_paused
        && loaded
            .as_ref()
            .is_some_and(|loaded| !loaded.queued.is_empty())
    {
        eprintln!(
            "t8ful: warning: {} queued follow-up(s) were restored paused; use the terminal interface \
             to resume or discard them",
            loaded.as_ref().map_or(0, |loaded| loaded.queued.len())
        );
    }

    let mut images = Vec::new();

    for path in &args.attach {
        match images::attach_path(&paths, journal.id(), path, settings.max_image_encoded_bytes) {
            Ok(image) => {
                if let Err(error) = journal.record_attachment(image.clone()).await {
                    eprintln!("t8ful: cannot record the attachment: {error}");

                    return exit::FAILURE;
                }

                images.push(image);
            }
            Err(error) => {
                eprintln!("t8ful: {error}");

                return exit::USAGE;
            }
        }
    }

    for message in skill_messages {
        if let Err(error) = journal
            .append(crate::session::JournalRecord::Message {
                message: message.clone(),
            })
            .await
        {
            eprintln!("t8ful: cannot record the skill message: {error}");

            return exit::FAILURE;
        }

        session.conversation.push(message);
    }

    let message = Message::user_with_images(Some(prompt), images);

    if let Err(error) = journal
        .append(crate::session::JournalRecord::Message {
            message: message.clone(),
        })
        .await
    {
        eprintln!("t8ful: cannot record the prompt: {error}");

        return exit::FAILURE;
    }

    session.conversation.push(message);

    let cancel = CancellationToken::new();
    let Session {
        agent,
        conversation,
        processes,
        agents,
        ..
    } = &mut session;

    let mut renderer = Renderer::new(args.format);
    let mut write_error: Option<i32> = None;
    let run = agent.run(conversation, &emitter, &cancel);

    tokio::pin!(run);

    let outcome = loop {
        tokio::select! {
            result = &mut run => break result,
            Some(event) = events.recv() => {
                if let Err(error) = renderer.write(&event) {
                    eprintln!("t8ful: cannot write output: {error}");
                    write_error = Some(exit::FAILURE);
                    cancel.cancel();
                }
            }
            _ = tokio::signal::ctrl_c() => {
                cancel.cancel();
                cancel_headless_children(agents, &emitter);
            }
        }
    };

    // A headless run is one turn. Children that outlive it have nobody to read
    // their reports, so they are stopped rather than left spending money.
    cancel_headless_children(agents, &emitter);

    while let Ok(event) = events.try_recv() {
        if let Err(error) = renderer.write(&event) {
            eprintln!("t8ful: cannot write output: {error}");
            write_error = Some(exit::FAILURE);
        }
    }

    if let Err(error) = renderer.finish() {
        eprintln!("t8ful: cannot flush output: {error}");
        write_error = Some(exit::FAILURE);
    }

    processes.shutdown().await;

    if let Some(code) = write_error {
        return code;
    }

    match outcome {
        Ok(turn) if turn.finish == TurnFinish::InputRequired => exit::INPUT_REQUIRED,
        Ok(_) => exit::SUCCESS,
        Err(t8ful_core::AgentError::Cancelled) => exit::INTERRUPTED,
        Err(error) => {
            eprintln!("t8ful: {error}");

            exit::FAILURE
        }
    }
}

/// Stop any child that outlived a headless turn, and say how many.
fn cancel_headless_children(
    agents: &Option<Arc<crate::spawner::SpawnerState>>,
    events: &t8ful_core::event::EventEmitter,
) {
    let Some(state) = agents else {
        return;
    };

    let cancelled = state.registry.cancel_all();

    if cancelled > 0 {
        events.send(t8ful_core::event::AgentEvent::Warning {
            message: format!(
                "cancelled {cancelled} child agent{} when the headless turn ended; their \
                 worktrees were kept",
                if cancelled == 1 { "" } else { "s" }
            ),
        });
    }
}

async fn run_models(cli: &Cli, args: &ModelsArgs) -> i32 {
    let mut settings = match settings_for(cli, CliOverrides::default(), false) {
        Ok(settings) => settings,
        Err(code) => return code,
    };

    if let Err(code) = apply_auth(&mut settings, &state_paths()) {
        return code;
    }

    let default_route =
        providers::Route::from_id(&settings.provider_kind).unwrap_or(providers::Route::OpenRouter);
    let routing = match providers::RoutingProvider::new(
        settings.openrouter_api_key.clone(),
        settings.base_url.clone(),
        settings.opencode_go_api_key.clone(),
        config::DEFAULT_OPENCODE_GO_BASE_URL,
        default_route,
    ) {
        Ok(routing) => routing,
        Err(error) => {
            eprintln!("t8ful: {error}");

            return exit::FAILURE;
        }
    };

    if !routing.has_credentials() {
        eprintln!("t8ful: {}", t8ful_core::ProviderError::MissingCredentials);

        return exit::FAILURE;
    }

    let catalog = routing.catalog(settings.catalog_timeout).await;

    // A provider whose catalog failed is reported, but the other one's models
    // remain usable, so this is not fatal unless nothing at all came back.
    for warning in &catalog.warnings {
        eprintln!("t8ful: warning: {warning}");
    }

    if catalog.models.is_empty() {
        if let Some(warning) = catalog.warnings.first() {
            eprintln!("t8ful: {warning}");
        } else {
            eprintln!("t8ful: the model catalog is empty");
        }

        return exit::FAILURE;
    }

    report_provider_warning(&routing);

    let models = catalog.models;

    if args.json {
        let payload = models
            .iter()
            .map(|model| {
                serde_json::json!({
                    "id": model.id,
                    "provider": model.provider,
                    "name": model.name,
                    "context_length": model.context_length,
                    "max_output_tokens": model.max_output_tokens,
                    "supports_images": model.supports_images,
                    "prompt_price": model.prompt_price,
                    "completion_price": model.completion_price,
                })
            })
            .collect::<Vec<_>>();

        match serde_json::to_string_pretty(&payload) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("t8ful: cannot serialize the model list: {error}");

                return exit::FAILURE;
            }
        }
    } else {
        print_model_table(&models);
    }

    let _ = std::io::stdout().flush();

    exit::SUCCESS
}

/// Print the merged catalog as aligned columns.
///
/// The provider column is what makes a merged list readable: the same id can
/// appear twice, and only the provider tells the two rows apart.
fn print_model_table(models: &[t8ful_core::ModelInfo]) {
    let header = ["ID", "PROVIDER", "NAME"];
    let mut widths = [header[0].len(), header[1].len(), header[2].len()];
    let rows = models
        .iter()
        .map(|model| {
            [
                terminal_safe_line(&model.id),
                terminal_safe_line(model.provider.as_deref().unwrap_or("-")),
                model
                    .name
                    .as_deref()
                    .map(terminal_safe_line)
                    .unwrap_or_default(),
            ]
        })
        .collect::<Vec<_>>();

    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row.iter()) {
            *width = (*width).max(cell.chars().count());
        }
    }

    println!(
        "{:<id$}  {:<provider$}  NAME",
        header[0],
        header[1],
        id = widths[0],
        provider = widths[1]
    );

    for row in &rows {
        if row[2].is_empty() {
            println!(
                "{:<id$}  {:<provider$}",
                row[0],
                row[1],
                id = widths[0],
                provider = widths[1]
            );
        } else {
            println!(
                "{:<id$}  {:<provider$}  {}",
                row[0],
                row[1],
                row[2],
                id = widths[0],
                provider = widths[1]
            );
        }
    }
}

/// Say which provider a run without one of the keys will not reach.
///
/// Absent credentials are not an error while the other provider works, but the
/// omission is worth one line: the user may not know half the catalog is
/// missing.
fn report_provider_warning(routing: &providers::RoutingProvider) {
    for (route, variable) in [
        (providers::Route::OpenRouter, "OPENROUTER_API_KEY"),
        (providers::Route::OpencodeGo, "OPENCODE_GO_API_KEY"),
    ] {
        if !routing.provider_authenticated(route) {
            eprintln!(
                "t8ful: warning: no {variable}; models from {} are not listed",
                route.id()
            );
        }
    }
}

fn run_sessions(cli: &Cli, args: &SessionsArgs) -> i32 {
    let _ = cli;
    let paths = state_paths();
    let entries = match std::fs::read_dir(&paths.sessions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if args.json {
                println!("[]");
            }

            return exit::SUCCESS;
        }
        Err(error) => {
            eprintln!("t8ful: cannot list sessions: {error}");

            return exit::FAILURE;
        }
    };
    let mut sessions = Vec::new();

    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(id) = SessionId::parse(name) else {
            continue;
        };

        match session::load_read_only(&paths, &id) {
            Ok(loaded) => sessions.push(loaded),
            Err(error) => eprintln!("t8ful: warning: skipping session {id}: {error}"),
        }
    }

    // A session with no prompt has nothing to resume, so it is not listed. The
    // journal may still exist: a mode change, an attachment, or a queued
    // follow-up is recorded before the first prompt.
    sessions.retain(|loaded| session::has_prompt(&loaded.conversation));
    sessions.sort_by(|left, right| {
        right
            .last_message_at()
            .cmp(&left.last_message_at())
            .then_with(|| left.header.id.cmp(&right.header.id))
    });

    if args.json {
        let payload = sessions
            .iter()
            .map(|loaded| {
                serde_json::json!({
                    "id": loaded.header.id.as_str(),
                    "created_at": loaded.header.created_at,
                    "last_message_at": loaded.last_message_at(),
                    "project_root": loaded.header.project_root.display().to_string(),
                    "model": loaded.model,
                    "mode": loaded.mode,
                    "messages": loaded.conversation.messages().len(),
                    "records": loaded.records,
                    "title": session::title_from_prompt(&loaded.conversation),
                })
            })
            .collect::<Vec<_>>();

        match serde_json::to_string_pretty(&payload) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("t8ful: cannot serialize sessions: {error}");

                return exit::FAILURE;
            }
        }
    } else {
        println!("ID\tLAST\tMESSAGES\tMODE\tMODEL\tROOT\tTITLE");

        for loaded in &sessions {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                terminal_safe_line(&loaded.header.id.to_string()),
                loaded.last_message_at(),
                loaded.conversation.messages().len(),
                loaded.mode.label(),
                terminal_safe_line(loaded.model.as_deref().unwrap_or("-")),
                terminal_safe_line(&loaded.header.project_root.display().to_string()),
                terminal_safe_line(&session::title_from_prompt(&loaded.conversation))
            );
        }
    }

    exit::SUCCESS
}

fn report_skill_warnings(session: &Session) {
    for warning in session.skills.warnings() {
        eprintln!("t8ful: warning: {warning}");
    }
}

/// Ask about the project's configuration when it needs a decision, then resolve
/// settings with the outcome.
///
/// The prompt appears only when the project configuration actually asks for
/// authority, so opening an ordinary repository stays silent. Answering is a
/// three-way choice: trust and remember, continue without the project layer for
/// this run, or exit.
async fn trust_gate(
    cli: &Cli,
    startup: &Startup,
    paths: &StatePaths,
    endpoints: &t8ful_tui::TuiEndpoints,
) -> Result<Settings, i32> {
    let overrides = CliOverrides {
        mode: cli.mode.map(Into::into),
        ..CliOverrides::default()
    };

    // `--trust-project` is a one-run grant and never consults or writes the
    // store, so a script cannot accidentally make it permanent.
    if cli.trust_project {
        return startup.resolve(overrides, false, ProjectTrust::Trusted);
    }

    let hash = trust::config_hash(startup.project_config());
    let recorded = match trust::state(paths, &startup.root.path, hash.as_deref()) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("t8ful: warning: {error}; project configuration is restricted");

            trust::TrustState::Absent
        }
    };

    if recorded.is_trusted() {
        return startup.resolve(overrides, false, ProjectTrust::Trusted);
    }

    // What would the project layer like to set? Resolved restricted, so the
    // probe cannot itself grant anything.
    let probe =
        startup.resolve_quietly(overrides.clone(), false, ProjectTrust::Restricted, false)?;
    let wanted = startup.project_keys().map_err(|error| {
        eprintln!("t8ful: {error}");

        exit::FAILURE
    })?;

    if wanted.is_empty() {
        // Nothing to ask about: keep the restricted resolution, which for such a
        // project is identical to the trusted one.
        return Ok(probe);
    }

    let request = trust_request(startup, &wanted, recorded);
    let outcome = t8ful_tui::ask(endpoints, request).await;

    // Row order matches `trust_request`: trust, continue, exit. A dismissed
    // prompt or an unusable terminal continues restricted, which is the safe
    // default and matches what happens when there is nothing to ask about.
    let answer = match outcome {
        t8ful_tui::AskOutcome::Chosen(0) => TrustAnswer::Trust,
        t8ful_tui::AskOutcome::Chosen(2) => TrustAnswer::Exit,
        _ => TrustAnswer::Continue,
    };

    match answer {
        TrustAnswer::Trust => {
            if let Err(error) = trust::record(
                paths,
                &startup.root.path,
                hash.as_deref(),
                t8ful_tui::now_seconds(),
            ) {
                eprintln!("t8ful: warning: cannot record the trust decision: {error}");
            }

            startup.resolve(overrides, false, ProjectTrust::Trusted)
        }
        // Report what was ignored, now that the user has chosen to continue.
        TrustAnswer::Continue => startup.resolve(overrides, false, ProjectTrust::Restricted),
        TrustAnswer::Exit => {
            eprintln!("t8ful: exiting; the project was not trusted");

            Err(exit::SUCCESS)
        }
    }
}

/// What the user chose at the trust prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrustAnswer {
    Trust,
    Continue,
    Exit,
}

/// The three-way prompt.
fn trust_request(
    startup: &Startup,
    wanted: &[String],
    recorded: trust::TrustState,
) -> QuestionRequest {
    // The question panel renders the question body, not the header, so the
    // heading has to be part of the text for the user to see it.
    let mut intro = if recorded == trust::TrustState::Changed {
        format!(
            "Trust this project?\nThe configuration of {} has changed since you trusted it, and \
             it now wants to:",
            startup.root.path.display()
        )
    } else {
        format!(
            "Trust this project?\n{} wants to:",
            startup.root.path.display()
        )
    };

    for key in wanted {
        intro.push_str(&format!("\n  - {key}"));
    }

    let option = |label: &str, description: &str| QuestionOption {
        label: label.to_owned(),
        description: description.to_owned(),
        preview: None,
    };

    QuestionRequest {
        questions: vec![Question {
            header: "Trust this project?".to_owned(),
            question: intro,
            options: vec![
                option(
                    "Trust this project",
                    "Apply its configuration and remember this decision",
                ),
                option(
                    "Continue without project configuration",
                    "Ordinary settings still apply; this run only",
                ),
                option("Exit", "Do not start the session"),
            ],
            multi_select: false,
        }],
    }
}

/// Inspect or revoke per-project trust decisions.
///
/// Revoking takes effect on the next run: this process has already resolved its
/// settings, and changing them mid-run would leave the session built from a
/// configuration the user just rejected.
fn run_trust(cli: &Cli, args: &TrustArgs) -> i32 {
    let paths = state_paths();

    match &args.command {
        TrustCommand::List => {
            let projects = match trust::list(&paths) {
                Ok(projects) => projects,
                Err(error) => {
                    eprintln!("t8ful: {error}");

                    return exit::FAILURE;
                }
            };

            if projects.is_empty() {
                println!("no trusted projects");

                return exit::SUCCESS;
            }

            for (path, entry) in projects {
                let age = t8ful_tui::format_age(entry.trusted_at, t8ful_tui::now_seconds());

                println!("{path}\ttrusted {age}");
            }

            exit::SUCCESS
        }
        TrustCommand::Revoke(options) => {
            let startup = match Startup::discover(cli) {
                Ok(startup) => startup,
                Err(code) => return code,
            };
            let root = match &options.path {
                Some(path) => match std::fs::canonicalize(path) {
                    Ok(path) => path,
                    Err(error) => {
                        eprintln!("t8ful: cannot resolve {}: {error}", path.display());

                        return exit::USAGE;
                    }
                },
                None => startup.root.path.clone(),
            };

            match trust::revoke(&paths, &root) {
                Ok(true) => {
                    println!("revoked trust for {}", root.display());

                    exit::SUCCESS
                }
                Ok(false) => {
                    println!("{} was not trusted", root.display());

                    exit::SUCCESS
                }
                Err(error) => {
                    eprintln!("t8ful: {error}");

                    exit::FAILURE
                }
            }
        }
    }
}

fn run_skills(cli: &Cli, args: &SkillsArgs) -> i32 {
    let settings = match settings_for(cli, CliOverrides::default(), false) {
        Ok(settings) => settings,
        Err(code) => return code,
    };

    match &args.command {
        SkillsCommand::List(options) => {
            let catalog = skills::SkillCatalog::discover(&settings.skill_roots);

            skills_list(&catalog, options.json)
        }
        SkillsCommand::Install(options) => skills_install(&settings, options),
        SkillsCommand::Update(options) => skills_update(&settings, options),
    }
}

fn skills_list(catalog: &skills::SkillCatalog, json: bool) -> i32 {
    for warning in catalog.warnings() {
        eprintln!("t8ful: warning: {warning}");
    }

    if json {
        let payload = catalog
            .skills()
            .iter()
            .map(|skill| {
                let managed = skill.managed.as_ref();

                serde_json::json!({
                    "name": skill.name,
                    "description": skill.description,
                    "source": skill.source.label(),
                    "path": skill.directory.display().to_string(),
                    "commit": managed.map(|managed| managed.commit.clone()),
                    "revision": managed.and_then(|managed| managed.revision.clone()),
                    "subdir": managed.and_then(|managed| managed.subdir.clone()),
                    "source_url": managed.map(|managed| managed.source_url.clone()),
                })
            })
            .collect::<Vec<_>>();

        match serde_json::to_string_pretty(&payload) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("t8ful: cannot serialize the skill list: {error}");

                return exit::FAILURE;
            }
        }
    } else if catalog.skills().is_empty() {
        println!("no skills found");
    } else {
        println!("NAME\tSOURCE\tCOMMIT\tDESCRIPTION");

        for skill in catalog.skills() {
            let commit = skill
                .managed
                .as_ref()
                .map(|managed| short_commit(&managed.commit))
                .unwrap_or("-");

            println!(
                "{}\t{}\t{}\t{}",
                terminal_safe_line(&skill.name),
                skill.source.label(),
                terminal_safe_line(commit),
                terminal_safe_line(&short_description(&skill.description))
            );
        }
    }

    exit::SUCCESS
}

fn skills_install(settings: &Settings, options: &SkillsInstallArgs) -> i32 {
    let Some(destination) =
        skills::install::destination_root(&settings.skill_roots, options.project)
    else {
        eprintln!("t8ful: cannot determine the skills directory for this installation");

        return exit::FAILURE;
    };
    let request = skills::install::InstallOptions {
        source: &options.source,
        revision: options.rev.as_deref(),
        subdir: options.subdir.as_deref(),
        name: options.name.as_deref(),
    };

    match skills::install::install(&settings.skill_roots, &destination, &request) {
        Ok(installed) => {
            println!(
                "installed {} at {} in {}",
                terminal_safe_line(&installed.name),
                short_commit(&installed.commit),
                terminal_safe_line(&installed.path.display().to_string())
            );

            for warning in installed.warnings {
                eprintln!("t8ful: warning: {warning}");
            }

            exit::SUCCESS
        }
        Err(error) => install_error_exit(&error),
    }
}

fn skills_update(settings: &Settings, options: &SkillsUpdateArgs) -> i32 {
    match skills::install::update(&settings.skill_roots, &options.name, options.rev.as_deref()) {
        Ok(skills::install::UpdateOutcome::Updated {
            name,
            old_commit,
            new_commit,
            warnings,
        }) => {
            println!(
                "updated {} {} → {}",
                terminal_safe_line(&name),
                short_commit(&old_commit),
                short_commit(&new_commit)
            );

            for warning in warnings {
                eprintln!("t8ful: warning: {warning}");
            }

            exit::SUCCESS
        }
        Ok(skills::install::UpdateOutcome::AlreadyCurrent { name, commit }) => {
            println!(
                "{} is already at {}",
                terminal_safe_line(&name),
                short_commit(&commit)
            );

            exit::SUCCESS
        }
        Err(error) => install_error_exit(&error),
    }
}

fn install_error_exit(error: &skills::install::InstallError) -> i32 {
    eprintln!("t8ful: {}", error.message());

    match error {
        skills::install::InstallError::Usage(_) => exit::USAGE,
        skills::install::InstallError::Failure(_) => exit::FAILURE,
    }
}

fn short_commit(commit: &str) -> &str {
    commit.get(..8).unwrap_or(commit)
}

fn short_description(description: &str) -> String {
    const LIMIT: usize = 60;

    if description.chars().count() <= LIMIT {
        return description.to_owned();
    }

    let mut shortened = description.chars().take(LIMIT - 1).collect::<String>();

    shortened.push('…');

    shortened
}

fn run_usage(cli: &Cli, args: &UsageArgs) -> i32 {
    let _ = cli;
    let paths = state_paths();
    let ledger = ledger(&paths);
    let entries = if args.rebuild {
        match usage::rebuild_entries(&paths) {
            Ok(entries) => {
                if let Err(error) = ledger.rebuild_from(&entries) {
                    eprintln!("t8ful: cannot write the rebuilt usage ledger: {error}");

                    return exit::FAILURE;
                }

                entries
            }
            Err(error) => {
                eprintln!("t8ful: cannot rebuild usage: {error}");

                return exit::FAILURE;
            }
        }
    } else {
        match ledger.entries() {
            Ok(entries) if entries.is_empty() => match usage::rebuild_entries(&paths) {
                Ok(entries) => entries,
                Err(error) => {
                    eprintln!("t8ful: cannot read usage: {error}");

                    return exit::FAILURE;
                }
            },
            Ok(entries) => entries,
            Err(error) => {
                eprintln!("t8ful: {error}");

                return exit::FAILURE;
            }
        }
    };
    let summary = usage::summarize(&entries);

    if args.json {
        let payload = serde_json::json!({
            "requests": summary.totals.requests,
            "input_tokens": summary.totals.input_tokens,
            "output_tokens": summary.totals.output_tokens,
            "input_unknown": summary.totals.unknown_input,
            "output_unknown": summary.totals.unknown_output,
            "cost_usd": summary.totals.cost_label(),
            "unknown_cost_records": summary.totals.unknown_cost,
            "merged_records": summary.totals.merged_records,
            "duplicate_generations": summary.totals.duplicate_generations,
            "by_model": summary.by_model.iter().map(|(model, totals)| {
                serde_json::json!({
                    "model": model,
                    "requests": totals.requests,
                    "input": totals.input_label(),
                    "output": totals.output_label(),
                    "cost": totals.cost_label(),
                })
            }).collect::<Vec<_>>(),
            "by_agent": summary.by_agent.iter().map(|(agent, totals)| {
                serde_json::json!({
                    "agent": agent,
                    "requests": totals.requests,
                    "input": totals.input_label(),
                    "output": totals.output_label(),
                    "cost": totals.cost_label(),
                })
            }).collect::<Vec<_>>(),
        });

        match serde_json::to_string_pretty(&payload) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("t8ful: cannot serialize usage: {error}");

                return exit::FAILURE;
            }
        }
    } else {
        println!(
            "{} request(s): input {} / output {} tokens, cost ${}",
            summary.totals.requests,
            summary.totals.input_label(),
            summary.totals.output_label(),
            summary.totals.cost_label()
        );

        if summary.totals.unknown_input > 0 || summary.totals.unknown_cost > 0 {
            println!(
                "partial: {} request(s) with unknown input, {} with unknown cost",
                summary.totals.unknown_input, summary.totals.unknown_cost
            );
        }

        if !summary.by_model.is_empty() {
            println!("\nBy model:");

            for (model, totals) in &summary.by_model {
                println!(
                    "  {}\t{} requests\t{}/{}\t${}",
                    terminal_safe_line(model),
                    totals.requests,
                    totals.input_label(),
                    totals.output_label(),
                    totals.cost_label()
                );
            }
        }
    }

    exit::SUCCESS
}

/// The newest journaled message, falling back to when the session started.
impl LoadedSession {
    fn last_message_at(&self) -> u64 {
        self.last_message_at.max(self.header.created_at)
    }
}

impl Session {
    fn id_string(&self) -> String {
        self.journal
            .as_ref()
            .map(|journal| journal.id().to_string())
            .unwrap_or_default()
    }
}

fn read_prompt(prompt: Option<&str>) -> Result<String, String> {
    match prompt {
        Some("-") => read_stdin(),
        Some(text) if !text.trim().is_empty() => Ok(text.to_owned()),
        Some(_) => Err("the prompt must not be empty".to_owned()),
        None => {
            if std::io::stdin().is_terminal() {
                return Err(
                    "no prompt given: pass one as an argument or pipe it on stdin".to_owned(),
                );
            }

            read_stdin()
        }
    }
}

fn read_stdin() -> Result<String, String> {
    let mut buffer = String::new();

    std::io::stdin()
        .read_to_string(&mut buffer)
        .map_err(|error| format!("cannot read the prompt from stdin: {error}"))?;

    if buffer.trim().is_empty() {
        return Err("the prompt read from stdin is empty".to_owned());
    }

    Ok(buffer)
}
