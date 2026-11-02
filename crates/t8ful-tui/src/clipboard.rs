//! Clipboard input and text copy.
//!
//! Image input runs in the driver's blocking task; the TUI never reads the
//! clipboard itself. Text helpers run asynchronously; only the UI writes the
//! terminal escape fallback. Wayland is preferred when available, then X11
//! through `xclip`, then OSC 52.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

/// Why an image paste produced nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardError {
    /// The clipboard is readable and simply holds no image. Silence is the
    /// right response: a person pasting text did not ask for it.
    NoImage,
    /// No backend exists, or a backend/helper failed. This is worth a warning.
    Unavailable(String),
}

impl ClipboardError {
    pub fn is_silent(&self) -> bool {
        matches!(self, Self::NoImage)
    }
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoImage => f.write_str("the clipboard contains no PNG, JPEG, WebP, or GIF image"),
            Self::Unavailable(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ClipboardError {}

pub trait ClipboardImageSource: Send + Sync {
    fn read_image(&self) -> Result<ClipboardImage, ClipboardError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClipboard;

impl ClipboardImageSource for SystemClipboard {
    fn read_image(&self) -> Result<ClipboardImage, ClipboardError> {
        read_image()
    }
}

pub fn read_image() -> Result<ClipboardImage, ClipboardError> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE")
            .is_ok_and(|value| value.eq_ignore_ascii_case("wayland"));
    let x11 = std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty());
    let search_path = std::env::var_os("PATH");

    read_image_with(wayland, x11, search_path.as_deref())
}

fn read_image_with(
    wayland: bool,
    x11: bool,
    search_path: Option<&std::ffi::OsStr>,
) -> Result<ClipboardImage, ClipboardError> {
    let mut failures = Vec::new();
    let mut no_image = false;

    if wayland {
        match read_wayland(search_path) {
            Ok(image) => return Ok(image),
            Err(ClipboardError::NoImage) => no_image = true,
            Err(ClipboardError::Unavailable(error)) => failures.push(format!("wl-paste: {error}")),
        }
    }

    if x11 {
        match read_x11(search_path) {
            Ok(image) => return Ok(image),
            Err(ClipboardError::NoImage) => no_image = true,
            Err(ClipboardError::Unavailable(error)) => failures.push(format!("xclip: {error}")),
        }
    }

    if !failures.is_empty() {
        Err(ClipboardError::Unavailable(format!(
            "clipboard image input is unavailable: {}",
            failures.join("; ")
        )))
    } else if no_image {
        Err(ClipboardError::NoImage)
    } else {
        Err(ClipboardError::Unavailable(
            "no Wayland or X11 clipboard is available; run t8ful in a graphical session or use \
             /attach <path>"
                .to_owned(),
        ))
    }
}

fn read_wayland(search_path: Option<&std::ffi::OsStr>) -> Result<ClipboardImage, ClipboardError> {
    let types =
        run("wl-paste", &["--list-types"], search_path).map_err(ClipboardError::Unavailable)?;
    let media_type =
        choose_image_type(&String::from_utf8_lossy(&types)).ok_or(ClipboardError::NoImage)?;
    let bytes = run(
        "wl-paste",
        &["--no-newline", "--type", &media_type],
        search_path,
    )
    .map_err(ClipboardError::Unavailable)?;

    if bytes.is_empty() {
        return Err(ClipboardError::NoImage);
    }

    Ok(ClipboardImage { bytes, media_type })
}

fn read_x11(search_path: Option<&std::ffi::OsStr>) -> Result<ClipboardImage, ClipboardError> {
    let types = run(
        "xclip",
        &["-selection", "clipboard", "-t", "TARGETS", "-o"],
        search_path,
    )
    .map_err(ClipboardError::Unavailable)?;
    let media_type =
        choose_image_type(&String::from_utf8_lossy(&types)).ok_or(ClipboardError::NoImage)?;
    let bytes = run(
        "xclip",
        &["-selection", "clipboard", "-t", &media_type, "-o"],
        search_path,
    )
    .map_err(ClipboardError::Unavailable)?;

    if bytes.is_empty() {
        return Err(ClipboardError::NoImage);
    }

    Ok(ClipboardImage { bytes, media_type })
}

/// How a text selection reached the clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMethod {
    Wayland,
    X11,
    Osc52,
}

