//! Managed child processes: bounded output, status inspection, and termination.
//!
//! On Linux every command owns a process group and a supervisor task. The
//! supervisor polls the leader without reaping it, so the group id stays valid
//! until the whole group has been signalled. Only then is the leader reaped.
//! Dropping a caller cancels the supervisor rather than abandoning the group.

use std::collections::BTreeMap;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::json;
use t8ful_core::tool::{ProgressReporter, ToolError, Utf8Progress, Utf8Stream};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Maximum output retained per managed process and per foreground command.
pub const OUTPUT_CAPACITY: usize = 1024 * 1024;

const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Grace between the group's SIGTERM and SIGKILL.
pub(crate) const TERM_GRACE: Duration = Duration::from_millis(500);

/// Bound on waiting for the output readers to reach EOF after the command ends.
pub(crate) const READER_GRACE: Duration = Duration::from_millis(250);

pub(crate) const REAP_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Running,
    Exited,
    Signalled,
    Failed,
}

impl ProcessState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Signalled => "signalled",
            Self::Failed => "failed",
        }
    }
}

/// Bounded byte buffer that remembers how much output was dropped.
#[derive(Debug)]
pub struct OutputBuffer {
    data: Vec<u8>,
    capacity: usize,
    total: u64,
}

impl OutputBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            data: Vec::new(),
            capacity,
            total: 0,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;

        if self.data.len() >= self.capacity {
            return;
        }

        let remaining = self.capacity - self.data.len();
        let take = remaining.min(chunk.len());

        self.data.extend_from_slice(&chunk[..take]);
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }

    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    pub fn truncated(&self) -> bool {
        self.total > self.data.len() as u64
    }

    pub fn dropped_bytes(&self) -> u64 {
        self.total - self.data.len() as u64
    }
}

/// One decoded display-only chunk from a background process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessChunk {
    pub id: String,
    pub chunk: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSnapshot {
    pub id: String,
    pub command: String,
    pub state: ProcessState,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub elapsed_ms: u128,
    pub total_bytes: u64,
    pub truncated: bool,
    /// A descendant kept an output pipe open past the cleanup bound.
    pub output_incomplete: bool,
    /// Process-group cleanup failed or lost ownership; the outcome is unknown.
    pub cleanup_error: Option<String>,
}

#[derive(Debug, Default)]
struct ManagedState {
    state: Option<ProcessState>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    finished: Option<Instant>,
    output_incomplete: bool,
    cleanup_error: Option<String>,
    /// Set when the user asked the process to stop, so its exit is not news.
    requested_stop: bool,
    /// Set when a wait observed the exit, so its notice is not news either.
    notice_claimed: bool,
}

#[derive(Debug)]
struct Managed {
    id: String,
    command: String,
    #[allow(dead_code)]
    pid: u32,
    started: Instant,
    output: Arc<Mutex<OutputBuffer>>,
    state: Mutex<ManagedState>,
    cancel: CancellationToken,
    done: watch::Sender<bool>,
}

impl Managed {
    fn is_running(&self) -> bool {
        self.lock_state().state.unwrap_or(ProcessState::Running) == ProcessState::Running
    }

    fn lock_state(&self) -> MutexGuard<'_, ManagedState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn lock_output(&self) -> MutexGuard<'_, OutputBuffer> {
        self.output
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn snapshot(&self) -> ProcessSnapshot {
        let state = self.lock_state();
        let output = self.lock_output();
        let ended = state.finished.unwrap_or_else(Instant::now);

        ProcessSnapshot {
            id: self.id.clone(),
            command: self.command.clone(),
            state: state.state.unwrap_or(ProcessState::Running),
            exit_code: state.exit_code,
            signal: state.signal,
            elapsed_ms: ended.duration_since(self.started).as_millis(),
            total_bytes: output.total_bytes(),
            truncated: output.truncated(),
            output_incomplete: state.output_incomplete,
            cleanup_error: state.cleanup_error.clone(),
        }
    }
}

#[derive(Debug)]
pub struct ForegroundOutcome {
    pub state: ProcessState,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub output: String,
    pub total_bytes: u64,
    pub truncated: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub output_incomplete: bool,
    pub cleanup_error: Option<String>,
    pub elapsed: Duration,
}

/// One background process published when its supervisor finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExit {
    pub snapshot: ProcessSnapshot,
    /// The exit followed an explicit `stop`, so it carries no notice.
    pub requested: bool,
}

