//! Versioned, append-only JSONL session journals.
//!
//! One journal is authoritative for a session. A single process holds an
//! exclusive lock for its lifetime and every append is flushed with `sync_data`
//! before callers proceed. Resume repairs only an incomplete trailing record;
//! malformed earlier records are reported instead of discarded.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use t8ful_core::content::ImageInput;
use t8ful_core::context::Projection;
use t8ful_core::journal::{
    AgentJournal, CompactionRecord, JournalError, ModelAttempt, UsageRecord,
};
use t8ful_core::message::{Conversation, Message, Role};
use t8ful_core::policy::Mode;
use t8ful_core::provider::ReasoningEffort;
use t8ful_core::task::TaskRecord;
use t8ful_core::tool::{ToolCall, ToolResult, ToolStatus};

use crate::state::{StatePaths, ensure_private_dir, ensure_private_file, open_private_append};
use crate::usage::UsageLedger;

pub const JOURNAL_VERSION: u64 = 1;

/// Largest journal loaded into memory for resume or listing.
pub const MAX_JOURNAL_BYTES: u64 = 128 * 1024 * 1024;

const MAX_ID_LEN: usize = 128;
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// A validated session identifier. It never contains path separators.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(String);

impl SessionId {
    pub fn parse(value: &str) -> Result<Self, SessionError> {
        if value.is_empty() || value.len() > MAX_ID_LEN {
            return Err(SessionError::InvalidId(value.to_owned()));
        }

        if !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        }) {
            return Err(SessionError::InvalidId(value.to_owned()));
        }

        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A session id that came from this process rather than from user input.
    ///
    /// Still validated: the ledger keys records by this string, and a path
    /// separator reaching it would write outside the usage directory.
    pub fn from_generated(value: &str) -> Result<Self, SessionError> {
        Self::parse(value)
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedInput {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHeader {
    pub id: SessionId,
    pub project_root: PathBuf,
    pub created_at: u64,
    pub model: Option<String>,
    pub mode: Mode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSession {
    pub header: SessionHeader,
    pub conversation: Conversation,
    /// The last committed summary projection, if any.
    pub projection: Option<Projection>,
    pub model: Option<String>,
    pub mode: Mode,
    pub reasoning_effort: ReasoningEffort,
    pub queued: Vec<QueuedInput>,
    pub queue_paused: bool,
    pub attachments: Vec<ImageInput>,
    pub usage: Vec<UsageRecord>,
    /// Task-graph changes in append order, ready for `TaskGraph::restore`.
    pub tasks: Vec<TaskRecord>,
    /// Unix seconds of the newest journaled message, or zero when the journal
    /// predates per-record timestamps.
    pub last_message_at: u64,
    pub records: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalRecord {
    Header {
        session_id: String,
        project_root: String,
        created_at: u64,
        model: Option<String>,
        mode: Mode,
    },
    Message {
        message: Message,
    },
    Settings {
        model: Option<String>,
        mode: Mode,
        #[serde(default)]
        reasoning_effort: ReasoningEffort,
    },
    QueueAdded {
        id: String,
        text: String,
    },
    QueueDelivered {
        id: String,
    },
    QueueCleared,
    QueuePaused {
        paused: bool,
    },
    ModelAttempt(ModelAttempt),
    Dispatch {
        call: ToolCall,
    },
    ToolResult {
        result: ToolResult,
    },
    Compaction(CompactionRecord),
    Usage {
        record: UsageRecord,
    },
    Attachment {
        attachment: ImageInput,
    },
    /// A task was added to the session graph.
    Task(TaskRecord),
    /// A child agent was started from this session.
    AgentSpawned {
        agent: String,
        role: String,
        task: Option<String>,
        isolation: String,
        worktree: Option<String>,
        branch: Option<String>,
    },
    /// A child agent stopped.
    AgentFinished {
        agent: String,
        status: String,
        steps: u32,
        elapsed_ms: u64,
        worktree: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct JournalLine {
    #[serde(default = "journal_version")]
    v: u64,
    /// Unix seconds when the record was appended. Journals written before the
    /// field existed read as zero.
    #[serde(default)]
    at: u64,
    #[serde(flatten)]
    record: JournalRecord,
}

fn journal_version() -> u64 {
    JOURNAL_VERSION
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("cannot access session storage: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid session id `{0}`")]
    InvalidId(String),

    #[error("session `{0}` does not exist")]
    NotFound(String),

    #[error("session is already open by another writer: {0}")]
    InUse(String),

    #[error("session journal is corrupt at line {line}: {message}")]
    Corrupt { line: usize, message: String },

    #[error("session journal is empty or its header is missing")]
    Empty,

    #[error("session journal version {0} is not supported")]
    UnsupportedVersion(u64),

    #[error(
        "session journal {path} is larger than the {limit}-byte load limit; archive or delete \
         the session to continue"
    )]
    TooLarge { path: PathBuf, limit: u64 },

    #[error("session `{id}` belongs to project {stored}, not the current project {current}")]
    RootMismatch {
        id: String,
        stored: String,
        current: String,
    },

    #[error("cannot serialize a session record: {0}")]
    Serialize(String),
}

impl From<SessionError> for JournalError {
    fn from(error: SessionError) -> Self {
        JournalError::new(error.to_string())
    }
}

/// What a journal needs to know before it touches the disk.
///
/// Frozen at construction so a lazily created journal can materialize from any
/// writer without re-reading the caller's state.
struct JournalParams {
    id: SessionId,
    paths: StatePaths,
    /// Canonical project root recorded in the header.
    project_root: PathBuf,
    model: Option<String>,
    mode: Mode,
    reasoning_effort: ReasoningEffort,
    ledger: Option<Arc<UsageLedger>>,
}

/// The file handle and exclusive lock of a materialized journal.
struct OpenJournal {
    file: Arc<Mutex<File>>,
    _lock: nix::fcntl::Flock<File>,
}

/// The single writer for one session journal.
///
/// The identifier exists from the moment the journal is requested; the journal
/// file appears on the first record. A session the user never writes to leaves
/// nothing behind on disk.
pub struct SessionJournal {
    params: Arc<JournalParams>,
    open: Arc<Mutex<Option<OpenJournal>>>,
}

impl std::fmt::Debug for SessionJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionJournal")
            .field("id", &self.params.id)
            .finish()
    }
}

impl SessionJournal {
    /// Allocate a session and its journal file immediately.
    pub async fn create(
        paths: &StatePaths,
        project_root: &Path,
        model: Option<String>,
        mode: Mode,
        reasoning_effort: ReasoningEffort,
        ledger: Arc<UsageLedger>,
    ) -> Result<Self, SessionError> {
        let journal = Self::pending(
            paths,
            project_root,
            model,
            mode,
            reasoning_effort,
            Some(ledger),
        )?;

        journal.materialize().await?;

        Ok(journal)
    }

    /// Allocate a session identifier without touching the state directory.
    ///
    /// The journal file is created by the first record, so an interactive
    /// session that never records anything leaves no session behind.
    pub fn pending(
        paths: &StatePaths,
        project_root: &Path,
        model: Option<String>,
        mode: Mode,
        reasoning_effort: ReasoningEffort,
        ledger: Option<Arc<UsageLedger>>,
    ) -> Result<Self, SessionError> {
        let canonical_root = std::fs::canonicalize(project_root)?;

        Ok(Self {
            params: Arc::new(JournalParams {
                id: new_session_id(),
                paths: paths.clone(),
                project_root: canonical_root,
                model,
                mode,
                reasoning_effort,
                ledger,
            }),
            open: Arc::new(Mutex::new(None)),
        })
    }

    /// Reopen a session for resume, acquiring the exclusive writer lock.
    pub async fn open(
        paths: &StatePaths,
        id: &SessionId,
        project_root: &Path,
        ledger: Arc<UsageLedger>,
    ) -> Result<(Self, LoadedSession), SessionError> {
        let dir = paths.session_dir(id.as_str());
        let metadata = std::fs::symlink_metadata(&dir).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SessionError::NotFound(id.0.clone())
            } else {
                SessionError::Io(error)
            }
        })?;

        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(SessionError::InvalidId(id.0.clone()));
        }

        let (file, lock) = open_locked(paths, id.as_str())?;
        let path = paths.journal_path(id.as_str());

        let bytes = repair_trailing(&file, &path)?;

        let loaded = load_from_bytes(&bytes, Some(project_root))?;
        let journal = Self {
            params: Arc::new(JournalParams {
                id: id.clone(),
                paths: paths.clone(),
                project_root: loaded.header.project_root.clone(),
                model: loaded.model.clone(),
                mode: loaded.mode,
                reasoning_effort: loaded.reasoning_effort,
                ledger: Some(ledger),
            }),
            open: Arc::new(Mutex::new(Some(OpenJournal {
                file: Arc::new(Mutex::new(file)),
                _lock: lock,
            }))),
        };

        Ok((journal, loaded))
    }

    pub fn id(&self) -> &SessionId {
        &self.params.id
    }

    /// The open file, creating the session directory and its header records the
    /// first time a record is written.
    async fn file(&self) -> Result<Arc<Mutex<File>>, SessionError> {
        if let Some(open) = lock_file_slot(&self.open).as_ref() {
            return Ok(Arc::clone(&open.file));
        }

        self.materialize().await?;

        let slot = lock_file_slot(&self.open);
        let open = slot.as_ref().expect("a materialized journal has a file");

        Ok(Arc::clone(&open.file))
    }

    /// Create the journal on disk, write its header, and take the writer lock.
    async fn materialize(&self) -> Result<(), SessionError> {
        if lock_file_slot(&self.open).is_some() {
            return Ok(());
        }

        let params = Arc::clone(&self.params);
        let open_slot = Arc::clone(&self.open);

        tokio::task::spawn_blocking(move || {
            let mut slot = lock_file_slot(&open_slot);

            if slot.is_some() {
                return Ok(());
            }

            let open = create_open_journal(&params)?;
            let mut line = header_lines(&params);

            {
                let mut file = lock_file(&open.file);

                for record in &mut line {
                    file.write_all(record)?;
                }

                file.sync_data()?;
            }

            *slot = Some(open);

            Ok(())
        })
        .await
        .map_err(|error| SessionError::Io(std::io::Error::other(error)))?
    }

    pub async fn append(&self, record: JournalRecord) -> Result<(), SessionError> {
        let file = self.file().await?;
        let line = JournalLine {
            v: JOURNAL_VERSION,
            at: now_seconds(),
            record,
        };
        let mut rendered = serde_json::to_vec(&line)
            .map_err(|error| SessionError::Serialize(error.to_string()))?;

        rendered.push(b'\n');

        tokio::task::spawn_blocking(move || {
            let mut file = lock_file(&file);
            file.write_all(&rendered)?;
            file.sync_data()
        })
        .await
        .map_err(|error| SessionError::Io(std::io::Error::other(error)))??;

        Ok(())
    }

    pub async fn record_settings(
        &self,
        model: Option<String>,
        mode: Mode,
        reasoning_effort: ReasoningEffort,
    ) -> Result<(), SessionError> {
        self.append(JournalRecord::Settings {
            model,
            mode,
            reasoning_effort,
        })
        .await
    }

    pub async fn record_attachment(&self, attachment: ImageInput) -> Result<(), SessionError> {
        self.append(JournalRecord::Attachment { attachment }).await
    }

    pub async fn queue_add(&self, id: String, text: String) -> Result<(), SessionError> {
        self.append(JournalRecord::QueueAdded { id, text }).await
    }

    pub async fn queue_delivered(&self, id: String) -> Result<(), SessionError> {
        self.append(JournalRecord::QueueDelivered { id }).await
    }

    pub async fn queue_clear(&self) -> Result<(), SessionError> {
        self.append(JournalRecord::QueueCleared).await
    }

    pub async fn queue_paused(&self, paused: bool) -> Result<(), SessionError> {
        self.append(JournalRecord::QueuePaused { paused }).await
    }

    async fn append_usage(&self, record: UsageRecord) -> Result<(), SessionError> {
        self.append(JournalRecord::Usage {
            record: record.clone(),
        })
        .await?;

        if let Some(ledger) = &self.params.ledger
            && let Err(error) = ledger.append(&self.params.id, &record).await
        {
            tracing::warn!(
                target: "t8ful::usage",
                "could not append the usage ledger; it can be rebuilt from the session: {error}"
            );
        }

        Ok(())
    }
}

