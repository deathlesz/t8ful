//! Per-agent journals.
//!
//! A child agent gets its own append-only file under
//! `sessions/<session>/agents/<agent>/journal.jsonl`. It uses the same record
//! schema as the root journal, so the same reader can inspect it, but the two
//! never interleave: a child's message, dispatch, result, and usage records land
//! in its own file, and the root journal keeps one `AgentSpawned` /
//! `AgentFinished` pair plus the notice it delivered.
//!
//! Usage records still reach the shared ledger, so cost is attributed per agent
//! without a screen of the child's conversation passing through the parent.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use t8ful_core::journal::{
    AgentJournal, CompactionRecord, JournalError, ModelAttempt, UsageRecord,
};
use t8ful_core::message::Message;
use t8ful_core::tool::{ToolCall, ToolResult};
use t8ful_core::usage::Usage;

use crate::state::{ensure_private_dir, ensure_private_file};
use crate::usage::{Decimal, UsageLedger};

/// Version of the child journal format. Shares the root journal's schema, so the
/// same number means the same shape.
const JOURNAL_VERSION: u64 = crate::session::JOURNAL_VERSION;

/// One journal line: the envelope plus the record under its own key.
///
/// The record is nested rather than flattened because a flattened usage record
/// carries its own `kind` field, which would silently overwrite the envelope's
/// and make the line ambiguous to a reader.
#[derive(serde::Serialize)]
struct Line<'a, T: serde::Serialize> {
    v: u64,
    at: u64,
    kind: &'static str,
    record: &'a T,
}

/// The journal one child agent writes to.
pub struct ChildJournal {
    path: PathBuf,
    file: Mutex<Option<File>>,
    agent: String,
    session: String,
    ledger: Option<std::sync::Arc<UsageLedger>>,
    /// Total of every usage record this child wrote, for its final report.
    usage: std::sync::Arc<std::sync::Mutex<Usage>>,
}

impl std::fmt::Debug for ChildJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildJournal")
            .field("agent", &self.agent)
            .field("session", &self.session)
            .field("path", &self.path)
            .finish()
    }
}

impl ChildJournal {
    /// The journal path for one agent. Created on the first record.
    pub fn path_for(session_dir: &Path, agent: &str) -> PathBuf {
        session_dir.join("agents").join(agent).join("journal.jsonl")
    }

    pub fn new(
        session_dir: &Path,
        session: &str,
        agent: &str,
        ledger: Option<std::sync::Arc<UsageLedger>>,
    ) -> Self {
        Self {
            path: Self::path_for(session_dir, agent),
            file: Mutex::new(None),
            agent: agent.to_owned(),
            session: session.to_owned(),
            ledger,
            usage: std::sync::Arc::new(std::sync::Mutex::new(Usage::default())),
        }
    }

    /// The running total of this child's usage.
    pub fn usage(&self) -> std::sync::Arc<std::sync::Mutex<Usage>> {
        std::sync::Arc::clone(&self.usage)
    }

    /// The file this journal appends to.
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn open(&self) -> Result<std::sync::MutexGuard<'_, Option<File>>, JournalError> {
        let mut guard = self
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());

        if guard.is_none() {
            ensure_private_file(&self.path).map_err(|error| {
                JournalError::new(format!("cannot create {}: {error}", self.path.display()))
            })?;

            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .map_err(|error| {
                    JournalError::new(format!("cannot open {}: {error}", self.path.display()))
                })?;

            *guard = Some(file);
        }

        Ok(guard)
    }

    fn write<T: serde::Serialize>(
        &self,
        kind: &'static str,
        record: &T,
    ) -> Result<(), JournalError> {
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        let mut rendered = serde_json::to_vec(&Line {
            v: JOURNAL_VERSION,
            at,
            kind,
            record,
        })
        .map_err(|error| JournalError::new(format!("cannot serialize a record: {error}")))?;

        rendered.push(b'\n');

        let mut guard = self.open()?;
        let file = guard.as_mut().expect("opened above");

        file.write_all(&rendered)
            .map_err(|error| JournalError::new(format!("append failed: {error}")))?;
        file.flush()
            .map_err(|error| JournalError::new(format!("flush failed: {error}")))?;

        Ok(())
    }

    /// Create the directory and file at startup so a failed child still leaves a
    /// trace saying it was started.
    pub fn materialize(&self) -> Result<(), JournalError> {
        if let Some(parent) = self.path.parent() {
            ensure_private_dir(parent).map_err(|error| {
                JournalError::new(format!("cannot create {}: {error}", parent.display()))
            })?;
        }

        self.open().map(|_| ())
    }
}

#[async_trait]
impl AgentJournal for ChildJournal {
    async fn record_message(&self, message: &Message) -> Result<(), JournalError> {
        #[derive(serde::Serialize)]
        struct Record<'a> {
            message: &'a Message,
        }