#[derive(Debug)]
struct SupervisorSlot {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

/// Owns every child process started by the shell tool and its supervisor.
#[derive(Debug)]
pub struct ProcessManager {
    max_output_bytes: usize,
    next_id: AtomicU64,
    processes: Mutex<BTreeMap<String, Arc<Managed>>>,
    supervisors: Mutex<Vec<SupervisorSlot>>,
    exits: broadcast::Sender<ProcessExit>,
    progress: broadcast::Sender<ProcessChunk>,
    shutting_down: Arc<AtomicBool>,
}

impl ProcessManager {
    pub fn new(max_output_bytes: usize) -> Self {
        Self {
            max_output_bytes,
            next_id: AtomicU64::new(0),
            processes: Mutex::new(BTreeMap::new()),
            supervisors: Mutex::new(Vec::new()),
            exits: broadcast::channel(64).0,
            progress: broadcast::channel(1024).0,
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Subscribe to background-process exits. Every subscriber receives every
    /// exit that happens after it subscribed.
    pub fn subscribe_exits(&self) -> broadcast::Receiver<ProcessExit> {
        self.exits.subscribe()
    }

    /// Subscribe to display-only output from background processes. A slow
    /// subscriber may lag; `output()` remains the authoritative snapshot.
    pub fn subscribe_progress(&self) -> broadcast::Receiver<ProcessChunk> {
        self.progress.subscribe()
    }

    pub fn capacity(&self) -> usize {
        self.max_output_bytes
    }

    fn lock_processes(&self) -> MutexGuard<'_, BTreeMap<String, Arc<Managed>>> {
        self.processes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn lock_supervisors(&self) -> MutexGuard<'_, Vec<SupervisorSlot>> {
        self.supervisors
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn managed(&self, id: &str) -> Result<Arc<Managed>, ToolError> {
        self.lock_processes()
            .get(id)
            .cloned()
            .ok_or_else(|| ToolError::Failed(format!("unknown process id `{id}`")))
    }

    fn track_supervisor(&self, cancel: CancellationToken, handle: JoinHandle<()>) {
        let mut supervisors = self.lock_supervisors();

        supervisors.retain(|slot| !slot.handle.is_finished());
        supervisors.push(SupervisorSlot { cancel, handle });
    }

    /// Run a command to completion with a timeout, capturing bounded output.
    ///
    /// The returned future only waits for the supervisor. Dropping it cancels
    /// the supervisor, which terminates and reaps the process group off to the
    /// side; no side effect is abandoned unmanaged.
    pub async fn run_foreground(
        &self,
        command: Command,
        timeout: Duration,
    ) -> Result<ForegroundOutcome, ToolError> {
        self.run_foreground_with_progress(command, timeout, None)
            .await
    }

    /// Run a command to completion, streaming display-only chunks as they arrive.
    ///
    /// Progress never affects the recorded outcome: only the bounded buffer is
    /// authoritative, and a reporter error cannot fail the command.
    pub async fn run_foreground_with_progress(
        &self,
        mut command: Command,
        timeout: Duration,
        progress: Option<ProgressReporter>,
    ) -> Result<ForegroundOutcome, ToolError> {
        if !group_cleanup_supported() {
            return Err(ToolError::Failed(
                "managed process groups require Linux; refusing to run without cleanup".to_owned(),
            ));
        }

        configure(&mut command, true);

        let started = Instant::now();
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::Failed(format!("cannot start the command: {error}")))?;
        let pid = child
            .id()
            .ok_or_else(|| ToolError::Failed("the command has no process id".to_owned()))?;
        let output = Arc::new(Mutex::new(OutputBuffer::new(self.max_output_bytes)));
        let readers = take_readers(&mut child, Arc::clone(&output), progress);
        let deadline = tokio::time::Instant::now() + timeout;

        let supervisor = ChildSupervisor {
            child: OwnedChild::new(child, pid),
            pid,
            readers,
            deadline: Some(deadline),
        };
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let (sender, receiver) = oneshot::channel();

        let handle = tokio::spawn(async move {
            let outcome = supervisor.run(&task_cancel).await;
            let _ = sender.send(outcome);
        });

        self.track_supervisor(cancel.clone(), handle);

        let _caller = CallerGuard::new(cancel);
        let outcome = receiver.await.map_err(|_| {
            ToolError::Failed("the process supervisor stopped without a result".to_owned())
        })?;

        let (state, exit_code, signal) =
            outcome
                .status
                .map(classify)
                .unwrap_or((ProcessState::Failed, None, None));
        let output = output.lock().unwrap_or_else(|poison| poison.into_inner());

        Ok(ForegroundOutcome {
            state,
            exit_code,
            signal,
            output: output.text(),
            total_bytes: output.total_bytes(),
            truncated: output.truncated(),
            timed_out: outcome.timed_out,
            cancelled: outcome.cancelled,
            output_incomplete: outcome.output_incomplete,
            cleanup_error: outcome.cleanup_error,
            elapsed: started.elapsed(),
        })
    }

    /// Start a command and return immediately with a process id.
    pub async fn spawn_background(
        &self,
        mut command: Command,
        label: String,
    ) -> Result<String, ToolError> {
        if !group_cleanup_supported() {
            return Err(ToolError::Failed(
                "managed process groups require Linux; refusing to start without cleanup"
                    .to_owned(),
            ));
        }

        configure(&mut command, false);

        let mut child = command
            .spawn()
            .map_err(|error| ToolError::Failed(format!("cannot start the command: {error}")))?;
        let pid = child
            .id()
            .ok_or_else(|| ToolError::Failed("the command has no process id".to_owned()))?;
        let id = format!(
            "process-{}",
            self.next_id.fetch_add(1, Ordering::Relaxed) + 1
        );
        let output = Arc::new(Mutex::new(OutputBuffer::new(self.max_output_bytes)));
        let progress =
            background_reporter(self.progress.clone(), id.clone(), self.max_output_bytes);
        let readers = take_readers(&mut child, Arc::clone(&output), Some(progress));
        let (done, _receiver) = watch::channel(false);
        let managed = Arc::new(Managed {
            id: id.clone(),
            command: label,
            pid,
            started: Instant::now(),
            output,
            state: Mutex::new(ManagedState::default()),
            cancel: CancellationToken::new(),
            done,
        });
        let supervisor = ChildSupervisor {
            child: OwnedChild::new(child, pid),
            pid,
            readers,
            deadline: None,
        };
        let cancel = managed.cancel.clone();
        let waiter = Arc::clone(&managed);
        let exits = self.exits.clone();
        let shutdown_flag = Arc::clone(&self.shutting_down);
        let handle = tokio::spawn(async move {
            let outcome = supervisor.run(&cancel).await;
            let mut state = waiter.lock_state();

            match outcome.status {
                Some(status) => {
                    let (process_state, exit_code, signal) = classify(status);

                    state.state = Some(process_state);
                    state.exit_code = exit_code;
                    state.signal = signal;
                }
                None => state.state = Some(ProcessState::Failed),
            }

            state.output_incomplete = outcome.output_incomplete;
            state.cleanup_error = outcome.cleanup_error;
            state.finished = Some(Instant::now());
            let requested = state.requested_stop;
            drop(state);

            let _ = waiter.done.send(true);

            // Shutdown is not news to an agent that is going away with it.
            if !shutdown_flag.load(Ordering::Acquire) {
                let _ = exits.send(ProcessExit {
                    snapshot: waiter.snapshot(),
                    requested,
                });
            }
        });

        self.track_supervisor(managed.cancel.clone(), handle);
        self.lock_processes().insert(id.clone(), managed);

        Ok(id)
    }

    pub fn snapshot(&self, id: &str) -> Result<ProcessSnapshot, ToolError> {
        Ok(self.managed(id)?.snapshot())
    }

    /// Output collected so far, plus whether older bytes were dropped.
    pub fn output(&self, id: &str) -> Result<(String, bool, u64), ToolError> {
        let managed = self.managed(id)?;
        let output = managed.lock_output();

        Ok((output.text(), output.truncated(), output.total_bytes()))
    }

    pub fn list(&self) -> Vec<ProcessSnapshot> {
        self.lock_processes()
            .values()
            .map(|managed| managed.snapshot())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.lock_processes().is_empty()
    }

    /// Cancel a supervisor and wait for its bounded cleanup to finish.
    pub async fn stop(&self, id: &str) -> Result<ProcessSnapshot, ToolError> {
        let managed = self.managed(id)?;

        managed.lock_state().requested_stop = true;

        if managed.is_running() {
            let mut done = managed.done.subscribe();

            managed.cancel.cancel();

            let finished = tokio::time::timeout(STOP_TIMEOUT, async {
                while !*done.borrow() {
                    if done.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;

            if finished.is_err() && managed.is_running() {
                return Err(ToolError::Failed(format!(
                    "process `{id}` did not finish cleanup; the outcome is indeterminate"
                )));
            }
        }

        Ok(managed.snapshot())
    }

    /// Wait for a process to exit, or up to `limit` when one is given.
    ///
    /// Returns the snapshot and whether it had exited. A finished process
    /// returns at once. A wait that observes the exit claims its notice, so the
    /// agent is not told about an exit it just read; a wait that gives up claims
    /// nothing, so the notice can still wake an idle agent.
    pub async fn wait_for_exit(
        &self,
        id: &str,
        limit: Option<Duration>,
    ) -> Result<(ProcessSnapshot, bool), ToolError> {
        let managed = self.managed(id)?;

        // Subscribe before inspecting the state: the watch only stores a value
        // while a receiver exists, so a receiver created after the exit would
        // otherwise see a stale `false` and wait for a wake that never comes.
        let mut done = managed.done.subscribe();
        let wait = async {
            // The state is written before the wake, so a finished process is
            // already visible here even when the wake was missed.
            while managed.is_running() && !*done.borrow() {
                if done.changed().await.is_err() {
                    break;
                }
            }
        };

        let exited = match limit {
            Some(limit) => tokio::time::timeout(limit, wait).await.is_ok(),
            None => {
                wait.await;
                true
            }
        };

        if exited {
            managed.lock_state().notice_claimed = true;
        }

        Ok((managed.snapshot(), exited))
    }

    /// Whether a wait already read this process's exit.
    ///
    /// An unknown id has claimed nothing, so a notice whose process is not in
    /// the table is still delivered.
    pub fn notice_claimed(&self, id: &str) -> bool {
        self.lock_processes()
            .get(id)
            .is_some_and(|managed| managed.lock_state().notice_claimed)
    }

    /// Terminate every managed process group and wait for every supervisor.
    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);

        let ids = self.lock_processes().keys().cloned().collect::<Vec<_>>();

        for id in ids {
            let _ = self.stop(&id).await;
        }

        let supervisors = std::mem::take(&mut *self.lock_supervisors());

        for supervisor in &supervisors {
            supervisor.cancel.cancel();
        }

        for supervisor in supervisors {
            let _ = supervisor.handle.await;
        }
    }

    pub fn describe(&self, id: &str) -> Result<serde_json::Value, ToolError> {
        let snapshot = self.snapshot(id)?;

        Ok(json!({
            "process_id": snapshot.id,
            "command": snapshot.command,
            "state": snapshot.state.as_str(),
            "exit_code": snapshot.exit_code,
            "signal": snapshot.signal,
            "elapsed_ms": snapshot.elapsed_ms,
            "total_bytes": snapshot.total_bytes,
            "truncated": snapshot.truncated,
            "output_incomplete": snapshot.output_incomplete,
            "cleanup_error": snapshot.cleanup_error,
        }))
    }
}

fn configure(command: &mut Command, kill_on_drop: bool) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(kill_on_drop);

    #[cfg(unix)]
    command.process_group(0);
}

/// Build the display-only reporter for one background process.
///
/// The reporter owns its decoder and byte budget, so stdout and stderr share
/// one ordering and a character split across two reads is not corrupted. It
/// announces truncation once and then stops copying bytes; the bounded output
/// buffer stays authoritative.
fn background_reporter(
    sender: broadcast::Sender<ProcessChunk>,
    id: String,
    budget: usize,
) -> ProgressReporter {
    struct State {
        stream: Utf8Stream,
        sent: usize,
        announced: bool,
    }

    let state = Arc::new(Mutex::new(State {
        stream: Utf8Stream::new(),
        sent: 0,
        announced: false,
    }));

    ProgressReporter::new(move |bytes: &[u8]| {
        if bytes.is_empty() {
            return;
        }

        let mut state = state.lock().unwrap_or_else(|poison| poison.into_inner());

        if state.announced {
            return;
        }

        let text = state.stream.push(bytes);

        if text.is_empty() {
            return;
        }

        let mut take = text.len().min(budget.saturating_sub(state.sent));

        while take > 0 && !text.is_char_boundary(take) {
            take -= 1;
        }

        state.sent += take;

        let truncated = state.sent >= budget;

        if truncated {
            state.announced = true;
        }

        let chunk = text[..take].to_owned();
        drop(state);

        let _ = sender.send(ProcessChunk {
            id: id.clone(),
            chunk,
            truncated,
        });
    })
}

fn take_readers(
    child: &mut Child,
    output: Arc<Mutex<OutputBuffer>>,
    progress: Option<ProgressReporter>,
) -> Vec<JoinHandle<()>> {
    let mut readers = Vec::new();

    if let Some(stdout) = child.stdout.take() {
        readers.push(tokio::spawn(pump(
            stdout,
            Arc::clone(&output),
            progress.clone().map(Utf8Progress::new),
        )));
    }

    if let Some(stderr) = child.stderr.take() {
        readers.push(tokio::spawn(pump(
            stderr,
            output,
            progress.map(Utf8Progress::new),
        )));
    }

    readers
}

async fn pump<R>(
    mut reader: R,
    output: Arc<Mutex<OutputBuffer>>,
    mut progress: Option<Utf8Progress>,
) where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 8192];

    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                output
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .push(&buffer[..read]);