#[async_trait]
impl AgentJournal for SessionJournal {
    async fn record_message(&self, message: &Message) -> Result<(), JournalError> {
        self.append(JournalRecord::Message {
            message: message.clone(),
        })
        .await
        .map_err(JournalError::from)
    }

    async fn record_model_attempt(&self, attempt: &ModelAttempt) -> Result<(), JournalError> {
        self.append(JournalRecord::ModelAttempt(attempt.clone()))
            .await
            .map_err(JournalError::from)
    }

    async fn record_dispatch(&self, call: &ToolCall) -> Result<(), JournalError> {
        self.append(JournalRecord::Dispatch { call: call.clone() })
            .await
            .map_err(JournalError::from)
    }

    async fn record_tool_result(&self, result: &ToolResult) -> Result<(), JournalError> {
        self.append(JournalRecord::ToolResult {
            result: result.clone(),
        })
        .await
        .map_err(JournalError::from)
    }

    async fn record_usage(&self, usage: &UsageRecord) -> Result<(), JournalError> {
        self.append_usage(usage.clone())
            .await
            .map_err(JournalError::from)
    }

    async fn record_compaction(&self, projection: &CompactionRecord) -> Result<(), JournalError> {
        self.append(JournalRecord::Compaction(projection.clone()))
            .await
            .map_err(JournalError::from)
    }
}

impl SessionJournal {
    /// Append a user-visible message and return the durable write.
    #[cfg(test)]
    pub async fn record_user_message(&self, text: impl Into<String>) -> Result<(), SessionError> {
        self.append(JournalRecord::Message {
            message: Message::user_text(text),
        })
        .await
    }
}

fn open_locked(
    paths: &StatePaths,
    id: &str,
) -> Result<(File, nix::fcntl::Flock<File>), SessionError> {
    let lock_path = paths.session_dir(id).join("lock");
    let lock_file = open_private_append(&lock_path)?;
    let lock = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusiveNonblock)
        .map_err(|(_, error)| SessionError::InUse(format!("{id}: {error}")))?;
    let journal_path = paths.journal_path(id);
    let file = open_private_append(&journal_path)?;

    ensure_private_file(&journal_path)?;

    Ok((file, lock))
}

/// Claim the session directory and open its journal under the writer lock.
fn create_open_journal(params: &JournalParams) -> Result<OpenJournal, SessionError> {
    let id = params.id.as_str();

    ensure_private_dir(&params.paths.sessions)?;

    // The identifier is handed out before the first write, so the directory may
    // already exist: an attachment claims it to store session-owned bytes. The
    // journal itself must not, or this id has already been used.
    ensure_private_dir(&params.paths.session_dir(id))?;
    ensure_private_dir(&params.paths.attachment_dir(id))?;

    if params.paths.journal_path(id).exists() {
        return Err(SessionError::InUse(format!(
            "{id}: the session already has a journal"
        )));
    }

    let (file, lock) = open_locked(&params.paths, id)?;

    Ok(OpenJournal {
        file: Arc::new(Mutex::new(file)),
        _lock: lock,
    })
}

