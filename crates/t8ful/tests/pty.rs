mod support;

use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use support::{FakeServer, Reply, text_stream, text_stream_with_usage, tool_stream};
use tempfile::TempDir;
use tokio::sync::Notify;

/// Mirrors `config::DEFAULT_MODEL` in the binary crate, which an integration
/// test cannot import. The terminal interface starts on this id when nothing
/// configured a model.
const DEFAULT_MODEL: &str = "deepseek/deepseek-v4.1-flash-1017";

/// Everything the child needs: an isolated home, a project root with a config
/// file pointing at the fake provider, and the provider credentials.
struct TuiFixture {
    project: TempDir,
    home: TempDir,
    base_url: String,
}

impl TuiFixture {
    fn new(server: &FakeServer) -> Self {
        Self::with_config(server, "[provider]\nmodel = \"vendor/test-model\"\n")
    }

    fn with_config(server: &FakeServer, config: &str) -> Self {
        let project = TempDir::new().expect("project dir");
        let home = TempDir::new().expect("home dir");

        std::fs::create_dir_all(project.path().join(".t8ful")).expect("config dir");
        std::fs::write(project.path().join(".t8ful/config.toml"), config).expect("write config");
        write_notify_shim(home.path());

        Self {
            project,
            home,
            base_url: server.base_url(),
        }
    }

    /// The same command with no environment credential, so the session starts
    /// disconnected until `/login`.
    fn command_without_key(&self) -> CommandBuilder {
        let mut command = self.command();

        command.env_remove("OPENROUTER_API_KEY");
        command.env_remove("OPENCODE_GO_API_KEY");

        command
    }

    fn write_user_skill(&self, name: &str, description: &str, body: &str) {
        let directory = self.home.path().join("config/t8ful/skills").join(name);

        std::fs::create_dir_all(&directory).expect("skill dir");
        std::fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .expect("skill file");
    }

    fn command(&self) -> CommandBuilder {
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_t8ful"));

        command.cwd(self.project.path());
        command.env("HOME", self.home.path().display().to_string());
        command.env(
            "XDG_CONFIG_HOME",
            self.home.path().join("config").display().to_string(),
        );
        command.env(
            "XDG_STATE_HOME",
            self.home.path().join("state").display().to_string(),
        );
        command.env(
            "XDG_CONFIG_DIRS",
            self.home.path().join("xdg").display().to_string(),
        );
        command.env("OPENROUTER_API_KEY", "test-key");
        command.env("T8FUL_BASE_URL", &self.base_url);
        command.env("T8FUL_LOG", "off");
        command.env("TERM", "xterm-256color");
        command.env("T8FUL_IMAGE_PROTOCOL", "halfblocks");
        command.env_remove("TMUX");
        command.env_remove("TERM_PROGRAM");
        command.env_remove("KITTY_WINDOW_ID");
        command.env_remove("T8FUL_MODEL");
        command.env_remove("T8FUL_MODE");
        command.env("GIT_CONFIG_GLOBAL", "/dev/null");
        command.env("GIT_CONFIG_SYSTEM", "/dev/null");
        command.env("GIT_CONFIG_NOSYSTEM", "1");
        // Shadow any real `notify-send` so a finished turn never raises a
        // desktop notification from the developer's machine.
        command.env(
            "PATH",
            format!(
                "{}:{}",
                notify_shim_dir(self.home.path()).display(),
                std::env::var("PATH").expect("test PATH")
            ),
        );
        command.env(
            "T8FUL_NOTIFY_LOG",
            notify_log(self.home.path()).display().to_string(),
        );

        command
    }