impl CopyMethod {
    pub fn label(self) -> &'static str {
        match self {
            Self::Wayland => "wl-copy",
            Self::X11 => "xclip",
            Self::Osc52 => "OSC 52",
        }
    }
}

impl std::fmt::Display for CopyMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Text copy never writes to the terminal from a worker. A fallback result
/// asks the UI loop to emit OSC 52 alongside its normal terminal output.
#[async_trait::async_trait]
pub trait TextClipboard: Send + Sync {
    async fn copy(&self, text: &str) -> Result<TextCopyOutcome, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextCopyOutcome {
    Copied(CopyMethod),
    TerminalFallback { failures: Vec<String> },
}

/// Session detection is separate from execution so tests need neither global
/// environment mutation nor access to the developer's real clipboard.
#[derive(Debug, Clone)]
pub struct ClipboardBackend {
    search_path: Option<std::ffi::OsString>,
    wayland: bool,
    x11: bool,
    remote: bool,
}

const HELPER_DEADLINE: Duration = Duration::from_secs(2);
const CLEANUP_DEADLINE: Duration = Duration::from_millis(250);
const DIAGNOSTIC_LIMIT: usize = 8 * 1024;

impl ClipboardBackend {
    pub fn detect() -> Self {
        Self {
            search_path: std::env::var_os("PATH"),
            wayland: std::env::var_os("WAYLAND_DISPLAY").is_some()
                || std::env::var("XDG_SESSION_TYPE")
                    .is_ok_and(|value| value.eq_ignore_ascii_case("wayland")),
            x11: std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty()),
            remote: std::env::var_os("SSH_CONNECTION").is_some()
                || std::env::var_os("MOSH_CONNECTION").is_some(),
        }
    }

    async fn copy_native(&self, text: &str, deadline: Duration) -> TextCopyOutcome {
        if self.remote {
            // A helper over SSH/Mosh would copy to the wrong machine.
            return TextCopyOutcome::TerminalFallback { failures: vec![] };
        }

        let mut failures = Vec::new();

        for (enabled, program, args, method) in [
            (
                self.wayland,
                "wl-copy",
                &["--type", "text/plain;charset=utf-8"][..],
                CopyMethod::Wayland,
            ),
            (
                self.x11,
                "xclip",
                &["-selection", "clipboard"][..],
                CopyMethod::X11,
            ),
        ] {
            if !enabled {
                continue;
            }

            let result = match find_in_search_path(self.search_path.as_deref(), program) {
                Some(path) => write_helper(&path, args, text, deadline).await,
                None => Err("is not installed or not on PATH".to_owned()),
            };

            match result {
                Ok(()) => return TextCopyOutcome::Copied(method),
                Err(error) => failures.push(format!("{program}: {error}")),
            }
        }

        TextCopyOutcome::TerminalFallback { failures }
    }
}