/// The header and initial settings of a new session, rendered and newline
/// terminated so they can be written before the file is published.
fn header_lines(params: &JournalParams) -> Vec<Vec<u8>> {
    let created_at = now_seconds();
    let records = [
        JournalRecord::Header {
            session_id: params.id.as_str().to_owned(),
            project_root: params.project_root.display().to_string(),
            created_at,
            model: params.model.clone(),
            mode: params.mode,
        },
        JournalRecord::Settings {
            model: params.model.clone(),
            mode: params.mode,
            reasoning_effort: params.reasoning_effort,
        },
    ];

    records
        .into_iter()
        .map(|record| {
            let line = JournalLine {
                v: JOURNAL_VERSION,
                at: created_at,
                record,
            };
            let mut rendered = serde_json::to_vec(&line)
                .unwrap_or_else(|error| panic!("a session record must serialize: {error}"));

            rendered.push(b'\n');

            rendered
        })
        .collect()
}

fn new_session_id() -> SessionId {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let counter = NEXT_ID.fetch_add(1, Ordering::Relaxed);

    SessionId(format!(
        "{:x}-{:x}-{:x}",
        nanos,
        std::process::id(),
        counter
    ))
}

pub fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn lock_file(file: &Mutex<File>) -> MutexGuard<'_, File> {
    file.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn lock_file_slot(slot: &Mutex<Option<OpenJournal>>) -> MutexGuard<'_, Option<OpenJournal>> {
    slot.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Truncate only an invalid, unterminated trailing record. A valid record
/// without a final newline is retained and terminated.
///
/// Returns the journal bytes it read, truncated to the repair boundary when it
/// truncated the file, so the caller can load the session without a second
/// read of the same journal.
fn repair_trailing(file: &File, path: &Path) -> Result<Vec<u8>, SessionError> {
    let mut reader = file.try_clone()?;
    let mut bytes = read_journal_bounded(&mut reader, path)?;

    if bytes.is_empty() {
        return Err(SessionError::Empty);
    }

    if bytes.last() == Some(&b'\n') {
        return Ok(bytes);
    }

    let boundary = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|position| position + 1)
        .unwrap_or(0);
    let tail = &bytes[boundary..];

    if serde_json::from_slice::<JournalLine>(tail).is_ok() {
        let mut writer = OpenOptions::new().append(true).open(path)?;
        writer.write_all(b"\n")?;
        writer.sync_data()?;

        return Ok(bytes);
    }

    if boundary == 0 {
        return Err(SessionError::Corrupt {
            line: 1,
            message: "the session header is incomplete".to_owned(),
        });
    }

    let writer = OpenOptions::new().write(true).open(path)?;
    writer.set_len(boundary as u64)?;
    writer.sync_data()?;

    // The in-memory copy has to match the file the repair just produced.
    bytes.truncate(boundary);

    Ok(bytes)
}

fn load_from_path(path: &Path, current_root: Option<&Path>) -> Result<LoadedSession, SessionError> {
    load_from_bytes(
        &read_journal_bounded(File::open(path)?, path)?,
        current_root,
    )
}

/// Read a journal through the resume/listing memory bound.
fn read_journal_bounded(reader: impl Read, path: &Path) -> Result<Vec<u8>, SessionError> {
    read_bounded(reader, path, MAX_JOURNAL_BYTES)
}

fn read_bounded(mut reader: impl Read, path: &Path, limit: u64) -> Result<Vec<u8>, SessionError> {
    let mut bytes = Vec::new();

    reader
        .by_ref()
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;

    if bytes.len() as u64 > limit {
        return Err(SessionError::TooLarge {
            path: path.to_path_buf(),
            limit,
        });
    }

    Ok(bytes)
}

/// Parse journal bytes that are already in memory.
///
/// A resume reads the journal once, repairs it, and hands the same buffer here;
/// records are decoded straight into the conversation instead of being kept in
/// a second, parallel vector of parsed records.
fn load_from_bytes(
    bytes: &[u8],
    current_root: Option<&Path>,
) -> Result<LoadedSession, SessionError> {
    if bytes.is_empty() {
        return Err(SessionError::Empty);
    }

    let mut header: Option<(SessionId, PathBuf, u64)> = None;
    let mut messages: Vec<Message> = Vec::new();
    let mut message_times: Vec<u64> = Vec::new();
    let mut dispatches: BTreeMap<String, ToolCall> = BTreeMap::new();
    let mut results: BTreeMap<String, ToolResult> = BTreeMap::new();
    let mut projection = None;
    let mut model: Option<String> = None;
    let mut mode = Mode::Coding;
    let mut reasoning_effort = ReasoningEffort::Default;
    let mut queue: VecDeque<QueuedInput> = VecDeque::new();
    let mut queue_paused = false;
    let mut attachments: Vec<ImageInput> = Vec::new();
    let mut usage = Vec::new();
    let mut tasks: Vec<TaskRecord> = Vec::new();
    let mut records = 0u64;

    for (index, raw) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if raw.is_empty() {
            continue;
        }

        let line =
            serde_json::from_slice::<JournalLine>(raw).map_err(|error| SessionError::Corrupt {
                line: index + 1,
                message: error.to_string(),
            })?;

        if line.v != JOURNAL_VERSION {
            return Err(SessionError::UnsupportedVersion(line.v));
        }

        records += 1;

        if records == 1 {
            let JournalRecord::Header {
                session_id,
                project_root,
                created_at,
                model: header_model,
                mode: header_mode,
            } = line.record
            else {
                return Err(SessionError::Corrupt {
                    line: 1,
                    message: "the first record is not a session header".to_owned(),
                });
            };

            header = Some((
                SessionId::parse(&session_id)?,
                PathBuf::from(project_root),
                created_at,
            ));
            model = header_model;
            mode = header_mode;

            continue;
        }

        match line.record {
            JournalRecord::Message { message } => {
                messages.push(message);
                message_times.push(line.at);
            }
            JournalRecord::Settings {
                model: selected,
                mode: selected_mode,
                reasoning_effort: selected_effort,
            } => {
                if selected.is_some() {
                    model = selected;
                }

                mode = selected_mode;
                reasoning_effort = selected_effort;
            }
            JournalRecord::QueueAdded { id, text } => {
                if !queue.iter().any(|item| item.id == id) {
                    queue.push_back(QueuedInput {
                        id: id.clone(),
                        text,
                    });
                }
            }
            JournalRecord::QueueDelivered { id } => {
                queue.retain(|item| item.id != id);
            }
            JournalRecord::QueueCleared => queue.clear(),
            JournalRecord::QueuePaused { paused } => queue_paused = paused,
            JournalRecord::Dispatch { call } => {
                dispatches.insert(call.id.as_str().to_owned(), call);
            }
            JournalRecord::ToolResult { result } => {
                results
                    .entry(result.call_id.as_str().to_owned())
                    .or_insert_with(|| result);
            }
            JournalRecord::Compaction(record) => {
                projection = Some(Projection {
                    summary: record.summary,
                    covered_messages: record.covered_messages,
                });
            }
            JournalRecord::Attachment { attachment } => {
                if !attachments.iter().any(|item| item.path == attachment.path) {
                    attachments.push(attachment);
                }
            }
            JournalRecord::Header { .. } => {
                return Err(SessionError::Corrupt {
                    line: 1,
                    message: "a second session header was found".to_owned(),
                });
            }
            JournalRecord::ModelAttempt(_) => {}
            JournalRecord::Usage { record } => usage.push(record),
            // Tasks and child agents are rebuilt by the driver from the journal
            // separately; they never contribute conversation messages.
            JournalRecord::Task(record) => tasks.push(record),
            JournalRecord::AgentSpawned { .. } | JournalRecord::AgentFinished { .. } => {}
        }
    }

    let Some((id, stored_root, created_at)) = header else {
        return Err(SessionError::Empty);
    };

    let stored_canonical = std::fs::canonicalize(&stored_root).unwrap_or(stored_root);

    if let Some(current_root) = current_root {
        let current = std::fs::canonicalize(current_root)?;

        if stored_canonical != current {
            return Err(SessionError::RootMismatch {
                id: id.0.clone(),
                stored: stored_canonical.display().to_string(),
                current: current.display().to_string(),
            });
        }
    }

    let mut conversation = Conversation::new();
    let mut last_message_at = 0;

    for (message, at) in messages.into_iter().zip(message_times) {
        let calls = if message.role == Role::Assistant {
            message.tool_calls().cloned().collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        last_message_at = at;
        conversation.push(message);

        if !calls.is_empty() {
            let mut batch = Vec::with_capacity(calls.len());

            for call in calls {
                let result = results.get(call.id.as_str()).cloned();

                match result {
                    Some(result) => batch.push(result),
                    None if dispatches.contains_key(call.id.as_str()) => {
                        batch.push(ToolResult::new(
                            &call,
                            ToolStatus::Indeterminate,
                            "the journal records a durable dispatch intent but no result; \
                             the outcome is unknown and the call was not replayed",
                            false,
                        ));
                    }
                    None => batch.push(ToolResult::not_executed(
                        &call,
                        "not executed: no dispatch record was written before the process \
                         stopped",
                    )),
                }
            }

            conversation.push(Message::tool_results(batch));
        }
    }

    if let Some(projection) = &projection
        && projection.covered_messages > conversation.messages().len()
    {
        return Err(SessionError::Corrupt {
            line: 0,
            message: format!(
                "compaction projection covers {} messages, but only {} are present",
                projection.covered_messages,
                conversation.messages().len()
            ),
        });
    }

    if !queue.is_empty() {
        queue_paused = true;
    }

    // An attachment that already appears in a submitted message is history,
    // not a pending upload. Requeuing it would attach the image a second time.
    let submitted = conversation
        .messages()
        .iter()
        .flat_map(Message::images)
        .map(|image| image.path.clone())
        .collect::<std::collections::HashSet<_>>();

    attachments.retain(|attachment| !submitted.contains(&attachment.path));

    Ok(LoadedSession {
        header: SessionHeader {
            id,
            project_root: stored_canonical,
            created_at,
            model: model.clone(),
            mode,
        },
        conversation,
        projection,
        model,
        mode,
        reasoning_effort,
        queued: queue.into_iter().collect(),
        queue_paused,
        attachments,
        usage,
        tasks,
        last_message_at,
        records,
    })
}