                if let Some(progress) = &mut progress {
                    progress.report(&buffer[..read]);
                }
            }
        }
    }

    if let Some(progress) = &mut progress {
        progress.finish();
    }
}

struct CallerGuard(CancellationToken);

impl CallerGuard {
    fn new(cancel: CancellationToken) -> Self {
        Self(cancel)
    }
}

impl Drop for CallerGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct OwnedChild {
    child: Child,
    pid: u32,
    armed: bool,
}

impl OwnedChild {
    fn new(child: Child, pid: u32) -> Self {
        Self {
            child,
            pid,
            armed: true,
        }
    }

    async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let result = self.child.wait().await;

        // Never signal after a failed reap: the pid may already belong to
        // another process and only the kernel knows.
        self.armed = false;

        result
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.armed {
            // Best effort: the supervisor is being dropped before reaping.
            let _ = kill_group(self.pid, GroupSignal::Kill);
        }
    }
}

struct SupervisorOutcome {
    status: Option<ExitStatus>,
    timed_out: bool,
    cancelled: bool,
    output_incomplete: bool,
    cleanup_error: Option<String>,
}

struct ChildSupervisor {
    child: OwnedChild,
    pid: u32,
    readers: Vec<JoinHandle<()>>,
    deadline: Option<tokio::time::Instant>,
}