        self.write("message", &Record { message })
    }

    async fn record_model_attempt(&self, attempt: &ModelAttempt) -> Result<(), JournalError> {
        self.write("model_attempt", attempt)
    }

    async fn record_dispatch(&self, call: &ToolCall) -> Result<(), JournalError> {
        #[derive(serde::Serialize)]
        struct Record<'a> {
            call: &'a ToolCall,
        }

        self.write("dispatch", &Record { call })
    }

    async fn record_tool_result(&self, result: &ToolResult) -> Result<(), JournalError> {
        #[derive(serde::Serialize)]
        struct Record<'a> {
            result: &'a ToolResult,
        }

        self.write("tool_result", &Record { result })
    }

    async fn record_usage(&self, usage: &UsageRecord) -> Result<(), JournalError> {
        {
            let mut total = self
                .usage
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());

            add_usage(&mut total, &usage.usage);
        }

        self.write("usage", usage)?;

        // The ledger is shared with the root session so per-agent cost is
        // attributed in one place. A ledger failure must not stop the child: the
        // journal still has the record, and the ledger is rebuildable from it.
        if let Some(ledger) = &self.ledger {
            match crate::session::SessionId::from_generated(&self.session) {
                Ok(id) => {
                    if let Err(error) = ledger.append(&id, usage).await {
                        tracing::warn!(
                            target: "t8ful::usage",
                            agent = %self.agent,
                            "could not append the usage ledger for a child agent: {error}"
                        );
                    }
                }
                Err(error) => tracing::warn!(
                    target: "t8ful::usage",
                    agent = %self.agent,
                    "session id is not usable as a ledger key: {error}"
                ),
            }
        }

        Ok(())
    }

    async fn record_compaction(&self, projection: &CompactionRecord) -> Result<(), JournalError> {
        self.write("compaction", projection)
    }
}