/// Read a session without taking the writer lock, for listing and ledger
/// rebuilding. Unreadable or malformed journals are reported to the caller.
pub fn load_read_only(paths: &StatePaths, id: &SessionId) -> Result<LoadedSession, SessionError> {
    let path = paths.journal_path(id.as_str());

    if !path.exists() {
        return Err(SessionError::NotFound(id.0.clone()));
    }

    load_from_path(&path, None)
}

/// Bytes read from the front of a journal before the scan widens its window.
const SUMMARY_PREFIX_BYTES: u64 = 256 * 1024;

/// Bytes read from the end of a journal to find its newest message timestamp.
const SUMMARY_TAIL_BYTES: u64 = 64 * 1024;

/// Largest window either scan tries before it gives up.
const SUMMARY_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Longest session title kept before eliding.
const TITLE_MAX_CHARS: usize = 72;

/// Shown when a conversation holds no user prompt.
const NO_PROMPT_TITLE: &str = "(no prompt yet)";

/// A session list row: enough to identify a session without loading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionListing {
    pub id: SessionId,
    pub created_at: u64,
    /// Unix seconds of the newest message, falling back to the header time.
    pub last_message_at: u64,
    pub project_root: PathBuf,
    pub title: String,
}

/// Summaries for every session holding at least one user prompt, newest
/// message first.
///
/// The scan is best-effort and bounded so the TUI can refresh it cheaply: only
/// the ends of each journal are read, and a journal that is unreadable, corrupt,
/// or missing its header is skipped instead of failing the whole list. A session
/// without a prompt is omitted, because there is nothing to resume.
pub fn summaries(paths: &StatePaths) -> Vec<SessionListing> {
    let Ok(entries) = std::fs::read_dir(&paths.sessions) else {
        return Vec::new();
    };

    let mut listings = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let id = SessionId::parse(name.to_str()?).ok()?;

            summary_for(paths, &id)
        })
        .collect::<Vec<_>>();

    listings.sort_by(|left, right| {
        right
            .last_message_at
            .cmp(&left.last_message_at)
            .then_with(|| left.id.cmp(&right.id))
    });

    listings
}

/// The first prompt in `conversation`, normalized to one line and elided.
pub fn title_from_prompt(conversation: &Conversation) -> String {
    conversation
        .messages()
        .iter()
        .filter(|message| is_prompt(message))
        .find_map(|message| title_from_text(&message.text()))
        .unwrap_or_else(|| NO_PROMPT_TITLE.to_owned())
}

/// Whether `conversation` holds a prompt worth resuming.
pub fn has_prompt(conversation: &Conversation) -> bool {
    conversation.messages().iter().any(is_prompt)
}

/// Whether `message` is something the user typed.
///
/// A loaded skill is journaled as a user message, so its wrapper text does not
/// count: resuming a session holding only one would show no request.
pub(crate) fn is_prompt(message: &Message) -> bool {
    if message.role != Role::User {
        return false;
    }

    let text = message.text();

    !text.starts_with(SKILL_MESSAGE_PREFIX) && title_from_text(&text).is_some()
}

/// The opening of the journaled skill wrapper written by the skill tool.
const SKILL_MESSAGE_PREFIX: &str = "<skill name=\"";

/// The newest `message` record's timestamp, and a `message`-only shim so a
/// journal never has to be decoded in full to find it.
#[derive(Deserialize)]
struct TimestampShim {
    #[serde(default)]
    at: u64,
    kind: String,
}

fn summary_for(paths: &StatePaths, id: &SessionId) -> Option<SessionListing> {
    let path = paths.journal_path(id.as_str());
    let scan = scan_prefix(&path, id)?;
    let title = title_from_text(scan.prompt.as_deref()?)?;
    let stored_root = PathBuf::from(scan.project_root);
    let project_root = std::fs::canonicalize(&stored_root).unwrap_or(stored_root);

    Some(SessionListing {
        id: id.clone(),
        created_at: scan.created_at,
        last_message_at: scan
            .last_message_at
            .max(tail_message_at(&path).unwrap_or_default())
            .max(scan.created_at),
        project_root,
        title,
    })
}

/// What the front of a journal says about a session.
struct PrefixScan {
    created_at: u64,
    project_root: String,
    /// The first user prompt found, verbatim.
    prompt: Option<String>,
    /// Unix seconds of the newest message inside the scanned window.
    last_message_at: u64,
}

/// Read the header and the first user prompt of a journal.
///
/// The first pass reads a bounded prefix for speed. A prefix may hold only
/// assistant and tool records, so a journal without a prompt there widens the
/// window instead of concluding that it has none. `None` means the journal
/// cannot be summarized at all.
fn scan_prefix(path: &Path, id: &SessionId) -> Option<PrefixScan> {
    let length = std::fs::metadata(path).ok()?.len().max(1);
    let mut window = SUMMARY_PREFIX_BYTES;

    loop {
        if let Some((scan, complete)) = read_prefix(path, window, length, id)
            && (scan.prompt.is_some() || complete)
        {
            return Some(scan);
        }

        if window >= length || window >= SUMMARY_MAX_BYTES {
            return None;
        }

        window = (window.saturating_mul(4)).min(length);
    }
}

