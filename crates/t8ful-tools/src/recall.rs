//! The `recall` tool: search prompts and assistant replies from past sessions.
//!
//! Reading journals belongs to the component that owns session storage, so the
//! scan lives behind [`SessionRecall`]. This module validates the request,
//! formats the hits, and keeps the answer inside the tool output budget. It
//! never calls the provider, so a search costs exactly one tool round trip.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use t8ful_core::policy::Mode;
use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolError, ToolOutput, ToolSpec};

/// Hits returned when the caller does not ask for a count.
pub const DEFAULT_LIMIT: usize = 6;

/// Largest hit count one call may ask for.
pub const MAX_LIMIT: usize = 20;

/// Which sessions a search may read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallScope {
    /// Only sessions recorded for the calling project.
    #[default]
    Project,
    /// Every project under the state directory.
    All,
}

impl RecallScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::All => "all",
        }
    }
}

/// One search request, already validated by the tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallRequest {
    pub query: String,
    pub scope: RecallScope,
    pub limit: usize,
    pub project_root: PathBuf,
    /// The session the caller is in, which is never searched.
    pub current_session: Option<String>,
}

/// Who wrote the recalled message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallRole {
    User,
    Assistant,
}

impl RecallRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One matching message, reduced to a bounded excerpt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallHit {
    pub session_id: String,
    pub title: String,
    pub project_root: PathBuf,
    pub mode: Mode,
    /// Unix seconds the message was appended.
    pub at: u64,
    pub role: RecallRole,
    pub excerpt: String,
}

/// What one search found and how much of the archive it read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecallReport {
    pub hits: Vec<RecallHit>,
    /// Journals inspected for matches.
    pub sessions_searched: usize,
    /// Journals excluded (another project, the current session) or unreadable.
    pub sessions_skipped: usize,
    /// Older journals the scan budget did not reach.
    pub sessions_not_scanned: usize,
}

/// Search backend used by [`RecallTool`].
pub trait SessionRecall: Send + Sync {
    fn search(&self, request: &RecallRequest) -> Result<RecallReport, String>;
}

/// A backend with no archive to search.
#[derive(Debug, Default)]
pub struct NoRecall;

impl SessionRecall for NoRecall {
    fn search(&self, _request: &RecallRequest) -> Result<RecallReport, String> {
        Err("session recall is not available in this build".to_owned())
    }
}

/// Search past session transcripts for prompts and assistant replies.
pub struct RecallTool {
    recall: Arc<dyn SessionRecall>,
    /// The session later records go to, shared with the driver so a `/resume`
    /// re-points recall instead of searching the session that was left.
    current_session: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for RecallTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecallTool").finish_non_exhaustive()
    }
}

impl RecallTool {
    pub fn new(
        recall: Arc<dyn SessionRecall>,
        current_session: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self {
            recall,
            current_session,
        }
    }