impl ChildSupervisor {
    async fn run(self, cancel: &CancellationToken) -> SupervisorOutcome {
        let mut timed_out = false;
        let mut cancelled = false;
        let mut cleanup_error = None;

        loop {
            if cancel.is_cancelled() {
                cancelled = true;
                break;
            }

            match leader_exited(self.pid) {
                LeaderPoll::Exited => return self.finish_normally().await,
                LeaderPoll::Running => {}
                LeaderPoll::OwnershipLost => {
                    return self.lost_ownership(timed_out, cancelled).await;
                }
                LeaderPoll::Failed(message) => {
                    cleanup_error = Some(message);
                    break;
                }
            }

            match self.deadline {
                Some(deadline) => {
                    let now = tokio::time::Instant::now();

                    if now >= deadline {
                        timed_out = true;
                        break;
                    }

                    tokio::select! {
                        () = cancel.cancelled() => {
                            cancelled = true;
                            break;
                        }
                        () = tokio::time::sleep_until((now + POLL_INTERVAL).min(deadline)) => {}
                    }
                }
                None => {
                    tokio::select! {
                        () = cancel.cancelled() => {
                            cancelled = true;
                            break;
                        }
                        () = tokio::time::sleep(POLL_INTERVAL) => {}
                    }
                }
            }
        }

        self.terminate(timed_out, cancelled, cleanup_error).await
    }