/// One windowed pass over the front of a journal.
///
/// The flag is false when the window stopped mid-record or before the end of
/// the file, which means the caller has to retry with a wider window.
fn read_prefix(
    path: &Path,
    window: u64,
    length: u64,
    id: &SessionId,
) -> Option<(PrefixScan, bool)> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file.take(window.min(length)));
    let mut line = Vec::new();
    let mut header: Option<(u64, String)> = None;
    let mut prompt = None;
    let mut last_message_at = 0;
    let mut complete = window >= length;

    loop {
        line.clear();

        let read = reader.read_until(b'\n', &mut line).ok()?;

        if read == 0 {
            break;
        }

        if !line.ends_with(b"\n") {
            // A record cut by the window boundary says nothing about the rest.
            complete = false;

            break;
        }

        observe_record(
            serde_json::from_slice::<JournalLine>(&line).ok()?,
            id,
            &mut header,
            &mut prompt,
            &mut last_message_at,
        )?;

        if prompt.is_some() {
            break;
        }
    }

    let (created_at, project_root) = header?;

    Some((
        PrefixScan {
            created_at,
            project_root,
            prompt,
            last_message_at,
        },
        complete,
    ))
}

fn observe_record(
    record: JournalLine,
    id: &SessionId,
    header: &mut Option<(u64, String)>,
    prompt: &mut Option<String>,
    last_message_at: &mut u64,
) -> Option<()> {
    match record.record {
        JournalRecord::Header {
            session_id,
            project_root,
            created_at,
            ..
        } => {
            if header.is_some() || session_id != id.as_str() {
                return None;
            }

            *header = Some((created_at, project_root));
        }
        JournalRecord::Message { message } => {
            header.as_ref()?;

            *last_message_at = (*last_message_at).max(record.at);

            if prompt.is_none() && is_prompt(&message) {
                *prompt = Some(message.text());
            }
        }
        _ => {}
    }

    Some(())
}

/// Unix seconds of the newest `message` record at the end of a journal.
///
/// Only the tail is read, and only the `at` and `kind` fields are decoded, so a
/// multi-megabyte journal is summarized without being parsed. Journals written
/// before records carried a timestamp fall back to the file's modification
/// time, which is the closest available estimate.
fn tail_message_at(path: &Path) -> Option<u64> {
    let mut window = SUMMARY_TAIL_BYTES;

    loop {
        if let Some(at) = read_tail(path, window) {
            return Some(at);
        }

        if window >= SUMMARY_MAX_BYTES {
            break;
        }

        window = window.saturating_mul(4);
    }

    let modified = std::fs::metadata(path).ok()?.modified().ok()?;

    Some(
        modified
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default(),
    )
}

fn read_tail(path: &Path, window: u64) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let start = length.saturating_sub(window);

    file.seek(SeekFrom::Start(start)).ok()?;

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;

    // A window that starts mid-file begins mid-record, so its first line is
    // dropped rather than parsed.
    let lines = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();

    for (position, raw) in lines.iter().enumerate().rev() {
        if start > 0 && position == 0 {
            break;
        }

        let Ok(shim) = serde_json::from_slice::<TimestampShim>(raw) else {
            continue;
        };

        if shim.kind == "message" && shim.at > 0 {
            return Some(shim.at);
        }
    }

    None
}