    /// The arguments of each notification raised so far, one tab-separated
    /// line per invocation.
    fn notifications(&self) -> Vec<String> {
        std::fs::read_to_string(notify_log(self.home.path()))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn notify_shim_dir(home: &std::path::Path) -> std::path::PathBuf {
    home.join("noop-bin")
}

fn notify_log(home: &std::path::Path) -> std::path::PathBuf {
    home.join("notifications.log")
}

/// Install a `notify-send` that only records its arguments.
///
/// The interface looks `notify-send` up on `PATH`, so a real one would raise a
/// desktop notification on every test turn. The shim keeps the suite off the
/// desktop and makes the notifier deterministic even on a host that has none.
fn write_notify_shim(home: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    let directory = notify_shim_dir(home);

    std::fs::create_dir_all(&directory).expect("notify shim dir");

    let path = directory.join("notify-send");
    // One log line per invocation, its arguments tab-separated, so a test can
    // tell the title from the message and see how many times it ran.
    let body = "#!/bin/sh\nif [ -n \"$T8FUL_NOTIFY_LOG\" ]; then\n  line=\"$1\"\n  shift\n  for argument in \"$@\"; do\n    line=\"$line\t$argument\"\n  done\n  printf '%s\\n' \"$line\" >> \"$T8FUL_NOTIFY_LOG\"\nfi\nexit 0\n";

    std::fs::write(&path, body).expect("notify shim");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// How long the interface may take to exit before the test kills it.
const QUIT_TIMEOUT: Duration = Duration::from_secs(10);

/// The session directories that exist so far.
///
/// A session is written lazily, so this is empty until the session records its
/// first document.
fn session_dirs(fixture: &TuiFixture) -> Vec<std::path::PathBuf> {
    let sessions = fixture.home.path().join("state/t8ful/sessions");
    let entries = match std::fs::read_dir(&sessions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("session directory: {error}"),
    };

    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

/// The journal text, or an empty string while the session has not been written.
fn session_journal(fixture: &TuiFixture) -> String {
    let dirs = session_dirs(fixture);

    assert!(dirs.len() <= 1, "expected at most one session: {dirs:?}");

    dirs.first()
        .map(|dir| dir.join("journal.jsonl"))
        .filter(|journal| journal.exists())
        .map(|journal| std::fs::read_to_string(journal).expect("session journal"))
        .unwrap_or_default()
}

fn single_session_id(fixture: &TuiFixture) -> String {
    let dirs = session_dirs(fixture);

    assert_eq!(dirs.len(), 1, "expected one session directory: {dirs:?}");

    dirs[0]
        .file_name()
        .and_then(|name| name.to_str())
        .expect("session id")
        .to_owned()
}

async fn wait_for_journal(fixture: &TuiFixture, needle: &str) {
    wait_for_journal_count(fixture, needle, 1).await;
}

async fn wait_for_journal_count(fixture: &TuiFixture, needle: &str, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);

    while Instant::now() < deadline {
        if session_journal(fixture).matches(needle).count() >= count {
            return;
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    panic!(
        "journal never contained {count} copies of {needle:?}: {}",
        session_journal(fixture)
    );
}

fn tiny_png() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();

    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&1_u32.to_be_bytes());
    bytes.extend_from_slice(&1_u32.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes.extend_from_slice(b"IEND");
    bytes
}

struct Session {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    receiver: std::sync::mpsc::Receiver<Vec<u8>>,
    /// Raw terminal output, kept for escape-sequence assertions.
    screen: String,
    /// Parsed alternate-screen contents, kept after the alternate screen exits.
    terminal: vt100::Parser,
    visible: String,
}

impl Session {
    fn start(fixture: &TuiFixture) -> Self {
        Self::start_command(fixture.command())
    }

    fn start_command(command: CommandBuilder) -> Self {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");

        let child = pair
            .slave
            .spawn_command(command)
            .expect("spawn t8ful under a pty");

        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let writer = pair.master.take_writer().expect("pty writer");
        let (sender, receiver) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let mut buffer = [0_u8; 4096];

            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if sender.send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Self {
            child,
            master: pair.master,
            writer,
            receiver,
            screen: String::new(),
            terminal: vt100::Parser::new(
                NonZeroU16::new(30).expect("height"),
                NonZeroU16::new(100).expect("width"),
                0,
            ),
            visible: String::new(),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize pty");
        self.terminal.set_size(
            NonZeroU16::new(rows).expect("height"),
            NonZeroU16::new(cols).expect("width"),
        );
    }

    /// Feed one PTY chunk to both the raw log and the terminal parser.
    fn feed(&mut self, chunk: &[u8]) {
        self.screen.push_str(&String::from_utf8_lossy(chunk));
        self.terminal.process(chunk);

        if self.terminal.screen().alternate_screen() {
            self.visible = self.terminal.screen().contents();
        }
    }

    /// One-based screen row containing `needle`, when it is visible.
    fn row_of(&self, needle: &str) -> Option<u16> {
        self.visible
            .lines()
            .position(|line| line.contains(needle))
            .and_then(|index| u16::try_from(index + 1).ok())
    }

    /// The visible terminal contents, with styling and cursor updates applied.
    fn text(&self) -> &str {
        &self.visible
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write to the pty");
        self.writer.flush().expect("flush the pty");
    }

    /// Wait until the visible screen no longer contains `needle`.
    fn wait_for_gone(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;

        while Instant::now() < deadline {
            if !self.visible.contains(needle) {
                return true;
            }

            match self.receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => self.feed(&chunk),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        !self.visible.contains(needle)
    }

    /// Wait until the busy line is gone.
    ///
    /// The busy line is the only place a spinner verb renders, and the TUI
    /// stops rendering it when `TurnFinished` arrives. Waiting for that draw
    /// is what makes a turn boundary observable; a separator that never
    /// appears is not a wait at all.
    fn wait_for_idle(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;

        while Instant::now() < deadline {
            if !self.busy_visible() {
                return true;
            }

            match self.receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => self.feed(&chunk),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        !self.busy_visible()
    }

    /// Wait until the busy line is drawn, which is how a turn the UI did not
    /// start becomes observable.
    fn wait_for_busy(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;

        while Instant::now() < deadline {
            if self.busy_visible() {
                return true;
            }

            if let Ok(chunk) = self.receiver.recv_timeout(Duration::from_millis(100)) {
                self.feed(&chunk);
            }
        }

        self.busy_visible()
    }

    fn busy_visible(&self) -> bool {
        t8ful_tui::spinner::VERBS
            .iter()
            .any(|verb| self.visible.contains(&format!("{verb}…")))
    }

    /// Wait until the currently visible screen contains `needle`.
    ///
    /// Unlike [`Session::wait_for`], historical bytes in the raw log do not
    /// count. This is how a test waits for a redraw (for example, the footer
    /// showing a newly selected model) instead of matching text that was
    /// already typed into a popup.
    fn wait_for_visible(&mut self, needle: &str, timeout: Duration) -> bool {
        if self.visible.contains(needle) {
            return true;
        }

        let deadline = Instant::now() + timeout;

        while Instant::now() < deadline {
            match self.receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => {
                    self.feed(&chunk);

                    if self.visible.contains(needle) {
                        return true;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        self.visible.contains(needle)
    }

    fn wait_for(&mut self, needle: &str, timeout: Duration) -> bool {
        // A status flash can come and go between two polls. The raw log keeps
        // what the visible screen no longer shows, so a test never misses it
        // merely because the machine was busy.
        if self.visible.contains(needle) || self.screen.contains(needle) {
            return true;
        }

        let deadline = Instant::now() + timeout;

        while Instant::now() < deadline {
            match self.receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => {
                    self.feed(&chunk);

                    if self.visible.contains(needle) || self.screen.contains(needle) {
                        return true;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        false
    }

    /// Wait until the raw terminal log contains `needle`. Used for bytes the
    /// parser consumes, such as the bell.
    fn wait_for_raw(&mut self, needle: &str, timeout: Duration) -> bool {
        if self.screen.contains(needle) {
            return true;
        }

        let deadline = Instant::now() + timeout;

        while Instant::now() < deadline {
            match self.receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => {
                    self.feed(&chunk);

                    if self.screen.contains(needle) {
                        return true;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        false
    }

    fn wait_for_ready(&mut self, timeout: Duration) -> bool {
        if self.ready() {
            return true;
        }

        let deadline = Instant::now() + timeout;

        while Instant::now() < deadline {
            match self.receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => {
                    self.feed(&chunk);

                    if self.ready() {
                        return true;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        false
    }

    fn ready(&self) -> bool {
        self.visible.contains("coding") || self.visible.contains("plan")
    }

    fn drain(&mut self) {
        while let Ok(chunk) = self.receiver.recv_timeout(Duration::from_millis(300)) {
            self.feed(&chunk);
        }
    }

    /// Wait for the interface to exit on its own, feeding output meanwhile.
    ///
    /// Returns `None` when it is still running at the deadline, which is how a
    /// swallowed quit key becomes a failed assertion instead of a hung test.
    fn wait_for_exit(&mut self, timeout: Duration) -> Option<portable_pty::ExitStatus> {
        let deadline = Instant::now() + timeout;

        loop {
            if let Some(status) = self.child.try_wait().expect("poll the pty child") {
                return Some(status);
            }

            if Instant::now() >= deadline {
                return None;
            }

            if let Ok(chunk) = self.receiver.recv_timeout(Duration::from_millis(50)) {
                self.feed(&chunk);
            }
        }
    }

    fn quit(self) -> (bool, String) {
        let (success, screen, _raw) = self.quit_with_raw();

        (success, screen)
    }

    /// Send `Ctrl-D` and wait for exit, bounded.
    ///
    /// `Ctrl-D` only quits from an empty composer, so a test that leaves text
    /// there would otherwise wait forever; a bounded wait reports the failure
    /// with the final screen and kills the child so the suite still finishes.
    fn quit_with_raw(mut self) -> (bool, String, String) {
        self.send(b"\x04");

        let status = self.wait_for_exit(QUIT_TIMEOUT);

        if status.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }

        self.drain();

        let status = status.unwrap_or_else(|| {
            panic!(
                "t8ful did not exit {:?} after Ctrl-D, so the composer was \
                 probably not empty: {:?}",
                QUIT_TIMEOUT, self.visible
            )
        });

        (status.success(), self.visible, self.screen)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tui_streams_a_turn_in_the_alternate_screen_and_restores_the_terminal() {
    let server = FakeServer::start(vec![Reply::body(text_stream("hello from the agent"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the TUI never drew its prompt"
    );

    session.send(b"say hello\r");

    assert!(
        session.wait_for("hello from the agent", Duration::from_secs(20)),
        "the streamed answer never appeared: {:?}",
        session.screen
    );

    let (success, _screen, raw) = session.quit_with_raw();

    assert!(success, "t8ful did not exit cleanly: {raw:?}");
    assert!(
        raw.contains("\u{1b}[?1049h"),
        "the alternate screen was never entered"
    );
    assert!(
        raw.contains("\u{1b}[?1049l"),
        "the alternate screen was not restored on exit"
    );
    assert!(
        raw.contains("\u{1b}[?1006h"),
        "mouse capture was never enabled"
    );
    assert!(
        raw.contains("\u{1b}[?1006l"),
        "mouse capture was not disabled on exit"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_frame_does_not_wait_for_the_model_catalog() {
    let gate = Arc::new(Notify::new());
    let models = serde_json::json!({ "data": [{ "id": "vendor/test-model" }] }).to_string();
    let server = FakeServer::start_with_models_reply(
        vec![Reply::body(text_stream("unused"))],
        Reply::json(models).with_gate(Arc::clone(&gate)),
    )
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(2)),
        "the first frame waited for the gated catalog: {:?}",
        session.screen
    );

    let deadline = Instant::now() + Duration::from_secs(5);

    while server.models_requests() == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(
        server.models_requests() >= 1,
        "the catalog request was never started"
    );

    let (success, screen) = session.quit();

    gate.notify_one();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_submitted_before_the_catalog_is_queued() {
    let gate = Arc::new(Notify::new());
    let models = serde_json::json!({
        "data": [{
            "id": "vendor/test-model",
            "context_length": 128000,
            "top_provider": { "max_completion_tokens": 16384 },
            "architecture": { "input_modalities": ["text", "image"] }
        }]
    })
    .to_string();
    let server = FakeServer::start_with_models_reply(
        vec![Reply::body(text_stream("after catalog"))],
        Reply::json(models).with_gate(Arc::clone(&gate)),
    )
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"hello\r");

    assert!(
        session.wait_for("Follow-up: hello", Duration::from_secs(20)),
        "the prompt was not held while metadata was pending: {:?}",
        session.screen
    );
    assert!(
        server.requests().is_empty(),
        "a chat request was made before metadata resolved"
    );

    gate.notify_one();

    assert!(
        session.wait_for("after catalog", Duration::from_secs(20)),
        "the queued prompt never ran: {:?}",
        session.screen
    );

    let requests = server.requests();

    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0].body["model"],
        serde_json::json!("vendor/test-model")
    );
    assert_eq!(requests[0].body["max_tokens"], serde_json::json!(16384));

    let (success, _screen) = session.quit();

    assert!(success);
}

/// `bash` from `PATH`, so the fixture is launched explicitly rather than
/// through its shebang: a Nix build sandbox has no `/usr/bin/env`.
fn bash_on_path() -> String {
    let path = std::env::var("PATH").expect("PATH must be set");

    std::env::split_paths(&path)
        .map(|directory| directory.join("bash"))
        .find(|candidate| candidate.is_file())
        .expect("bash must be on PATH for the MCP tests")
        .display()
        .to_string()
}

/// The fake MCP stdio server shipped with `t8ful-mcp`.
fn mcp_server_script() -> std::path::PathBuf {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../t8ful-mcp/tests/support/fake_server.sh");

    assert!(
        path.is_file(),
        "the fake MCP server fixture is missing: {}",
        path.display()
    );

    path
}

/// One MCP server whose handshake is held until a file appears.
///
/// The gate is a file rather than a timer, so the test observes the real
/// window in which startup is pending without guessing at a duration.
struct GatedMcp {
    gate: std::path::PathBuf,
    methods: std::path::PathBuf,
    /// `slow` holds the handshake until the gate exists; `ok` answers it at
    /// once, for a test that needs the tools installed and then acts.
    mode: &'static str,
}

impl GatedMcp {
    fn new(fixture: &TuiFixture) -> Self {
        Self {
            gate: fixture.home.path().join("mcp-gate"),
            methods: fixture.home.path().join("mcp-methods.log"),
            mode: "slow",
        }
    }

    /// The same server with an immediate handshake.
    fn ungated(fixture: &TuiFixture) -> Self {
        Self {
            mode: "ok",
            ..Self::new(fixture)
        }
    }

    fn config(&self) -> String {
        format!(
            "[provider]\nmodel = \"vendor/test-model\"\n\n[mcp.servers.fake]\ncommand = [\"{}\", \"{}\", \"{}\", \"{}\"]\npass_env = [\"T8FUL_FAKE_METHOD_LOG\"]\n",
            bash_on_path(),
            mcp_server_script().display(),
            self.mode,
            self.gate.display(),
        )
    }

    /// Let the server answer `initialize`.
    fn release(&self) {
        std::fs::write(&self.gate, b"go").expect("release the MCP handshake");
    }

    fn methods(&self) -> String {
        std::fs::read_to_string(&self.methods).unwrap_or_default()
    }

    /// The command that runs the interface against the gated server.
    ///
    /// A project layer may define an MCP server only when it is trusted, and
    /// `--trust-project` grants that for this run without recording it.
    fn command(&self, fixture: &TuiFixture) -> CommandBuilder {
        let mut command = fixture.command();

        command.arg("--trust-project");
        command.env("T8FUL_FAKE_METHOD_LOG", self.methods.display().to_string());

        command
    }
}

fn with_gated_mcp(server: &FakeServer) -> (TuiFixture, GatedMcp) {
    let fixture = TuiFixture::with_config(server, "[provider]\nmodel = \"vendor/test-model\"\n");
    let gated = GatedMcp::new(&fixture);

    std::fs::write(
        fixture.project.path().join(".t8ful/config.toml"),
        gated.config(),
    )
    .expect("write project config");

    (fixture, gated)
}

/// Wait until the fake server recorded one request method.
async fn wait_for_mcp_method(gated: &GatedMcp, method: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if gated.methods().lines().any(|line| line == method) {
            return true;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    gated.methods().lines().any(|line| line == method)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_frame_does_not_wait_for_mcp_servers() {
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let (fixture, gated) = with_gated_mcp(&server);
    let mut session = Session::start_command(gated.command(&fixture));

    // The same budget the catalog test uses. The handshake is held, so a first
    // frame that appears at all proves startup no longer waits for it.
    assert!(
        session.wait_for_ready(Duration::from_secs(2)),
        "the first frame waited for the MCP handshake: {:?}",
        session.screen
    );
    assert!(
        wait_for_mcp_method(&gated, "initialize", Duration::from_secs(5)).await,
        "the MCP server was never contacted: {:?}",
        gated.methods()
    );

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_submitted_before_mcp_settles_is_queued_then_runs_with_its_tools() {
    let server = FakeServer::start(vec![Reply::body(text_stream("after mcp"))]).await;
    let (fixture, gated) = with_gated_mcp(&server);
    let mut session = Session::start_command(gated.command(&fixture));

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"hello\r");

    // The catalog may still be loading too, so either wording proves the hold;
    // what matters is that the prompt waits for discovery instead of running.
    assert!(
        session.wait_for("before running this prompt", Duration::from_secs(20)),
        "the prompt was not held while MCP startup was pending: {:?}",
        session.screen
    );
    assert!(
        server.requests().is_empty(),
        "a chat request was made before the MCP servers settled"
    );

    gated.release();

    assert!(
        session.wait_for("after mcp", Duration::from_secs(20)),
        "the queued prompt never ran: {:?}",
        session.screen
    );

    let requests = server.requests();

    assert_eq!(requests.len(), 1, "{requests:?}");

    // The first request must already carry the settled registry and the
    // prompt rebuilt from it: a tool that appeared mid-turn would change what
    // the model was told it could do.
    let tools = requests[0].body["tools"].as_array().expect("tools");

    assert!(
        tools
            .iter()
            .any(|tool| tool["function"]["name"] == serde_json::json!("mcp__fake__echo")),
        "the first request did not offer the MCP tool: {tools:?}"
    );

    let system = requests[0].body["messages"][0]["content"]
        .as_str()
        .expect("system content");

    assert!(
        system.contains("mcp__fake__echo"),
        "the rebuilt system prompt did not name the MCP tool: {system}"
    );

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_session_withdraws_the_previous_sessions_mcp_tools() {
    let server = FakeServer::start(vec![Reply::body(text_stream("fresh session"))]).await;
    let fixture = TuiFixture::with_config(&server, "[provider]\nmodel = \"vendor/test-model\"\n");
    let mcp = GatedMcp::ungated(&fixture);

    std::fs::write(
        fixture.project.path().join(".t8ful/config.toml"),
        mcp.config(),
    )
    .expect("write project config");

    let mut session = Session::start_command(mcp.command(&fixture));

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    assert!(
        wait_for_mcp_method(&mcp, "tools/list", Duration::from_secs(20)).await,
        "the MCP server never listed its tools: {:?}",
        mcp.methods()
    );

    // The driver handles the two commands in order: the switch first, then the
    // prompt, so the request below is the new session's first.
    session.send(b"/new\r");
    session.send(b"hello\r");

    assert!(
        session.wait_for("fresh session", Duration::from_secs(20)),
        "the prompt never ran in the new session: {:?}",
        session.screen
    );

    let requests = server.requests();

    assert_eq!(requests.len(), 1, "{requests:?}");

    let tools = requests[0].body["tools"].as_array().expect("tools");

    assert!(
        !tools.iter().any(|tool| {
            tool["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .starts_with("mcp__")
        }),
        "the new session kept the previous session's MCP tools: {tools:?}"
    );

    let system = requests[0].body["messages"][0]["content"]
        .as_str()
        .expect("system content");

    assert!(
        !system.contains("<mcp>"),
        "the new session's prompt still lists MCP tools: {system}"
    );

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_mcp_server_does_not_block_the_first_frame() {
    let server = FakeServer::start(vec![Reply::body(text_stream("without mcp"))]).await;
    let fixture = TuiFixture::with_config(
        &server,
        "[provider]\nmodel = \"vendor/test-model\"\n\n[mcp.servers.broken]\ncommand = [\"/nonexistent/t8ful-test-mcp\"]\n",
    );
    let mut command = fixture.command();

    command.arg("--trust-project");

    let mut session = Session::start_command(command);

    assert!(
        session.wait_for_ready(Duration::from_secs(2)),
        "the first frame waited for a failing MCP server: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("could not start", Duration::from_secs(20)),
        "the failed server produced no warning: {:?}",
        session.screen
    );

    session.send(b"hello\r");

    assert!(
        session.wait_for("without mcp", Duration::from_secs(20)),
        "the run stopped with the server that failed: {:?}",
        session.screen
    );

    let requests = server.requests();

    assert_eq!(requests.len(), 1, "{requests:?}");

    let tools = requests[0].body["tools"].as_array().expect("tools");

    assert!(
        !tools.iter().any(|tool| {
            tool["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .starts_with("mcp__")
        }),
        "a server that never started contributed tools: {tools:?}"
    );

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quitting_while_mcp_is_pending_exits_cleanly() {
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let (fixture, gated) = with_gated_mcp(&server);
    let mut session = Session::start_command(gated.command(&fixture));

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    assert!(
        wait_for_mcp_method(&gated, "initialize", Duration::from_secs(5)).await,
        "the MCP server was never contacted: {:?}",
        gated.methods()
    );

    let (success, screen) = session.quit();

    assert!(
        success,
        "the pending MCP startup kept the process alive: {screen:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_without_a_configured_model_uses_the_built_in_default() {
    let server = FakeServer::start(vec![Reply::body(text_stream("answered"))]).await;
    let fixture = TuiFixture::with_config(&server, "");
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    // Nothing configured a model, so the header already carries the built-in
    // default and no `/model` step is needed before the first prompt.
    assert!(
        session.wait_for_visible("deepseek/deepseek-v4.1-flash-1017", Duration::from_secs(20)),
        "the header never showed the built-in default model: {:?}",
        session.screen
    );

    session.send(b"hello\r");

    assert!(
        session.wait_for("answered", Duration::from_secs(20)),
        "the prompt never ran with the built-in default model: {:?}",
        session.screen
    );

    let requests = server.requests();

    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0].body["model"],
        serde_json::json!("deepseek/deepseek-v4.1-flash-1017")
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_follow_up_runs_with_the_default_model() {
    // The catalog is gated so the follow-up is queued while metadata is still
    // pending; it must then drain at the boundary without a `/model` step.
    let gate = Arc::new(Notify::new());
    let models = serde_json::json!({ "data": [{ "id": "vendor/test-model" }] }).to_string();
    let server = FakeServer::start_with_models_reply(
        vec![Reply::body(text_stream("queued after default"))],
        Reply::json(models).with_gate(Arc::clone(&gate)),
    )
    .await;
    let fixture = TuiFixture::with_config(&server, "");
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"held prompt");
    session.send(b"\x11");

    assert!(
        session.wait_for("Follow-up: held prompt", Duration::from_secs(20)),
        "the follow-up was not queued: {:?}",
        session.screen
    );
    assert!(
        server.requests().is_empty(),
        "a queued prompt ran before the catalog resolved"
    );

    gate.notify_one();

    assert!(
        session.wait_for("queued after default", Duration::from_secs(20)),
        "the queued prompt never ran with the default model: {:?}",
        session.screen
    );

    let requests = server.requests();

    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0].body["model"],
        serde_json::json!("deepseek/deepseek-v4.1-flash-1017")
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_catalog_failure_warns_and_still_runs_the_prompt() {
    let server = FakeServer::start_with_models_reply(
        vec![Reply::body(text_stream("after failure"))],
        Reply::status(500, "{\"error\":\"catalog down\"}"),
    )
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"hello\r");

    assert!(
        session.wait_for("unavailable", Duration::from_secs(20)),
        "no visible catalog warning appeared: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("after failure", Duration::from_secs(20)),
        "the prompt did not run with fallback limits: {:?}",
        session.screen
    );

    let requests = server.requests();

    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0].body["max_tokens"],
        serde_json::json!(3276),
        "the fallback output budget was not used"
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_startup_warning_never_reaches_the_composer() {
    // Nothing configured a model, so the interface starts on the built-in
    // default, which this catalog does not list. The driver both pushes the
    // unresolved-model warning to the transcript and logs it. The log must not
    // reach stderr: in the alternate screen that is the composer itself.
    let models = serde_json::json!({ "data": [{ "id": "vendor/test-model" }] }).to_string();
    let server = FakeServer::start_with_models_reply(vec![], Reply::json(models)).await;
    let fixture = TuiFixture::with_config(&server, "[provider]\n");
    let mut command = fixture.command();

    command.env("T8FUL_LOG", "warn");

    let mut session = Session::start_command(command);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    assert!(
        session.wait_for(
            "was not found in the model catalog",
            Duration::from_secs(20)
        ),
        "the transcript never showed the model warning: {:?}",
        session.screen
    );
    // The raw log keeps every byte, so a log line is caught even if the next
    // frame overwrote it on the visible screen.
    assert!(
        !session.screen.contains("WARN"),
        "a tracing log line reached the terminal: {:?}",
        session.screen
    );

    let log = std::fs::read_to_string(fixture.home.path().join("state/t8ful/logs/t8ful.log"))
        .expect("the diagnostic log file");

    assert!(log.contains("WARN"), "no warning was logged: {log:?}");

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_catalog_timeout_warns_and_still_runs_the_prompt() {
    let gate = Arc::new(Notify::new());
    let models = serde_json::json!({ "data": [{ "id": "vendor/test-model" }] }).to_string();
    let server = FakeServer::start_with_models_reply(
        vec![Reply::body(text_stream("after timeout"))],
        Reply::json(models).with_gate(Arc::clone(&gate)),
    )
    .await;
    let fixture = TuiFixture::new(&server);

    std::fs::write(
        fixture.project.path().join(".t8ful/config.toml"),
        "[provider]\nmodel = \"vendor/test-model\"\ncatalog_timeout_ms = 500\n",
    )
    .expect("write config");

    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"hello\r");

    let started = Instant::now();

    assert!(
        session.wait_for("unavailable", Duration::from_secs(10)),
        "the catalog timeout was not reported: {:?}",
        session.screen
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the metadata deadline was not honored"
    );
    assert!(
        session.wait_for("after timeout", Duration::from_secs(20)),
        "the prompt did not run after the timeout: {:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_tui_stops_redrawing() {
    let server = FakeServer::start(vec![Reply::body(text_stream("quiet"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.drain();

    let before = session.screen.len();

    std::thread::sleep(Duration::from_millis(300));
    session.drain();

    assert_eq!(session.screen.len(), before, "the idle TUI kept redrawing");

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_terminal_is_restored_after_a_provider_error() {
    let server = FakeServer::start(vec![Reply::status(500, "{\"error\":\"boom\"}")]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the TUI never drew its prompt"
    );

    session.send(b"trigger an error\r");

    assert!(
        session.wait_for("error:", Duration::from_secs(20)),
        "the provider error never appeared: {:?}",
        session.screen
    );

    let (success, _screen, raw) = session.quit_with_raw();

    assert!(success, "t8ful did not exit cleanly: {raw:?}");
    assert!(
        raw.contains("\u{1b}[?1049l"),
        "the alternate screen was not restored after an error"
    );
    assert!(
        raw.contains("\u{1b}[?1006l"),
        "mouse capture was not disabled after an error"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ordinary_edit_applies_without_an_approval_panel() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "edit_file",
            &serde_json::json!({ "path": "app.txt", "edits": [{ "old_text": "old", "new_text": "new" }] }),
        )),
        Reply::body(text_stream("edit applied")),
    ])
    .await;

    let fixture = TuiFixture::new(&server);

    std::fs::write(fixture.project.path().join("app.txt"), "old\n").expect("write file");

    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the TUI never drew its prompt"
    );

    session.send(b"rename old to new\r");

    assert!(
        session.wait_for("edit applied", Duration::from_secs(20)),
        "the ordinary edit never finished: {:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
    assert_eq!(
        std::fs::read_to_string(fixture.project.path().join("app.txt")).expect("read file"),
        "new\n"
    );
    assert!(
        screen.contains("✓") && screen.contains("Edit"),
        "{screen:?}"
    );
    assert!(!screen.contains("approval required"), "{screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_outside_write_applies_without_an_approval_panel() {
    let content = "BEGIN-MARKER written outside END-WRITE-SENTINEL";
    let outside = tempfile::Builder::new()
        .prefix("t8ful-pty-outside-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("outside");
    let path = outside.path().join("final-destination.txt");
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "write_file",
            &serde_json::json!({ "content": content, "path": path }),
        )),
        Reply::body(text_stream("write complete")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the TUI never drew its prompt"
    );

    session.send(b"write outside the project\r");

    assert!(
        session.wait_for("write complete", Duration::from_secs(20)),
        "the outside write never finished: {:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
    assert_eq!(std::fs::read_to_string(&path).expect("read file"), content);
    assert!(!screen.contains("approval required"), "{screen:?}");
    assert!(!screen.contains("danger:"), "{screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transcript_long_tool_output_is_visible_and_scrollable() {
    let body = std::iter::once("FIRST-TRANSCRIPT-SENTINEL".to_owned())
        .chain((1..=120).map(|line| format!("output line {line}")))
        .chain(std::iter::once("FINAL-TRANSCRIPT-SENTINEL".to_owned()))
        .collect::<Vec<_>>()
        .join("\n");
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "read_file",
            &serde_json::json!({ "path": "long-output.txt" }),
        )),
        Reply::body(text_stream("done")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);

    std::fs::write(fixture.project.path().join("long-output.txt"), &body).expect("write file");

    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the TUI never drew its prompt"
    );

    session.send(b"show the long output\r");

    assert!(
        session.wait_for("FIRST-TRANSCRIPT-SENTINEL", Duration::from_secs(20)),
        "the start of the tool preview never appeared: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("more lines, ctrl+o to expand", Duration::from_secs(20)),
        "the collapsed preview marker was missing: {:?}",
        session.screen
    );

    // Expand the output before paging to its beginning.
    session.send(b"\x0f");
    session.drain();

    assert!(
        session.wait_for("FINAL-TRANSCRIPT-SENTINEL", Duration::from_secs(20)),
        "the end of the expanded tool output never appeared: {:?}",
        session.screen
    );

    session.resize(50, 16);
    // Let the resize redraw settle before page keys arrive, so the page step
    // is computed from the new viewport rather than the previous one.
    session.drain();

    for _ in 0..30 {
        session.send(b"\x1b[5~");
    }

    assert!(
        session.wait_for("output line 1", Duration::from_secs(20)),
        "the start of the tool output could not be reached: {:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patch_previews_show_the_diff_and_expand_with_ctrl_o() {
    let mut patch = String::from("--- /dev/null\n+++ b/preview.txt\n@@ -0,0 +1,10 @@\n");

    for line in 1..=10 {
        patch.push_str(&format!("+preview line {line}\n"));
    }

    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "apply_patch",
            &serde_json::json!({ "patch": patch }),
        )),
        Reply::body(text_stream("patched")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the TUI never drew its prompt"
    );

    session.send(b"create the preview file\r");

    assert!(
        session.wait_for("✓ Patch", Duration::from_secs(20)),
        "the friendly patch header never appeared: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("applied patch to 1 file(s)", Duration::from_secs(20)),
        "the patch summary never appeared: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("+preview line 1", Duration::from_secs(20)),
        "the first patch lines were missing: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("more lines, ctrl+o to expand", Duration::from_secs(20)),
        "the collapsed preview marker was missing: {:?}",
        session.screen
    );
    assert!(
        !session.text().contains("+preview line 10"),
        "the patch preview was not collapsed: {:?}",
        session.screen
    );

    session.send(b"\x0f");

    assert!(
        session.wait_for("+preview line 10", Duration::from_secs(20)),
        "ctrl+o did not expand the patch preview: {:?}",
        session.screen
    );
    assert!(fixture.project.path().join("preview.txt").exists());

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scratch_files_in_tmp_are_read_and_written_without_approval() {
    let path = format!("/tmp/t8ful-pty-scratch-{}.txt", std::process::id());

    let _ = std::fs::remove_file(&path);

    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "write_file",
            &serde_json::json!({ "path": path, "content": "scratch-content" }),
        )),
        Reply::body(tool_stream(
            "call-2",
            "read_file",
            &serde_json::json!({ "path": path }),
        )),
        Reply::body(text_stream("scratch ok")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the TUI never drew its prompt"
    );

    session.send(b"use scratch space\r");

    assert!(
        session.wait_for("scratch ok", Duration::from_secs(20)),
        "the scratch turn never finished: {:?}",
        session.screen
    );
    assert!(
        !session.screen.contains("approval required"),
        "a scratch write or read asked for approval: {:?}",
        session.screen
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("scratch file"),
        "scratch-content"
    );
    assert!(
        session.text().contains("scratch-content"),
        "the /tmp read preview was missing: {:?}",
        session.screen
    );

    let (success, screen) = session.quit();
    let _ = std::fs::remove_file(&path);

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mouse_wheel_scrolls_the_transcript_and_capture_is_restored() {
    let body = std::iter::once("FIRST-MOUSE-SENTINEL".to_owned())
        .chain((1..=120).map(|line| format!("mouse line {line}")))
        .chain(std::iter::once("FINAL-MOUSE-SENTINEL".to_owned()))
        .collect::<Vec<_>>()
        .join("\n");
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "read_file",
            &serde_json::json!({ "path": "mouse-output.txt" }),
        )),
        Reply::body(text_stream("done")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);

    std::fs::write(fixture.project.path().join("mouse-output.txt"), &body).expect("write file");

    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"show the long output\r");

    assert!(
        session.wait_for("FIRST-MOUSE-SENTINEL", Duration::from_secs(20)),
        "the start of the tool preview never appeared: {:?}",
        session.screen
    );

    session.send(b"\x0f");
    session.drain();

    assert!(
        session.wait_for("FINAL-MOUSE-SENTINEL", Duration::from_secs(20)),
        "the end of the expanded tool output never appeared: {:?}",
        session.screen
    );

    for _ in 0..60 {
        session.send(b"\x1b[<64;10;10M");
    }

    assert!(
        session.wait_for("FIRST-MOUSE-SENTINEL", Duration::from_secs(20)),
        "the mouse wheel could not reach the start: {:?}",
        session.screen
    );

    let (success, _screen, raw) = session.quit_with_raw();

    assert!(success, "t8ful did not exit cleanly: {raw:?}");
    assert!(
        raw.contains("\u{1b}[?1006l"),
        "mouse capture was not disabled after wheel scrolling"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_trust_tui_uses_the_trusted_endpoint() {
    let trusted = FakeServer::start(vec![Reply::body(text_stream("trusted answer"))]).await;
    let untrusted = FakeServer::start(vec![Reply::body(text_stream("untrusted answer"))]).await;
    let fixture = TuiFixture::new(&trusted);

    std::fs::write(
        fixture.project.path().join(".t8ful/config.toml"),
        format!(
            "[provider]\nmodel = \"vendor/test-model\"\nbase_url = \"{}\"\n",
            untrusted.base_url()
        ),
    )
    .expect("write project config");

    let mut session = Session::start(&fixture);

    // The project selects an endpoint, so it needs a trust decision. Continuing
    // without project configuration is the behavior this test has always
    // checked: the user's own endpoint is used.
    assert!(session.wait_for("Do not start the session", Duration::from_secs(20)));
    session.send(b"2");

    assert!(
        session.wait_for_gone("Do not start the session", Duration::from_secs(20)),
        "the trust prompt never closed: {:?}",
        session.screen
    );
    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the TUI never drew its prompt"
    );

    session.send(b"say hello\r");

    assert!(
        session.wait_for("trusted answer", Duration::from_secs(20)),
        "the trusted endpoint's answer never appeared: {:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
    assert!(untrusted.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tui_turn_reads_then_edits_without_approval() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "read_file",
            &serde_json::json!({ "path": "app.txt" }),
        )),
        Reply::body(tool_stream(
            "call-2",
            "edit_file",
            &serde_json::json!({ "path": "app.txt", "edits": [{ "old_text": "old", "new_text": "new" }] }),
        )),
        Reply::body(text_stream("read and edit complete")),
    ])
    .await;

    let fixture = TuiFixture::new(&server);

    std::fs::write(fixture.project.path().join("app.txt"), "old\n").expect("write file");

    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the TUI never drew its prompt"
    );

    session.send(b"read and change app.txt\r");

    assert!(
        session.wait_for("read and edit complete", Duration::from_secs(20)),
        "the turn never finished: {:?}",
        session.screen
    );
    assert!(
        session.text().contains("✓") && session.text().contains("Read"),
        "the read result was not reported: {:?}",
        session.screen
    );
    assert!(
        !session.text().contains("approval required"),
        "an ordinary edit asked for approval: {:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
    assert_eq!(
        std::fs::read_to_string(fixture.project.path().join("app.txt")).expect("read file"),
        "new\n"
    );
    assert!(
        screen.contains("✓") && screen.contains("Edit"),
        "{screen:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enter_steers_a_running_turn_at_the_next_safe_boundary() {
    let gate = Arc::new(Notify::new());
    let server = FakeServer::start(vec![
        Reply::body(text_stream("first answer")).with_gate(Arc::clone(&gate)),
        Reply::body(text_stream("steered answer")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the TUI never drew its prompt"
    );

    session.send(b"start a slow answer\r");

    wait_for_requests(&server, 1).await;

    session.send(b"use the other file\r");
    gate.notify_one();

    let deadline = Instant::now() + Duration::from_secs(20);

    while server.requests().len() < 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let requests = server.requests();

    assert!(requests.len() >= 2, "{requests:?}");
    let body = &requests[1].body;
    let contains_steering = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .any(|message| message["content"].as_str() == Some("use the other file"));

    assert!(contains_steering, "{body}");
    assert!(
        session.wait_for("steered answer", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let screen = session.text();
    let delivered = screen
        .lines()
        .filter(|line| line.contains("use the other file"))
        .count();

    assert_eq!(
        delivered, 1,
        "the steer should be one transcript line: {screen:?}"
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_tool_call_survives_a_steer() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "bash",
            &serde_json::json!({ "command": "sleep 1; printf survived" }),
        )),
        Reply::body(text_stream("tool done")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"run a slow command\r");
    wait_for_requests(&server, 1).await;

    assert!(
        session.wait_for("sleep 1", Duration::from_secs(20)),
        "the slow call never started: {:?}",
        session.screen
    );

    session.send(b"steer during the tool\r");

    assert!(
        session.wait_for("survived", Duration::from_secs(20)),
        "the steer cancelled the running call: {:?}",
        session.screen
    );
    assert!(
        !session.screen.contains("not executed"),
        "a running call was marked not executed: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("tool done", Duration::from_secs(20)),
        "the turn never reached the next model step: {:?}",
        session.screen
    );

    let requests = server.requests();

    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests[1].body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .any(|message| { message["content"].as_str() == Some("steer during the tool") }),
        "the steer was missing from the next request: {}",
        requests[1].body
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alt_up_recalls_an_undelivered_steer_into_the_composer() {
    let gate = Arc::new(Notify::new());
    let server = FakeServer::start(vec![
        Reply::body(text_stream("first answer")).with_gate(Arc::clone(&gate)),
        Reply::body(text_stream("second answer")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"start a slow answer\r");
    wait_for_requests(&server, 1).await;

    session.send(b"recall me\r");

    assert!(
        session.wait_for("Steering: recall me", Duration::from_secs(20)),
        "the steer was not offered for recall: {:?}",
        session.screen
    );

    session.send(b"\x1b[1;3A");

    assert!(
        session.wait_for_gone("Steering: recall me", Duration::from_secs(20)),
        "Alt-Up did not retract the steer: {:?}",
        session.screen
    );
    assert!(
        session.visible.contains("recall me"),
        "the steer did not return to the composer: {:?}",
        session.screen
    );

    gate.notify_one();

    assert!(
        session.wait_for("first answer", Duration::from_secs(20)),
        "the gated answer never ran: {:?}",
        session.screen
    );

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        server.requests().len(),
        1,
        "a retracted steer still started another model step"
    );

    // The recalled text is a normal composer draft now, so submitting it runs
    // a fresh turn instead of arriving as an automatic continuation.
    session.send(b"\r");

    assert!(
        session.wait_for("second answer", Duration::from_secs(20)),
        "the recalled steer was never submitted: {:?}",
        session.screen
    );

    let requests = server.requests();

    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests[1].body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .any(|message| message["content"].as_str() == Some("recall me")),
        "the recalled steer was missing from the fresh turn: {}",
        requests[1].body
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_follow_ups_run_fifo_after_a_successful_turn() {
    let gate = Arc::new(Notify::new());
    let server = FakeServer::start(vec![
        Reply::body(text_stream("first answer")).with_gate(Arc::clone(&gate)),
        Reply::body(text_stream("queued answer")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"first prompt\r");
    wait_for_requests(&server, 1).await;

    session.send(b"second prompt");
    session.send(b"\x11");
    assert!(
        session.wait_for("Follow-up: second prompt", Duration::from_secs(20)),
        "the follow-up was not queued before the turn ended: {:?}",
        session.screen
    );
    gate.notify_one();

    assert!(
        session.wait_for("queued answer", Duration::from_secs(20)),
        "the queued turn did not run: {:?}",
        session.screen
    );

    let requests = server.requests();

    assert!(requests.len() >= 2, "{requests:?}");
    let messages = requests[1].body["messages"].as_array().expect("messages");
    let has_second = messages
        .iter()
        .any(|message| message["content"].as_str() == Some("second prompt"));

    assert!(has_second, "{}", requests[1].body);

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reasoning_picker_shows_the_levels_the_model_advertises() {
    // Shapes taken from the live catalog: `gpt-6.1-sol` accepts through `max`,
    // while a narrower model stops at `high`.
    let models = serde_json::json!({
        "data": [
            { "id": "vendor/test-model", "context_length": 128000,
              "supported_parameters": ["reasoning"],
              "reasoning": { "mandatory": true, "default_enabled": true,
                             "supported_efforts": ["max", "xhigh", "high", "medium", "low"] } },
            { "id": "vendor/narrow-model", "context_length": 128000,
              "supported_parameters": ["reasoning"],
              "reasoning": { "mandatory": true, "default_enabled": true,
                             "supported_efforts": ["high", "medium", "low"] } }
        ]
    })
    .to_string();
    let server = FakeServer::start_with_models(vec![Reply::body(text_stream("ok"))], &models).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    // Alt+e opens the picker; the advertised levels must be offered.
    session.send(b"\x1be");

    assert!(
        session.wait_for("xhigh", Duration::from_secs(20)),
        "the advertised xhigh was not offered: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("max", Duration::from_secs(20)),
        "the advertised max was not offered: {:?}",
        session.screen
    );

    assert!(
        session.wait_for("↑↓ choose", Duration::from_secs(20)),
        "the picker never opened: {:?}",
        session.screen
    );

    // Walk to `max` and apply it, closing the picker in the same keystroke.
    for _ in 0..5 {
        session.send(b"\x1b[B");
    }

    session.send(b"\r");

    // Switching to the narrower model republishes its levels and clamps the
    // selection, which the footer's model row reports.
    assert!(
        session.wait_for("reasoning max", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"/model vendor/narrow-model\r");

    assert!(
        session.wait_for("reasoning high", Duration::from_secs(20)),
        "the effort did not clamp to the narrower model: {:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_picker_changes_the_model_at_a_turn_boundary() {
    let models = serde_json::json!({
        "data": [
            { "id": "vendor/test-model", "context_length": 32000 },
            { "id": "vendor/other-model", "context_length": 32000 }
        ]
    })
    .to_string();
    let server = FakeServer::start_with_models(
        vec![
            Reply::body(text_stream("first answer")),
            Reply::body(text_stream("second answer")),
        ],
        &models,
    )
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"first\r");

    assert!(
        session.wait_for("first answer", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    // Opening the picker and submitting the next prompt must wait for the
    // turn boundary; the busy line disappearing is that boundary.
    assert!(
        session.wait_for_idle(Duration::from_secs(20)),
        "the first turn never finished: {:?}",
        session.screen
    );
    session.send(b"/model\r");

    assert!(
        session.wait_for("model picker", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    assert!(
        session.wait_for("vendor/other-model", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"[B\r");

    assert!(
        session.wait_for("vendor/other-model", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"second\r");

    assert!(
        session.wait_for("second answer", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let requests = server.requests();

    assert!(requests.len() >= 2, "{requests:?}");
    assert_eq!(
        requests[1].body["model"],
        serde_json::json!("vendor/other-model")
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_picker_updates_when_catalog_metadata_arrives() {
    let gate = Arc::new(Notify::new());
    let models = serde_json::json!({
        "data": [
            { "id": "vendor/test-model", "context_length": 32000 },
            { "id": "vendor/other-model", "context_length": 32000 }
        ]
    })
    .to_string();
    let server = FakeServer::start_with_models_reply(
        vec![Reply::body(text_stream("picker answer"))],
        Reply::json(models).with_gate(Arc::clone(&gate)),
    )
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"/model\r");

    assert!(
        session.wait_for("model picker", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    assert!(
        session.wait_for("vendor/test-model", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    assert!(
        !session.text().contains("vendor/other-model"),
        "the catalog was not actually gated: {:?}",
        session.screen
    );

    gate.notify_one();

    assert!(
        session.wait_for("vendor/other-model", Duration::from_secs(20)),
        "the open picker did not update after catalog load: {:?}",
        session.screen
    );

    session.send(b"\x1b[B\r");
    session.send(b"hello\r");

    assert!(
        session.wait_for("picker answer", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let requests = server.requests();

    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0].body["model"],
        serde_json::json!("vendor/other-model")
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structured_questions_receive_a_choice_from_the_tui() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-question",
            "ask_user_question",
            &serde_json::json!({
                "questions": [{
                    "header": "Scope",
                    "question": "Which file?",
                    "options": [
                        { "label": "a.rs", "description": "first" },
                        { "label": "b.rs", "description": "second" }
                    ]
                }]
            }),
        )),
        Reply::body(text_stream("question handled")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"ask me\r");

    assert!(
        session.wait_for("Which file?", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"1");

    assert!(
        session.wait_for("question handled", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_multi_select_question_collects_checks_and_a_typed_answer() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-multi",
            "ask_user_question",
            &serde_json::json!({
                "questions": [{
                    "header": "Scope",
                    "question": "Which files?",
                    "multiSelect": true,
                    "options": [
                        { "label": "a.rs" },
                        { "label": "b.rs" }
                    ]
                }]
            }),
        )),
        Reply::body(text_stream("MULTI-COMPLETE")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"ask me\r");

    assert!(
        session.wait_for("Which files?", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    // Space checks the highlighted option and 2 checks the second, then a
    // typed answer joins them before Done submits the question.
    session.send(b" ");
    session.send(b"2");
    session.send(b"t");
    session.send(b"and benches");
    session.send(b"\r");
    session.send(b"\x1b[B");
    session.send(b"\r");

    assert!(
        session.wait_for("MULTI-COMPLETE", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    wait_for_journal(&fixture, "a.rs, b.rs").await;

    let journal = session_journal(&fixture);

    assert!(
        journal.contains("a.rs, b.rs; wrote: and benches"),
        "{journal}"
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_turn_holds_the_queue_until_alt_up() {
    let gate = Arc::new(Notify::new());
    let server = FakeServer::start(vec![
        Reply::status(500, "{\"error\":\"boom\"}").with_gate(Arc::clone(&gate)),
        Reply::body(text_stream("queued after resume")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"first\r");
    wait_for_requests(&server, 1).await;
    session.send(b"second");
    session.send(b"\x11");
    assert!(
        session.wait_for("Follow-up: second", Duration::from_secs(20)),
        "the follow-up was not queued before the turn failed: {:?}",
        session.screen
    );
    gate.notify_one();

    assert!(
        session.wait_for("error:", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    assert!(
        session.wait_for("held ·", Duration::from_secs(20)),
        "the held queue was not marked: {:?}",
        session.screen
    );

    // Alt-Up folds the held follow-up back into the composer; submitting it
    // is what actually runs it. If Alt-Up did nothing the composer stays empty
    // and the turn below never happens.
    session.send(b"\x1b[1;3A");
    session.send(b"\r");

    assert!(
        session.wait_for("queued after resume", Duration::from_secs(20)),
        "the held follow-up did not run after Alt-Up: {:?}",
        session.screen
    );

    let requests = server.requests();

    assert!(requests.len() >= 2, "{requests:?}");

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_picker_is_available_before_a_model_is_configured() {
    let models = serde_json::json!({
        "data": [{ "id": "vendor/test-model", "context_length": 32000 }]
    })
    .to_string();
    let server =
        FakeServer::start_with_models(vec![Reply::body(text_stream("configured later"))], &models)
            .await;
    let fixture = TuiFixture::new(&server);

    std::fs::write(
        fixture.project.path().join(".t8ful/config.toml"),
        "[provider]\n",
    )
    .unwrap();

    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the TUI did not start without a model: {:?}",
        session.screen
    );
    // A model is present before the picker selection: the built-in default.
    assert!(
        session.wait_for_visible(DEFAULT_MODEL, Duration::from_secs(20)),
        "the built-in default never reached the footer: {:?}",
        session.screen
    );
    session.send(b"/model\r");

    assert!(
        session.wait_for("model picker", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    assert!(
        session.wait_for("vendor/test-model", Duration::from_secs(20)),
        "the picker never showed the discovered model: {:?}",
        session.screen
    );
    // The active model is the first row, so the catalog model is one below it.
    session.send(b"\x1b[B");
    session.send(b"\r");

    // The picker closes immediately, but the footer model is updated by the
    // driver's ModelChanged event, so the redraw is what proves the switch.
    assert!(
        session.wait_for_gone("model picker", Duration::from_secs(20)),
        "the picker never closed: {:?}",
        session.screen
    );
    assert!(
        session.wait_for_visible("vendor/test-model", Duration::from_secs(20)),
        "the selected model never reached the footer: {:?}",
        session.screen
    );
    session.send(b"hello\r");

    assert!(
        session.wait_for("configured later", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let requests = server.requests();

    assert_eq!(
        requests[0].body["model"],
        serde_json::json!("vendor/test-model")
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_updates_the_system_prompt_for_the_next_request() {
    let server = FakeServer::start(vec![
        Reply::body(text_stream("coding answer")),
        Reply::body(text_stream("first planning answer")),
        Reply::body(text_stream("second planning answer")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"first question\r");

    assert!(session.wait_for("coding answer", Duration::from_secs(20)));

    session.send(b"/plan\r");

    assert!(
        session.wait_for("coding → plan", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"second question\r");

    assert!(
        session.wait_for("first planning answer", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"third question\r");

    assert!(
        session.wait_for("second planning answer", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let requests = server.requests();
    let coding = requests[0].body["messages"][0]["content"]
        .as_str()
        .expect("coding request system prompt");

    assert!(
        coding.contains("Do not substitute shell commands for ordinary source edits"),
        "the tuned editing contract did not reach the coding request: {coding}"
    );

    let system = requests[1].body["messages"][0]["content"]
        .as_str()
        .expect("switch request system prompt");

    assert!(system.contains("You are in plan mode"), "{system}");
    assert!(
        system.contains("edit_file for one or more exact-text replacements in one file"),
        "the tuned editing contract did not reach the planning request: {system}"
    );
    assert!(system.contains("<mode_change>"), "{system}");
    assert!(system.contains("from coding mode to plan mode"), "{system}");

    // The notice is for the first request after the switch only.
    let later = requests[2].body["messages"][0]["content"]
        .as_str()
        .expect("later request system prompt");

    assert!(!later.contains("<mode_change>"), "{later}");
    assert!(later.contains("You are in plan mode"), "{later}");

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_resume_keeps_the_current_session_journal_active() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "write-after-failed-resume",
            "write_file",
            &serde_json::json!({
                "path": "after-failed-resume.txt",
                "content": "recorded"
            }),
        )),
        Reply::body(text_stream("write finished")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let command = fixture.command();

    let mut session = Session::start_command(command);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"/resume missing-session\r");

    assert!(
        session.wait_for("cannot resume session", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"write the fixture file\r");

    assert!(
        session.wait_for("write finished", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let journal = session_journal(&fixture);

    assert!(
        fixture
            .project
            .path()
            .join("after-failed-resume.txt")
            .exists()
    );
    assert!(journal.contains("write the fixture file"), "{journal}");
    assert!(journal.contains("write-after-failed-resume"), "{journal}");

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_is_only_changed_by_user_commands() {
    let server = FakeServer::start(vec![Reply::body(text_stream("ok"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"/plan\r");

    wait_for_journal(&fixture, "\"mode\":\"plan\"").await;

    session.send(b"/code\r");

    wait_for_journal(&fixture, "\"mode\":\"coding\"").await;

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bare_configured_model_uses_the_catalog_vendor_id() {
    let server = FakeServer::start(vec![Reply::body(text_stream("BARE-MODEL-ANSWER"))]).await;
    let fixture = TuiFixture::new(&server);

    std::fs::write(
        fixture.project.path().join(".t8ful/config.toml"),
        "[provider]\nmodel = \"test-model\"\n",
    )
    .expect("write config");

    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the TUI never drew its prompt"
    );

    session.send(b"say something\r");

    assert!(
        session.wait_for("BARE-MODEL-ANSWER", Duration::from_secs(20)),
        "the turn never finished: {:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");

    let requests = server.requests();

    assert_eq!(
        requests[0].body["model"],
        serde_json::json!("vendor/test-model"),
        "{requests:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_models_updates_the_output_budget() {
    let models = serde_json::json!({
        "data": [
            {
                "id": "vendor/test-model",
                "context_length": 32000,
                "top_provider": { "max_completion_tokens": 8000 }
            },
            {
                "id": "vendor/small-model",
                "context_length": 16000,
                "top_provider": { "max_completion_tokens": 64 }
            }
        ]
    })
    .to_string();
    let server = FakeServer::start_with_models(
        vec![
            Reply::body(text_stream("first answer")),
            Reply::body(text_stream("second answer")),
        ],
        &models,
    )
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"first\r");
    assert!(session.wait_for("first answer", Duration::from_secs(20)));
    session.send(b"/model vendor/small-model\r");

    wait_for_journal(&fixture, "\"model\":\"vendor/small-model\"").await;
    session.send(b"second\r");

    assert!(
        session.wait_for("second answer", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let requests = server.requests();

    assert!(requests.len() >= 2, "{requests:?}");
    assert_eq!(requests[1].body["model"], "vendor/small-model");
    assert_eq!(requests[1].body["max_tokens"], 64);

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resuming_a_session_restores_its_transcript() {
    let server = FakeServer::start(vec![
        Reply::body(text_stream("HISTORY_FIRST_ANSWER")),
        Reply::body(text_stream("HISTORY_SECOND_ANSWER")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b" HISTORY_FIRST_PROMPT\r");

    assert!(
        session.wait_for("HISTORY_FIRST_ANSWER", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    wait_for_journal(&fixture, "HISTORY_FIRST_ANSWER").await;

    let (success, _screen) = session.quit();

    assert!(success);

    let id = single_session_id(&fixture);
    let mut command = fixture.command();

    command.arg("--resume");
    command.arg(&id);

    let mut resumed = Session::start_command(command);

    assert!(
        resumed.wait_for("HISTORY_FIRST_PROMPT", Duration::from_secs(20)),
        "the restored prompt was not rendered: {:?}",
        resumed.screen
    );
    assert!(
        resumed.wait_for("HISTORY_FIRST_ANSWER", Duration::from_secs(20)),
        "the restored answer was not rendered: {:?}",
        resumed.screen
    );

    resumed.send(b"HISTORY_SECOND_PROMPT\r");

    assert!(
        resumed.wait_for("HISTORY_SECOND_ANSWER", Duration::from_secs(20)),
        "{:?}",
        resumed.screen
    );

    let (success, _screen) = resumed.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resuming_a_session_restores_its_usage_totals() {
    let server = FakeServer::start(vec![Reply::body(text_stream_with_usage(
        "USAGE_ANSWER",
        1_200,
        340,
        "0.25",
        "gen-usage",
    ))])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"count the tokens\r");

    assert!(
        session.wait_for("USAGE_ANSWER", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    assert!(
        session.wait_for("↑1.2k ↓340", Duration::from_secs(20)),
        "the footer never showed the recorded usage: {:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);

    let id = single_session_id(&fixture);
    let mut command = fixture.command();

    command.arg("--resume");
    command.arg(&id);

    let mut resumed = Session::start_command(command);

    assert!(
        resumed.wait_for("↑1.2k ↓340", Duration::from_secs(20)),
        "the resumed footer lost the journaled usage: {:?}",
        resumed.screen
    );

    let (success, _screen) = resumed.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_attachment_is_not_resent_with_a_new_prompt() {
    let server = FakeServer::start(vec![
        Reply::body(text_stream("IMAGE_FIRST_ANSWER")),
        Reply::body(text_stream("IMAGE_SECOND_ANSWER")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let image = fixture.project.path().join("tiny.png");

    std::fs::write(&image, tiny_png()).unwrap();

    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(format!("/attach {}\r", image.display()).as_bytes());

    assert!(
        session.wait_for("attach: 1 PNG 1x1", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"FIRST_IMAGE_PROMPT\r");

    assert!(
        session.wait_for("IMAGE_FIRST_ANSWER", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    wait_for_journal(&fixture, "IMAGE_FIRST_ANSWER").await;

    let (success, _screen) = session.quit();

    assert!(success);

    let id = single_session_id(&fixture);
    let mut command = fixture.command();

    command.arg("--resume");
    command.arg(&id);

    let mut resumed = Session::start_command(command);

    assert!(resumed.wait_for_ready(Duration::from_secs(20)));
    resumed.send(b"SECOND_IMAGE_PROMPT\r");

    assert!(
        resumed.wait_for("IMAGE_SECOND_ANSWER", Duration::from_secs(20)),
        "{:?}",
        resumed.screen
    );

    let requests = server.requests();
    let last = requests.last().expect("second request");
    let users = last.body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] == "user")
        .collect::<Vec<_>>();
    let last_user = users.last().expect("latest user message");
    let content = last_user["content"].to_string();

    assert!(
        !content.contains("image_url"),
        "the old image was reattached: {content}"
    );

    let (success, _screen) = resumed.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_to_a_text_only_model_rejects_images() {
    let models = serde_json::json!({
        "data": [
            {
                "id": "vendor/test-model",
                "context_length": 32000,
                "architecture": { "input_modalities": ["text", "image"] }
            },
            {
                "id": "vendor/text-only",
                "context_length": 32000,
                "architecture": { "input_modalities": ["text"] }
            }
        ]
    })
    .to_string();
    let server = FakeServer::start_with_models(
        vec![
            Reply::body(text_stream("first answer")),
            Reply::body(text_stream("should not be requested")),
        ],
        &models,
    )
    .await;
    let fixture = TuiFixture::new(&server);
    let image = fixture.project.path().join("tiny.png");

    std::fs::write(&image, tiny_png()).unwrap();

    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"first\r");
    assert!(session.wait_for("first answer", Duration::from_secs(20)));
    wait_for_journal(&fixture, "first answer").await;
    session.send(b"/model vendor/text-only\r");
    wait_for_journal(&fixture, "vendor/text-only").await;
    session.send(format!("/attach {}\r", image.display()).as_bytes());

    assert!(
        session.wait_for("attach: 1 PNG 1x1", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"describe the image\r");
    wait_for_journal(&fixture, "describe the image").await;

    assert!(
        session.wait_for("image input", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    assert_eq!(server.requests().len(), 1);

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dangerous_commands_run_without_an_approval_prompt() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "bash",
            &serde_json::json!({ "command": "printf dangerous-tui-ok" }),
        )),
        Reply::body(text_stream("dangerous command done")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"run it\r");

    assert!(
        session.wait_for("dangerous command done", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    assert!(
        session.screen.contains("dangerous-tui-ok"),
        "{:?}",
        session.screen
    );
    assert!(
        !session.screen.contains("approval required"),
        "{:?}",
        session.screen
    );
    assert!(!session.screen.contains("danger:"), "{:?}", session.screen);

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tab_toggles_between_coding_and_plan_mode() {
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"\t");

    wait_for_journal(&fixture, "\"mode\":\"plan\"").await;
    // The journal is written before the driver forwards ModeChanged to the TUI.
    // Let the TUI apply the new mode so the next Tab requests the opposite one.
    tokio::time::sleep(Duration::from_millis(500)).await;

    session.send(b"\t");

    wait_for_journal_count(&fixture, "\"mode\":\"coding\"", 2).await;

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tab_after_a_presented_plan_draws_a_divider_and_keeps_the_plan() {
    let server = FakeServer::start(vec![Reply::body(tool_stream(
        "call-plan",
        "present_plan",
        &serde_json::json!({ "plan": "# PLAN-MARKER-77\n\nA short plan." }),
    ))])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"/plan\r");

    assert!(
        session.wait_for("coding → plan", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"present the plan\r");

    assert!(
        session.wait_for("Plan ready", Duration::from_secs(20)),
        "the plan hand-off never appeared: {:?}",
        session.screen
    );
    // Reply by keeping the plan; dismissing the panel reveals the plan block,
    // after which the normal Tab binding applies again.
    session.send(b"\x1b");

    assert!(
        session.wait_for_gone("Plan ready", Duration::from_secs(20)),
        "the plan hand-off never closed: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("PLAN-MARKER-77", Duration::from_secs(20)),
        "the plan disappeared with the panel: {:?}",
        session.screen
    );

    session.send(b"\t");

    assert!(
        session.wait_for("plan → coding", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    assert!(
        session.text().contains("PLAN-MARKER-77"),
        "the plan block disappeared after the switch: {:?}",
        session.text()
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

/// Typing at the plan hand-off dismisses it and reaches the composer.
///
/// Regression: the panel branch returned early for every key and its fallback
/// dropped the rest, so typing did nothing and the composer never saw a
/// character.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typing_at_the_plan_hand_off_dismisses_it_and_reaches_the_composer() {
    let server = FakeServer::start(vec![Reply::body(tool_stream(
        "call-plan",
        "present_plan",
        &serde_json::json!({ "plan": "# PLAN-MARKER-78\n\nA short plan." }),
    ))])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"/plan\r");

    assert!(
        session.wait_for("coding → plan", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"present the plan\r");

    assert!(
        session.wait_for("Plan ready", Duration::from_secs(20)),
        "the plan hand-off never appeared: {:?}",
        session.screen
    );

    session.send(b"TYPED-X");

    assert!(
        session.wait_for_gone("Plan ready", Duration::from_secs(10)),
        "typing did not dismiss the hand-off: {:?}",
        session.screen
    );
    assert!(
        session.wait_for_visible("TYPED-X", Duration::from_secs(10)),
        "the typed text never reached the composer: {:?}",
        session.screen
    );

    // Ctrl-D only quits from an empty composer, so clear the typed text with
    // the editor's kill-to-start and wait for the redraw before asking to quit.
    // Sending Escape and Ctrl-D back to back would not work: the terminal
    // parser reads the byte after a lone Escape as part of that key, so both
    // keys would be lost.
    session.send(b"\x15");

    assert!(
        session.wait_for_gone("TYPED-X", Duration::from_secs(10)),
        "kill-to-start did not empty the composer: {:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "the interface did not exit cleanly: {screen:?}");
}

/// Ctrl-C keeps its meaning while the plan hand-off is up.
///
/// Regression: the panel swallowed every unhandled key, so Ctrl-C did nothing
/// and the interface could not be quit without dismissing the panel first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_c_at_the_plan_hand_off_still_quits() {
    let server = FakeServer::start(vec![Reply::body(tool_stream(
        "call-plan",
        "present_plan",
        &serde_json::json!({ "plan": "# PLAN-MARKER-79\n\nA short plan." }),
    ))])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"/plan\r");

    assert!(
        session.wait_for("coding → plan", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"present the plan\r");

    assert!(
        session.wait_for("Plan ready", Duration::from_secs(20)),
        "the plan hand-off never appeared: {:?}",
        session.screen
    );

    session.send(b"\x03");

    let status = session.wait_for_exit(Duration::from_secs(10));

    assert!(
        status.is_some_and(|status| status.success()),
        "Ctrl-C at the plan hand-off did not quit: {:?}",
        session.screen
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slash_completion_filters_and_tab_completes_a_command() {
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"/");

    assert!(
        session.wait_for("Tab complete", Duration::from_secs(20)),
        "the command popup never appeared: {:?}",
        session.screen
    );
    assert!(
        session.screen.contains("/reasoning"),
        "the popup did not list commands: {:?}",
        session.screen
    );

    session.send(b"pla");

    assert!(session.screen.contains("/plan"), "{:?}", session.screen);

    session.send(b"\t");
    session.send(b"\r");

    wait_for_journal(&fixture, "\"mode\":\"plan\"").await;

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_without_an_environment_key_connects_and_reloads_the_catalog() {
    let server = FakeServer::start(vec![Reply::body(text_stream("connected answer"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start_command(fixture.command_without_key());

    assert!(
        session.wait_for("not connected; run /login", Duration::from_secs(20)),
        "the disconnected session did not say so: {:?}",
        session.screen
    );
    assert_eq!(
        server.models_requests(),
        0,
        "a catalog request was made without credentials"
    );

    session.send(b"/login\r");

    assert!(
        session.wait_for("login openrouter", Duration::from_secs(20)),
        "the login prompt never opened: {:?}",
        session.screen
    );
    assert!(
        session.text().contains("not connected"),
        "the prompt did not show the credential source: {:?}",
        session.screen
    );

    session.send(b"sk-or-typed\r");

    // The status flash is short; the catalog request is the durable proof that
    // login connected.
    wait_for_models_requests(&server, 1).await;

    let authorizations = server.models_authorizations();

    assert_eq!(
        authorizations.last(),
        Some(&Some("Bearer sk-or-typed".to_owned())),
        "{authorizations:?}"
    );
    assert!(
        !session.text().contains("sk-or-typed"),
        "the key was echoed to the screen: {:?}",
        session.screen
    );

    session.send(b"hello\r");

    assert!(
        session.wait_for("connected answer", Duration::from_secs(20)),
        "the logged-in session never ran a prompt: {:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "{screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_is_rejected_while_a_turn_is_running() {
    let gate = Arc::new(Notify::new());
    let server = FakeServer::start(vec![
        Reply::body(text_stream("done")).with_gate(Arc::clone(&gate)),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"first\r");
    wait_for_requests(&server, 1).await;

    assert!(
        !server.requests().is_empty(),
        "the turn never reached the provider"
    );

    session.send(b"/login\r");

    assert!(
        session.wait_for("login openrouter", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"sk-or-typed\r");

    assert!(
        session.wait_for(
            "finish the current turn before logging in/out",
            Duration::from_secs(20)
        ),
        "{:?}",
        session.screen
    );

    gate.notify_one();

    assert!(
        session.wait_for("done", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "{screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_without_an_environment_key_disconnects_the_session() {
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start_command(fixture.command_without_key());

    assert!(
        session.wait_for("not connected; run /login", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    session.send(b"/login\r");

    assert!(
        session.wait_for("login openrouter", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"sk-or-typed\r");

    let stored = fixture.home.path().join("state/t8ful/auth.json");
    let deadline = Instant::now() + Duration::from_secs(20);

    while !stored.exists() {
        assert!(
            Instant::now() < deadline,
            "login never saved the key: {:?}",
            session.screen
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    session.send(b"/logout\r");

    let deadline = Instant::now() + Duration::from_secs(20);
    let still_stored = || {
        std::fs::read_to_string(&stored)
            .map(|contents| contents.contains("sk-or-typed"))
            .unwrap_or(false)
    };

    while Instant::now() < deadline && still_stored() {
        std::thread::sleep(Duration::from_millis(25));
    }

    assert!(!still_stored(), "logout left the stored key in place");
    assert!(
        session.wait_for(
            "model catalog unavailable (not connected; run /login)",
            Duration::from_secs(20)
        ),
        "{:?}",
        session.screen
    );

    session.send(b"hello\r");

    assert!(
        session.wait_for("missing provider credentials", Duration::from_secs(20)),
        "the disconnected session still tried to run a prompt: {:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "{screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_with_an_environment_key_keeps_the_session_connected() {
    let server = FakeServer::start(vec![Reply::body(text_stream("still connected"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"/logout\r");

    assert!(
        session.wait_for(
            "OPENROUTER_API_KEY is set, so the session stays connected",
            Duration::from_secs(20)
        ),
        "{:?}",
        session.screen
    );

    session.send(b"hello\r");

    assert!(
        session.wait_for("still connected", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "{screen:?}");
}

/// `/login opencode-go` stores the Go key under its own name and leaves the
/// OpenRouter credential alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_opencode_go_reports_the_go_credential_source() {
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start_command(fixture.command_without_key());

    assert!(
        session.wait_for("not connected; run /login", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    session.send(b"/login opencode-go\r");

    assert!(
        session.wait_for("login opencode-go", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    // The modal names the variable that overrides the stored key for the
    // provider being logged into.
    assert!(
        session.wait_for(
            "OPENCODE_GO_API_KEY overrides a stored key while it is set",
            Duration::from_secs(20)
        ),
        "{:?}",
        session.screen
    );

    session.send(b"oc_sk-typed\r");

    let stored = fixture.home.path().join("state/t8ful/auth.json");
    let deadline = Instant::now() + Duration::from_secs(20);

    while !stored.exists() {
        assert!(
            Instant::now() < deadline,
            "login never saved the Go key: {:?}",
            session.screen
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let document = std::fs::read_to_string(&stored).expect("read the credentials file");

    assert!(document.contains("opencode_go"), "{document}");
    assert!(document.contains("oc_sk-typed"), "{document}");
    assert!(!document.contains("openrouter"), "{document}");

    // Logging out of one provider must not touch the other's stored key.
    session.send(b"/logout opencode-go\r");

    let deadline = Instant::now() + Duration::from_secs(20);

    while Instant::now() < deadline {
        let gone = std::fs::read_to_string(&stored)
            .map(|contents| !contents.contains("oc_sk-typed"))
            .unwrap_or(true);

        if gone {
            break;
        }

        std::thread::sleep(Duration::from_millis(25));
    }

    let document = std::fs::read_to_string(&stored).expect("read the credentials file");

    assert!(!document.contains("oc_sk-typed"), "{document}");

    let (success, screen) = session.quit();

    assert!(success, "{screen:?}");
}

fn init_git_repo(dir: &std::path::Path) {
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t8ful")
            .env("GIT_AUTHOR_EMAIL", "t8ful@t8ful.invalid")
            .env("GIT_COMMITTER_NAME", "t8ful")
            .env("GIT_COMMITTER_EMAIL", "t8ful@t8ful.invalid")
            .status()
            .expect("run git");

        assert!(status.success(), "git {args:?} failed");
    };

    run(&["init", "--quiet"]);
    run(&["add", "."]);
    run(&["commit", "--quiet", "-m", "initial"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn markdown_responses_render_headings_lists_and_code() {
    let answer = "# Heading\n\n- one\n- two\n\n```rust\nfn main() { let x = 1; }\n```\n";
    let server = FakeServer::start(vec![Reply::body(text_stream(answer))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"render markdown\r");

    assert!(
        session.wait_for("Heading", Duration::from_secs(20)),
        "the markdown heading never rendered: {:?}",
        session.screen
    );
    assert!(
        !session.text().contains("# Heading"),
        "the raw heading syntax was shown: {:?}",
        session.screen
    );
    assert!(
        session.text().contains('•')
            && session.text().contains("one")
            && session.text().contains("two"),
        "the markdown list never rendered: {:?}",
        session.screen
    );
    assert!(
        session.text().contains("fn main()"),
        "the code block never rendered: {:?}",
        session.screen
    );
    assert!(
        !session.text().contains("```"),
        "the raw fence syntax was shown: {:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_footer_shows_the_git_branch_of_the_launch_directory() {
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::new(&server);

    init_git_repo(fixture.project.path());

    let status = std::process::Command::new("git")
        .args(["-C", fixture.project.path().to_str().expect("path")])
        .args(["checkout", "--quiet", "-b", "pi-branch"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("run git");

    assert!(status.success());

    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for("(pi-branch)", Duration::from_secs(20)),
        "the footer never showed the branch: {:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ansi_theme_starts_successfully() {
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::new(&server);

    std::fs::write(
        fixture.project.path().join(".t8ful/config.toml"),
        "[provider]\nmodel = \"vendor/test-model\"\n[ui]\ntheme = \"ansi\"\n",
    )
    .expect("write config");

    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the ANSI theme did not draw the interface: {:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "t8ful did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_picker_opens_from_the_command_and_alt_e() {
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"/reasoning\r");

    assert!(
        session.wait_for(
            "↑↓ choose · Enter apply · Esc close",
            Duration::from_secs(20)
        ),
        "the reasoning picker never opened: {:?}",
        session.screen
    );

    session.send(b"\x1b");

    assert!(
        session.wait_for_gone("reasoning level", Duration::from_secs(20)),
        "the picker did not close: {:?}",
        session.screen
    );

    // Alt-E opens the picker; ctrl-e is end-of-line in the composer now.
    session.send(b"\x1be");

    assert!(
        session.wait_for(
            "↑↓ choose · Enter apply · Esc close",
            Duration::from_secs(20)
        ),
        "alt+e did not open the reasoning picker: {:?}",
        session.screen
    );

    session.send(b"\x1b[B");
    session.send(b"\r");

    wait_for_journal(&fixture, "\"reasoning_effort\":\"off\"").await;

    assert!(
        session.wait_for("reasoning off", Duration::from_secs(20)),
        "the footer did not show the effort: {:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_card_appears_at_dispatch_and_streams_its_output() {
    let gate_dir = tempfile::TempDir::new().expect("gate dir");
    let gate = gate_dir.path().join("release");
    let command = format!(
        "printf 'streamed-line'\\n'; while [ ! -e '{}' ]; do sleep 0.02; done; printf 'done\\n'",
        gate.display()
    );
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "bash",
            &serde_json::json!({ "command": command }),
        )),
        Reply::body(text_stream("after the tool")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"run it\r");

    assert!(
        session.wait_for("Bash printf", Duration::from_secs(20)),
        "the tool card header never appeared: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("streamed-line", Duration::from_secs(20)),
        "the running card did not stream its output: {:?}",
        session.screen
    );
    assert!(
        !session.text().contains("[exit code 0"),
        "the card was drawn as a finished result while the command was running: {:?}",
        session.screen
    );

    std::fs::write(&gate, "go").expect("release the command");

    assert!(
        session.wait_for("after the tool", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_bash_card_streams_output_before_the_process_exits() {
    let gate_dir = tempfile::TempDir::new().expect("gate dir");
    let gate = gate_dir.path().join("release");
    let command = format!(
        "printf 'bg-live'\\n'; while [ ! -e '{}' ]; do sleep 0.02; done; printf 'bg-done\\n'",
        gate.display()
    );
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "bash",
            &serde_json::json!({ "command": command, "background": true }),
        )),
        Reply::body(text_stream("process started")),
        Reply::body(text_stream("process finished")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"run it\r");

    assert!(
        session.wait_for("Bash printf", Duration::from_secs(20)),
        "the background card header never appeared: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("bg-live", Duration::from_secs(20)),
        "the background card did not stream before exit: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("process started", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    std::fs::write(&gate, "go").expect("release the process");

    assert!(
        session.wait_for("process finished", Duration::from_secs(20)),
        "the exit notice did not wake the agent: {:?}",
        session.screen
    );
    assert!(
        session.text().contains("bg-done"),
        "the committed card lost the tail: {:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);

    let journal = session_journal(&fixture);

    assert!(
        !journal.contains("process_progress"),
        "display-only progress was journaled: {journal}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_background_card_stays_before_later_tool_output() {
    let gate_dir = tempfile::TempDir::new().expect("gate dir");
    let gate = gate_dir.path().join("release");
    let command = format!(
        "printf 'BG-OUTPUT\\n'; while [ ! -e '{}' ]; do sleep 0.02; done",
        gate.display()
    );
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "bash",
            &serde_json::json!({ "command": command, "background": true }),
        )),
        Reply::body(tool_stream(
            "call-2",
            "read_file",
            &serde_json::json!({ "path": "order-marker.txt" }),
        )),
        Reply::body(text_stream("ORDER-FINAL")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);

    std::fs::write(
        fixture.project.path().join("order-marker.txt"),
        "ORDER-MARKER-CONTENT\n",
    )
    .expect("write marker");

    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"run both\r");

    assert!(
        session.wait_for("BG-OUTPUT", Duration::from_secs(20)),
        "the background card never streamed: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("ORDER-MARKER-CONTENT", Duration::from_secs(20)),
        "the foreground result never arrived: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("ORDER-FINAL", Duration::from_secs(20)),
        "the turn never finished: {:?}",
        session.screen
    );

    // Let the last frame land before comparing rows.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(session.wait_for("ORDER-FINAL", Duration::from_secs(5)));

    let background = session
        .row_of("Bash printf")
        .expect("the live background card");
    let foreground = session.row_of("Read").expect("the foreground card");
    let answer = session.row_of("ORDER-FINAL").expect("the final answer");

    assert!(
        background < foreground,
        "the background card moved below the later tool: {:?}",
        session.screen
    );
    assert!(
        foreground < answer,
        "the tool card moved below the answer: {:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_reports_list_view_and_kill_a_background_command() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "bash",
            &serde_json::json!({ "command": "sleep 30", "background": true }),
        )),
        Reply::body(text_stream("process started")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"start it in the background\r");

    assert!(
        session.wait_for("started process-1", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    // The running-process snapshot is published at the turn boundary.
    tokio::time::sleep(Duration::from_millis(500)).await;

    session.send(b"/process list\r");

    assert!(
        session.wait_for("bytes", Duration::from_secs(20)),
        "the process report never appeared: {:?}",
        session.screen
    );
    assert!(
        session.text().contains("process-1"),
        "the report did not name the process: {:?}",
        session.screen
    );
    assert!(
        session.text().contains("sleep 30"),
        "the report did not show the command: {:?}",
        session.screen
    );

    session.send(b"/process view process-1\r");

    assert!(
        session.wait_for("process-1 output:", Duration::from_secs(20)),
        "the collected output was not printed: {:?}",
        session.screen
    );

    session.send(b"/process stop process-1\r");

    assert!(
        session.wait_for("process-1 stopped", Duration::from_secs(20)),
        "the stop was never reported: {:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_three_question_request_can_go_back_and_change_an_answer() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-ask",
            "ask_user_question",
            &serde_json::json!({
                "questions": [
                    {
                        "header": "Scope",
                        "question": "How wide?",
                        "options": [
                            { "label": "Option One" },
                            { "label": "Option Two" }
                        ]
                    },
                    {
                        "header": "Tests",
                        "question": "Which suite?",
                        "options": [
                            { "label": "Fast" },
                            { "label": "Full" }
                        ]
                    },
                    {
                        "header": "Style",
                        "question": "How tidy?",
                        "options": [
                            { "label": "Minimal" },
                            { "label": "Thorough" }
                        ]
                    }
                ]
            }),
        )),
        Reply::body(text_stream("QUESTION-COMPLETE")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"ask me\r");

    assert!(
        session.wait_for("[Scope] Tests Style Review", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    // Answer the first question, go back, change it, then answer the rest.
    session.send(b"1");
    assert!(
        session.wait_for("Scope [Tests] Style Review", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"\x1b[D");
    assert!(
        session.wait_for("[Scope] Tests Style Review", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"2");
    assert!(
        session.wait_for("Scope [Tests] Style Review", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"1");
    assert!(
        session.wait_for("Scope Tests [Style] Review", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"2");
    assert!(
        session.wait_for("Scope Tests Style [Review]", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    session.send(b"\r");

    assert!(
        session.wait_for("QUESTION-COMPLETE", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    wait_for_journal(&fixture, "Option Two").await;

    let journal = session_journal(&fixture);

    assert!(journal.contains("Scope: Option Two"), "{journal}");
    assert!(!journal.contains("Scope: Option One"), "{journal}");
    assert!(journal.contains("Tests: Fast"), "{journal}");
    assert!(journal.contains("Style: Thorough"), "{journal}");

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_turn_rings_the_terminal_bell() {
    let server = FakeServer::start(vec![Reply::body(text_stream("BELL-MARKER"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"make a sound\r");

    assert!(
        session.wait_for("BELL-MARKER", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    assert!(
        session.wait_for_raw("\u{7}", Duration::from_secs(20)),
        "the turn finish did not ring the bell: {:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_turn_notifies_once_with_a_brief_label() {
    let server = FakeServer::start(vec![Reply::body(text_stream("NOTIFY-MARKER"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"say something\r");

    assert!(
        session.wait_for("NOTIFY-MARKER", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    // The desktop message is raised from a detached thread, so the log appears
    // shortly after the turn finished rather than with the reply.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut notifications = fixture.notifications();

    while notifications.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;

        notifications = fixture.notifications();
    }

    // Let a second message, if there were one, land before asserting.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let notifications = fixture.notifications();
    let (success, _screen) = session.quit();

    assert!(success);
    assert_eq!(
        notifications,
        vec!["t8ful\tTurn finished".to_owned()],
        "a finished turn must raise exactly one brief desktop message"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn implementing_here_switches_mode_and_runs_the_plan() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-plan",
            "present_plan",
            &serde_json::json!({ "plan": "# PLAN-HERE-91" }),
        )),
        Reply::body(text_stream("PLAN-IMPLEMENTED-CONTENT")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"/plan\r");

    assert!(session.wait_for("coding → plan", Duration::from_secs(20)));
    session.send(b"present the plan\r");

    assert!(
        session.wait_for("Plan ready", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    assert!(
        session.wait_for_raw("\u{7}", Duration::from_secs(20)),
        "the plan hand-off did not ring the bell: {:?}",
        session.screen
    );

    session.send(b"1");

    assert!(
        session.wait_for("PLAN-IMPLEMENTED-CONTENT", Duration::from_secs(20)),
        "the plan was not implemented: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("plan → coding", Duration::from_secs(20)),
        "the mode switch was not announced: {:?}",
        session.screen
    );
    assert!(
        session_journal(&fixture).contains("Implement the plan you presented."),
        "{}",
        session_journal(&fixture)
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn implementing_in_a_new_session_seeds_the_first_message() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-plan",
            "present_plan",
            &serde_json::json!({ "plan": "# PLAN-SEED-52" }),
        )),
        Reply::body(text_stream("PLAN-NEW-SESSION-REPLY")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"/plan\r");

    assert!(session.wait_for("coding → plan", Duration::from_secs(20)));
    session.send(b"present the plan\r");

    assert!(
        session.wait_for("Plan ready", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    session.send(b"2");

    assert!(
        session.wait_for("Approved plan:", Duration::from_secs(20)),
        "the seed never reached the transcript: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("PLAN-NEW-SESSION-REPLY", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);

    let dirs = session_dirs(&fixture);

    assert_eq!(
        dirs.len(),
        2,
        "expected the old and the new session: {dirs:?}"
    );

    let seeded = dirs
        .iter()
        .filter_map(|dir| std::fs::read_to_string(dir.join("journal.jsonl")).ok())
        .find(|journal| journal.contains("Approved plan:"))
        .expect("a seeded journal");

    assert!(seeded.contains("# PLAN-SEED-52"), "{seeded}");
    assert!(seeded.contains("Implement it now."), "{seeded}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirming_the_plan_hand_off_with_enter_seeds_the_plan() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-plan",
            "present_plan",
            &serde_json::json!({ "plan": "# PLAN-SEED-53" }),
        )),
        Reply::body(text_stream("PLAN-ENTER-SESSION-REPLY")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"/plan\r");

    assert!(session.wait_for("coding → plan", Duration::from_secs(20)));
    session.send(b"present the plan\r");

    assert!(
        session.wait_for("Plan ready", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    // Down highlights "implement in a new session"; Enter confirms it.
    session.send(b"\x1b[B\r");

    assert!(
        session.wait_for("Approved plan:", Duration::from_secs(20)),
        "the seed never reached the transcript: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("PLAN-ENTER-SESSION-REPLY", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);

    let dirs = session_dirs(&fixture);

    assert_eq!(
        dirs.len(),
        2,
        "expected the old and the new session: {dirs:?}"
    );

    let seeded = dirs
        .iter()
        .filter_map(|dir| std::fs::read_to_string(dir.join("journal.jsonl")).ok())
        .find(|journal| journal.contains("Approved plan:"))
        .expect("a seeded journal");

    assert!(
        seeded.contains("# PLAN-SEED-53"),
        "the confirmed plan was not seeded: {seeded}"
    );
    assert!(seeded.contains("Implement it now."), "{seeded}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_exit_wakes_an_idle_agent() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-bg",
            "bash",
            &serde_json::json!({
                "command": "sleep 0.2; printf BG-DONE",
                "background": true
            }),
        )),
        Reply::body(text_stream("PROCESS-NOTICE-SEEN")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"start the watcher\r");

    assert!(
        session.wait_for("PROCESS-NOTICE-SEEN", Duration::from_secs(20)),
        "the process exit never woke the agent: {:?}",
        session.screen
    );

    wait_for_journal(&fixture, "<process_notice").await;

    let journal = session_journal(&fixture);

    assert!(journal.contains("<process_notice"), "{journal}");
    assert!(journal.contains("BG-DONE"), "{journal}");
    assert!(journal.contains("process_output"), "{journal}");

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exit_read_through_process_wait_does_not_wake_the_agent() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-bg",
            "bash",
            &serde_json::json!({
                "command": "sleep 0.2; printf WAIT-OUTPUT",
                "background": true
            }),
        )),
        Reply::body(tool_stream(
            "call-wait",
            "process_wait",
            &serde_json::json!({ "process_id": "process-1", "timeout_s": 20 }),
        )),
        Reply::body(text_stream("READ-THE-EXIT")),
        // Only a wake turn could consume this reply, and it must never come.
        Reply::body(text_stream("WAKE-MARKER")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"start the watcher and wait for it\r");

    assert!(
        session.wait_for("READ-THE-EXIT", Duration::from_secs(30)),
        "process_wait never reported the exit: {:?}",
        session.screen
    );
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert!(
        !session.text().contains("WAKE-MARKER"),
        "the claimed exit started a wake turn: {:?}",
        session.screen
    );
    assert_eq!(
        server.requests().len(),
        3,
        "the claimed exit cost another request"
    );
    assert!(
        !session_journal(&fixture).contains("<process_notice"),
        "the claimed exit was announced again: {}",
        session_journal(&fixture)
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_on_exit_false_waits_for_the_next_prompt() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-bg",
            "bash",
            &serde_json::json!({
                "command": "sleep 0.2; printf BG-DONE",
                "background": true
            }),
        )),
        Reply::body(text_stream("AFTER-PROMPT-SEEN")),
    ])
    .await;
    let fixture = TuiFixture::with_config(
        &server,
        "[provider]\nmodel = \"vendor/test-model\"\n[shell]\nnotify_on_exit = false\n",
    );
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"start the watcher\r");

    assert!(
        session.wait_for("started process-1", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert!(
        !session.text().contains("AFTER-PROMPT-SEEN"),
        "the exit started a turn although notify_on_exit was off: {:?}",
        session.screen
    );

    session.send(b"check the process\r");

    assert!(
        session.wait_for("AFTER-PROMPT-SEEN", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    // The wake check above proves the exit waited; the notice arriving in this
    // turn means `submit_prompt` delivered it with the prompt.
    wait_for_journal(&fixture, "<process_notice").await;

    assert!(
        session_journal(&fixture).contains("<process_notice"),
        "the queued notice was not delivered with the prompt: {}",
        session_journal(&fixture)
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_process_produces_no_notice() {
    let server = FakeServer::start(vec![Reply::body(tool_stream(
        "call-bg",
        "bash",
        &serde_json::json!({
            "command": "sleep 30",
            "background": true
        }),
    ))])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"start the watcher\r");

    assert!(
        session.wait_for("started process-1", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    session.send(b"/process stop process-1\r");

    assert!(
        session.wait_for("process-1 stopped", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        !session_journal(&fixture).contains("<process_notice"),
        "{}",
        session_journal(&fixture)
    );

    let requests = server.requests();

    assert_eq!(requests.len(), 1, "a stopped process started a turn");

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dragging_over_the_transcript_copies_the_selection() {
    let server = FakeServer::start(vec![Reply::body(text_stream("noted"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut command = fixture.command();

    // No graphical clipboard helper: the terminal escape sequence is the only
    // path, which is exactly what a hermetic test can observe.
    command.env_remove("WAYLAND_DISPLAY");
    command.env_remove("XDG_SESSION_TYPE");
    command.env_remove("DISPLAY");
    command.env_remove("SSH_CONNECTION");
    command.env_remove("MOSH_CONNECTION");

    let mut session = Session::start_command(command);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"copy this line\r");

    assert!(
        session.wait_for("noted", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    // Press on the first transcript row, drag across the user line, release.
    session.send(b"\x1b[<0;3;1M\x1b[<32;15;1M\x1b[<0;15;1m");

    assert!(
        session.wait_for_raw("\u{1b}]52;", Duration::from_secs(20)),
        "the drag selection was never copied: {:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

/// Write an executable helper script and return its directory.
fn helper_directory(body: &str) -> TempDir {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDir::new().expect("helper dir");

    for name in ["wl-copy", "xclip"] {
        let path = directory.path().join(name);

        // The helper runs with a PATH that holds only this directory, so its
        // own utilities come from the real search path passed by the test.
        std::fs::write(&path, format!("#!/bin/sh\nPATH=$T8FUL_TEST_PATH\n{body}\n"))
            .expect("helper script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    directory
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pasting_a_text_only_clipboard_is_silent() {
    use std::os::unix::fs::PermissionsExt;

    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut command = fixture.command();
    let directory = TempDir::new().expect("helper dir");
    let paste = directory.path().join("wl-paste");

    std::fs::write(&paste, "#!/bin/sh\necho text/plain\n").expect("helper script");
    std::fs::set_permissions(&paste, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    command.env(
        "PATH",
        format!(
            "{}:{}",
            directory.path().display(),
            std::env::var("PATH").expect("test PATH")
        ),
    );
    command.env("WAYLAND_DISPLAY", "wayland-0");
    command.env_remove("XDG_SESSION_TYPE");
    command.env_remove("DISPLAY");

    let mut session = Session::start_command(command);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"\x16");
    tokio::time::sleep(Duration::from_millis(300)).await;
    session.drain();

    assert!(
        !session.screen.contains(" attach:"),
        "a text-only clipboard added a tray entry: {:?}",
        session.screen
    );
    assert!(
        !session.screen.contains("the clipboard contains no"),
        "a text-only clipboard produced a warning: {:?}",
        session.screen
    );
    assert!(
        !session.screen.contains("reading the clipboard"),
        "Ctrl+V announced a read that then found nothing: {:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copying_with_a_daemonizing_helper_does_not_freeze_the_tui() {
    let server = FakeServer::start(vec![Reply::body(text_stream("noted"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut command = fixture.command();
    let target = TempDir::new().expect("copy target");
    let copied = target.path().join("copied");
    let directory = helper_directory(&format!(
        "cat > {}\n( sleep 20 ) &\nexit 0",
        copied.display()
    ));

    command.env("PATH", directory.path().display().to_string());
    command.env("T8FUL_TEST_PATH", std::env::var("PATH").expect("test PATH"));
    command.env("WAYLAND_DISPLAY", "wayland-0");
    command.env_remove("XDG_SESSION_TYPE");
    command.env_remove("DISPLAY");

    let mut session = Session::start_command(command);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"copy this line\r");

    assert!(
        session.wait_for("noted", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    // Select the user line itself; its row depends on the banner and any
    // startup warnings, so locate it instead of assuming a position.
    let row = session.row_of("copy this line").expect("user row");

    session.send(format!("\x1b[<0;2;{row}M\x1b[<32;20;{row}M\x1b[<0;20;{row}m").as_bytes());

    // The helper's background owner still holds stderr open. A copy that waits
    // for EOF instead of the direct child would never report here, and the UI
    // would stop responding to input.
    let copy_deadline = Instant::now() + Duration::from_secs(10);

    while std::fs::read_to_string(&copied).is_err() {
        assert!(
            Instant::now() < copy_deadline,
            "a daemonizing helper froze the copy: {:?}",
            session.screen
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // The UI must still accept input after copying.
    session.send(b"\x1b[6~");
    session.send(b"still responsive\r");

    assert!(
        session.wait_for("still responsive", Duration::from_secs(20)),
        "the UI stopped accepting input after a copy: {:?}",
        session.screen
    );

    assert_eq!(
        std::fs::read_to_string(&copied).expect("helper recorded the selection"),
        "copy this line"
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_helper_keeps_the_tui_usable_and_falls_back() {
    let server = FakeServer::start(vec![Reply::body(text_stream("noted"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut command = fixture.command();
    let directory = helper_directory("sleep 30");

    command.env("PATH", directory.path().display().to_string());
    command.env("T8FUL_TEST_PATH", std::env::var("PATH").expect("test PATH"));
    command.env("WAYLAND_DISPLAY", "wayland-0");
    command.env_remove("XDG_SESSION_TYPE");
    command.env_remove("DISPLAY");

    let mut session = Session::start_command(command);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"copy this line\r");

    assert!(
        session.wait_for("noted", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let row = session.row_of("copy this line").expect("user row");

    session.send(format!("\x1b[<0;2;{row}M\x1b[<32;16;{row}M\x1b[<0;16;{row}m").as_bytes());

    // Input is handled while the helper is stuck: this keystroke is visible
    // long before the copy deadline expires.
    session.send(b"?");
    assert!(
        session.wait_for("help", Duration::from_millis(1_500)),
        "the UI was blocked by a stalled helper: {:?}",
        session.screen
    );
    session.send(b"\x1b");

    assert!(
        session.wait_for_raw("\u{1b}]52;", Duration::from_secs(15)),
        "a stalled helper never fell back to the terminal: {:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slash_new_starts_an_empty_session_and_shows_the_banner_again() {
    let server = FakeServer::start(vec![Reply::body(text_stream("first answer"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"say something\r");

    assert!(
        session.wait_for("first answer", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    session.send(b"/new\r");

    // `/new` replaces the transcript; wait for the old answer to disappear
    // rather than for the banner, which was already on screen.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut cleared = false;

    while Instant::now() < deadline {
        session.drain();

        if !session.text().contains("first answer") {
            cleared = true;

            break;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(
        cleared,
        "the previous transcript survived /new: {:?}",
        session.screen
    );
    assert!(
        session.text().contains("Thoughts matter."),
        "the banner was not re-seeded for the new session: {:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_background_process_leaves_the_notice() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "bash",
            &serde_json::json!({ "command": "sleep 2", "background": true }),
        )),
        Reply::body(text_stream("process started")),
        Reply::body(text_stream("second turn done")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"start a background process\r");

    assert!(
        session.wait_for("process started", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    assert!(
        session.wait_for("started process-1", Duration::from_secs(20)),
        "the running process was not in the notice: {:?}",
        session.screen
    );

    tokio::time::sleep(Duration::from_millis(2500)).await;

    session.send(b"idle\r");

    assert!(
        session.wait_for("second turn done", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    // The notice is redrawn from the driver's running-process snapshot at the
    // end of the second turn. That snapshot is a separate event, so under load
    // it can render a frame after the answer; wait for it instead of assuming
    // both share one frame.
    let deadline = Instant::now() + Duration::from_secs(10);

    while session.text().contains("process running") && Instant::now() < deadline {
        session.drain();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(
        !session.text().contains("process running"),
        "a finished process still occupied the notice: {:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_shows_saved_sessions_with_their_first_prompt() {
    let server = FakeServer::start(vec![Reply::body(text_stream("noted"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"remember the lantern\r");

    assert!(
        session.wait_for("noted", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let id = single_session_id(&fixture);

    // The first call prints the startup snapshot and asks for a rescan, so a
    // later call is the one that shows the fresh title.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut report = None;

    while Instant::now() < deadline {
        session.send(b"/sessions\r");
        tokio::time::sleep(Duration::from_millis(150)).await;
        session.drain();

        let screen = session.text();

        // The newest report is the last one still on screen.
        if let Some(line) = screen.lines().rev().find(|line| line.contains(&id))
            && line.contains("remember the lantern")
        {
            report = Some(line.trim().to_owned());

            break;
        }
    }

    let report = report.expect("the refreshed report never showed the prompt");

    assert!(report.contains(&id), "{report}");
    assert!(
        report.contains(".tmp"),
        "the report names the project: {report}"
    );
    assert!(report.contains("just now"), "{report}");

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_is_only_written_once_it_has_something_to_record() {
    let server = FakeServer::start(vec![Reply::body(text_stream("noted"))]).await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    // The identifier exists from startup, but an untouched session leaves no
    // directory behind for `/sessions` to list or for a later run to trip on.
    assert!(
        session_dirs(&fixture).is_empty(),
        "a fresh session wrote itself: {:?}",
        session_dirs(&fixture)
    );

    // The usage ledger is lazy for the same reason: its directory appears with
    // the first entry, not when the interface starts.
    let usage = fixture.home.path().join("state/t8ful/usage");

    assert!(!usage.exists(), "startup created the usage directory");

    session.send(b"remember the lantern\r");

    assert!(
        session.wait_for("noted", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    wait_for_journal(&fixture, "remember the lantern").await;

    let dirs = session_dirs(&fixture);

    assert_eq!(dirs.len(), 1, "the prompt created the session: {dirs:?}");

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_picker_lists_titles_and_resumes_the_chosen_session() {
    let server = FakeServer::start(vec![
        Reply::body(text_stream("first answer")),
        Reply::body(text_stream("second answer")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut first = Session::start(&fixture);

    assert!(first.wait_for_ready(Duration::from_secs(20)));
    first.send(b"prime the lantern\r");

    assert!(
        first.wait_for("first answer", Duration::from_secs(20)),
        "{:?}",
        first.screen
    );

    let id = single_session_id(&fixture);
    let (success, _screen) = first.quit();

    assert!(success);

    let mut second = Session::start(&fixture);

    assert!(second.wait_for_ready(Duration::from_secs(20)));
    second.send(b"/resume\r");

    assert!(
        second.wait_for("resume session", Duration::from_secs(20)),
        "{:?}",
        second.screen
    );
    assert!(
        second.wait_for("filter: all sessions", Duration::from_secs(20)),
        "{:?}",
        second.screen
    );
    assert!(
        second.wait_for(&id, Duration::from_secs(20)),
        "{:?}",
        second.screen
    );
    assert!(
        second.wait_for("prime the lantern", Duration::from_secs(20)),
        "{:?}",
        second.screen
    );

    second.send(b"\r");

    // Resuming restores the saved history and keeps the transcript of the
    // picked session rather than the empty one the process started with.
    assert!(
        second.wait_for("first answer", Duration::from_secs(20)),
        "{:?}",
        second.screen
    );
    second.send(b"resume and continue\r");

    assert!(
        second.wait_for("second answer", Duration::from_secs(20)),
        "{:?}",
        second.screen
    );

    let journal = std::fs::read_to_string(
        fixture
            .home
            .path()
            .join("state/t8ful/sessions")
            .join(&id)
            .join("journal.jsonl"),
    )
    .expect("resumed journal");

    assert!(journal.contains("prime the lantern"), "{journal}");
    assert!(journal.contains("resume and continue"), "{journal}");

    let (success, _screen) = second.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_created_during_a_turn_is_mentionable_afterwards() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-1",
            "write_file",
            &serde_json::json!({ "path": "created_after.rs", "content": "fn main() {}\n" }),
        )),
        Reply::body(text_stream("CREATED-ANSWER")),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"create created_after.rs\r");

    assert!(
        session.wait_for("CREATED-ANSWER", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    session.send(b"@created_after");

    assert!(
        session.wait_for("Enter accept", Duration::from_secs(20)),
        "the refreshed index never offered the new file: {:?}",
        session.screen
    );

    session.send(b"\t");

    assert!(
        session.wait_for("@created_after.rs", Duration::from_secs(20)),
        "Tab did not insert the refreshed mention: {:?}",
        session.screen
    );

    // Empty the composer with plain DEL bytes so ctrl+d quits instead of
    // being ignored while the completed mention is still in the input.
    session.send(&[0x7f; 20]);

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_with_a_prompt_loads_it_and_runs_the_prompt_in_one_turn() {
    let server = FakeServer::start(vec![Reply::body(text_stream("combined answer"))]).await;
    let fixture = TuiFixture::new(&server);

    fixture.write_user_skill(
        "review",
        "Review a change.",
        "SKILL-BODY-SENTINEL: check the change.",
    );

    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    wait_for_skills(&mut session).await;
    session.send(b"/skill review SUMMARIZE-THE-CHANGE\r");

    assert!(
        session.wait_for("Skill review", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    assert!(
        session.wait_for("SUMMARIZE-THE-CHANGE", Duration::from_secs(20)),
        "the attached prompt was not echoed: {:?}",
        session.screen
    );
    assert!(
        session.wait_for("combined answer", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let screen = session.text();
    let card = screen.find("Skill review").expect("skill card");
    let prompt = screen.find("SUMMARIZE-THE-CHANGE").expect("prompt echo");

    assert!(card < prompt, "{screen:?}");

    let requests = server.requests();

    assert_eq!(requests.len(), 1, "the attached prompt ran in one turn");

    let messages = requests[0].body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>();
    let skill = messages
        .iter()
        .position(|content| content.contains("<skill name=\"review\""))
        .expect("skill message");
    let prompt = messages
        .iter()
        .position(|content| *content == "SUMMARIZE-THE-CHANGE")
        .expect("prompt message");

    assert!(skill < prompt, "{messages:?}");
    assert!(
        messages[skill].contains("SKILL-BODY-SENTINEL"),
        "{messages:?}"
    );

    let journal = session_journal(&fixture);
    let skill = journal.find("SKILL-BODY-SENTINEL").expect("skill journal");
    let prompt = journal
        .find("SUMMARIZE-THE-CHANGE")
        .expect("prompt journal");

    assert!(skill < prompt, "{journal}");

    let (success, _screen) = session.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_skill_command_lists_the_catalog() {
    let server = FakeServer::start(Vec::new()).await;
    let fixture = TuiFixture::new(&server);

    fixture.write_user_skill("review", "Review a change.", "Check the change.");

    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    session.send(b"/skill\r");

    assert!(
        session.wait_for("review — Review a change. — user", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "{screen:?}");
    assert!(server.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_activation_loads_the_instructions_before_the_next_request() {
    let server = FakeServer::start(vec![Reply::body(text_stream("after skill"))]).await;
    let fixture = TuiFixture::new(&server);

    fixture.write_user_skill(
        "review",
        "Review a change.",
        "SKILL-BODY-SENTINEL: check the change.",
    );

    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    wait_for_skills(&mut session).await;
    session.send(b"/skill review\r");

    assert!(
        session.wait_for("Skill review", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    session.send(b"do it\r");

    assert!(
        session.wait_for("after skill", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let requests = server.requests();
    let messages = requests[0].body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>();

    let skill = messages
        .iter()
        .position(|content| content.contains("<skill name=\"review\""))
        .expect("skill message");
    let prompt = messages
        .iter()
        .position(|content| *content == "do it")
        .expect("prompt message");

    assert!(skill < prompt, "{messages:?}");
    assert!(
        messages[skill].contains("SKILL-BODY-SENTINEL"),
        "{messages:?}"
    );

    let id = single_session_id(&fixture);
    let (success, _screen) = session.quit();

    assert!(success);

    let mut command = fixture.command();

    command.arg("--resume");
    command.arg(&id);

    let mut resumed = Session::start_command(command);

    assert!(
        resumed.wait_for("Skill review", Duration::from_secs(20)),
        "the restored skill card was not rendered: {:?}",
        resumed.screen
    );
    assert!(
        resumed.wait_for("do it", Duration::from_secs(20)),
        "{:?}",
        resumed.screen
    );

    let (success, _screen) = resumed.quit();

    assert!(success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_skill_writes_a_transcript_line() {
    let server = FakeServer::start(Vec::new()).await;
    let fixture = TuiFixture::new(&server);

    fixture.write_user_skill("review", "Review a change.", "Check the change.");

    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    wait_for_skills(&mut session).await;
    session.send(b"/skill missing\r");

    assert!(
        session.wait_for("unknown skill missing", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );
    assert!(!session.text().contains("Skill missing"));

    let (success, _screen) = session.quit();

    assert!(success);
    assert!(server.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skill_is_refused_while_a_turn_is_running() {
    let gate = Arc::new(Notify::new());
    let server = FakeServer::start(vec![
        Reply::body(text_stream("after gate")).with_gate(Arc::clone(&gate)),
    ])
    .await;
    let fixture = TuiFixture::new(&server);

    fixture.write_user_skill("review", "Review a change.", "Check the change.");

    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));
    wait_for_skills(&mut session).await;
    session.send(b"hello\r");

    let deadline = Instant::now() + Duration::from_secs(20);

    while server.requests().is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(
        !server.requests().is_empty(),
        "the turn never reached the provider"
    );

    session.send(b"/skill review\r");

    assert!(
        session.wait_for(
            "finish the current turn before loading a skill",
            Duration::from_secs(20)
        ),
        "{:?}",
        session.screen
    );

    gate.notify_one();

    assert!(
        session.wait_for("after gate", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    let (success, _screen) = session.quit();

    assert!(success);
}

/// Ctrl-C during a turn must actually stop it, not just say it will.
///
/// Regression: the status line reported "cancelling…" while the provider request
/// stayed in flight, so a turn that could not be stopped appeared to be stopping.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_stops_an_in_flight_turn_and_says_so() {
    let gate = Arc::new(Notify::new());
    let server = FakeServer::start(vec![
        Reply::body(text_stream("after the gate")).with_gate(Arc::clone(&gate)),
    ])
    .await;
    let fixture = TuiFixture::new(&server);

    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the TUI never drew its prompt"
    );

    session.send(b"say something\r");

    let deadline = Instant::now() + Duration::from_secs(20);

    while server.requests().is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(
        !server.requests().is_empty(),
        "the turn never reached the provider"
    );

    // The reply is still gated, so the turn is genuinely in flight.
    session.send(b"\x03");

    assert!(
        session.wait_for(
            "turn cancelled while waiting for the model",
            Duration::from_secs(10)
        ),
        "the turn did not reach a cancelled state: {:?}",
        session.screen
    );

    let (success, screen) = session.quit();

    assert!(success, "the interface did not exit cleanly: {screen:?}");
}

/// Cancel while the parent is blocked in `agent_wait` must stop the children and
/// end the turn.
///
/// This is the case the interface reported as "cancelling…" without cancelling:
/// `agent_wait` blocks inside a tool call, so nothing observes the token until
/// that tool returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_stops_children_a_parent_is_waiting_on() {
    let child_gate = Arc::new(Notify::new());
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-spawn",
            "spawn_agents",
            &serde_json::json!({ "agents": [{ "prompt": "slow work" }] }),
        ))
        .for_parent(),
        // Held open so the child stays running while the parent waits on it.
        Reply::body(text_stream("child finished"))
            .for_child_agent()
            .with_gate(Arc::clone(&child_gate)),
        Reply::body(tool_stream(
            "call-wait",
            "agent_wait",
            &serde_json::json!({}),
        ))
        .for_parent(),
        Reply::body(text_stream("parent finished")).for_parent(),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"delegate and wait\r");

    // The agent card is what persists; the tool result scrolls away once more
    // output arrives, so the card is the stable signal that the child started.
    assert!(
        session.wait_for("Agent a1", Duration::from_secs(20)),
        "the child never started: {:?}",
        session.screen
    );

    // The parent is now blocked in agent_wait.
    let deadline = Instant::now() + Duration::from_secs(20);

    while Instant::now() < deadline {
        if session.visible.contains("agent_wait")
            && t8ful_tui::spinner::VERBS
                .iter()
                .any(|verb| session.visible.contains(verb))
        {
            break;
        }

        if let Ok(chunk) = session.receiver.recv_timeout(Duration::from_millis(50)) {
            session.feed(&chunk);
        }
    }

    session.send(b"\x03");

    assert!(
        session.wait_for("cancelled while running", Duration::from_secs(15)),
        "the turn blocked in agent_wait did not cancel: {:?}",
        session.screen
    );

    // The cancelled child still reports. That notice must NOT start a new turn:
    // a cancel that quietly resumes work is the bug this test exists for.
    let requests_after_cancel = server.requests().len();

    child_gate.notify_one();
    tokio::time::sleep(Duration::from_secs(3)).await;

    assert!(
        server.requests().len() == requests_after_cancel,
        "a cancelled turn started a new request (was {requests_after_cancel}, now {}): the cancel \
         did not hold",
        server.requests().len()
    );

    assert!(
        !session.visible.contains("parent finished"),
        "the cancelled turn resumed and produced a new answer: {:?}",
        session.screen
    );

    // The child's task records the cancellation rather than looking pending.
    wait_for_journal(&fixture, "\"state\":\"cancelled\"").await;

    let (success, screen) = session.quit();

    assert!(success, "the interface did not exit cleanly: {screen:?}");
}

/// Ask for the skill report until discovery has published it.
async fn wait_for_skills(session: &mut Session) {
    for _ in 0..40 {
        session.send(b"/skill\r");

        if session.wait_for("Review a change.", Duration::from_millis(500)) {
            return;
        }
    }

    panic!("skills never loaded: {:?}", session.screen);
}

/// Wait until the fake provider has answered at least `count` catalog requests.
async fn wait_for_models_requests(server: &FakeServer, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);

    while server.models_requests() < count {
        assert!(
            Instant::now() < deadline,
            "the provider saw {} catalog requests, wanted {count}",
            server.models_requests()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until the fake provider has answered at least `count` chat requests.
async fn wait_for_requests(server: &FakeServer, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);

    while server.requests().len() < count {
        assert!(
            Instant::now() < deadline,
            "the provider saw {} requests, wanted {count}",
            server.requests().len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_parent_is_woken_with_exactly_one_notice() {
    let child_gate = Arc::new(Notify::new());
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-spawn",
            "spawn_agents",
            &serde_json::json!({ "agents": [{ "prompt": "do the work" }] }),
        ))
        .for_parent(),
        Reply::body(text_stream("child report"))
            .for_child_agent()
            .with_gate(Arc::clone(&child_gate)),
        Reply::body(text_stream("parent started")).for_parent(),
        Reply::body(text_stream("parent saw the report")).for_parent(),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"delegate\r");

    assert!(
        session.wait_for("parent started", Duration::from_secs(20)),
        "the parent turn never finished: {:?}",
        session.screen
    );

    // The parent is idle now and the child has not reported yet.
    child_gate.notify_one();

    assert!(
        session.wait_for("parent saw the report", Duration::from_secs(20)),
        "the idle parent was never woken by the report: {:?}",
        session.screen
    );

    // One injection, one journal record: the report is not replayed.
    wait_for_journal_count(&fixture, "<agent_notice", 1).await;

    assert_eq!(
        session_journal(&fixture).matches("<agent_notice").count(),
        1,
        "the report was injected more than once"
    );

    let (success, screen) = session.quit();

    assert!(success, "the interface did not exit cleanly: {screen:?}");
}

/// An idle wake turn must be cancellable like any other turn.
///
/// Regression: the wake ran on a private channel nothing could write to, so
/// Ctrl-C, Esc, and quit went unread while an unattended request stayed in
/// flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wake_turn_can_be_cancelled_and_the_interface_stays_responsive() {
    let child_gate = Arc::new(Notify::new());
    let wake_gate = Arc::new(Notify::new());
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-spawn",
            "spawn_agents",
            &serde_json::json!({ "agents": [{ "prompt": "do the work" }] }),
        ))
        .for_parent(),
        Reply::body(text_stream("child report"))
            .for_child_agent()
            .with_gate(Arc::clone(&child_gate)),
        Reply::body(text_stream("parent started")).for_parent(),
        // The wake turn's request, held open so it is genuinely in flight.
        Reply::body(text_stream("wake reply"))
            .for_parent()
            .with_gate(Arc::clone(&wake_gate)),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"delegate\r");

    assert!(
        session.wait_for("parent started", Duration::from_secs(20)),
        "the parent turn never finished: {:?}",
        session.screen
    );

    // Wait for the first turn's boundary before the report can wake a new one.
    // Without this the spinner still drawn for the first turn satisfies the
    // busy wait below, and the Ctrl-C then races that turn's last event: while
    // the interface looks idle Ctrl-C means quit, not cancel.
    assert!(
        session.wait_for_idle(Duration::from_secs(20)),
        "the first turn never reached its boundary: {:?}",
        session.screen
    );

    // The parent is idle; the child's report wakes it with a new turn, whose
    // request is the fourth one and is held open by the server.
    child_gate.notify_one();
    wait_for_requests(&server, 4).await;

    // A cancel only means "stop this turn" once the wake turn is visibly
    // running; while the interface still looks idle Ctrl-C means quit.
    assert!(
        session.wait_for_busy(Duration::from_secs(10)),
        "the wake turn never became visible: {:?}",
        session.screen
    );

    session.send(b"\x03");

    assert!(
        session.wait_for(
            "turn cancelled while waiting for the model",
            Duration::from_secs(10)
        ),
        "the wake turn could not be cancelled: {:?}",
        session.screen
    );

    // A cancelled wake must leave the interface reading input.
    session.send(b"RESPONSIVE-Z");

    assert!(
        session.wait_for_visible("RESPONSIVE-Z", Duration::from_secs(10)),
        "the interface stopped reading input after cancelling the wake: {:?}",
        session.screen
    );

    // Idle again, so Ctrl-C quits rather than cancelling.
    session.send(b"\x03");

    let status = session.wait_for_exit(Duration::from_secs(10));

    assert!(
        status.is_some_and(|status| status.success()),
        "the interface did not exit cleanly after a cancelled wake: {:?}",
        session.screen
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_on_finish_false_delivers_the_report_with_the_next_prompt() {
    let child_gate = Arc::new(Notify::new());
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-spawn",
            "spawn_agents",
            &serde_json::json!({ "agents": [{ "prompt": "do the work" }] }),
        ))
        .for_parent(),
        Reply::body(text_stream("child report"))
            .for_child_agent()
            .with_gate(Arc::clone(&child_gate)),
        Reply::body(text_stream("parent started")).for_parent(),
        Reply::body(text_stream("parent answered")).for_parent(),
    ])
    .await;
    let fixture = TuiFixture::with_config(
        &server,
        "[provider]\nmodel = \"vendor/test-model\"\n\n[agents]\nwake_on_finish = false\n",
    );
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"delegate\r");

    assert!(
        session.wait_for("parent started", Duration::from_secs(20)),
        "the parent turn never finished: {:?}",
        session.screen
    );

    child_gate.notify_one();
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        server.requests().len(),
        3,
        "a disabled wake must not spend a request: {:?}",
        server.requests()
    );

    session.send(b"continue\r");

    assert!(
        session.wait_for("parent answered", Duration::from_secs(20)),
        "the next prompt never ran: {:?}",
        session.screen
    );

    let sent = server.requests();
    let last = sent.last().expect("the prompt's request");
    let context = last.body["messages"].to_string();

    assert!(
        context.contains("child report"),
        "the pending report was delivered with the prompt: {context}"
    );
    assert!(
        context.contains("continue"),
        "the user's prompt is in the same request: {context}"
    );

    let (success, screen) = session.quit();

    assert!(success, "the interface did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_wait_consumes_the_notice_so_it_is_not_delivered_twice() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-spawn",
            "spawn_agents",
            &serde_json::json!({ "agents": [{ "prompt": "do the work" }] }),
        ))
        .for_parent(),
        Reply::body(text_stream("child report")).for_child_agent(),
        Reply::body(tool_stream(
            "call-wait",
            "agent_wait",
            &serde_json::json!({}),
        ))
        .for_parent(),
        Reply::body(text_stream("parent finished")).for_parent(),
    ])
    .await;
    let fixture = TuiFixture::new(&server);
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"delegate and wait\r");

    assert!(
        session.wait_for("parent finished", Duration::from_secs(20)),
        "the parent never finished: {:?}",
        session.screen
    );

    let requests = server.requests().len();

    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        server.requests().len(),
        requests,
        "agent_wait read the report, so no notice turn should run"
    );

    let sent = server.requests();
    let last = sent.last().expect("the final request");
    let context = last.body["messages"].to_string();

    assert_eq!(
        context.matches("child report").count(),
        1,
        "the report appears exactly once: {context}"
    );

    let (success, screen) = session.quit();

    assert!(success, "the interface did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_spawn_budget_resets_at_the_next_turn() {
    let server = FakeServer::start(vec![
        Reply::body(tool_stream(
            "call-first",
            "spawn_agents",
            &serde_json::json!({ "agents": [{ "prompt": "first child" }] }),
        ))
        .for_parent(),
        Reply::body(text_stream("first child done")).for_child_agent(),
        Reply::body(tool_stream(
            "call-second",
            "spawn_agents",
            &serde_json::json!({ "agents": [{ "prompt": "second child" }] }),
        ))
        .for_parent(),
        Reply::body(text_stream("first turn")).for_parent(),
        Reply::body(tool_stream(
            "call-third",
            "spawn_agents",
            &serde_json::json!({ "agents": [{ "prompt": "third child" }] }),
        ))
        .for_parent(),
        Reply::body(text_stream("second turn")).for_parent(),
    ])
    .await;
    let fixture = TuiFixture::with_config(
        &server,
        "[provider]\nmodel = \"vendor/test-model\"\n\n[agents]\nmax_spawns_per_turn = 1\n\
         wake_on_finish = false\n",
    );
    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(b"first\r");

    assert!(
        session.wait_for("first turn", Duration::from_secs(20)),
        "the first turn never finished: {:?}",
        session.screen
    );
    assert!(
        session.wait_for_idle(Duration::from_secs(20)),
        "the first turn never reached its boundary: {:?}",
        session.screen
    );

    wait_for_requests(&server, 4).await;
    // The collapsed tool card does not show the refusal text, but the journal
    // keeps the tool result.
    wait_for_journal(&fixture, "budget resets at the next turn").await;

    session.send(b"second\r");

    assert!(
        session.wait_for("second turn", Duration::from_secs(20)),
        "the second turn never finished: {:?}",
        session.screen
    );

    wait_for_journal(&fixture, "a2 started").await;
    // The second prompt must have begun a new turn, not continued the first:
    // each turn's first request is step 1.
    wait_for_journal_count(&fixture, "\"step\":1", 2).await;

    let (success, screen) = session.quit();

    assert!(success, "the interface did not exit cleanly: {screen:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attachment_moves_from_the_tray_into_the_submitted_transcript() {
    let server = FakeServer::start(vec![Reply::body(text_stream("described the image"))]).await;
    let fixture = TuiFixture::new(&server);
    let image_path = fixture.project.path().join("one.png");
    let image = image::RgbImage::from_pixel(16, 16, image::Rgb([20, 80, 160]));

    image.save(&image_path).expect("write image");

    let mut session = Session::start(&fixture);

    assert!(session.wait_for_ready(Duration::from_secs(20)));

    session.send(format!("/attach {}\r", image_path.display()).as_bytes());

    assert!(
        session.wait_for("attach: 1 PNG 16x16", Duration::from_secs(20)),
        "the pending attachment tray never appeared: {:?}",
        session.screen
    );

    session.send(b"describe it\r");

    assert!(
        session.wait_for("described the image", Duration::from_secs(20)),
        "{:?}",
        session.screen
    );

    // The enlarged preview occupies the viewport, so scroll up to the image
    // caption instead of assuming it is visible below the answer.
    session.send(b"\x1b[5~");
    session.send(b"\x1b[5~");
    tokio::time::sleep(Duration::from_millis(100)).await;
    session.drain();

    let screen = session.text();

    assert!(
        screen.contains("[1] PNG · 16x16"),
        "the submitted transcript lost the image entry: {screen:?}"
    );
    assert!(
        !screen.contains("attach: 1 PNG"),
        "a submitted attachment stayed in the tray: {screen:?}"
    );

    let requests = server.requests();
    let user = &requests[0].body["messages"][1]["content"];

    assert_eq!(user[0]["type"], serde_json::json!("text"));
    assert_eq!(user[1]["type"], serde_json::json!("image_url"));

    let (success, _screen) = session.quit();

    assert!(success);
}

/// The project configuration a trust prompt is triggered by.
///
/// The endpoint is deliberately bogus: a trusted run must use it and therefore
/// fail to reach the provider, which is how the test proves the project layer
/// was applied rather than silently ignored.
fn untrusted_project_config() -> &'static str {
    "[provider]\nmodel = \"vendor/test-model\"\nbase_url = \"http://127.0.0.1:1\"\n"
}

fn trust_path(fixture: &TuiFixture) -> std::path::PathBuf {
    fixture.home.path().join("state/t8ful/trust.json")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_trust_prompt_lists_what_the_project_wants() {
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::with_config(&server, untrusted_project_config());
    let mut session = Session::start(&fixture);

    // The last option row is the final thing drawn, so waiting for it means
    // every earlier row is on screen too.
    assert!(
        session.wait_for("Do not start the session", Duration::from_secs(20)),
        "the trust prompt never appeared: {:?}",
        session.screen
    );

    for needle in [
        // The panel renders the question body, so the heading must be in it.
        "Trust this project?",
        "wants to:",
        "provider.base_url",
        "Trust this project",
        "Continue without project configuration",
        "Do not start the session",
    ] {
        assert!(
            session.screen.contains(needle),
            "the prompt is missing {needle:?}: {:?}",
            session.screen
        );
    }

    session.send(b"3");

    let status = session.wait_for_exit(Duration::from_secs(10));

    assert!(
        status.is_some_and(|status| status.success()),
        "choosing exit did not quit cleanly: {:?}",
        session.screen
    );
    assert!(
        !trust_path(&fixture).exists(),
        "exiting must not record a decision"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ordinary_project_never_prompts() {
    // Only authority-bearing keys justify a decision. A repository that merely
    // configures ordinary settings must open silently, or the prompt becomes
    // noise the user learns to dismiss.
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::with_config(
        &server,
        "[provider]\nmodel = \"vendor/test-model\"\n\
         [shell]\nmax_output_bytes = 4096\n\
         [agent]\nmax_steps = 4\n",
    );
    let mut session = Session::start(&fixture);

    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the session did not start: {:?}",
        session.screen
    );
    assert!(
        !session.screen.contains("wants to:"),
        "an ordinary project prompted: {:?}",
        session.screen
    );

    let (clean, _screen) = session.quit();

    assert!(clean, "the session did not quit cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_prompt_does_not_report_stripping_before_the_answer() {
    // The "ignoring …" warnings describe the *restricted* outcome. Showing them
    // while the question is still open would be premature, and false if the
    // user then trusts the project.
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::with_config(&server, untrusted_project_config());
    let mut session = Session::start(&fixture);

    assert!(session.wait_for("Do not start the session", Duration::from_secs(20)));
    assert!(
        !session.screen.contains("ignoring"),
        "warnings appeared before the decision: {:?}",
        session.screen
    );

    session.send(b"1");

    assert!(
        wait_for_trust_file(&fixture),
        "the decision was not recorded"
    );

    let (clean, _screen) = session.quit();

    assert!(clean, "the session did not quit cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continuing_without_project_configuration_starts_restricted() {
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::with_config(&server, untrusted_project_config());
    let mut session = Session::start(&fixture);

    assert!(session.wait_for("Do not start the session", Duration::from_secs(20)));
    session.send(b"2");

    // The session starts: the composer is ready and the bogus project endpoint
    // was not adopted, because this command's scripted server still answers.
    assert!(
        session.wait_for_gone("Do not start the session", Duration::from_secs(20)),
        "the trust prompt never closed: {:?}",
        session.screen
    );
    assert!(
        session.wait_for_ready(Duration::from_secs(20)),
        "the session did not start after continuing: {:?}",
        session.screen
    );
    assert!(
        !trust_path(&fixture).exists(),
        "continuing must not record a decision"
    );

    let (clean, _screen) = session.quit();

    assert!(clean, "the session did not quit cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dismissing_the_trust_prompt_continues_restricted() {
    // Esc is the safe default: it must behave like "continue without project
    // configuration", never like "trust".
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::with_config(&server, untrusted_project_config());
    let mut session = Session::start(&fixture);

    assert!(session.wait_for("Do not start the session", Duration::from_secs(20)));
    session.send(b"\x1b");

    assert!(
        session.wait_for_gone("Do not start the session", Duration::from_secs(20)),
        "Esc did not close the prompt: {:?}",
        session.screen
    );
    assert!(
        !trust_path(&fixture).exists(),
        "Esc must not record a decision"
    );

    let (clean, _screen) = session.quit();

    assert!(clean, "the session did not quit cleanly after Esc");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trusting_a_project_records_the_decision_and_stops_asking() {
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::with_config(&server, untrusted_project_config());
    let mut session = Session::start(&fixture);

    assert!(session.wait_for("Do not start the session", Duration::from_secs(20)));
    session.send(b"1");

    let status = wait_for_trust_file(&fixture);

    assert!(
        status,
        "the decision was not recorded: {:?}",
        session.screen
    );

    let (clean, _screen) = session.quit();

    assert!(clean, "the session did not quit cleanly");

    // A second run must not ask again.
    let mut second = Session::start(&fixture);

    assert!(
        !second.wait_for_visible("Do not start the session", Duration::from_secs(5)),
        "a recorded decision still prompted: {:?}",
        second.screen
    );

    let (clean, _screen) = second.quit();

    assert!(clean, "the second session did not quit cleanly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_project_configuration_prompts_again() {
    let server = FakeServer::start(vec![Reply::body(text_stream("unused"))]).await;
    let fixture = TuiFixture::with_config(&server, untrusted_project_config());
    let mut first = Session::start(&fixture);

    assert!(first.wait_for("Do not start the session", Duration::from_secs(20)));
    first.send(b"1");
    assert!(
        wait_for_trust_file(&fixture),
        "the decision was not recorded"
    );

    let (clean, _screen) = first.quit();

    assert!(clean, "the first session did not quit cleanly");

    // A later edit means the decision was made about different content. The
    // project now asks for one more thing than it did when it was trusted.
    std::fs::write(
        fixture.project.path().join(".t8ful/config.toml"),
        format!(
            "{}[mcp.servers.added]\ncommand = [\"true\"]\n",
            untrusted_project_config()
        ),
    )
    .expect("rewrite project config");

    let mut second = Session::start(&fixture);

    assert!(
        second.wait_for("has changed since you trusted it", Duration::from_secs(20)),
        "a changed configuration did not ask again: {:?}",
        second.screen
    );
    // Wait for the last option row so the prompt is fully drawn and listening.
    assert!(
        second.wait_for("Do not start the session", Duration::from_secs(20)),
        "the re-prompt never finished drawing: {:?}",
        second.screen
    );
    assert!(
        second.screen.contains("mcp.servers.added"),
        "the prompt did not name the new key: {:?}",
        second.screen
    );

    second.send(b"3");

    let status = second.wait_for_exit(Duration::from_secs(10));

    assert!(
        status.is_some_and(|status| status.success()),
        "exiting after the re-prompt did not quit cleanly: {:?}",
        second.screen
    );
}

/// Wait for the trust file to appear, since the prompt records it asynchronously.
fn wait_for_trust_file(fixture: &TuiFixture) -> bool {
    let path = trust_path(fixture);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);

    while std::time::Instant::now() < deadline {
        if path.exists() {
            return true;
        }

        std::thread::sleep(Duration::from_millis(25));
    }

    false
}