    async fn finish_normally(mut self) -> SupervisorOutcome {
        let mut cleanup_error = None;

        // The shell leader is a zombie and keeps the group id allocated. Kill
        // every remaining member immediately, then reap the leader.
        if let Err(error) = kill_group(self.pid, GroupSignal::Kill) {
            cleanup_error = Some(error);
        }

        let (status, reap_error) = self.reap().await;

        if cleanup_error.is_none() {
            cleanup_error = reap_error;
        }

        let output_incomplete = self.drain_readers().await;
        let timed_out = self
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline);

        SupervisorOutcome {
            status,
            timed_out,
            cancelled: false,
            output_incomplete,
            cleanup_error,
        }
    }

    async fn terminate(
        mut self,
        timed_out: bool,
        cancelled: bool,
        mut cleanup_error: Option<String>,
    ) -> SupervisorOutcome {
        if let Err(error) = kill_group(self.pid, GroupSignal::Term) {
            cleanup_error.get_or_insert(error);
        }

        let grace_end = Instant::now() + TERM_GRACE;

        loop {
            match leader_exited(self.pid) {
                LeaderPoll::Exited => break,
                LeaderPoll::Running => {
                    if Instant::now() >= grace_end {
                        break;
                    }

                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                LeaderPoll::OwnershipLost => {
                    self.child.disarm();

                    let output_incomplete = self.drain_readers().await;

                    return SupervisorOutcome {
                        status: None,
                        timed_out,
                        cancelled,
                        output_incomplete,
                        cleanup_error: Some(
                            "lost ownership of the process group; cleanup is indeterminate"
                                .to_owned(),
                        ),
                    };
                }
                LeaderPoll::Failed(message) => {
                    cleanup_error.get_or_insert(message);
                    break;
                }
            }
        }

        if let Err(error) = kill_group(self.pid, GroupSignal::Kill) {
            cleanup_error.get_or_insert(error);
        }

        let (status, reap_error) = self.reap().await;

        if cleanup_error.is_none() {
            cleanup_error = reap_error;
        }

        let output_incomplete = self.drain_readers().await;

        SupervisorOutcome {
            status,
            timed_out,
            cancelled,
            output_incomplete,
            cleanup_error,
        }
    }

    async fn lost_ownership(mut self, timed_out: bool, cancelled: bool) -> SupervisorOutcome {
        self.child.disarm();

        let output_incomplete = self.drain_readers().await;

        SupervisorOutcome {
            status: None,
            timed_out,
            cancelled,
            output_incomplete,
            cleanup_error: Some(
                "lost ownership of the process group; cleanup is indeterminate".to_owned(),
            ),
        }
    }

    async fn reap(&mut self) -> (Option<ExitStatus>, Option<String>) {
        match tokio::time::timeout(REAP_TIMEOUT, self.child.wait()).await {
            Ok(Ok(status)) => (Some(status), None),
            Ok(Err(error)) => (None, Some(format!("cannot reap the command: {error}"))),
            Err(_) => (None, Some("timed out while reaping the command".to_owned())),
        }
    }

    /// Wait a bounded time for the readers to reach EOF, then abort them.
    async fn drain_readers(&mut self) -> bool {
        let deadline = Instant::now() + READER_GRACE;
        let mut incomplete = false;

        for mut reader in self.readers.drain(..) {
            let remaining = deadline.saturating_duration_since(Instant::now());

            if remaining.is_zero() {
                reader.abort();
                incomplete = true;
                continue;
            }

            if tokio::time::timeout(remaining, &mut reader).await.is_err() {
                reader.abort();
                let _ = reader.await;
                incomplete = true;
            }
        }

        incomplete
    }
}