/// The first non-blank line of `text`, whitespace collapsed, elided to a title.
pub(crate) fn title_from_text(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title = collapsed.chars().take(TITLE_MAX_CHARS).collect::<String>();

    if collapsed.chars().count() > TITLE_MAX_CHARS {
        title.pop();
        title.push('…');
    }

    Some(title)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use t8ful_core::journal::AgentJournal;
    use t8ful_core::policy::Mode;
    use t8ful_core::provider::ReasoningEffort;
    use t8ful_core::tool::{ToolCall, ToolResult};
    use tempfile::TempDir;

    use super::{JournalLine, JournalRecord, SessionError, SessionId, SessionJournal};
    use crate::state::StatePaths;
    use crate::usage::UsageLedger;

    #[test]
    fn a_journal_read_is_bounded() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("journal.jsonl");
        let over = std::io::Cursor::new(b"0123456789".to_vec());
        let error = super::read_bounded(over, &path, 5).expect_err("over the limit");

        assert!(matches!(error, SessionError::TooLarge { .. }), "{error}");
        assert!(error.to_string().contains("larger than"), "{error}");

        let at_limit = std::io::Cursor::new(b"12345".to_vec());

        assert_eq!(
            super::read_bounded(at_limit, &path, 5).expect("at the limit"),
            b"12345"
        );
    }

    fn fixture() -> (TempDir, std::path::PathBuf, StatePaths, Arc<UsageLedger>) {
        let dir = TempDir::new().unwrap();
        let state = StatePaths {
            root: dir.path().join("state"),
            sessions: dir.path().join("state/sessions"),
            usage: dir.path().join("state/usage"),
        };
        // The state root and the project root must be separate.
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let ledger = Arc::new(UsageLedger::open(&state));

        (dir, project, state, ledger)
    }

    #[tokio::test]
    async fn a_new_session_writes_a_private_header_and_reloads() {
        let (_dir, project, state, ledger) = fixture();
        let journal = SessionJournal::create(
            &state,
            &project,
            Some("vendor/model".to_owned()),
            Mode::Plan,
            ReasoningEffort::Default,
            Arc::clone(&ledger),
        )
        .await
        .unwrap();
        let id = journal.id().clone();

        journal.record_user_message("hello").await.unwrap();
        drop(journal);

        let (loaded_journal, loaded) = SessionJournal::open(&state, &id, &project, ledger)
            .await
            .unwrap();

        assert_eq!(loaded.mode, Mode::Plan);
        assert_eq!(loaded.model.as_deref(), Some("vendor/model"));
        assert_eq!(loaded.conversation.messages().len(), 1);
        assert_eq!(loaded.conversation.messages()[0].text(), "hello");
        assert_eq!(loaded.records, 3);

        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(state.journal_path(id.as_str()))
                .unwrap()
                .permissions(),
        );

        assert_eq!(mode & 0o777, 0o600);
        drop(loaded_journal);
    }

    #[tokio::test]
    async fn a_pending_session_writes_nothing_until_its_first_record() {
        let (_dir, project, state, ledger) = fixture();
        let journal = SessionJournal::pending(
            &state,
            &project,
            Some("vendor/model".to_owned()),
            Mode::Coding,
            ReasoningEffort::Default,
            Some(Arc::clone(&ledger)),
        )
        .unwrap();
        let id = journal.id().clone();

        assert!(
            !state.session_dir(id.as_str()).exists(),
            "allocating a session must not touch the state directory"
        );
        assert!(!state.journal_path(id.as_str()).exists());

        journal.record_user_message("first").await.unwrap();

        assert!(state.journal_path(id.as_str()).exists());
        assert_eq!(journal.id(), &id, "the id never changes once handed out");

        drop(journal);

        let (journal, loaded) = SessionJournal::open(&state, &id, &project, ledger)
            .await
            .unwrap();

        assert_eq!(loaded.conversation.messages().len(), 1);
        assert_eq!(loaded.conversation.messages()[0].text(), "first");
        assert_eq!(loaded.records, 3, "header, settings, and the prompt");
        assert!(loaded.last_message_at > 0, "records are timestamped");
        drop(journal);
    }

    #[tokio::test]
    async fn every_record_is_stamped_and_the_newest_message_wins() {
        let (_dir, project, state, ledger) = fixture();
        let journal = SessionJournal::create(
            &state,
            &project,
            None,
            Mode::Coding,
            ReasoningEffort::Default,
            Arc::clone(&ledger),
        )
        .await
        .unwrap();
        let id = journal.id().clone();

        journal.record_user_message("hello").await.unwrap();

        let path = state.journal_path(id.as_str());
        let first = std::fs::read_to_string(&path).unwrap();

        assert_eq!(first.lines().count(), 3, "{first}");
        assert!(
            first
                .lines()
                .all(|line| serde_json::from_str::<serde_json::Value>(line)
                    .unwrap()
                    .get("at")
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|at| at > 0)),
            "{first}"
        );

        // A later message must be stamped with at least the same second; a
        // pinned clock is impossible here, so only the ordering holds.
        journal.record_user_message("again").await.unwrap();
        drop(journal);

        let contents = std::fs::read_to_string(&path).unwrap();
        let newest = contents
            .lines()
            .filter_map(|line| serde_json::from_str::<JournalLine>(line).ok())
            .filter(|line| matches!(line.record, JournalRecord::Message { .. }))
            .map(|line| line.at)
            .next_back()
            .expect("a message record");
        let loaded = super::load_read_only(&state, &id).unwrap();

        assert_eq!(loaded.last_message_at, newest);
        assert_eq!(loaded.conversation.messages().len(), 2);
        assert!(newest > 0);
    }

    #[tokio::test]
    async fn reasoning_effort_round_trips_through_settings() {
        let (_dir, project, state, ledger) = fixture();
        let journal = SessionJournal::create(
            &state,
            &project,
            Some("vendor/model".to_owned()),
            Mode::Coding,
            ReasoningEffort::High,
            Arc::clone(&ledger),
        )
        .await
        .unwrap();
        let id = journal.id().clone();

        drop(journal);

        let (journal, loaded) = SessionJournal::open(&state, &id, &project, Arc::clone(&ledger))
            .await
            .unwrap();

        assert_eq!(loaded.reasoning_effort, ReasoningEffort::High);

        journal
            .record_settings(None, Mode::Plan, ReasoningEffort::Low)
            .await
            .unwrap();
        drop(journal);

        let (_, loaded) = SessionJournal::open(&state, &id, &project, Arc::clone(&ledger))
            .await
            .unwrap();

        assert_eq!(loaded.reasoning_effort, ReasoningEffort::Low);
    }

    #[tokio::test]
    async fn an_old_settings_record_without_an_effort_reads_as_default() {
        let (_dir, project, state, ledger) = fixture();
        let journal = SessionJournal::create(
            &state,
            &project,
            None,
            Mode::Coding,
            ReasoningEffort::High,
            Arc::clone(&ledger),
        )
        .await
        .unwrap();
        let id = journal.id().clone();
        let path = state.journal_path(id.as_str());

        drop(journal);

        use std::io::Write;

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();

        file.write_all(b"{\"v\":1,\"kind\":\"settings\",\"model\":null,\"mode\":\"coding\"}\n")
            .unwrap();
        drop(file);

        let (_, loaded) = SessionJournal::open(&state, &id, &project, ledger)
            .await
            .unwrap();

        assert_eq!(loaded.reasoning_effort, ReasoningEffort::Default);
    }

    #[tokio::test]
    async fn a_second_writer_is_refused() {
        let (_dir, project, state, ledger) = fixture();
        let journal = SessionJournal::create(
            &state,
            &project,
            None,
            Mode::Coding,
            ReasoningEffort::Default,
            Arc::clone(&ledger),
        )
        .await
        .unwrap();
        let id = journal.id().clone();

        let error = SessionJournal::open(&state, &id, &project, ledger)
            .await
            .expect_err("second writer");

        assert!(matches!(error, SessionError::InUse(_)), "{error}");
        drop(journal);
    }

    #[tokio::test]
    async fn an_incomplete_trailing_record_is_removed_but_earlier_corruption_is_rejected() {
        let (_dir, project, state, ledger) = fixture();
        let journal = SessionJournal::create(
            &state,
            &project,
            None,
            Mode::Coding,
            ReasoningEffort::Default,
            Arc::clone(&ledger),
        )
        .await
        .unwrap();
        let id = journal.id().clone();
        let path = state.journal_path(id.as_str());

        journal.record_user_message("kept").await.unwrap();
        drop(journal);

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        file.write_all(b"{\"kind\":\"message\",\"message\":")
            .unwrap();
        drop(file);

        let (journal, loaded) = SessionJournal::open(&state, &id, &project, ledger.clone())
            .await
            .unwrap();
        assert_eq!(loaded.conversation.messages().len(), 1);
        drop(journal);

        let mut contents = std::fs::read_to_string(&path).unwrap();
        contents.insert_str(contents.find('\n').unwrap() + 1, "not-json\n");
        std::fs::write(&path, contents).unwrap();

        let error = SessionJournal::open(&state, &id, &project, ledger)
            .await
            .expect_err("corruption");

        assert!(matches!(error, SessionError::Corrupt { .. }), "{error}");
    }

    #[tokio::test]
    async fn resume_reconstructs_indeterminate_and_not_executed_calls() {
        let (_dir, project, state, ledger) = fixture();
        let journal = SessionJournal::create(
            &state,
            &project,
            None,
            Mode::Coding,
            ReasoningEffort::Default,
            Arc::clone(&ledger),
        )
        .await
        .unwrap();
        let id = journal.id().clone();
        let first = ToolCall::new("call-1", "write_file", serde_json::json!({}));
        let second = ToolCall::new("call-2", "read_file", serde_json::json!({}));
        let assistant = t8ful_core::message::Message::assistant(vec![
            t8ful_core::content::Content::ToolCall(first.clone()),
            t8ful_core::content::Content::ToolCall(second.clone()),
        ]);

        journal.record_message(&assistant).await.unwrap();
        journal.record_dispatch(&first).await.unwrap();
        journal
            .record_tool_result(&ToolResult::new(
                &first,
                t8ful_core::tool::ToolStatus::Ok,
                "done",
                false,
            ))
            .await
            .unwrap();
        drop(journal);

        let (journal, loaded) = SessionJournal::open(&state, &id, &project, ledger)
            .await
            .unwrap();
        let messages = loaded.conversation.messages();

        assert_eq!(messages.len(), 2);
        let results = messages[1].results().collect::<Vec<_>>();

        assert_eq!(results[0].status, t8ful_core::tool::ToolStatus::Ok);
        assert_eq!(results[1].status, t8ful_core::tool::ToolStatus::NotExecuted);
        drop(journal);
    }

    #[tokio::test]
    async fn a_different_project_root_is_rejected() {
        let (dir, project, state, ledger) = fixture();
        let journal = SessionJournal::create(
            &state,
            &project,
            None,
            Mode::Coding,
            ReasoningEffort::Default,
            Arc::clone(&ledger),
        )
        .await
        .unwrap();
        let id = journal.id().clone();
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        drop(journal);

        let error = SessionJournal::open(&state, &id, &other, ledger)
            .await
            .expect_err("root mismatch");

        assert!(
            matches!(error, SessionError::RootMismatch { .. }),
            "{error}"
        );
    }

    #[test]
    fn ids_reject_path_separators() {
        assert!(SessionId::parse("2025-abc_1").is_ok());
        assert!(SessionId::parse("../escape").is_err());
        assert!(SessionId::parse("a/b").is_err());
        assert!(SessionId::parse("").is_err());
    }

    #[test]
    fn every_record_is_versioned() {
        let record = JournalRecord::QueueCleared;
        let line = super::JournalLine {
            v: 1,
            at: 1,
            record,
        };
        let rendered = serde_json::to_string(&line).unwrap();

        assert!(rendered.contains("\"v\":1"));
    }
}

#[cfg(test)]
mod extra_tests {
    use std::sync::Arc;

    use crate::session::JournalRecord;

    use t8ful_core::journal::AgentJournal;
    use t8ful_core::policy::Mode;
    use t8ful_core::provider::ReasoningEffort;
    use tempfile::TempDir;

    use super::SessionJournal;
    use crate::state::StatePaths;
    use crate::usage::UsageLedger;

    fn fixture() -> (TempDir, std::path::PathBuf, StatePaths, Arc<UsageLedger>) {
        let dir = TempDir::new().unwrap();
        let state = StatePaths {
            root: dir.path().join("state"),
            sessions: dir.path().join("state/sessions"),
            usage: dir.path().join("state/usage"),
        };
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let ledger = Arc::new(UsageLedger::open(&state));

        (dir, project, state, ledger)
    }