#[async_trait::async_trait]
impl TextClipboard for ClipboardBackend {
    async fn copy(&self, text: &str) -> Result<TextCopyOutcome, String> {
        Ok(self.copy_native(text, HELPER_DEADLINE).await)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemTextClipboard;

#[async_trait::async_trait]
impl TextClipboard for SystemTextClipboard {
    async fn copy(&self, text: &str) -> Result<TextCopyOutcome, String> {
        ClipboardBackend::detect().copy(text).await
    }
}

/// A failed or cancelled invocation owns its whole process group. Success
/// disarms cleanup: the daemonized clipboard owner must outlive the TUI.
struct Helper {
    child: tokio::process::Child,
    pid: u32,
    armed: bool,
}

impl Helper {
    fn kill(&mut self) {
        if !self.armed {
            return;
        }

        #[cfg(target_os = "linux")]
        {
            use nix::sys::signal::{Signal, killpg};
            use nix::unistd::Pid;

            let _ = killpg(Pid::from_raw(self.pid as i32), Signal::SIGKILL);
        }

        let _ = self.child.start_kill();
        self.armed = false;
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        // Cancellation may race with successful completion. Preserve an owner
        // when the direct child has already reported success. Tokio reaps a
        // killed direct child on drop, including on error/terminal teardown.
        if self.armed
            && self
                .child
                .try_wait()
                .is_ok_and(|status| status.is_some_and(|status| status.success()))
        {
            self.armed = false;
        }

        self.kill();
    }
}

async fn write_helper(
    path: &Path,
    args: &[&str],
    text: &str,
    deadline: Duration,
) -> Result<(), String> {
    let mut command = tokio::process::Command::new(path);

    command
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    #[cfg(target_os = "linux")]
    command.process_group(0);

    let child = command.spawn().map_err(|error| error.to_string())?;
    let pid = child
        .id()
        .ok_or_else(|| "helper has no process id".to_owned())?;
    let mut helper = Helper {
        child,
        pid,
        armed: true,
    };
    let mut stdin = helper
        .child
        .stdin
        .take()
        .ok_or_else(|| "helper has no stdin".to_owned())?;
    let mut stderr = helper
        .child
        .stderr
        .take()
        .ok_or_else(|| "helper has no stderr".to_owned())?;
    let mut diagnostics = Vec::new();

    let operation = async {
        let completion = async {
            let written = stdin.write_all(text.as_bytes()).await;

            // ChildStdin::shutdown is a no-op for pipes. Drop the handle so a
            // helper waiting for EOF actually sees it before we wait for exit.
            drop(stdin);

            let status = helper
                .child
                .wait()
                .await
                .map_err(|error| error.to_string())?;

            if !status.success() {
                return Err(format!("exited with {status}"));
            }

            written.map_err(|error| format!("cannot write selection: {error}"))
        };

        tokio::pin!(completion);

        loop {
            let mut chunk = [0_u8; 1024];

            tokio::select! {
                result = &mut completion => break result,
                read = stderr.read(&mut chunk) => match read {
                    Ok(0) | Err(_) => break completion.await,
                    Ok(read) => append_diagnostics(&mut diagnostics, &chunk[..read]),
                },
            }
        }
    };

    let result = match tokio::time::timeout(deadline, operation).await {
        Ok(result) => result,
        Err(_) => Err(format!("timed out after {deadline:?}")),
    };

    if result.is_ok() {
        helper.armed = false;
        return Ok(());
    }

    helper.kill();
    let _ = tokio::time::timeout(CLEANUP_DEADLINE, helper.child.wait()).await;

    // Read already-buffered diagnostics, but never wait for the background
    // owner's EOF. Also bound this drain if a descendant writes continuously.
    let _ = tokio::time::timeout(Duration::from_millis(20), async {
        let mut chunk = [0_u8; 1024];

        while let Ok(read) = stderr.read(&mut chunk).await {
            if read == 0 {
                break;
            }

            append_diagnostics(&mut diagnostics, &chunk[..read]);
        }
    })
    .await;

    let error = result.expect_err("unsuccessful helper");
    let detail = String::from_utf8_lossy(&diagnostics);
    let detail = detail.trim();

    Err(if detail.is_empty() {
        error
    } else {
        format!("{error}: {detail}")
    })
}

fn append_diagnostics(diagnostics: &mut Vec<u8>, chunk: &[u8]) {
    diagnostics.extend_from_slice(chunk);

    if diagnostics.len() > DIAGNOSTIC_LIMIT {
        diagnostics.drain(..diagnostics.len() - DIAGNOSTIC_LIMIT);
    }
}

fn find_in_search_path(search_path: Option<&std::ffi::OsStr>, program: &str) -> Option<PathBuf> {
    std::env::split_paths(search_path?)
        .map(|directory| directory.join(program))
        .find(|candidate| is_executable(candidate))
}

pub(crate) struct CopyCompletion {
    pub text: String,
    pub result: Result<TextCopyOutcome, String>,
}

/// One active operation plus a replaceable pending selection. Workers never
/// touch stdout; stale completions cannot emit terminal clipboard sequences.
pub(crate) struct CopyCoordinator {
    clipboard: std::sync::Arc<dyn TextClipboard>,
    active: Option<tokio::task::JoinHandle<()>>,
    pending: Option<(u64, String)>,
    generation: u64,
    tx: tokio::sync::mpsc::UnboundedSender<(u64, CopyCompletion)>,
    rx: tokio::sync::mpsc::UnboundedReceiver<(u64, CopyCompletion)>,
}

impl CopyCoordinator {
    pub fn new(clipboard: std::sync::Arc<dyn TextClipboard>) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            clipboard,
            active: None,
            pending: None,
            generation: 0,
            tx,
            rx,
        }
    }