#[derive(Clone, Copy)]
pub(crate) enum GroupSignal {
    Term,
    Kill,
}

fn group_cleanup_supported() -> bool {
    cfg!(target_os = "linux")
}

enum LeaderPoll {
    Running,
    Exited,
    OwnershipLost,
    Failed(String),
}

#[cfg(target_os = "linux")]
fn leader_exited(pid: u32) -> LeaderPoll {
    use nix::errno::Errno;
    use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
    use nix::unistd::Pid;

    let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;

    match waitid(Id::Pid(Pid::from_raw(pid as i32)), flags) {
        Ok(WaitStatus::StillAlive) => LeaderPoll::Running,
        Ok(_) => LeaderPoll::Exited,
        Err(Errno::ECHILD) => LeaderPoll::OwnershipLost,
        Err(error) => LeaderPoll::Failed(format!(
            "cannot inspect the process group while it is still owned: {error}"
        )),
    }
}

#[cfg(not(target_os = "linux"))]
fn leader_exited(_pid: u32) -> LeaderPoll {
    LeaderPoll::Failed("managed process groups require Linux".to_owned())
}

#[cfg(target_os = "linux")]
pub(crate) fn kill_group(pid: u32, signal: GroupSignal) -> Result<(), String> {
    use nix::errno::Errno;
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;

    let signal = match signal {
        GroupSignal::Term => Signal::SIGTERM,
        GroupSignal::Kill => Signal::SIGKILL,
    };

    match killpg(Pid::from_raw(pid as i32), signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(format!("cannot signal process group {pid}: {error}")),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn kill_group(_pid: u32, _signal: GroupSignal) -> Result<(), String> {
    Err("managed process groups require Linux".to_owned())
}

fn classify(status: ExitStatus) -> (ProcessState, Option<i32>, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        match status.code() {
            Some(code) => (ProcessState::Exited, Some(code), None),
            None => (ProcessState::Signalled, None, status.signal()),
        }
    }

    #[cfg(not(unix))]
    {
        (ProcessState::Exited, status.code(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::OutputBuffer;
    use std::time::Duration;

    #[test]
    fn output_buffer_keeps_the_first_bytes_and_counts_the_rest() {
        let mut buffer = OutputBuffer::new(8);

        buffer.push(b"abcd");
        buffer.push(b"efghijkl");

        assert_eq!(buffer.text(), "abcdefgh");
        assert_eq!(buffer.total_bytes(), 12);
        assert!(buffer.truncated());
        assert_eq!(buffer.dropped_bytes(), 4);
    }

    #[tokio::test]
    async fn foreground_progress_reports_each_chunk_without_changing_the_outcome() {
        let manager = super::ProcessManager::new(1024);
        let mut command = tokio::process::Command::new("bash");

        command.arg("-c").arg("printf 'alpha'; printf 'beta'");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = std::sync::Arc::clone(&seen);
        let progress = t8ful_core::tool::ProgressReporter::new(move |bytes| {
            sink.lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push_str(&String::from_utf8_lossy(bytes));
        });
        let outcome = manager
            .run_foreground_with_progress(
                command,
                std::time::Duration::from_secs(10),
                Some(progress),
            )
            .await
            .expect("command runs");

        assert_eq!(outcome.output, "alphabeta");
        assert!(outcome.exit_code == Some(0));

        let seen = seen
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();

        assert_eq!(seen, "alphabeta", "every byte is reported exactly once");
    }

    #[test]
    fn output_buffer_reports_no_truncation_below_capacity() {
        let mut buffer = OutputBuffer::new(16);

        buffer.push(b"hello");

        assert!(!buffer.truncated());
        assert_eq!(buffer.dropped_bytes(), 0);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn background_progress_arrives_before_the_process_exits() {
        use std::sync::{Arc, Mutex};

        let manager = super::ProcessManager::new(4096);
        let mut progress = manager.subscribe_progress();
        let mut command = tokio::process::Command::new("bash");

        command.arg("-c").arg(
            "printf 'first\\n'; printf 'to-stderr\\n' >&2; printf '\\303'; sleep 0.2; printf '\\251\\n'; sleep 10",
        );

        let id = manager
            .spawn_background(command, "stream".to_owned())
            .await
            .expect("spawn");
        let seen = Arc::new(Mutex::new(String::new()));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        while tokio::time::Instant::now() < deadline {
            let Ok(Ok(chunk)) =
                tokio::time::timeout(Duration::from_millis(500), progress.recv()).await
            else {
                continue;
            };

            assert_eq!(chunk.id, id);
            seen.lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push_str(&chunk.chunk);

            let seen = seen
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone();

            if seen.contains("first") && seen.contains("to-stderr") && seen.contains("é") {
                break;
            }
        }

        let seen = seen
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();

        assert!(seen.contains("first"), "{seen:?}");
        assert!(seen.contains("to-stderr"), "{seen:?}");
        assert!(
            seen.contains("é"),
            "a split multibyte character was corrupted: {seen:?}"
        );

        manager.stop(&id).await.expect("stop");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_natural_exit_is_published_exactly_once() {
        use std::time::Duration;

        let manager = super::ProcessManager::new(4096);
        let mut exits = manager.subscribe_exits();
        let mut command = tokio::process::Command::new("bash");

        command.arg("-c").arg("printf done");

        let id = manager
            .spawn_background(command, "printf done".to_owned())
            .await
            .expect("spawn");
        let exit = tokio::time::timeout(Duration::from_secs(10), exits.recv())
            .await
            .expect("an exit event")
            .expect("sender alive");

        assert_eq!(exit.snapshot.id, id);
        assert_eq!(exit.snapshot.state, super::ProcessState::Exited);
        assert_eq!(exit.snapshot.exit_code, Some(0));
        assert!(!exit.requested);
        assert!(exits.try_recv().is_err(), "the exit was published twice");

        manager.shutdown().await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_wait_that_reads_an_exit_claims_its_notice() {
        use std::time::Duration;

        let manager = super::ProcessManager::new(4096);
        let mut exits = manager.subscribe_exits();
        let mut command = tokio::process::Command::new("bash");

        command.arg("-c").arg("printf done");

        let id = manager
            .spawn_background(command, "printf done".to_owned())
            .await
            .expect("spawn");
        let exit = tokio::time::timeout(Duration::from_secs(10), exits.recv())
            .await
            .expect("an exit event")
            .expect("sender alive");

        assert_eq!(exit.snapshot.id, id);

        // The process is already finished, so the wait must not sleep on a wake
        // that has come and gone.
        let (snapshot, exited) = tokio::time::timeout(
            Duration::from_millis(500),
            manager.wait_for_exit(&id, Some(Duration::from_secs(30))),
        )
        .await
        .expect("a finished process returns at once")
        .expect("wait");

        assert!(exited);
        assert_eq!(snapshot.state, super::ProcessState::Exited);
        assert_eq!(snapshot.exit_code, Some(0));
        assert!(manager.notice_claimed(&id));

        manager.shutdown().await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_wait_that_times_out_claims_nothing() {
        use std::time::Duration;

        let manager = super::ProcessManager::new(4096);
        let mut command = tokio::process::Command::new("bash");

        command.arg("-c").arg("sleep 30");

        let id = manager
            .spawn_background(command, "sleep 30".to_owned())
            .await
            .expect("spawn");
        let (snapshot, exited) = manager
            .wait_for_exit(&id, Some(Duration::from_millis(100)))
            .await
            .expect("wait");

        assert!(!exited);
        assert_eq!(snapshot.state, super::ProcessState::Running);
        assert!(
            !manager.notice_claimed(&id),
            "a timed-out wait claimed the notice"
        );

        manager.shutdown().await;
    }

    #[test]
    fn an_unknown_process_has_claimed_nothing() {
        let manager = super::ProcessManager::new(1024);

        assert!(!manager.notice_claimed("process-999"));
        assert!(manager.snapshot("process-999").is_err());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_requested_stop_is_flagged_and_shutdown_publishes_nothing() {
        use std::time::Duration;

        let manager = super::ProcessManager::new(4096);
        let mut exits = manager.subscribe_exits();
        let mut command = tokio::process::Command::new("bash");

        command.arg("-c").arg("sleep 30");

        let id = manager
            .spawn_background(command, "sleep 30".to_owned())
            .await
            .expect("spawn");
        let snapshot = manager.stop(&id).await.expect("stop");
        let exit = tokio::time::timeout(Duration::from_secs(10), exits.recv())
            .await
            .expect("an exit event")
            .expect("sender alive");

        assert_eq!(snapshot.id, id);
        assert_eq!(exit.snapshot.id, id);
        assert!(exit.requested);

        let mut shutdown_exits = manager.subscribe_exits();
        let mut command = tokio::process::Command::new("bash");

        command.arg("-c").arg("sleep 30");
        manager
            .spawn_background(command, "sleep 30".to_owned())
            .await
            .expect("spawn");
        manager.shutdown().await;

        assert!(
            shutdown_exits.try_recv().is_err(),
            "shutdown published a process exit"
        );
    }
}