    #[tokio::test]
    async fn a_valid_unterminated_record_is_retained() {
        let (_dir, project, state, ledger) = fixture();
        let journal = SessionJournal::create(
            &state,
            &project,
            None,
            Mode::Coding,
            ReasoningEffort::Default,
            Arc::clone(&ledger),
        )
        .await
        .unwrap();
        let id = journal.id().clone();
        let path = state.journal_path(id.as_str());

        journal.record_user_message("kept").await.unwrap();
        drop(journal);

        let contents = std::fs::read(&path).unwrap();
        let without_newline = contents.strip_suffix(b"\n").unwrap_or(&contents).to_vec();

        std::fs::write(&path, without_newline).unwrap();

        let (journal, loaded) = SessionJournal::open(&state, &id, &project, ledger)
            .await
            .unwrap();

        assert_eq!(loaded.conversation.messages().len(), 1);
        assert!(std::fs::read(&path).unwrap().ends_with(b"\n"));
        drop(journal);
    }

    #[tokio::test]
    async fn loading_a_repair_buffer_matches_loading_the_file() {
        let (_dir, project, state, ledger) = fixture();
        let journal = SessionJournal::create(
            &state,
            &project,
            Some("vendor/model".to_owned()),
            Mode::Coding,
            ReasoningEffort::Default,
            Arc::clone(&ledger),
        )
        .await
        .unwrap();
        let id = journal.id().clone();
        let path = state.journal_path(id.as_str());

        journal.record_user_message("first").await.unwrap();
        journal
            .record_settings(
                Some("vendor/other".to_owned()),
                Mode::Plan,
                ReasoningEffort::High,
            )
            .await
            .unwrap();
        journal.record_user_message("second").await.unwrap();
        journal
            .record_attachment(t8ful_core::content::ImageInput::new(
                "image/png",
                std::path::PathBuf::from("/tmp/image.png"),
                4,
                3,
                100,
            ))
            .await
            .unwrap();
        journal
            .queue_add("queue-1".to_owned(), "later".to_owned())
            .await
            .unwrap();
        drop(journal);

        let bytes = std::fs::read(&path).unwrap();
        let loaded_bytes = super::load_from_bytes(&bytes, Some(&project)).unwrap();
        let loaded_path = super::load_from_path(&path, Some(&project)).unwrap();

        assert_eq!(loaded_bytes, loaded_path);
        assert_eq!(loaded_bytes.records, 7);
        assert_eq!(loaded_bytes.conversation.messages().len(), 2);
        assert_eq!(loaded_bytes.model.as_deref(), Some("vendor/other"));
        assert_eq!(loaded_bytes.mode, Mode::Plan);
        assert_eq!(loaded_bytes.reasoning_effort, ReasoningEffort::High);
        assert_eq!(loaded_bytes.queued.len(), 1);
        assert!(loaded_bytes.queue_paused);
        assert_eq!(loaded_bytes.attachments.len(), 1);
    }

    #[tokio::test]
    async fn a_submitted_attachment_is_not_restored_as_pending() {
        let (_dir, project, state, ledger) = fixture();
        let journal = SessionJournal::create(
            &state,
            &project,
            None,
            Mode::Coding,
            ReasoningEffort::Default,
            Arc::clone(&ledger),
        )
        .await
        .unwrap();
        let image = t8ful_core::content::ImageInput::new(
            "image/png",
            std::path::PathBuf::from("/tmp/submitted.png"),
            4,
            3,
            100,
        );
        let pending = t8ful_core::content::ImageInput::new(
            "image/png",
            std::path::PathBuf::from("/tmp/pending.png"),
            2,
            2,
            80,
        );

        journal.record_attachment(image.clone()).await.unwrap();
        journal.record_attachment(pending.clone()).await.unwrap();
        journal
            .append(JournalRecord::Message {
                message: t8ful_core::message::Message::user_with_images(
                    Some("look".to_owned()),
                    vec![image],
                ),
            })
            .await
            .unwrap();
        let id = journal.id().clone();
        let path = state.journal_path(id.as_str());

        drop(journal);

        let loaded = super::load_from_path(&path, Some(&project)).unwrap();

        assert_eq!(loaded.attachments, vec![pending]);
    }

    #[tokio::test]
    async fn compaction_and_queue_state_are_restored() {
        let (_dir, project, state, ledger) = fixture();
        let journal = SessionJournal::create(
            &state,
            &project,
            None,
            Mode::Plan,
            ReasoningEffort::Default,
            Arc::clone(&ledger),
        )
        .await
        .unwrap();
        let id = journal.id().clone();

        journal.record_user_message("older").await.unwrap();
        journal.record_user_message("newer").await.unwrap();
        journal
            .record_compaction(&t8ful_core::CompactionRecord {
                summary: "older summary".to_owned(),
                covered_messages: 1,
                model: "vendor/model".to_owned(),
                chunks: 1,
            })
            .await
            .unwrap();
        journal
            .queue_add("queue-1".to_owned(), "follow up".to_owned())
            .await
            .unwrap();
        drop(journal);

        let (journal, loaded) = SessionJournal::open(&state, &id, &project, ledger)
            .await
            .unwrap();

        assert_eq!(loaded.mode, Mode::Plan);
        assert_eq!(loaded.projection.unwrap().summary, "older summary");
        assert_eq!(loaded.queued.len(), 1);
        assert_eq!(loaded.queued[0].text, "follow up");
        assert!(loaded.queue_paused);
        drop(journal);
    }
}

#[cfg(test)]
mod summary_tests {
    use std::time::{Duration, SystemTime};

    use t8ful_core::message::{Conversation, Message};
    use t8ful_core::policy::Mode;
    use tempfile::TempDir;

    use super::{
        JournalLine, JournalRecord, NO_PROMPT_TITLE, SessionId, TITLE_MAX_CHARS, has_prompt,
        summaries, title_from_prompt,
    };
    use crate::state::StatePaths;

    fn paths(dir: &TempDir) -> StatePaths {
        StatePaths {
            root: dir.path().join("state"),
            sessions: dir.path().join("state/sessions"),
            usage: dir.path().join("state/usage"),
        }
    }

    fn project(dir: &TempDir) -> std::path::PathBuf {
        let project = dir.path().join("project");

        std::fs::create_dir_all(&project).unwrap();

        project
    }

    fn header(id: &str, project_root: &str, created_at: u64) -> JournalRecord {
        JournalRecord::Header {
            session_id: id.to_owned(),
            project_root: project_root.to_owned(),
            created_at,
            model: None,
            mode: Mode::Coding,
        }
    }

    fn user(text: &str) -> JournalRecord {
        JournalRecord::Message {
            message: Message::user_text(text),
        }
    }

    fn assistant(text: &str) -> JournalRecord {
        JournalRecord::Message {
            message: Message::assistant(vec![t8ful_core::content::Content::text(text)]),
        }
    }

    /// Write a journal whose records carry the given timestamps.
    fn write_journal_at(paths: &StatePaths, id: &str, records: Vec<(u64, JournalRecord)>) {
        let dir = paths.sessions.join(id);

        std::fs::create_dir_all(&dir).unwrap();

        let body = records
            .into_iter()
            .map(|(at, record)| serde_json::to_string(&JournalLine { v: 1, at, record }).unwrap())
            .collect::<Vec<_>>()
            .join("\n");

        std::fs::write(dir.join("journal.jsonl"), format!("{body}\n")).unwrap();
    }

    /// Write a journal whose records all share the header's timestamp.
    fn write_journal(paths: &StatePaths, id: &str, records: Vec<JournalRecord>) {
        let at = records
            .iter()
            .find_map(|record| match record {
                JournalRecord::Header { created_at, .. } => Some(*created_at),
                _ => None,
            })
            .unwrap_or_default();

        write_journal_at(
            paths,
            id,
            records.into_iter().map(|record| (at, record)).collect(),
        );
    }