    pub fn enqueue(&mut self, text: String) -> u64 {
        self.generation += 1;
        let request = (self.generation, text);
        if self.active.is_some() {
            self.pending = Some(request);
        } else {
            self.start(request);
        }
        self.generation
    }

    fn start(&mut self, (id, text): (u64, String)) {
        let clipboard = self.clipboard.clone();
        let tx = self.tx.clone();
        self.active = Some(tokio::spawn(async move {
            let result = clipboard.copy(&text).await;
            let _ = tx.send((id, CopyCompletion { text, result }));
        }));
    }

    /// Whether a copy is still running, so the loop keeps polling for it.
    pub fn busy(&self) -> bool {
        self.active.is_some()
    }

    pub fn poll(&mut self) -> Option<CopyCompletion> {
        let (id, completion) = self.rx.try_recv().ok()?;
        self.active.take();
        if let Some(request) = self.pending.take() {
            self.start(request);
        }
        (id == self.generation).then_some(completion)
    }

    pub async fn shutdown(&mut self) {
        self.pending = None;
        if let Some(active) = self.active.take() {
            active.abort();
            let _ = active.await;
        }
        while self.rx.try_recv().is_ok() {}
    }
}

impl Drop for CopyCoordinator {
    fn drop(&mut self) {
        if let Some(active) = self.active.take() {
            active.abort();
        }
    }
}

/// Write the selection as an OSC 52 terminal clipboard sequence.
pub(crate) fn write_osc52(text: &str) -> Result<CopyMethod, String> {
    let payload = base64(text.as_bytes());
    let mut stdout = std::io::stdout().lock();

    write!(stdout, "\x1b]52;c;{payload}\x07")
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("cannot write the terminal clipboard sequence: {error}"))?;

    Ok(CopyMethod::Osc52)
}

/// Standard base64 with padding; the OSC 52 payload has no line breaks.
pub fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);

    for chunk in data.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let combined = (first << 16) | (second << 8) | third;

        out.push(TABLE[(combined >> 18) as usize & 63] as char);
        out.push(TABLE[(combined >> 12) as usize & 63] as char);

        if chunk.len() > 1 {
            out.push(TABLE[(combined >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }

        if chunk.len() > 2 {
            out.push(TABLE[combined as usize & 63] as char);
        } else {
            out.push('=');
        }
    }

    out
}

pub fn choose_image_type(types: &str) -> Option<String> {
    for preferred in ["image/png", "image/jpeg", "image/webp", "image/gif"] {
        if types
            .lines()
            .any(|line| line.trim().eq_ignore_ascii_case(preferred))
        {
            return Some(preferred.to_owned());
        }
    }

    None
}

fn run(
    program: &str,
    args: &[&str],
    search_path: Option<&std::ffi::OsStr>,
) -> Result<Vec<u8>, String> {
    let path = find_in_path(program, search_path)
        .ok_or_else(|| format!("{program} is not installed or not on PATH"))?;
    let output = Command::new(path)
        .args(args)
        .output()
        .map_err(|error| error.to_string())?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();

        return Err(if stderr.is_empty() {
            format!("{program} exited with {}", output.status)
        } else {
            stderr
        });
    }

    Ok(output.stdout)
}