    fn current_session(&self) -> Option<String> {
        self.current_session
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecallArgs {
    query: String,
    #[serde(default)]
    scope: Option<RecallScope>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for RecallTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "recall".to_owned(),
            description: "Search earlier t8ful sessions of this project for prompts and assistant \
                          replies that mention the query. Returns bounded excerpts with the session \
                          id, age, mode, title, and role. Tool calls and tool results are never \
                          searched, and the current session is skipped because it is already in \
                          context."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Words to look for; matching is case-insensitive, and messages matching more of the words rank higher"
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["project", "all"],
                        "description": "`project` searches this project's sessions (default); `all` searches every project"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "description": "Number of excerpts to return (default 6)"
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let args: RecallArgs = serde_json::from_value(call.arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let query = args.query.trim();

        if query.is_empty() {
            return Err(ToolError::InvalidArguments(
                "the recall query must not be empty".to_owned(),
            ));
        }

        let limit = args.limit.unwrap_or(DEFAULT_LIMIT);

        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(ToolError::InvalidArguments(format!(
                "the recall limit must be between 1 and {MAX_LIMIT}"
            )));
        }

        let request = RecallRequest {
            query: query.to_owned(),
            scope: args.scope.unwrap_or_default(),
            limit,
            project_root: ctx.project_root.clone(),
            current_session: self.current_session(),
        };
        let report = self.recall.search(&request).map_err(ToolError::Failed)?;
        let (content, truncated) = crate::text::truncate(
            format_report(&request, &report, now()),
            ctx.max_output_bytes,
        );

        Ok(ToolOutput::text(content).with_truncation(truncated))
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// Render the search result for the model.
pub fn format_report(request: &RecallRequest, report: &RecallReport, now: u64) -> String {
    let mut text = format!("<recall scope=\"{}\">\n", request.scope.label());

    if report.hits.is_empty() {
        text.push_str(&format!("No message matched `{}`.\n", request.query));
    } else {
        text.push_str(&format!(
            "{} from {}. Only prompts and assistant replies are searched.\n",
            counted(report.hits.len(), "matching message"),
            counted(report.sessions_searched, "session")
        ));

        for hit in &report.hits {
            text.push_str(&format!(
                "\n- {} · {} · {} · {} · {}\n  title: {}\n  {}\n",
                hit.session_id,
                hit.role.label(),
                relative_age(hit.at, now),
                hit.mode.label(),
                hit.project_root.display(),
                hit.title,
                hit.excerpt
            ));
        }

        text.push_str(
            "\nResume one with `t8ful --resume <id>`, or `/resume` in the terminal interface.\n",
        );
    }

    if report.sessions_skipped > 0 || report.sessions_not_scanned > 0 {
        text.push_str(&format!(
            "\nThe scan searched {}",
            counted(report.sessions_searched, "session")
        ));

        if report.sessions_skipped > 0 {
            text.push_str(&format!(
                " and skipped {}",
                counted(report.sessions_skipped, "session")
            ));
        }

        if report.sessions_not_scanned > 0 {
            text.push_str(&format!(
                "; {} were not scanned because the recall budget ran out.",
                counted(report.sessions_not_scanned, "older session")
            ));
        } else {
            text.push('.');
        }
    }

    text.push_str("</recall>");

    text
}

/// `1 session` / `2 sessions`, so the report reads like a sentence.
fn counted(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// A short age for a hit, from "just now" to whole years.
pub fn relative_age(then: u64, now: u64) -> String {
    let elapsed = now.saturating_sub(then);

    match elapsed {
        0..=59 => "just now".to_owned(),
        60..=3_599 => format!("{}m ago", elapsed / 60),
        3_600..=86_399 => format!("{}h ago", elapsed / 3_600),
        86_400..=2_591_999 => format!("{}d ago", elapsed / 86_400),
        2_592_000..=31_535_999 => format!("{}mo ago", elapsed / 2_592_000),
        _ => format!("{}y ago", elapsed / 31_536_000),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use t8ful_core::policy::Mode;
    use t8ful_core::tool::{Tool, ToolCall, ToolContext, ToolEffect, ToolError};

    use super::{
        DEFAULT_LIMIT, MAX_LIMIT, NoRecall, RecallHit, RecallReport, RecallRole, RecallScope,
        RecallTool, SessionRecall, format_report, relative_age,
    };

    struct StubRecall {
        report: RecallReport,
        seen: Mutex<Option<super::RecallRequest>>,
    }

    impl StubRecall {
        fn new(report: RecallReport) -> Arc<Self> {
            Arc::new(Self {
                report,
                seen: Mutex::new(None),
            })
        }

        fn seen(&self) -> super::RecallRequest {
            self.seen
                .lock()
                .unwrap()
                .clone()
                .expect("a request reached the backend")
        }
    }

    impl SessionRecall for StubRecall {
        fn search(&self, request: &super::RecallRequest) -> Result<RecallReport, String> {
            *self.seen.lock().unwrap() = Some(request.clone());

            Ok(self.report.clone())
        }
    }

    fn context() -> ToolContext {
        ToolContext::new(
            PathBuf::from("/work/project"),
            PathBuf::from("/work/project/sub"),
        )
    }

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall::new("call-1", "recall", arguments)
    }

    fn tool(recall: Arc<dyn SessionRecall>) -> RecallTool {
        RecallTool::new(recall, Arc::new(Mutex::new(Some("current".to_owned()))))
    }

    fn hit() -> RecallHit {
        RecallHit {
            session_id: "abc123".to_owned(),
            title: "Fix the flaky test".to_owned(),
            project_root: PathBuf::from("/work/project"),
            mode: Mode::Coding,
            at: 1_000_000,
            role: RecallRole::Assistant,
            excerpt: "the «flaky» test was a race".to_owned(),
        }
    }

    #[test]
    fn the_spec_documents_the_scope_and_limit() {
        let spec = tool(Arc::new(NoRecall)).spec();

        assert_eq!(spec.name, "recall");
        assert_eq!(spec.parameters["required"], serde_json::json!(["query"]));
        assert_eq!(
            spec.parameters["additionalProperties"],
            serde_json::json!(false)
        );
        assert_eq!(spec.parameters["properties"]["scope"]["enum"][1], "all");
        assert_eq!(
            spec.parameters["properties"]["limit"]["maximum"],
            serde_json::json!(MAX_LIMIT)
        );
        assert!(spec.description.contains("current session"));
    }

    #[test]
    fn the_tool_is_read_only() {
        assert_eq!(tool(Arc::new(NoRecall)).effect(), ToolEffect::ReadOnly);
    }

    #[tokio::test]
    async fn project_scope_and_the_default_limit_reach_the_backend() {
        let backend = StubRecall::new(RecallReport::default());
        let tool = tool(backend.clone());

        tool.execute(
            &call(serde_json::json!({ "query": "  flaky test  " })),
            &context(),
        )
        .await
        .expect("search");

        let request = backend.seen();

        assert_eq!(request.query, "flaky test");
        assert_eq!(request.scope, RecallScope::Project);
        assert_eq!(request.limit, DEFAULT_LIMIT);
        assert_eq!(request.project_root, PathBuf::from("/work/project"));
        assert_eq!(request.current_session.as_deref(), Some("current"));
    }

    #[tokio::test]
    async fn an_explicit_scope_and_limit_reach_the_backend() {
        let backend = StubRecall::new(RecallReport::default());
        let tool = tool(backend.clone());

        tool.execute(
            &call(serde_json::json!({ "query": "decision", "scope": "all", "limit": 3 })),
            &context(),
        )
        .await
        .expect("search");

        let request = backend.seen();

        assert_eq!(request.scope, RecallScope::All);
        assert_eq!(request.limit, 3);
    }

    #[tokio::test]
    async fn bad_arguments_are_rejected_before_the_backend_runs() {
        let backend = StubRecall::new(RecallReport::default());
        let tool = tool(backend.clone());

        for arguments in [
            serde_json::json!({ "query": "   " }),
            serde_json::json!({ "query": "x", "scope": "elsewhere" }),
            serde_json::json!({ "query": "x", "limit": 0 }),
            serde_json::json!({ "query": "x", "limit": MAX_LIMIT + 1 }),
            serde_json::json!({ "query": "x", "extra": true }),
            serde_json::json!({ "limit": 2 }),
        ] {
            let error = tool
                .execute(&call(arguments.clone()), &context())
                .await
                .expect_err("rejected");

            assert!(
                matches!(error, ToolError::InvalidArguments(_)),
                "{arguments}: {error}"
            );
        }

        assert!(backend.seen.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn the_output_names_the_session_role_age_and_excerpt() {
        let backend = StubRecall::new(RecallReport {
            hits: vec![hit()],
            sessions_searched: 4,
            sessions_skipped: 1,
            sessions_not_scanned: 0,
        });
        let tool = tool(backend);

        let output = tool
            .execute(&call(serde_json::json!({ "query": "flaky" })), &context())
            .await
            .expect("search");

        for needle in [
            "<recall scope=\"project\">",
            "abc123",
            "assistant",
            "coding",
            "/work/project",
            "Fix the flaky test",
            "the «flaky» test was a race",
            "t8ful --resume <id>",
            "searched 4 sessions",
        ] {
            assert!(
                output.content.contains(needle),
                "missing {needle}: {}",
                output.content
            );
        }

        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn a_budget_that_stops_the_scan_is_reported() {
        let backend = StubRecall::new(RecallReport {
            hits: vec![hit()],
            sessions_searched: 2,
            sessions_skipped: 0,
            sessions_not_scanned: 7,
        });
        let tool = tool(backend);

        let output = tool
            .execute(&call(serde_json::json!({ "query": "flaky" })), &context())
            .await
            .expect("search");

        assert!(
            output.content.contains("7 older sessions were not scanned"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn a_small_output_budget_truncates_and_says_so() {
        let backend = StubRecall::new(RecallReport {
            hits: vec![hit()],
            sessions_searched: 1,
            ..RecallReport::default()
        });
        let tool = tool(backend);
        let mut ctx = context();

        ctx.max_output_bytes = 80;

        let output = tool
            .execute(&call(serde_json::json!({ "query": "flaky" })), &ctx)
            .await
            .expect("search");

        assert!(output.truncated);
        assert!(output.content.len() <= 80, "{}", output.content.len());
    }

    #[tokio::test]
    async fn an_unavailable_backend_is_a_failure_not_an_empty_answer() {
        let error = tool(Arc::new(NoRecall))
            .execute(&call(serde_json::json!({ "query": "x" })), &context())
            .await
            .expect_err("unavailable");

        assert!(matches!(error, ToolError::Failed(_)), "{error}");
    }

    #[test]
    fn ages_cover_the_ranges_without_rounding_up() {
        let cases = [
            (0, "just now"),
            (59, "just now"),
            (60, "1m ago"),
            (3_599, "59m ago"),
            (3_600, "1h ago"),
            (86_399, "23h ago"),
            (86_400, "1d ago"),
            (2_592_000, "1mo ago"),
            (31_536_000, "1y ago"),
        ];

        for (elapsed, expected) in cases {
            assert_eq!(relative_age(1_000_000, 1_000_000 + elapsed), expected);
        }

        // A clock skew never produces a negative age.
        assert_eq!(relative_age(2_000_000, 1_000_000), "just now");
    }

    #[test]
    fn an_empty_report_says_nothing_matched() {
        let request = super::RecallRequest {
            query: "nothing".to_owned(),
            scope: RecallScope::Project,
            limit: DEFAULT_LIMIT,
            project_root: PathBuf::from("/work"),
            current_session: None,
        };
        let text = format_report(&request, &RecallReport::default(), 1_000_000);

        assert!(text.contains("No message matched `nothing`"), "{text}");
        assert!(text.ends_with("</recall>"));
    }
}