    fn listing<'a>(listings: &'a [super::SessionListing], id: &str) -> &'a super::SessionListing {
        listings
            .iter()
            .find(|listing| listing.id.as_str() == id)
            .unwrap_or_else(|| panic!("{id} was not listed: {listings:?}"))
    }

    #[test]
    fn the_first_prompt_becomes_the_title() {
        let dir = TempDir::new().unwrap();
        let paths = paths(&dir);
        let project = project(&dir);

        write_journal(
            &paths,
            "session-a",
            vec![
                header("session-a", project.to_str().unwrap(), 10),
                assistant("thinking"),
                user("add a fuzzy picker\nand keep it small"),
            ],
        );

        let listings = summaries(&paths);

        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].id, SessionId::parse("session-a").unwrap());
        assert_eq!(listings[0].created_at, 10);
        assert_eq!(listings[0].title, "add a fuzzy picker");
        assert_eq!(
            listings[0].project_root,
            std::fs::canonicalize(&project).unwrap()
        );
    }

    #[test]
    fn titles_collapse_whitespace_and_elide_long_prompts() {
        let dir = TempDir::new().unwrap();
        let paths = paths(&dir);
        let project = project(&dir);
        let long = "word ".repeat(40);

        write_journal(
            &paths,
            "session-spaced",
            vec![
                header("session-spaced", project.to_str().unwrap(), 1),
                user("\n\n  fix   the\tspacing \nsecond line"),
            ],
        );
        write_journal(
            &paths,
            "session-long",
            vec![
                header("session-long", project.to_str().unwrap(), 2),
                user(&long),
            ],
        );

        let listings = summaries(&paths);

        assert_eq!(
            listing(&listings, "session-spaced").title,
            "fix the spacing"
        );

        let title = listing(&listings, "session-long").title.clone();

        assert_eq!(title.chars().count(), TITLE_MAX_CHARS);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn a_journal_without_a_prompt_is_not_listed() {
        let dir = TempDir::new().unwrap();
        let paths = paths(&dir);
        let project = project(&dir);

        write_journal(
            &paths,
            "session-assistant-only",
            vec![
                header("session-assistant-only", project.to_str().unwrap(), 3),
                assistant("I have nothing to do yet"),
            ],
        );
        write_journal(
            &paths,
            "session-header-only",
            vec![header("session-header-only", project.to_str().unwrap(), 4)],
        );

        assert!(summaries(&paths).is_empty());
    }

    #[test]
    fn a_journal_holding_only_a_skill_message_is_not_listed() {
        let dir = TempDir::new().unwrap();
        let paths = paths(&dir);
        let project = project(&dir);

        write_journal(
            &paths,
            "session-skill",
            vec![
                header("session-skill", project.to_str().unwrap(), 5),
                user("<skill name=\"review\" root=\"/skills/review\">\nbody\n</skill>"),
            ],
        );

        assert!(summaries(&paths).is_empty());
    }

    #[test]
    fn a_prompt_beyond_the_first_window_is_still_found() {
        let dir = TempDir::new().unwrap();
        let paths = paths(&dir);
        let project = project(&dir);
        let filler = "x".repeat(300 * 1024);

        write_journal(
            &paths,
            "session-quiet",
            vec![
                header("session-quiet", project.to_str().unwrap(), 6),
                assistant(&filler),
                user("far into the journal"),
            ],
        );

        let listings = summaries(&paths);

        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].title, "far into the journal");
    }

    #[test]
    fn corrupt_and_headerless_journals_are_skipped() {
        let dir = TempDir::new().unwrap();
        let paths = paths(&dir);
        let project = project(&dir);

        write_journal(
            &paths,
            "session-good",
            vec![
                header("session-good", project.to_str().unwrap(), 7),
                user("kept"),
            ],
        );

        for id in ["session-garbage", "session-headerless"] {
            std::fs::create_dir_all(paths.sessions.join(id)).unwrap();
        }

        std::fs::write(
            paths.sessions.join("session-garbage/journal.jsonl"),
            b"{not json}\n",
        )
        .unwrap();
        write_journal(&paths, "session-headerless", vec![user("no header")]);

        let listings = summaries(&paths);

        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].id.as_str(), "session-good");
    }

    #[test]
    fn listings_are_newest_message_first_and_ties_break_by_id() {
        let dir = TempDir::new().unwrap();
        let paths = paths(&dir);
        let project = project(&dir);
        let root = project.to_str().unwrap().to_owned();

        // `session-old` started first but was resumed last, so it leads.
        write_journal_at(
            &paths,
            "session-old",
            vec![
                (10, header("session-old", &root, 10)),
                (20, user("first")),
                (90, assistant("later")),
            ],
        );
        write_journal_at(
            &paths,
            "session-new",
            vec![(30, header("session-new", &root, 30)), (30, user("new"))],
        );
        write_journal_at(
            &paths,
            "session-b",
            vec![(20, header("session-b", &root, 20)), (20, user("b"))],
        );
        write_journal_at(
            &paths,
            "session-a",
            vec![(20, header("session-a", &root, 20)), (20, user("a"))],
        );

        let listings = summaries(&paths);

        assert_eq!(
            listings
                .iter()
                .map(|listing| listing.id.to_string())
                .collect::<Vec<_>>(),
            vec!["session-old", "session-new", "session-a", "session-b"]
        );
        assert_eq!(listing(&listings, "session-old").created_at, 10);
        assert_eq!(listing(&listings, "session-old").last_message_at, 90);
    }

    #[test]
    fn the_newest_message_is_read_from_beyond_one_tail_window() {
        let dir = TempDir::new().unwrap();
        let paths = paths(&dir);
        let project = project(&dir);
        let root = project.to_str().unwrap().to_owned();
        let filler = "x".repeat(300 * 1024);

        // The last record is larger than the tail window, so only a wider
        // window can see the timestamp it carries.
        write_journal_at(
            &paths,
            "session-big",
            vec![
                (5, header("session-big", &root, 5)),
                (6, user("start")),
                (7, assistant(&filler)),
            ],
        );

        let listings = summaries(&paths);

        assert_eq!(listing(&listings, "session-big").last_message_at, 7);
    }

    #[test]
    fn a_journal_without_record_timestamps_falls_back_to_its_modification_time() {
        let dir = TempDir::new().unwrap();
        let paths = paths(&dir);
        let project = project(&dir);
        let root = project.to_str().unwrap().to_owned();
        let directory = paths.sessions.join("session-legacy");

        std::fs::create_dir_all(&directory).unwrap();

        // An `at`-less journal is what a build before the field wrote.
        let body = [
            header("session-legacy", &root, 11),
            user("from before the field"),
        ]
        .into_iter()
        .map(|record| {
            let mut value = serde_json::to_value(&JournalLine {
                v: 1,
                at: 0,
                record,
            })
            .unwrap();

            value.as_object_mut().unwrap().remove("at");

            serde_json::to_string(&value).unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n");
        let journal = directory.join("journal.jsonl");

        std::fs::write(&journal, format!("{body}\n")).unwrap();

        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(4_242);
        std::fs::File::options()
            .write(true)
            .open(&journal)
            .unwrap()
            .set_modified(modified)
            .unwrap();

        let listings = summaries(&paths);

        assert_eq!(listing(&listings, "session-legacy").last_message_at, 4_242);
    }

    #[test]
    fn a_loaded_conversation_yields_the_same_title() {
        let mut conversation = Conversation::new();

        conversation.push(Message::system_text("system"));
        conversation.push(Message::user_text("  first\nsecond  "));

        assert_eq!(title_from_prompt(&conversation), "first");
        assert!(has_prompt(&conversation));
        assert_eq!(title_from_prompt(&Conversation::new()), NO_PROMPT_TITLE);
        assert!(!has_prompt(&Conversation::new()));
    }

    #[test]
    fn a_skill_message_alone_is_not_a_prompt() {
        let mut conversation = Conversation::new();

        conversation.push(Message::user_text(
            "<skill name=\"review\" root=\"/skills/review\">\nbody\n</skill>",
        ));

        assert!(!has_prompt(&conversation));

        conversation.push(Message::user_text("now review it"));

        assert!(has_prompt(&conversation));
        assert_eq!(title_from_prompt(&conversation), "now review it");
    }
}