fn find_in_path(
    program: &str,
    search_path: Option<&std::ffi::OsStr>,
) -> Option<std::path::PathBuf> {
    let path = search_path?;

    for directory in std::env::split_paths(path) {
        let candidate = directory.join(program);

        if is_executable(&candidate) {
            return Some(candidate);
        }
    }

    None
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{ClipboardBackend, base64, choose_image_type};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// Two seconds is the production deadline; tests stay under it so a
    /// regression fails instead of stalling the suite.
    const TEST_DEADLINE: Duration = Duration::from_secs(2);

    /// Serializes helper creation with helper spawning.
    ///
    /// A child forked by a parallel test holds every inherited descriptor until
    /// it execs, including another test's script opened for writing, and exec
    /// then fails with `ETXTBSY`. Holding this lock across both steps removes
    /// the race without weakening what the tests exercise.
    static SPAWN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn spawn_lock() -> std::sync::MutexGuard<'static, ()> {
        SPAWN_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Install a shell script as an executable clipboard helper.
    fn helper_script(directory: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let _guard = spawn_lock();
        let path = directory.join(name);

        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("helper script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        path
    }

    /// A fake `wl-copy`/`xclip` that records what it was given and then exits.
    fn recording_backend(directory: &Path, body: &str) -> ClipboardBackend {
        helper_script(directory, "wl-copy", body);

        ClipboardBackend {
            search_path: Some(directory.as_os_str().to_owned()),
            wayland: true,
            x11: false,
            remote: false,
        }
    }

    /// Read an image while holding the spawn lock, so helper creation and the
    /// `exec` that follows it cannot overlap with another test's fork.
    fn read_image_locked(
        wayland: bool,
        x11: bool,
        search_path: Option<&std::ffi::OsStr>,
    ) -> Result<super::ClipboardImage, super::ClipboardError> {
        let _guard = spawn_lock();
        super::read_image_with(wayland, x11, search_path)
    }

    fn copy(backend: &ClipboardBackend, text: &str) -> super::TextCopyOutcome {
        let _guard = spawn_lock();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        runtime.block_on(backend.copy_native(text, TEST_DEADLINE))
    }

    #[test]
    fn a_helper_that_leaves_a_stderr_owner_behind_still_completes() {
        let directory = tempfile::tempdir().expect("helper dir");
        let payload = directory.path().join("payload");
        let backend = recording_backend(
            directory.path(),
            &format!("cat > {}\n( sleep 30 ) &\nexit 0", payload.display()),
        );

        let result = copy(&backend, "selected text");

        assert!(
            matches!(
                result,
                super::TextCopyOutcome::Copied(super::CopyMethod::Wayland)
            ),
            "the forked clipboard owner blocked completion: {result:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&payload).expect("payload"),
            "selected text"
        );
    }

    #[test]
    fn a_helper_that_never_reads_its_input_times_out() {
        let directory = tempfile::tempdir().expect("helper dir");
        let backend = recording_backend(directory.path(), "sleep 30");
        let payload = "x".repeat(512 * 1024);

        let started = std::time::Instant::now();
        let result = copy(&backend, &payload);

        assert!(
            matches!(result, super::TextCopyOutcome::TerminalFallback { ref failures } if failures.join(";").contains("timed out")),
            "a helper that never reads stdin was not bounded: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_failing_helper_reports_bounded_stderr() {
        let directory = tempfile::tempdir().expect("helper dir");
        let backend = recording_backend(directory.path(), "echo 'no display' >&2\nexit 3");

        let result = copy(&backend, "selected text");

        match result {
            super::TextCopyOutcome::TerminalFallback { failures } => {
                let error = failures.join(";");
                assert!(error.contains("no display"), "{error}");
                assert!(error.contains("wl-copy"), "{error}");
            }
            other => panic!("expected a reported failure, got {other:?}"),
        }
    }

    #[test]
    fn unicode_and_trailing_newlines_survive_intact() {
        let directory = tempfile::tempdir().expect("helper dir");
        let payload = directory.path().join("payload");
        let backend = recording_backend(directory.path(), &format!("cat > {}", payload.display()));
        let text = "héllo\twörld\nsecond line\n";

        let result = copy(&backend, text);

        assert!(
            matches!(result, super::TextCopyOutcome::Copied(_)),
            "{result:?}"
        );
        assert_eq!(std::fs::read_to_string(&payload).expect("payload"), text);
    }

    #[test]
    fn a_missing_helper_is_a_failure_the_terminal_can_cover() {
        let directory = tempfile::tempdir().expect("helper dir");
        let backend = ClipboardBackend {
            search_path: Some(directory.path().as_os_str().to_owned()),
            wayland: true,
            x11: false,
            remote: false,
        };

        let result = copy(&backend, "selected text");

        assert!(
            matches!(result, super::TextCopyOutcome::TerminalFallback { ref failures } if failures.join(";").contains("not on PATH")),
            "{result:?}"
        );
    }

    #[test]
    fn stderr_is_drained_even_after_a_quiet_start() {
        let directory = tempfile::tempdir().expect("helper dir");
        let backend = recording_backend(
            directory.path(),
            "cat >/dev/null\nsleep 0.1\ni=0\nwhile [ $i -lt 10000 ]; do echo diagnostic-line >&2; i=$((i+1)); done\necho final-diagnostic >&2\nexit 3",
        );

        let result = copy(&backend, "selection");

        match result {
            super::TextCopyOutcome::TerminalFallback { failures } => {
                let error = failures.join(";");
                assert!(error.contains("final-diagnostic"), "{error}");
                assert!(
                    error.len() <= 8300,
                    "unbounded diagnostics: {}",
                    error.len()
                );
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn a_successful_exit_does_not_hide_a_failed_input_write() {
        let directory = tempfile::tempdir().expect("helper dir");
        let backend = recording_backend(directory.path(), "exit 0");

        let result = copy(&backend, &"x".repeat(512 * 1024));

        assert!(
            matches!(result, super::TextCopyOutcome::TerminalFallback { .. }),
            "{result:?}"
        );
    }

    #[test]
    fn x11_is_tried_after_wayland_fails() {
        let directory = tempfile::tempdir().expect("helper dir");
        let mut backend = recording_backend(directory.path(), "echo no-wayland >&2\nexit 3");
        let payload = directory.path().join("payload");

        helper_script(
            directory.path(),
            "xclip",
            &format!("cat > '{}'", payload.display()),
        );
        backend.x11 = true;

        assert_eq!(
            copy(&backend, "selection"),
            super::TextCopyOutcome::Copied(super::CopyMethod::X11)
        );
        assert_eq!(std::fs::read_to_string(payload).unwrap(), "selection");
    }

    #[test]
    fn remote_sessions_never_run_native_helpers() {
        let directory = tempfile::tempdir().expect("helper dir");
        let marker = directory.path().join("ran");
        let mut backend =
            recording_backend(directory.path(), &format!("touch '{}'", marker.display()));

        backend.remote = true;
        assert!(matches!(
            copy(&backend, "selection"),
            super::TextCopyOutcome::TerminalFallback { .. }
        ));
        assert!(!marker.exists());
    }

    #[test]
    fn an_unavailable_clipboard_is_reported_without_panicking() {
        let error = super::read_image_with(false, false, None).expect_err("no clipboard");

        assert!(matches!(error, super::ClipboardError::Unavailable(_)));
        assert!(
            error.to_string().contains("no Wayland or X11 clipboard"),
            "{error}"
        );
        assert!(!error.is_silent());
    }

    #[test]
    fn a_clipboard_without_an_image_is_silent_on_either_backend() {
        for (program, body, wayland, x11) in [
            ("wl-paste", "echo 'text/plain'", true, false),
            ("xclip", "echo 'TARGETS'\necho 'text/plain'", false, true),
        ] {
            let directory = tempfile::tempdir().expect("helper dir");

            helper_script(directory.path(), program, body);

            let error = read_image_locked(wayland, x11, Some(directory.path().as_os_str()))
                .expect_err("no image");

            assert!(matches!(error, super::ClipboardError::NoImage), "{program}");
            assert!(error.is_silent(), "{program}");
        }
    }

    #[test]
    fn a_failing_clipboard_helper_is_reported_on_either_backend() {
        for (program, body, wayland, x11, needle) in [
            (
                "wl-paste",
                "echo 'no display' >&2\nexit 3",
                true,
                false,
                "no display",
            ),
            (
                "xclip",
                "echo 'cannot open display' >&2\nexit 1",
                false,
                true,
                "cannot open display",
            ),
        ] {
            let directory = tempfile::tempdir().expect("helper dir");

            helper_script(directory.path(), program, body);

            let error = read_image_locked(wayland, x11, Some(directory.path().as_os_str()))
                .expect_err("helper failed");

            assert!(
                matches!(error, super::ClipboardError::Unavailable(_)),
                "{program}"
            );
            assert!(!error.is_silent(), "{program}");
            assert!(error.to_string().contains(needle), "{program}: {error}");
        }
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"hello\n"), "aGVsbG8K");
        assert_eq!(base64("héllo".as_bytes()), "aMOpbGxv");
    }

    #[tokio::test]
    async fn copies_are_serialized_and_superseded_results_are_not_reported() {
        struct Controlled {
            tx: tokio::sync::mpsc::UnboundedSender<(String, tokio::sync::oneshot::Sender<()>)>,
        }

        #[async_trait::async_trait]
        impl super::TextClipboard for Controlled {
            async fn copy(&self, text: &str) -> Result<super::TextCopyOutcome, String> {
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.tx.send((text.to_owned(), tx)).unwrap();
                rx.await.unwrap();
                Ok(super::TextCopyOutcome::TerminalFallback { failures: vec![] })
            }
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut copies = super::CopyCoordinator::new(std::sync::Arc::new(Controlled { tx }));
        copies.enqueue("old".into());
        let (text, release) = rx.recv().await.unwrap();
        assert_eq!(text, "old");
        assert!(copies.poll().is_none());
        copies.enqueue("replaced".into());
        copies.enqueue("newest".into());
        assert!(rx.try_recv().is_err());
        release.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(copies.poll().is_none());
        let (text, release) = rx.recv().await.unwrap();
        assert_eq!(text, "newest");
        release.send(()).unwrap();
        tokio::task::yield_now().await;
        assert_eq!(copies.poll().unwrap().text, "newest");
        copies.enqueue("cancel".into());
        let (_, release) = rx.recv().await.unwrap();
        copies.enqueue("pending".into());
        copies.shutdown().await;
        assert!(release.is_closed());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn picks_a_supported_clipboard_type() {
        let types = "TARGETS\nimage/jpeg\nimage/png\nUTF8_STRING\n";

        assert_eq!(choose_image_type(types).as_deref(), Some("image/png"));
        assert_eq!(choose_image_type("text/plain"), None);
    }
}