/// Sum one provider-reported usage into a running total.
fn add_usage(total: &mut Usage, usage: &Usage) {
    total.input_tokens = add_optional(total.input_tokens, usage.input_tokens);
    total.output_tokens = add_optional(total.output_tokens, usage.output_tokens);
    total.cache_read_tokens = add_optional(total.cache_read_tokens, usage.cache_read_tokens);
    total.cache_write_tokens = add_optional(total.cache_write_tokens, usage.cache_write_tokens);
    total.reasoning_tokens = add_optional(total.reasoning_tokens, usage.reasoning_tokens);
    total.cost_usd = match (total.cost_usd.take(), usage.cost_usd.as_deref()) {
        (Some(left), Some(right)) => match (Decimal::parse(&left), Decimal::parse(right)) {
            (Some(left), Some(right)) => Some(left.add(right).render()),
            _ => Some(left),
        },
        (left, right) => left.or_else(|| right.map(str::to_owned)),
    };
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

/// Read every usage record from this session's child journals.
///
/// One malformed line is skipped rather than rejecting the session: a child
/// journal is append-only and a torn write must not lose the rest of the cost
/// ledger. Directories and lines are visited in a stable order.
pub fn load_usage(session_dir: &Path) -> Vec<UsageRecord> {
    let agents = session_dir.join("agents");
    let Ok(entries) = std::fs::read_dir(&agents) else {
        return Vec::new();
    };
    let mut directories = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();

    directories.sort();

    let mut records = Vec::new();

    for directory in directories {
        let path = directory.join("journal.jsonl");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };

        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };

            if value.get("kind").and_then(|kind| kind.as_str()) != Some("usage") {
                continue;
            }

            let Some(record) = value.get("record") else {
                continue;
            };

            if let Ok(usage) = serde_json::from_value::<UsageRecord>(record.clone()) {
                records.push(usage);
            }
        }
    }

    records
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use t8ful_core::journal::{AgentJournal, UsageRecord};
    use t8ful_core::message::Message;
    use t8ful_core::usage::Usage;

    use super::ChildJournal;

    fn lines(path: &std::path::Path) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(path).expect("read");

        text.lines()
            .map(|line| serde_json::from_str(line).expect("json line"))
            .collect()
    }

    #[tokio::test]
    async fn records_land_in_the_agents_own_file() {
        let dir = tempfile::tempdir().expect("dir");
        let journal = ChildJournal::new(dir.path(), "s1", "a1", None);

        journal
            .record_message(&Message::user_text("go and look"))
            .await
            .expect("recorded");

        let path = ChildJournal::path_for(dir.path(), "a1");

        assert!(path.exists());
        assert_eq!(journal.path(), path);

        let written = lines(&path);

        assert_eq!(written.len(), 1);
        assert_eq!(written[0]["kind"], "message");
        assert_eq!(written[0]["record"]["message"]["role"], "user");
        assert!(written[0]["at"].as_u64().is_some());
        assert_eq!(written[0]["v"], crate::session::JOURNAL_VERSION);
    }

    #[tokio::test]
    async fn two_children_never_share_a_file() {
        let dir = tempfile::tempdir().expect("dir");
        let first = ChildJournal::new(dir.path(), "s1", "a1", None);
        let second = ChildJournal::new(dir.path(), "s1", "a2", None);

        first
            .record_message(&Message::user_text("first child"))
            .await
            .expect("first");
        second
            .record_message(&Message::user_text("second child"))
            .await
            .expect("second");

        assert_ne!(first.path(), second.path());
        assert_eq!(lines(first.path()).len(), 1);
        assert_eq!(lines(second.path()).len(), 1);
        assert!(lines(first.path())[0].to_string().contains("first child"));
        assert!(lines(second.path())[0].to_string().contains("second child"));
    }

    #[tokio::test]
    async fn usage_is_journaled_with_the_child_as_its_agent() {
        let dir = tempfile::tempdir().expect("dir");
        let journal = ChildJournal::new(dir.path(), "s1", "a3", None);

        journal
            .record_usage(&UsageRecord {
                request_id: "s1-pid-a3-1-1".to_owned(),
                model: "test/model".to_owned(),
                agent: "a3".to_owned(),
                kind: "turn".to_owned(),
                usage: Usage {
                    input_tokens: Some(100),
                    output_tokens: Some(20),
                    ..Usage::default()
                },
            })
            .await
            .expect("recorded");

        let written = lines(journal.path());

        // The envelope says what kind of record this is; the record keeps its own
        // fields, including the agent it belongs to.
        assert_eq!(written[0]["kind"], "usage");
        assert_eq!(written[0]["record"]["agent"], "a3");
        assert_eq!(written[0]["record"]["request_id"], "s1-pid-a3-1-1");
        assert_eq!(written[0]["record"]["kind"], "turn");
        assert_eq!(written[0]["record"]["usage"]["input_tokens"], 100);
        assert_eq!(written[0]["record"]["usage"]["output_tokens"], 20);
    }

    #[tokio::test]
    async fn usage_records_sum_into_the_child_total() {
        let dir = tempfile::tempdir().expect("dir");
        let journal = ChildJournal::new(dir.path(), "s1", "a1", None);

        for (request, input, output, cost) in [("r1", 100, 20, "0.5"), ("r2", 200, 40, "0.25")] {
            journal
                .record_usage(&UsageRecord {
                    request_id: request.to_owned(),
                    model: "test/model".to_owned(),
                    agent: "a1".to_owned(),
                    kind: "turn".to_owned(),
                    usage: Usage {
                        input_tokens: Some(input),
                        output_tokens: Some(output),
                        cost_usd: Some(cost.to_owned()),
                        ..Usage::default()
                    },
                })
                .await
                .expect("recorded");
        }

        let total = journal
            .usage()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();

        assert_eq!(total.input_tokens, Some(300));
        assert_eq!(total.output_tokens, Some(60));
        assert_eq!(total.cost_usd.as_deref(), Some("0.75"));
    }

    #[tokio::test]
    async fn loading_child_usage_reads_every_agent_and_skips_bad_lines() {
        let dir = tempfile::tempdir().expect("dir");
        let first = ChildJournal::new(dir.path(), "s1", "a1", None);
        let second = ChildJournal::new(dir.path(), "s1", "a2", None);

        for (journal, request, agent) in [(&first, "r1", "a1"), (&second, "r2", "a2")] {
            journal
                .record_usage(&UsageRecord {
                    request_id: request.to_owned(),
                    model: "test/model".to_owned(),
                    agent: agent.to_owned(),
                    kind: "turn".to_owned(),
                    usage: Usage {
                        input_tokens: Some(10),
                        ..Usage::default()
                    },
                })
                .await
                .expect("recorded");
        }

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(first.path())
            .expect("open");
        std::io::Write::write_all(&mut file, b"{ not json\n").expect("write");

        let usage = super::load_usage(dir.path());

        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].agent, "a1");
        assert_eq!(usage[1].agent, "a2");
    }

    #[tokio::test]
    async fn a_child_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("dir");
        let journal = ChildJournal::new(dir.path(), "s1", "a1", None);

        journal.materialize().expect("materialized");

        let mode = std::fs::metadata(journal.path())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o600, "a child journal is owner-only");
    }

    #[tokio::test]
    async fn a_usage_record_without_a_ledger_still_journals() {
        let dir = tempfile::tempdir().expect("dir");
        let journal = ChildJournal::new(dir.path(), "s1", "a1", None);
        let ledger: Option<Arc<crate::usage::UsageLedger>> = None;

        assert!(ledger.is_none());

        journal
            .record_usage(&UsageRecord {
                request_id: "r1".to_owned(),
                model: "m".to_owned(),
                agent: "a1".to_owned(),
                kind: "turn".to_owned(),
                usage: Usage::default(),
            })
            .await
            .expect("recorded");
    }
}
