use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use t8ful_core::event::AgentEvent;
use t8ful_core::provider::{ReasoningEffort, ReasoningProfile};
use t8ful_core::question::{Question, QuestionAnswer, QuestionRequest};
use t8ful_core::tool::ToolStatus;

use crate::bridge::{
    CatalogSnapshot, McpSnapshot, ProjectFilesSnapshot, SessionSnapshot, SessionSummary, SkillView,
    SkillsSnapshot,
};
use crate::commands;
use crate::edit::Editor;
use crate::fuzzy;
use crate::theme::Theme;

/// Static information shown in the header.
#[derive(Debug, Clone)]
pub struct Header {
    pub model: String,
    pub mode: t8ful_core::policy::Mode,
    pub session_id: String,
    pub project_root: PathBuf,
    pub launch_dir: PathBuf,
    pub version: String,
    /// Current Git branch of the launch directory, when one is known.
    pub branch: Option<String>,
}

/// How a tool preview should be colored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKind {
    None,
    Diff,
    Code,
}

/// Display-only preview derived from a tool call and its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPreview {
    pub text: String,
    pub kind: PreviewKind,
    pub language: Option<String>,
}

impl Default for ToolPreview {
    fn default() -> Self {
        Self {
            text: String::new(),
            kind: PreviewKind::None,
            language: None,
        }
    }
}

impl ToolPreview {
    pub fn is_empty(&self) -> bool {
        self.kind == PreviewKind::None || self.text.is_empty()
    }
}

/// Buffer for the masked `/login` prompt.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginModal {
    /// Provider id the key belongs to.
    pub provider: String,
    /// Typed characters, rendered as bullets and never echoed elsewhere.
    pub buffer: Editor,
}

impl LoginModal {
    /// What the masked input looks like on screen.
    pub fn masked(&self) -> String {
        "•".repeat(self.buffer.len())
    }
}

/// Whether a name is one of the providers t8ful talks to.
pub fn is_known_provider(provider: &str) -> bool {
    let trimmed = provider.trim();

    trimmed.eq_ignore_ascii_case("openrouter")
        || trimmed.eq_ignore_ascii_case("opencode-go")
        || trimmed.eq_ignore_ascii_case("opencode")
        || trimmed.eq_ignore_ascii_case("opencode_go")
}

/// Canonical spelling of a provider name the TUI accepts.
pub fn normalize_provider(provider: &str) -> &'static str {
    if login_environment_variable(provider) == "OPENCODE_GO_API_KEY" {
        "opencode-go"
    } else {
        "openrouter"
    }
}

/// The credential variable shown in the login modal for one provider.
///
/// The TUI names it for display only; the driver decides where a key is stored.
pub fn login_environment_variable(provider: &str) -> &'static str {
    if provider.eq_ignore_ascii_case("opencode-go")
        || provider.eq_ignore_ascii_case("opencode")
        || provider.eq_ignore_ascii_case("opencode_go")
    {
        "OPENCODE_GO_API_KEY"
    } else {
        "OPENROUTER_API_KEY"
    }
}

impl std::fmt::Debug for LoginModal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginModal")
            .field("provider", &self.provider)
            .field("buffer", &self.masked())
            .finish()
    }
}

/// Short, human-facing tool name.
///
/// An MCP tool is `mcp__server__tool`, which reads badly in a card header, so
/// it becomes `Server · tool` with the server name title-cased. Everything else
/// is a static label or the id itself, borrowed rather than allocated.
pub fn friendly_tool_name(id: &str) -> Cow<'_, str> {
    match id {
        "apply_patch" => Cow::Borrowed("Patch"),
        "edit_file" => Cow::Borrowed("Edit"),
        "write_file" => Cow::Borrowed("Write"),
        "read_file" => Cow::Borrowed("Read"),
        "bash" => Cow::Borrowed("Bash"),
        // The domain noun leads and the detail follows, the same convention
        // `Read src/lib.rs` uses. These five carry no path or command, so the
        // label is the whole header and has to say which call it is.
        "process_output" => Cow::Borrowed("Process output"),
        "process_status" => Cow::Borrowed("Process status"),
        "process_list" => Cow::Borrowed("Process list"),
        "process_wait" => Cow::Borrowed("Process wait"),
        "process_stop" => Cow::Borrowed("Process stop"),
        "ask_user_question" => Cow::Borrowed("Ask"),
        "present_plan" => Cow::Borrowed("Plan"),
        "skill" => Cow::Borrowed("Skill"),
        "recall" => Cow::Borrowed("Recall"),
        other => match t8ful_mcp::split_model_name(other) {
            Some((server, tool)) => Cow::Owned(format!("{} · {}", title_case(server), tool)),
            None => Cow::Borrowed(other),
        },
    }
}

/// `my-server` becomes `My-Server`: the first character of each `-`/`_` part is
/// upper-cased and the separators are kept, so a server name stays readable
/// without a lookup table. It deliberately does not try to reproduce an
/// acronym's real casing (`github` becomes `Github`), because guessing would be
/// wrong more often than right.
fn title_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut start = true;

    for character in name.chars() {
        if character == '-' || character == '_' {
            out.push(character);
            start = true;

            continue;
        }

        if start {
            out.extend(character.to_uppercase());
            start = false;
        } else {
            out.push(character);
        }
    }

    out
}

/// Where a tool card belongs in the transcript.
///
/// `anchor` is the number of committed entries when the call was dispatched, so
/// the card renders after entry `anchor - 1` and before entry `anchor`. `seq`
/// keeps parallel calls in dispatch order when several share an anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CardPlacement {
    pub anchor: usize,
    pub seq: u64,
}

impl CardPlacement {
    /// A card that stays where it was committed instead of moving to a
    /// dispatch anchor. Restored history is already in transcript order, and a
    /// test that builds entries directly has no dispatch to point at.
    pub const INLINE: Self = Self {
        anchor: usize::MAX,
        seq: 0,
    };
}

impl Default for CardPlacement {
    fn default() -> Self {
        Self::INLINE
    }
}

/// One rendered transcript item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    User(String),
    /// Images submitted with the preceding user prompt, in transcript order.
    Images(Vec<t8ful_core::content::ImageInput>),
    Assistant(String),
    /// The startup banner. It is seeded once for a fresh session and never
    /// restored from a journal.
    Startup {
        version: String,
    },
    Tool {
        call_id: String,
        name: String,
        status: String,
        content: String,
        truncated: bool,
        /// Short description taken from the tool arguments, e.g. a path.
        title: String,
        /// Argument-derived preview body. Never sent to the model.
        preview: ToolPreview,
        /// Measured from the start event; restored history has no duration.
        duration: Option<Duration>,
        /// Transcript position of the call that produced this card.
        placement: CardPlacement,
        /// Images the tool returned, rendered under the card body.
        images: Vec<t8ful_core::content::ImageInput>,
    },
    /// Provider reasoning, retained in memory for display only.
    Reasoning(String),
    /// A background-process or child-agent completion, restored from the
    /// journal. It is drawn as a card so it reads the way the live session
    /// showed it, instead of exposing the wire envelope to a person.
    Notice {
        title: String,
        body: String,
        /// `ok`, `error`, or `info`; picks the card's glyph and color.
        status: String,
    },
    Error(String),
    Info(String),
    /// A plan presented with `present_plan`. It renders as one panel and is
    /// never collapsed; `mode` picks the footer hint for the mode it arrived
    /// in.
    Plan {
        body: String,
        mode: t8ful_core::policy::Mode,
        /// The call's position, so the document replaces its tool card.
        placement: CardPlacement,
    },
    /// A user-initiated mode switch, drawn as a divider in the transcript.
    ModeChange {
        from: t8ful_core::policy::Mode,
        to: t8ful_core::policy::Mode,
    },
    /// A child agent's span: the prompt it was given, and its report once it
    /// stops. Rendering one card per child keeps the child's own output out of
    /// the parent transcript, which is the point of delegating.
    Agent {
        id: String,
        role: String,
        status: String,
        isolation: String,
        task: Option<String>,
        worktree: Option<String>,
        branch: Option<String>,
        /// The child's prompt, shown collapsed.
        prompt: String,
        /// The child's report, present once it stops.
        report: Option<String>,
    },
}

impl Entry {
    pub fn label(&self) -> &'static str {
        match self {
            Self::User(_) => "you",
            Self::Images(_) => "images",
            Self::Assistant(_) => "t8ful",
            Self::Startup { .. } => "startup",
            Self::Tool { .. } => "tool",
            Self::Reasoning(_) => "reasoning",
            Self::Notice { .. } => "notice",
            Self::Error(_) => "error",
            Self::Info(_) => "info",
            Self::Plan { .. } => "plan",
            Self::ModeChange { .. } => "mode",
            Self::Agent { .. } => "agent",
        }
    }

    pub fn body(&self) -> &str {
        match self {
            Self::User(text)
            | Self::Assistant(text)
            | Self::Error(text)
            | Self::Info(text)
            | Self::Reasoning(text) => text,
            Self::Startup { .. } => "",
            Self::Images(_) => "",
            Self::Tool { content, .. } => content,
            Self::Notice { body, .. } => body,
            Self::Plan { body, .. } => body,
            Self::ModeChange { .. } => "",
            Self::Agent { prompt, report, .. } => report.as_deref().unwrap_or(prompt),
        }
    }

    /// Where this entry belongs when it is a tool card.
    pub fn placement(&self) -> Option<CardPlacement> {
        match self {
            Self::Tool { placement, .. } | Self::Plan { placement, .. } => Some(*placement),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageTotals {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    /// Hit rate of the latest request, not of the whole session.
    pub cache_hit_rate: Option<f64>,
    /// Context tokens of the latest request; cleared by a compaction.
    pub context_used: Option<u64>,
    pub cost_usd: String,
}

impl UsageTotals {
    fn record(
        &mut self,
        input: Option<u64>,
        output: Option<u64>,
        cache_read: Option<u64>,
        cache_write: Option<u64>,
        cost: Option<&str>,
    ) {
        self.input_tokens = add(self.input_tokens, input);
        self.output_tokens = add(self.output_tokens, output);
        self.cache_read_tokens = add(self.cache_read_tokens, cache_read);
        self.cache_write_tokens = add(self.cache_write_tokens, cache_write);

        if let Some(cost) = cost {
            let subtotal = cost.parse::<f64>().unwrap_or(0.0);
            let total = self.cost_usd.parse::<f64>().unwrap_or(0.0) + subtotal;

            self.cost_usd = format!("{total:.6}");
        }

        // The input count is the provider's prompt count and the cache counts
        // are subsets of it, so the hit rate is a share of the input and the
        // context the prompt once plus the output.
        let prompt = input.unwrap_or(0);

        // The cache counts are a subset of the prompt, so a provider that
        // reports them outside it would push the share past one; clamp rather
        // than display an impossible rate.
        self.cache_hit_rate = if prompt == 0 {
            None
        } else {
            Some((cache_read.unwrap_or(0) as f64 / prompt as f64).min(1.0))
        };
        self.context_used = input.map(|input| input.saturating_add(output.unwrap_or(0)));
    }

    /// A compaction replaced the conversation, so the last request's context
    /// size no longer describes what the next one will send.
    fn reset_context(&mut self) {
        self.context_used = None;
    }

    /// Replay journaled records into the totals a resumed session shows.
    ///
    /// The journal is the only source: a resume that skipped this would draw a
    /// footer of zeros even though the conversation spent money.
    pub fn from_records(records: &[t8ful_core::journal::UsageRecord]) -> Self {
        let mut totals = Self::default();

        for record in records {
            totals.record(
                record.usage.input_tokens,
                record.usage.output_tokens,
                record.usage.cache_read_tokens,
                record.usage.cache_write_tokens,
                record.usage.cost_usd.as_deref(),
            );
        }

        totals
    }
}

fn add(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0) + right.unwrap_or(0)),
    }
}

#[derive(Debug, Clone)]
pub struct QuestionProgress {
    pub request: QuestionRequest,
    /// Current question; `questions.len()` is the Review step of a multi-question
    /// request.
    pub index: usize,
    pub answers: Vec<QuestionAnswer>,
    pub custom_mode: bool,
    pub custom: Editor,
    /// Free-form answer saved in multi-select mode until `Done` stores it.
    pub custom_draft: Option<String>,
    /// Checked options of the current multi-select question, ascending.
    pub checked: Vec<usize>,
    /// Option row selected with the arrow keys. Rows are the options, then the
    /// `Type something.` row, then `Done` for a multi-select question.
    pub highlight: usize,
}

impl QuestionProgress {
    pub fn begin(request: QuestionRequest) -> Self {
        Self {
            request,
            index: 0,
            answers: Vec::new(),
            custom_mode: false,
            custom: Editor::new(),
            custom_draft: None,
            checked: Vec::new(),
            highlight: 0,
        }
    }

    /// Whether the request is showing its Review step.
    pub fn review(&self) -> bool {
        self.index >= self.request.questions.len()
    }

    pub fn current(&self) -> Option<&Question> {
        self.request.questions.get(self.index)
    }

    /// Whether the current question accepts several options.
    pub fn is_multi(&self) -> bool {
        self.current().is_some_and(|question| question.multi_select)
    }

    /// Number of selectable rows: every option, `Type something.`, and `Done`
    /// for a multi-select question.
    fn row_count(&self) -> usize {
        self.current()
            .map(|question| question.options.len() + 1 + usize::from(question.multi_select))
            .unwrap_or(0)
    }

    /// Move the highlighted row, clamped to the current question.
    pub fn move_highlight(&mut self, down: bool) {
        let last = self.row_count().saturating_sub(1);

        self.highlight = if down {
            (self.highlight + 1).min(last)
        } else {
            self.highlight.saturating_sub(1)
        };
    }

    /// Whether the highlight is on the `Type something.` row.
    pub fn on_custom_row(&self) -> bool {
        self.current()
            .is_some_and(|question| self.highlight == question.options.len())
    }

    /// Whether the highlight is on the `Done` row.
    pub fn on_done_row(&self) -> bool {
        self.is_multi()
            && self
                .current()
                .is_some_and(|question| self.highlight == question.options.len() + 1)
    }

    /// Check or uncheck one option of the current multi-select question.
    pub fn toggle_option(&mut self, option: usize) {
        let Some(question) = self.current() else {
            return;
        };

        if !question.multi_select || option >= question.options.len() {
            return;
        }

        match self.checked.binary_search(&option) {
            Ok(position) => {
                self.checked.remove(position);
            }
            Err(position) => self.checked.insert(position, option),
        }
    }

    pub fn choose(&mut self, option: usize) -> Option<Vec<QuestionAnswer>> {
        let question = self.current()?.clone();

        if question.multi_select || option >= question.options.len() {
            return None;
        }

        let answers = self.record(QuestionAnswer {
            header: question.header.clone(),
            question: question.question.clone(),
            selected: vec![option],
            custom: None,
        });

        self.custom.clear();
        self.custom_mode = false;
        answers
    }

    /// Submit the typed answer. A single-select question records it and moves
    /// on; a multi-select question keeps it as a draft beside the checked
    /// options until `Done`.
    pub fn submit_custom(&mut self) -> Option<Vec<QuestionAnswer>> {
        let text = self.custom.as_str().trim().to_owned();

        if self.is_multi() {
            self.custom_draft = (!text.is_empty()).then_some(text);
            self.custom.clear();
            self.custom_mode = false;

            return None;
        }

        if text.is_empty() {
            return None;
        }

        let question = self.current()?.clone();
        let answers = self.record(QuestionAnswer {
            header: question.header.clone(),
            question: question.question.clone(),
            selected: Vec::new(),
            custom: Some(text),
        });

        self.custom.clear();
        self.custom_mode = false;
        answers
    }

    /// Finish the current multi-select question. `None` while nothing is
    /// checked and no free-form answer was typed.
    pub fn done(&mut self) -> Option<Vec<QuestionAnswer>> {
        if !self.is_multi() || (self.checked.is_empty() && self.custom_draft.is_none()) {
            return None;
        }

        let question = self.current()?.clone();
        let answers = self.record(QuestionAnswer {
            header: question.header.clone(),
            question: question.question.clone(),
            selected: self.checked.clone(),
            custom: self.custom_draft.clone(),
        });

        self.custom.clear();
        self.custom_mode = false;
        answers
    }

    /// Store an answer for the current question, discarding any answers after
    /// it: changing an earlier choice invalidates the later ones.
    fn record(&mut self, answer: QuestionAnswer) -> Option<Vec<QuestionAnswer>> {
        self.answers.truncate(self.index);
        self.answers.push(answer);
        self.advance()
    }

    /// A single-question request submits as soon as it is answered; a longer
    /// one moves to the next question and finally to Review.
    fn advance(&mut self) -> Option<Vec<QuestionAnswer>> {
        if self.request.questions.len() == 1 {
            self.reset_question();

            return Some(self.answers.clone());
        }

        self.index += 1;
        self.reset_question();

        None
    }

    fn reset_question(&mut self) {
        self.highlight = 0;
        self.custom_mode = false;
        self.custom.clear();
        self.custom_draft = None;
        self.checked.clear();
    }

    /// Move back one question and restore the answer already given for it.
    /// Returns `false` on the first question.
    pub fn back(&mut self) -> bool {
        if self.index == 0 {
            return false;
        }

        self.index -= 1;
        self.restore_highlight();

        true
    }

    /// Move forward when the current question has an answer. Returns `false`
    /// at the end or while the current question is unanswered.
    pub fn forward(&mut self) -> bool {
        if self.review() || self.answers.len() <= self.index {
            return false;
        }

        if self.request.questions.len() == 1 {
            return false;
        }

        self.index += 1;

        if self.review() {
            self.reset_question();
        } else {
            self.restore_highlight();
        }

        true
    }

    /// Submit the reviewed answers. `None` unless every question is answered.
    pub fn submit_review(&mut self) -> Option<Vec<QuestionAnswer>> {
        if !self.review() || self.answers.len() < self.request.questions.len() {
            return None;
        }

        Some(self.answers.clone())
    }

    fn restore_highlight(&mut self) {
        self.reset_question();

        let Some(answer) = self.answers.get(self.index).cloned() else {
            return;
        };

        self.checked = answer.selected.clone();
        self.custom_draft = answer.custom.clone();

        if let Some(first) = answer.selected.first() {
            self.highlight = *first;
        } else if answer.custom.is_some() {
            self.highlight = self.current().map(|q| q.options.len()).unwrap_or(0);
        }

        if !self.is_multi() && answer.custom.is_some() {
            self.custom_mode = true;
            self.custom.set_text(answer.custom.unwrap_or_default());
        }
    }

    pub fn answers(&self) -> &[QuestionAnswer] {
        &self.answers
    }

    pub fn is_custom(&self) -> bool {
        self.custom_mode
    }

    pub fn toggle_custom(&mut self) {
        self.custom_mode = true;
        self.custom.clear();

        if let Some(draft) = &self.custom_draft {
            self.custom.set_text(draft.clone());
        }
    }
}

/// The modal offered after `present_plan` succeeds in plan mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanChoice {
    pub plan: String,
    pub highlight: usize,
}

/// What the user picked in the plan hand-off modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAction {
    ImplementHere,
    ImplementInNewSession,
    Stay,
}

impl PlanChoice {
    pub const ROWS: usize = 3;

    pub fn move_highlight(&mut self, down: bool) {
        let last = Self::ROWS - 1;

        self.highlight = if down {
            (self.highlight + 1).min(last)
        } else {
            self.highlight.saturating_sub(1)
        };
    }

    pub fn action(&self) -> PlanAction {
        match self.highlight {
            0 => PlanAction::ImplementHere,
            1 => PlanAction::ImplementInNewSession,
            _ => PlanAction::Stay,
        }
    }
}

/// Everything the TUI draws and edits.
#[derive(Debug, Clone)]
pub struct AppState {
    pub header: Header,
    pub theme: Theme,
    /// Provider reasoning is display-only and hidden unless enabled.
    pub show_reasoning: bool,
    pub entries: Vec<Entry>,
    pub streaming: String,
    /// Reasoning is buffered until the next content boundary so it stays
    /// associated with the message it precedes.
    reasoning_buffer: String,
    /// Revision of the buffered reasoning text, so the live block can cache.
    reasoning_generation: u64,
    pub input: Editor,
    /// The keyboard and command overlay is open.
    pub help: bool,
    pub status: String,
    /// Lines between the bottom of the transcript and the viewport.
    pub scroll: usize,
    /// Largest useful value for [`AppState::scroll`], set by the renderer.
    pub max_scroll: usize,
    /// Interior height of the transcript viewport, set by the renderer.
    pub transcript_height: usize,
    pub busy: bool,
    /// When the current turn started, for the busy line.
    pub turn_started: Option<Instant>,
    /// Stable busy-line verb seed for this turn.
    pub turn_seed: u64,
    /// Index of the rotating busy frame.
    pub spinner_phase: usize,
    pub should_quit: bool,
    pub usage: UsageTotals,
    pub queue: Vec<String>,
    pub queue_paused: bool,
    /// Where the active credential comes from; the login modal shows it.
    pub auth: crate::bridge::AuthSnapshot,
    /// The `/login` modal, while it is open.
    pub login: Option<LoginModal>,
    /// The merged catalog, one row per (id, provider) pair.
    pub models: Vec<crate::bridge::ModelRow>,
    /// Index of the highlighted row in [`AppState::picker_rows`].
    pub picker: Option<usize>,
    /// Fuzzy filter typed into the model picker.
    picker_filter: Editor,
    /// `Some(false)` means the selected model does not advertise reasoning.
    pub supports_reasoning: Option<bool>,
    /// Levels the selected model offers, from the driver.
    pub reasoning_levels: Vec<ReasoningEffort>,
    /// Where `reasoning_levels` came from, so a later capability event does not
    /// overwrite a model-specific list.
    pub reasoning_source: ReasoningSource,
    pub reasoning_effort: ReasoningEffort,
    /// Index into [`ReasoningEffort::ALL`] while choosing an effort.
    pub reasoning_picker: Option<usize>,
    /// Command and argument rows currently offered above the composer.
    pub completions: Vec<commands::Completion>,
    /// Selected position inside `completions`.
    pub completion: Option<usize>,
    completion_dismissed: bool,
    /// Saved sessions published by the driver, newest message first.
    pub sessions: Vec<SessionSummary>,
    sessions_loaded: bool,
    /// Skills discovered by the driver when the session started.
    pub skills: Vec<SkillView>,
    skills_loaded: bool,
    /// Project files and directories offered after `@` in the composer.
    pub files: Vec<String>,
    files_loaded: bool,
    /// The UI asked for a turn and is waiting for it to start. An error before
    /// `TurnStarted` clears the busy state instead of leaving it stuck.
    awaiting_turn: bool,
    /// Index of the highlighted row in [`AppState::session_picker_rows`].
    pub session_picker: Option<usize>,
    /// Fuzzy filter typed into the session picker.
    session_filter: Editor,
    pub question: Option<QuestionProgress>,
    /// The plan hand-off modal offered after `present_plan` succeeds.
    pub plan_choice: Option<PlanChoice>,
    /// Context window of the selected model, published by the driver.
    pub context_limit: Option<u64>,
    /// Running managed processes, one display line each.
    pub processes: Vec<String>,
    /// Child agents, in start order. Running ones drive the footer count; a
    /// finished one keeps its report so `/agents` can show it.
    pub agents: Vec<crate::bridge::AgentView>,
    /// The task graph, as the driver last published it.
    pub tasks: Vec<crate::bridge::TaskView>,
    /// Full background-process rows for `/process`.
    pub process_views: Vec<crate::bridge::ProcessView>,
    processes_loaded: bool,
    /// Process id a `/process stop` is waiting on, for the status line.
    pending_stop: Option<String>,
    /// The first restored history decides whether the startup banner is shown.
    pub banner_pending: bool,
    /// Validated images waiting to be submitted with the next prompt.
    pub pending_attachments: Vec<t8ful_core::content::ImageInput>,
    /// Tool outputs are collapsed by default; `ctrl+o` shows every line.
    pub expanded: bool,
    /// Tool calls that have started but not finished, in dispatch order.
    pub tool_runs: Vec<ToolRun>,
    tool_run_index: HashMap<String, usize>,
    /// Dispatch counter behind [`CardPlacement::seq`].
    next_card_seq: u64,
    /// Display-only output for a managed process that has not been bound to a
    /// Bash card yet, or whose card is no longer live.
    process_buffers: HashMap<String, PendingProcessOutput>,
    /// Processes that exited before their Bash result linked the card.
    process_exits: HashMap<String, (String, Option<i32>)>,
    /// Steering text the driver has accepted but not yet delivered.
    pending_steering: Vec<String>,
    /// Mouse drag selection over the transcript.
    pub selection: Option<Selection>,
    /// Screen area and first rendered line of the transcript, set by the
    /// renderer so mouse positions can be mapped back to transcript lines.
    pub transcript_area: ratatui::layout::Rect,
    pub transcript_start: usize,
    entries_generation: u64,
    streaming_generation: u64,
    catalog_pending: bool,
    mcp_pending: bool,
    /// The catalog fetch failed and has not been retried since.
    catalog_failed: bool,
    catalog_warning: Option<String>,
    /// Number of completion scans run, so tests can prove cursor-only edits
    /// do not rescan a large project tree.
    #[cfg(test)]
    completion_scans: usize,
}

impl AppState {
    pub fn new(header: Header) -> Self {
        Self {
            header,
            theme: Theme::pi(),
            show_reasoning: false,
            entries: Vec::new(),
            streaming: String::new(),
            reasoning_buffer: String::new(),
            reasoning_generation: 0,
            input: Editor::new(),
            help: false,
            status: "ready".to_owned(),
            scroll: 0,
            max_scroll: 0,
            transcript_height: 0,
            busy: false,
            turn_started: None,
            turn_seed: 0,
            spinner_phase: 0,
            should_quit: false,
            usage: UsageTotals::default(),
            queue: Vec::new(),
            queue_paused: false,
            auth: crate::bridge::AuthSnapshot::default(),
            login: None,
            models: Vec::new(),
            picker: None,
            picker_filter: Editor::new(),
            supports_reasoning: None,
            reasoning_levels: ReasoningProfile::unknown().levels(),
            reasoning_source: ReasoningSource::Default,
            reasoning_effort: ReasoningEffort::Default,
            reasoning_picker: None,
            completions: Vec::new(),
            completion: None,
            completion_dismissed: false,
            sessions: Vec::new(),
            sessions_loaded: false,
            skills: Vec::new(),
            skills_loaded: false,
            files: Vec::new(),
            files_loaded: false,
            awaiting_turn: false,
            session_picker: None,
            session_filter: Editor::new(),
            question: None,
            plan_choice: None,
            context_limit: None,
            processes: Vec::new(),
            agents: Vec::new(),
            tasks: Vec::new(),
            process_views: Vec::new(),
            processes_loaded: false,
            pending_stop: None,
            banner_pending: true,
            pending_attachments: Vec::new(),
            expanded: false,
            tool_runs: Vec::new(),
            tool_run_index: HashMap::new(),
            next_card_seq: 0,
            process_buffers: HashMap::new(),
            process_exits: HashMap::new(),
            pending_steering: Vec::new(),
            selection: None,
            transcript_area: ratatui::layout::Rect::default(),
            transcript_start: 0,
            entries_generation: 0,
            streaming_generation: 0,
            catalog_pending: false,
            mcp_pending: false,
            catalog_failed: false,
            catalog_warning: None,
            #[cfg(test)]
            completion_scans: 0,
        }
    }

    pub fn catalog_pending(&self) -> bool {
        self.catalog_pending
    }

    /// Record a status message. It stays internal; the footer shows usage
    /// only, so state changes never flash there.
    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }

    /// Apply the selected model's context window from the driver.
    pub fn apply_context(&mut self, limit: u64) {
        self.context_limit = Some(limit);
    }

    pub fn dismiss_plan_choice(&mut self) {
        self.plan_choice = None;
    }

    pub fn move_plan_choice(&mut self, down: bool) {
        if let Some(choice) = &mut self.plan_choice {
            choice.move_highlight(down);
        }
    }

    /// Replace the agent rows from the driver.
    pub fn apply_agents(&mut self, snapshot: crate::bridge::AgentsSnapshot) {
        self.agents = snapshot.agents;
    }

    /// Replace the task rows from the driver.
    pub fn apply_tasks(&mut self, snapshot: crate::bridge::TasksSnapshot) {
        self.tasks = snapshot.tasks;
    }

    /// Append one informational line and scroll to it.
    pub fn push_info(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::Info(text.into()));
        self.entries_generation = self.entries_generation.wrapping_add(1);
        self.scroll = 0;
    }

    /// Revision of the complete entry list. Appending is detected by length;
    /// replacing history increments this counter.
    pub fn entries_generation(&self) -> u64 {
        self.entries_generation
    }

    /// Revision of the in-progress assistant text.
    pub fn streaming_generation(&self) -> u64 {
        self.streaming_generation
    }

    /// Show or hide full tool output.
    ///
    /// The renderer rebuilds only tool entries when this flips, so a toggle
    /// never re-parses a long transcript.
    pub fn toggle_expanded(&mut self) {
        self.expanded = !self.expanded;
    }

    /// Show or hide provider reasoning, rebuilding only reasoning entries.
    pub fn toggle_reasoning(&mut self) {
        self.show_reasoning = !self.show_reasoning;
    }

    /// One transcript viewport, with a minimum step for tiny terminals.
    pub fn page_step(&self) -> usize {
        self.transcript_height.max(1)
    }

    /// Replace the model list used by the picker, preserving the selection.
    ///
    /// Rows are deduplicated on the `(id, provider)` pair: the same id served
    /// by both providers stays two rows, so its Go copy stays reachable.
    pub fn set_models(&mut self, models: Vec<crate::bridge::ModelRow>) {
        let selected = self.selected_model();
        let mut merged = models;

        if !self.header.model.trim().is_empty()
            && !merged
                .iter()
                .any(|model| model.id == self.header.model.trim())
        {
            merged.insert(
                0,
                crate::bridge::ModelRow::new(self.header.model.clone(), None),
            );
        }

        let mut seen = HashSet::new();

        merged.retain(|model| seen.insert((model.id.clone(), model.provider.clone())));
        self.models = merged;

        if self.picker.is_some() {
            // The filter is re-applied to the new catalog; keep the highlighted
            // row when it survived, otherwise start again at the top row.
            let rows = self.picker_rows();
            let preserved =
                selected.and_then(|row| rows.iter().position(|&index| self.models[index] == row));

            self.picker = Some(preserved.unwrap_or(0));
        }
    }

    /// Model rows matching the typed filter, best match first.
    ///
    /// The filter matches the id *and* the provider name, so `/model opencode`
    /// narrows to the Go catalog. The sort is stable and empty filters score
    /// every model equally, so the catalog order survives until the user types.
    pub fn picker_rows(&self) -> Vec<usize> {
        let mut rows = self
            .models
            .iter()
            .enumerate()
            .filter_map(|(index, model)| {
                Some((
                    fuzzy::score(self.picker_filter.as_str(), &model.filter_text())?,
                    index,
                ))
            })
            .collect::<Vec<_>>();

        rows.sort_by(|(left, _), (right, _)| right.cmp(left));
        rows.into_iter().map(|(_, index)| index).collect()
    }

    /// Text shown on the model picker's filter line.
    pub fn picker_filter(&self) -> &str {
        self.picker_filter.as_str()
    }

    /// Apply one editing command to the model filter.
    pub fn edit_picker_filter(&mut self, edit: crate::edit::Edit) {
        self.picker_filter.apply(edit);
        self.picker = Some(0);
    }

    pub fn move_picker(&mut self, down: bool) {
        let last = self.picker_rows().len().saturating_sub(1);
        let position = self.picker.unwrap_or(0);

        self.picker = Some(if down {
            (position + 1).min(last)
        } else {
            position.saturating_sub(1)
        });
    }

    /// Edit the model filter. Any edit restarts the selection at the top row.
    pub fn picker_push_char(&mut self, character: char) {
        self.picker_filter.insert(character);
        self.picker = Some(0);
    }

    pub fn picker_backspace(&mut self) {
        self.picker_filter.backspace();
        self.picker = Some(0);
    }

    pub fn selected_model(&self) -> Option<crate::bridge::ModelRow> {
        let rows = self.picker_rows();
        let index = *rows.get(self.picker?)?;

        self.models.get(index).cloned()
    }

    /// Close the model picker and forget its filter.
    pub fn close_picker(&mut self) {
        self.picker = None;
        self.picker_filter.clear();
    }

    /// Apply TUI-only saved-session state published by the driver.
    pub fn apply_sessions(&mut self, snapshot: SessionSnapshot) {
        match snapshot {
            SessionSnapshot::Pending => self.sessions_loaded = false,
            SessionSnapshot::Loaded { sessions } => {
                self.sessions = sessions;
                self.sessions_loaded = true;

                // A refresh can shrink the list under an open picker.
                if self.session_picker.is_some() {
                    self.reselect_session();
                }
            }
        }
    }

    pub fn sessions_loaded(&self) -> bool {
        self.sessions_loaded
    }

    /// Apply TUI-only background-process state published by the driver.
    pub fn apply_processes(&mut self, snapshot: crate::bridge::ProcessSnapshot) {
        match snapshot {
            crate::bridge::ProcessSnapshot::Pending => self.processes_loaded = false,
            crate::bridge::ProcessSnapshot::Loaded { processes } => {
                self.process_views = processes;
                self.processes_loaded = true;
            }
        }
    }

    pub fn processes_loaded(&self) -> bool {
        self.processes_loaded
    }

    /// Remember a requested stop so the status line can report its outcome.
    pub fn expect_process_stop(&mut self, id: impl Into<String>) {
        self.pending_stop = Some(id.into());
    }

    /// The multi-line `/process list` report.
    pub fn process_report(&self) -> String {
        if self.process_views.is_empty() {
            return "no background processes have been started".to_owned();
        }

        self.process_views
            .iter()
            .map(|process| {
                let mut state = process.state.clone();

                if let Some(code) = process.exit_code {
                    state.push_str(&format!(" (exit {code})"));
                }

                if let Some(signal) = process.signal {
                    state.push_str(&format!(" (signal {signal})"));
                }

                format!(
                    "{:<12}  {:<16}  {:>7}  {} bytes  {}",
                    process.id,
                    state,
                    format!("{:.1}s", process.elapsed_ms as f64 / 1000.0),
                    process.total_bytes,
                    process.command
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Apply TUI-only skill state published by the driver.
    pub fn apply_skills(&mut self, snapshot: SkillsSnapshot) {
        self.skills = snapshot.skills;
        self.skills_loaded = snapshot.loaded;
    }

    pub fn skills_loaded(&self) -> bool {
        self.skills_loaded
    }

    /// Apply the TUI-only project file index published by the driver.
    pub fn apply_files(&mut self, snapshot: ProjectFilesSnapshot) {
        self.files = snapshot.files;
        self.files_loaded = snapshot.loaded;

        // A late index must not reopen a popup the user dismissed; the next
        // edit will refresh it again.
        if !self.completion_dismissed {
            self.refresh_completions();
        }
    }

    pub fn files_loaded(&self) -> bool {
        self.files_loaded
    }

    /// The multi-line `/skill` report: name, description, source, and the
    /// short commit for skills t8ful installed.
    pub fn skill_report(&self) -> String {
        if self.skills.is_empty() {
            return "no skills installed".to_owned();
        }

        self.skills
            .iter()
            .map(|skill| match &skill.commit {
                Some(commit) => format!(
                    "{} — {} — {} — {}",
                    skill.name,
                    skill.description,
                    skill.source,
                    short_commit(commit)
                ),
                None => format!("{} — {} — {}", skill.name, skill.description, skill.source),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Sessions this project can resume: same root, not the open one.
    pub fn resumable_sessions(&self) -> impl Iterator<Item = &SessionSummary> {
        self.sessions
            .iter()
            .filter(|session| session.resumable && session.id != self.header.session_id)
    }

    /// The multi-line `/sessions` report, including cross-project sessions,
    /// newest message first. The age column is the newest message, not the
    /// session's creation time.
    pub fn session_report(&self) -> String {
        if self.sessions.is_empty() {
            return "no saved sessions".to_owned();
        }

        let now = now_seconds();
        // The driver publishes the authoritative order, but a filter or a
        // manual snapshot can arrive unsorted; the report is always readable
        // newest message first.
        let mut sessions = self.sessions.iter().collect::<Vec<_>>();

        sessions.sort_by(|left, right| {
            right
                .last_message_at
                .cmp(&left.last_message_at)
                .then_with(|| left.id.cmp(&right.id))
        });

        sessions
            .into_iter()
            .map(|session| {
                let marker = if session.id == self.header.session_id {
                    "→ "
                } else {
                    "  "
                };

                format!(
                    "{marker}{:<30}  {:<5}  {:<16}  {}",
                    session.id,
                    format_age(session.last_message_at, now),
                    session.project,
                    session.title
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Rows the session picker shows: resumable sessions ranked by the filter.
    pub fn session_picker_rows(&self) -> Vec<SessionSummary> {
        let mut rows = self
            .resumable_sessions()
            .filter_map(|session| {
                let score = fuzzy::best(
                    self.session_filter.as_str(),
                    [session.id.as_str(), session.title.as_str()],
                )?;

                Some((score, session.last_message_at, session.clone()))
            })
            .collect::<Vec<_>>();

        rows.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
        rows.into_iter().map(|(_, _, session)| session).collect()
    }

    /// Text shown on the session picker's filter line.
    pub fn session_picker_filter(&self) -> &str {
        self.session_filter.as_str()
    }

    /// Apply one editing command to the session filter.
    pub fn edit_session_filter(&mut self, edit: crate::edit::Edit) {
        self.session_filter.apply(edit);
        self.reselect_session();
    }

    pub fn selected_session(&self) -> Option<SessionSummary> {
        let rows = self.session_picker_rows();

        rows.get(self.session_picker?).cloned()
    }

    pub fn open_session_picker(&mut self) {
        self.session_filter.clear();
        self.session_filter.set_cursor(0);
        self.session_picker = Some(0);
    }

    pub fn close_session_picker(&mut self) {
        self.session_picker = None;
        self.session_filter.clear();
    }

    pub fn move_session_picker(&mut self, down: bool) {
        let last = self.session_picker_rows().len().saturating_sub(1);
        let position = self.session_picker.unwrap_or(0);

        self.session_picker = Some(if down {
            (position + 1).min(last)
        } else {
            position.saturating_sub(1)
        });
    }

    /// Narrow the picker, keeping the selected session when it still matches.
    pub fn session_picker_push_char(&mut self, character: char) {
        self.session_filter.insert(character);
        self.reselect_session();
    }

    pub fn session_picker_backspace(&mut self) {
        self.session_filter.backspace();
        self.reselect_session();
    }

    fn reselect_session(&mut self) {
        let selected = self.selected_session().map(|session| session.id);
        let rows = self.session_picker_rows();
        let preserved = selected.and_then(|id| rows.iter().position(|session| session.id == id));

        self.session_picker = Some(preserved.unwrap_or(0));
    }

    /// Apply TUI-only catalog state published by the driver.
    /// Apply the selected model's published reasoning levels.
    pub fn apply_reasoning(&mut self, snapshot: &crate::bridge::ReasoningSnapshot) {
        let crate::bridge::ReasoningSnapshot::Loaded { levels } = snapshot else {
            return;
        };

        self.reasoning_levels = levels.clone();
        self.reasoning_source = ReasoningSource::Model;
        self.clamp_reasoning_selection();
    }

    /// Move the selected effort to one the model offers.
    fn clamp_reasoning_selection(&mut self) {
        self.reasoning_effort = clamp_effort(&self.reasoning_levels, self.reasoning_effort);

        if self.reasoning_picker.is_some() {
            self.reasoning_picker = Some(self.reasoning_index());
        }
    }

    /// The status line for whichever discovery step is still pending.
    ///
    /// The catalog comes first because it is also the gate that selects the
    /// model, and it usually settles before MCP does. Recomputing from both
    /// flags keeps the line honest when one step settles while the other is
    /// still running: neither snapshot can claim "ready" over the other.
    fn set_discovery_status(&mut self) {
        if self.busy {
            return;
        }

        if self.catalog_pending {
            self.set_status("loading model catalog…".to_owned());
        } else if self.mcp_pending {
            self.set_status("starting MCP servers…".to_owned());
        } else if self.catalog_failed {
            self.set_status("model catalog unavailable; using fallback limits".to_owned());
        } else {
            self.set_status("ready".to_owned());
        }
    }

    pub fn apply_catalog(&mut self, snapshot: CatalogSnapshot) {
        match snapshot {
            CatalogSnapshot::Pending => {
                self.catalog_pending = true;
                self.catalog_failed = false;
                self.set_discovery_status();
            }
            CatalogSnapshot::Loaded { models } => {
                self.catalog_pending = false;
                self.catalog_failed = false;
                self.set_models(models);
                self.set_discovery_status();
            }
            CatalogSnapshot::Failed { message } => {
                self.catalog_pending = false;
                self.catalog_failed = true;
                self.catalog_warning = Some(message.clone());
                self.entries.push(catalog_warning_entry(&message));
                self.set_discovery_status();
            }
        }
    }

    /// Apply the MCP startup state.
    ///
    /// The status line reports whichever discovery step is still pending, so a
    /// prompt is never queued behind a line that says "ready".
    pub fn apply_mcp(&mut self, snapshot: McpSnapshot) {
        match snapshot {
            McpSnapshot::Pending => {
                self.mcp_pending = true;
                self.set_discovery_status();
            }
            McpSnapshot::Ready { .. } => {
                self.mcp_pending = false;
                self.set_discovery_status();
            }
        }
    }

    /// Apply one agent event. Text deltas are coalesced into `streaming` until
    /// the model reports the completed message.
    pub fn apply_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TurnStarted { turn } => {
                self.busy = true;
                self.awaiting_turn = false;
                self.turn_started = Some(Instant::now());
                self.turn_seed = crate::spinner::turn_seed(turn, &self.header.session_id);
                self.spinner_phase = 0;
                self.set_status(format!("turn {turn} started"));
            }
            AgentEvent::ModelStarted { step, .. } => {
                self.set_status(format!("waiting for the model (step {step})"));
            }
            AgentEvent::ReasoningDelta { text } => {
                self.reasoning_buffer.push_str(&text);
                self.reasoning_generation = self.reasoning_generation.wrapping_add(1);
            }
            AgentEvent::TextDelta { text } => {
                self.commit_reasoning();
                self.streaming.push_str(&text);
                self.streaming_generation = self.streaming_generation.wrapping_add(1);
            }
            AgentEvent::TextCompleted { text } => {
                self.commit_reasoning();

                if !self.streaming.is_empty() {
                    self.streaming_generation = self.streaming_generation.wrapping_add(1);
                }

                self.streaming.clear();

                // A whitespace-only completion renders nothing, and an entry for
                // it would still separate the cards around it.
                if !text.trim().is_empty() {
                    self.entries.push(Entry::Assistant(text));
                }
            }
            AgentEvent::ToolCallStarted {
                call_id,
                tool,
                arguments,
            } => {
                self.commit_reasoning();
                self.set_status(format!("running {}", friendly_tool_name(&tool)));
                self.push_tool_run(call_id, tool, arguments);
            }
            AgentEvent::ToolProgress {
                call_id,
                chunk,
                truncated,
            } => {
                if let Some(index) = self.tool_run_index.get(&call_id).copied() {
                    let run = &mut self.tool_runs[index];

                    if run.progress.len() < TOOL_PROGRESS_CAP {
                        let room = TOOL_PROGRESS_CAP - run.progress.len();
                        let mut take = chunk.len().min(room);

                        // Never split a character: the reporter hands over
                        // arbitrary byte chunks.
                        while take > 0 && !chunk.is_char_boundary(take) {
                            take -= 1;
                        }

                        run.progress.push_str(&chunk[..take]);
                    }

                    run.truncated |= truncated;
                    run.generation = run.generation.wrapping_add(1);
                }
            }
            AgentEvent::UserMessage {
                text,
                source,
                images,
            } => {
                if source == t8ful_core::event::UserMessageSource::Steered {
                    // The transcript entry replaces the unsent input line, so
                    // the two never show the same text twice.
                    if let Some(position) = self
                        .pending_steering
                        .iter()
                        .position(|pending| pending == &text)
                    {
                        self.pending_steering.remove(position);
                    }
                }

                if !text.is_empty() {
                    self.entries.push(Entry::User(text));
                }

                if !images.is_empty() {
                    // The driver confirmed this submission, so the tray entry
                    // becomes transcript content instead of being requeued.
                    self.pending_attachments
                        .retain(|pending| !images.iter().any(|image| image.path == pending.path));
                    self.entries.push(Entry::Images(images));
                }

                if source == t8ful_core::event::UserMessageSource::Submitted {
                    self.set_status("requesting the model…".to_owned());
                }
            }
            AgentEvent::ToolResult {
                call_id,
                tool,
                status,
                content,
                truncated,
                process_id,
                images,
            } => {
                self.commit_reasoning();

                // A background Bash result only says the process started. Keep
                // its live card until the process exits so later output binds
                // to it instead of creating a second card. Every other result,
                // including `process_status`, `process_output`, `process_wait`,
                // and `process_stop` (which also carry a process id), is an
                // ordinary call and commits like one.
                if tool == "bash"
                    && let Some(process_id) = process_id
                    && let Some(run) = self.tool_runs.iter_mut().find(|run| run.call_id == call_id)
                {
                    run.process_id = Some(process_id.clone());
                    run.result = Some(ToolRunResult {
                        status: status_label(status),
                        content,
                        truncated,
                    });

                    if let Some(buffer) = self.process_buffers.get(&process_id) {
                        run.progress = buffer.text.clone();
                        run.truncated |= buffer.truncated;
                    }

                    run.generation = run.generation.wrapping_add(1);

                    // A fast process can exit before this result reaches the
                    // frontend. Commit now rather than leaving a live card
                    // that will never receive another event.
                    if self.process_exits.remove(&process_id).is_some() {
                        self.commit_process_run(&process_id);
                    }

                    return;
                }

                let start = self.take_tool_run(&call_id);
                // A result with no matching start event has no dispatch
                // position, so it belongs where it arrived.
                let placement = start
                    .as_ref()
                    .map(|start| start.placement)
                    .unwrap_or_else(|| self.next_card_placement());

                // A successful plan is a document, not a tool card: the plan
                // text from the call's own arguments renders as one block.
                if tool == "present_plan" && status == ToolStatus::Ok {
                    let body = start
                        .as_ref()
                        .and_then(|start| start.arguments.get("plan"))
                        .and_then(serde_json::Value::as_str);

                    if let Some(body) = body {
                        self.entries.push(Entry::Plan {
                            body: body.to_owned(),
                            mode: self.header.mode,
                            placement,
                        });
                        self.set_status("plan presented".to_owned());

                        // A live plan in plan mode offers the hand-off. Resume
                        // and coding mode never do.
                        if self.header.mode == t8ful_core::policy::Mode::Plan {
                            self.plan_choice = Some(PlanChoice {
                                plan: body.to_owned(),
                                highlight: 0,
                            });
                        }

                        return;
                    }
                }

                let preview = start
                    .as_ref()
                    .map(|start| {
                        preview_from_result(
                            &tool,
                            &start.arguments,
                            &start.preview,
                            status,
                            &content,
                        )
                    })
                    .unwrap_or_default();

                self.entries.push(Entry::Tool {
                    call_id,
                    name: tool,
                    status: status_label(status),
                    content,
                    truncated,
                    title: start
                        .as_ref()
                        .map(|start| start.title.clone())
                        .unwrap_or_default(),
                    preview,
                    duration: start.map(|start| start.started.elapsed()),
                    placement,
                    images,
                });
            }
            AgentEvent::SkillLoaded {
                name,
                content,
                truncated,
            } => {
                self.commit_reasoning();
                let placement = self.next_card_placement();

                self.entries.push(Entry::Tool {
                    call_id: format!("skill:{name}"),
                    name: "skill".to_owned(),
                    status: "ok".to_owned(),
                    content,
                    truncated,
                    title: name.clone(),
                    preview: ToolPreview::default(),
                    duration: None,
                    placement,
                    images: Vec::new(),
                });

                if !self.busy {
                    self.set_status(format!("skill {name} loaded"));
                }
            }
            AgentEvent::ProcessStarted {
                process_id,
                command,
            } => {
                self.processes
                    .push(format!("{process_id} running {command}"));
            }
            AgentEvent::ProcessStatus {
                process_id,
                state,
                exit_code,
            } => {
                if state != "running" && !self.commit_process_run(&process_id) {
                    self.process_exits
                        .insert(process_id.clone(), (state.clone(), exit_code));
                }

                if self.pending_stop.as_deref() == Some(process_id.as_str()) {
                    self.pending_stop = None;
                    self.entries
                        .push(Entry::Info(format!("{process_id} stopped ({state})")));
                }

                self.processes.retain(|line| !line.starts_with(&process_id));

                // A finished process keeps its transcript result and can still
                // be queried with process_status; it no longer occupies a
                // running-process slot in the notice.
                if state == "running" {
                    let exit = exit_code
                        .map(|code| format!(" (exit {code})"))
                        .unwrap_or_default();

                    self.processes.push(format!("{process_id} {state}{exit}"));
                }
            }
            AgentEvent::ProcessOutput {
                process_id,
                chunk,
                truncated,
            } => {
                let marker = if truncated {
                    "\n[output truncated]"
                } else {
                    ""
                };

                self.entries.push(Entry::Info(format!(
                    "{process_id} output:\n{chunk}{marker}"
                )));
            }
            AgentEvent::ProcessProgress {
                process_id,
                chunk,
                truncated,
                reset,
            } => {
                let buffer = self.process_buffers.entry(process_id.clone()).or_default();

                if reset {
                    buffer.text = chunk;
                } else {
                    let room = TOOL_PROGRESS_CAP.saturating_sub(buffer.text.len());
                    let mut take = chunk.len().min(room);

                    while take > 0 && !chunk.is_char_boundary(take) {
                        take -= 1;
                    }

                    buffer.text.push_str(&chunk[..take]);

                    if take < chunk.len() {
                        buffer.truncated = true;
                    }
                }

                buffer.truncated |= truncated;

                if let Some(run) = self
                    .tool_runs
                    .iter_mut()
                    .find(|run| run.process_id.as_deref() == Some(process_id.as_str()))
                {
                    run.progress = buffer.text.clone();
                    run.truncated |= buffer.truncated;
                    run.generation = run.generation.wrapping_add(1);
                }
            }
            AgentEvent::ProcessSnapshot { entries } => {
                self.processes = entries;
            }
            AgentEvent::UsageRecorded {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                cost_usd,
                ..
            } => {
                self.usage.record(
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    cost_usd.as_deref(),
                );
            }
            AgentEvent::TurnFinished { reason, .. } => {
                self.flush_streaming();
                self.retain_process_tool_runs();
                // The driver demoted anything undelivered into the follow-up
                // queue, which arrives as `QueueUpdated`.
                self.pending_steering.clear();
                self.busy = false;
                self.awaiting_turn = false;
                self.turn_started = None;
                self.set_status(format!("turn finished ({reason})"));
            }
            AgentEvent::Error { message } => {
                self.flush_streaming();

                if self.awaiting_turn {
                    self.awaiting_turn = false;
                    self.busy = false;
                }

                self.entries.push(Entry::Error(message));
                self.set_status("error".to_owned());
            }
            AgentEvent::InputRequired { kind, detail } => {
                self.entries
                    .push(Entry::Info(format!("input required ({kind}): {detail}")));
                self.set_status("input required".to_owned());
            }
            AgentEvent::Warning { message } => {
                self.entries
                    .push(Entry::Info(format!("warning: {message}")));
                self.set_status("warning".to_owned());
            }
            AgentEvent::ModeChanged { mode, previous } => {
                self.header.mode = mode;
                self.set_status(format!("mode: {}", mode.label()));

                // Only a user-initiated switch earns a divider; startup and
                // resume syncs announce the current mode with no previous one.
                if let Some(from) = previous
                    && from != mode
                {
                    self.entries.push(Entry::ModeChange { from, to: mode });
                }
            }
            AgentEvent::ModelChanged {
                model,
                supports_reasoning,
            } => {
                self.header.model = model;
                self.supports_reasoning = supports_reasoning;

                // Without a published level list, fall back to the capability
                // flag so the picker still hides reasoning for models without it.
                if self.reasoning_source == ReasoningSource::Default {
                    self.reasoning_levels =
                        ReasoningProfile::from_support(supports_reasoning).levels();
                    self.clamp_reasoning_selection();
                }
            }
            AgentEvent::ReasoningEffortChanged { effort } => {
                self.reasoning_effort = effort;
                self.set_status(format!("reasoning effort: {}", effort.label()));
            }
            AgentEvent::CompactionStarted {
                estimated_tokens,
                context_limit,
            } => {
                self.set_status(format!(
                    "compacting context ({}/{} tokens)",
                    crate::ui::format_tokens(Some(estimated_tokens)),
                    crate::ui::format_tokens(Some(context_limit))
                ));
            }
            AgentEvent::CompactionFinished {
                covered_messages,
                summary_chars,
            } => {
                self.entries.push(Entry::Info(format!(
                    "compacted {covered_messages} messages into a {summary_chars}-character summary"
                )));
                self.usage.reset_context();
            }
            AgentEvent::AttachmentAdded {
                path,
                media_type,
                width,
                height,
                bytes,
            } => {
                let name = std::path::Path::new(&path)
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .unwrap_or(&path)
                    .to_owned();
                let image = t8ful_core::content::ImageInput::new(
                    media_type,
                    std::path::PathBuf::from(path),
                    width,
                    height,
                    bytes,
                );

                if !self
                    .pending_attachments
                    .iter()
                    .any(|pending| pending.path == image.path)
                {
                    self.pending_attachments.push(image);
                }

                self.set_status(format!("attached {name}"));
            }
            AgentEvent::QueueUpdated { items, paused } => {
                self.queue = items;
                self.queue_paused = paused;
            }
            AgentEvent::PlanPresented { .. } => {
                // The plan body already arrived as `Entry::Plan` with the tool
                // result; this event only closes the turn.
                self.flush_streaming();
                self.set_status("plan presented".to_owned());
            }
            AgentEvent::AgentSpawned {
                agent,
                role,
                task,
                isolation,
                worktree,
            } => {
                self.set_status(format!("{agent} started ({role})"));
                self.agents.push(crate::bridge::AgentView {
                    id: agent.clone(),
                    role: role.clone(),
                    status: "running".to_owned(),
                    elapsed_ms: 0,
                    steps: 0,
                    task: task.clone(),
                    worktree: worktree.clone(),
                    branch: None,
                    report: None,
                });
                self.entries.push(Entry::Agent {
                    id: agent,
                    role,
                    status: "running".to_owned(),
                    isolation,
                    task,
                    worktree,
                    branch: None,
                    prompt: String::new(),
                    report: None,
                });
                self.entries_generation = self.entries_generation.wrapping_add(1);
            }
            AgentEvent::AgentFinished {
                agent,
                role,
                status,
                summary,
                steps,
                worktree,
                branch,
                elapsed_ms,
                ..
            } => {
                let detail = format!(
                    "{status} after {steps} step{} in {} ms",
                    if steps == 1 { "" } else { "s" },
                    elapsed_ms
                );
                let report = if summary.trim().is_empty() {
                    detail.clone()
                } else {
                    format!("{detail}\n\n{summary}")
                };

                if let Some(view) = self.agents.iter_mut().find(|view| view.id == agent) {
                    view.status = status.clone();
                    view.elapsed_ms = u128::from(elapsed_ms);
                    view.steps = steps;
                    view.worktree.clone_from(&worktree);
                    view.branch.clone_from(&branch);
                    view.report = Some(report.clone());
                } else {
                    self.agents.push(crate::bridge::AgentView {
                        id: agent.clone(),
                        role: role.clone(),
                        status: status.clone(),
                        elapsed_ms: u128::from(elapsed_ms),
                        steps,
                        task: None,
                        worktree: worktree.clone(),
                        branch: branch.clone(),
                        report: Some(report.clone()),
                    });
                }

                if let Some(Entry::Agent {
                    status: entry_status,
                    report: entry_report,
                    worktree: entry_worktree,
                    branch: entry_branch,
                    ..
                }) = self
                    .entries
                    .iter_mut()
                    .find(|entry| matches!(entry, Entry::Agent { id, .. } if id == &agent))
                {
                    *entry_status = status.clone();
                    *entry_report = Some(report);
                    entry_worktree.clone_from(&worktree);
                    entry_branch.clone_from(&branch);
                }

                self.set_status(format!("{agent} {status}"));
                self.entries_generation = self.entries_generation.wrapping_add(1);
            }
            AgentEvent::AgentEvent { agent, event } => {
                // The child's own stream is display detail; the parent card is
                // the transcript. Keeping the latest tool name is enough for a
                // status line without pulling the child's output across.
                if let AgentEvent::ToolCallStarted { tool, .. } = *event {
                    self.set_status(format!("{agent} running {}", friendly_tool_name(&tool)));
                }
            }
            AgentEvent::AgentNoticeDelivered { agent } => {
                self.set_status(format!("{agent} reported"));
            }
            AgentEvent::ProcessNoticeDelivered { process_id } => {
                self.commit_process_run(&process_id);
                self.set_status(format!("{process_id} finished"));
            }
            AgentEvent::TaskChanged {
                id,
                title,
                state,
                blocked,
                owner,
            } => {
                let view = crate::bridge::TaskView {
                    id: id.clone(),
                    title,
                    state,
                    blocked,
                    owner,
                    deps: Vec::new(),
                };

                match self.tasks.iter_mut().find(|task| task.id == id) {
                    Some(existing) => *existing = view,
                    None => self.tasks.push(view),
                }
            }
            AgentEvent::SessionChanged { session_id } => {
                // A different id means a new or resumed session: every live
                // block belongs to the previous one and must be dropped, and a
                // new session gets the startup banner again.
                if session_id != self.header.session_id {
                    self.reset_for_session();
                    self.banner_pending = true;
                }

                self.header.session_id = session_id.clone();
                self.set_status(format!("session {session_id}"));
            }
            AgentEvent::HistoryRestored { entries } => {
                if !self.streaming.is_empty() {
                    self.streaming_generation = self.streaming_generation.wrapping_add(1);
                }

                self.streaming.clear();
                self.reasoning_buffer.clear();
                self.clear_tool_runs();
                self.process_buffers.clear();
                self.process_exits.clear();
                self.pending_steering.clear();
                self.selection = None;
                self.entries_generation = self.entries_generation.wrapping_add(1);
                self.scroll = 0;
                let restored = entries.len();
                let mode = self.header.mode;
                self.entries = entries
                    .into_iter()
                    .flat_map(|entry| {
                        let role = entry.role;

                        if role == t8ful_core::event::HistoryRole::User {
                            let mut restored = Vec::new();

                            if !entry.text.is_empty() {
                                restored.push(Entry::User(entry.text));
                            }

                            if !entry.images.is_empty() {
                                restored.push(Entry::Images(entry.images));
                            }

                            return restored;
                        }

                        vec![match role {
                            t8ful_core::event::HistoryRole::Assistant => {
                                if entry.text.trim().is_empty() {
                                    return Vec::new();
                                }

                                Entry::Assistant(entry.text)
                            }
                            t8ful_core::event::HistoryRole::Tool => {
                                let name = entry.tool.unwrap_or_else(|| "tool".to_owned());

                                // A restored plan looks like the live transcript:
                                // the call arguments still carry the plan text.
                                if name == "present_plan"
                                    && entry.status == Some(ToolStatus::Ok)
                                    && let Some(body) = entry
                                        .arguments
                                        .as_ref()
                                        .and_then(|arguments| arguments.get("plan"))
                                        .and_then(serde_json::Value::as_str)
                                {
                                    return vec![Entry::Plan {
                                        body: body.to_owned(),
                                        mode,
                                        placement: CardPlacement::default(),
                                    }];
                                }

                                let status = entry
                                    .status
                                    .map(status_label)
                                    .unwrap_or_else(|| "ok".to_owned());
                                let preview = entry
                                    .arguments
                                    .as_ref()
                                    .map(|arguments| {
                                        preview_from_restored(
                                            &name,
                                            arguments,
                                            entry.status,
                                            &entry.text,
                                        )
                                    })
                                    .unwrap_or_default();
                                let title = entry
                                    .arguments
                                    .as_ref()
                                    .map(|arguments| tool_title(&name, arguments))
                                    .unwrap_or_default();

                                Entry::Tool {
                                    call_id: String::new(),
                                    name,
                                    status,
                                    content: entry.text,
                                    truncated: entry.truncated,
                                    title,
                                    preview,
                                    duration: None,
                                    placement: CardPlacement::default(),
                                    images: entry.images,
                                }
                            }
                            t8ful_core::event::HistoryRole::User => unreachable!("handled above"),
                            t8ful_core::event::HistoryRole::Info => {
                                notice_entry(&entry.text).unwrap_or(Entry::Info(entry.text))
                            }
                        }]
                    })
                    .collect();

                // Catalog discovery can fail before the driver's history event
                // reaches this channel. Keep that warning attached to the
                // replacement transcript instead of letting it be clobbered.
                if let Some(message) = &self.catalog_warning {
                    self.entries.push(catalog_warning_entry(message));
                }

                if self.banner_pending {
                    self.banner_pending = false;

                    if restored == 0 {
                        self.entries.push(Entry::Startup {
                            version: self.header.version.clone(),
                        });
                    }
                }

                if restored > 0 {
                    self.set_status(format!("restored {restored} transcript entries"));
                }

                if self.catalog_warning.is_some() && !self.busy {
                    self.set_status("model catalog unavailable; using fallback limits".to_owned());
                }
            }
        }
    }

    /// Commit buffered reasoning as its own display-only transcript entry.
    pub fn commit_reasoning(&mut self) {
        if self.reasoning_buffer.is_empty() {
            return;
        }

        let text = std::mem::take(&mut self.reasoning_buffer);

        self.reasoning_generation = self.reasoning_generation.wrapping_add(1);

        if !text.trim().is_empty() {
            self.entries.push(Entry::Reasoning(text));
        }
    }

    /// Revision of the live reasoning block.
    pub fn reasoning_generation(&self) -> u64 {
        self.reasoning_generation
    }

    /// Reasoning text received but not yet committed to the transcript.
    pub fn reasoning(&self) -> &str {
        &self.reasoning_buffer
    }

    /// Placement for a card dispatched now: after everything committed so far.
    fn next_card_placement(&mut self) -> CardPlacement {
        let placement = CardPlacement {
            anchor: self.entries.len(),
            seq: self.next_card_seq,
        };

        self.next_card_seq = self.next_card_seq.wrapping_add(1);

        placement
    }

    fn push_tool_run(&mut self, call_id: String, tool: String, arguments: serde_json::Value) {
        let title = tool_title(&tool, &arguments);
        let preview = tool_preview(&tool, &arguments);
        let placement = self.next_card_placement();
        let run = ToolRun {
            call_id: call_id.clone(),
            tool,
            title,
            preview,
            arguments,
            started: Instant::now(),
            progress: String::new(),
            truncated: false,
            generation: 0,
            process_id: None,
            result: None,
            placement,
        };

        self.tool_run_index.insert(call_id, self.tool_runs.len());
        self.tool_runs.push(run);
    }

    fn take_tool_run(&mut self, call_id: &str) -> Option<ToolRun> {
        let index = self.tool_run_index.remove(call_id)?;

        Some(self.remove_tool_run_at(index))
    }

    fn remove_tool_run_at(&mut self, index: usize) -> ToolRun {
        let run = self.tool_runs.remove(index);

        // Removing shifts every later index; rebuild the lookup.
        self.tool_run_index.clear();

        for (position, run) in self.tool_runs.iter().enumerate() {
            self.tool_run_index.insert(run.call_id.clone(), position);
        }

        run
    }

    /// Move a background Bash card from the live area into the transcript.
    ///
    /// The tool result and the streamed output are joined once; the result
    /// stays authoritative and the process buffer is released.
    fn commit_process_run(&mut self, process_id: &str) -> bool {
        let Some(index) = self
            .tool_runs
            .iter()
            .position(|run| run.process_id.as_deref() == Some(process_id))
        else {
            return false;
        };

        let run = self.remove_tool_run_at(index);
        let ToolRun {
            call_id,
            tool,
            title,
            preview,
            started,
            progress,
            mut truncated,
            result,
            placement,
            ..
        } = run;
        let status = result
            .as_ref()
            .map(|result| result.status.clone())
            .unwrap_or_else(|| "running".to_owned());
        let mut content = result
            .as_ref()
            .map(|result| result.content.clone())
            .unwrap_or_default();

        if let Some(result) = &result {
            truncated |= result.truncated;
        }

        if !progress.is_empty() {
            if !content.is_empty() {
                content.push('\n');
            }

            content.push_str(&progress);
        }

        if truncated && !progress.is_empty() {
            content.push_str("\n[streamed output truncated]\n");
        }

        self.process_buffers.remove(process_id);
        self.entries.push(Entry::Tool {
            call_id,
            name: tool,
            status,
            content,
            truncated,
            title,
            preview,
            duration: Some(started.elapsed()),
            placement,
            images: Vec::new(),
        });

        true
    }

    /// A turn boundary commits finished calls but keeps a background process
    /// card live; its exit is not tied to the turn that started it.
    fn retain_process_tool_runs(&mut self) {
        self.tool_runs.retain(|run| run.process_id.is_some());
        self.tool_run_index.clear();

        for (position, run) in self.tool_runs.iter().enumerate() {
            self.tool_run_index.insert(run.call_id.clone(), position);
        }
    }

    fn clear_tool_runs(&mut self) {
        self.tool_runs.clear();
        self.tool_run_index.clear();
    }

    /// Steering text the driver accepted but has not delivered yet.
    pub fn pending_steering(&self) -> &[String] {
        &self.pending_steering
    }

    /// Remember a steer the driver is about to receive.
    pub fn remember_steering(&mut self, text: String) {
        self.pending_steering.push(text);
    }

    /// Drop every live block and per-session display state.
    fn reset_for_session(&mut self) {
        // Totals belong to the session that recorded them; a switch starts
        // from zero and a resume replays its own journal into them.
        self.usage = UsageTotals::default();
        self.streaming.clear();
        self.reasoning_buffer.clear();
        self.reasoning_generation = self.reasoning_generation.wrapping_add(1);
        self.streaming_generation = self.streaming_generation.wrapping_add(1);
        self.clear_tool_runs();
        self.process_buffers.clear();
        self.process_exits.clear();
        self.pending_steering.clear();
        self.selection = None;
        self.scroll = 0;
        self.expanded = false;
        self.busy = false;
        self.awaiting_turn = false;
        self.turn_started = None;
        self.plan_choice = None;
        self.pending_attachments.clear();
        self.set_status("ready".to_owned());
    }

    /// Mark the UI as waiting for a driver-started turn.
    pub fn request_turn(&mut self, status: impl Into<String>) {
        self.busy = true;
        self.awaiting_turn = true;
        self.set_status(status);
    }

    /// Typing an answer dismisses the plan hand-off: the user replied with
    /// changes instead of picking an action.
    fn note_composer_activity(&mut self) {
        self.plan_choice = None;
    }

    /// The composer text.
    pub fn input(&self) -> &str {
        self.input.as_str()
    }

    pub fn input_editor(&self) -> &Editor {
        &self.input
    }

    pub fn set_input(&mut self, text: impl Into<String>) {
        self.input.set_text(text);
        self.note_composer_activity();
        self.refresh_completions();
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.refresh_completions();
    }

    /// Apply one editing command to the composer.
    ///
    /// Cursor movement leaves the text alone, so it must not rerun the
    /// completion scan: an `@` mention over a large tree is the most expensive
    /// thing the composer can ask for.
    pub fn edit_input(&mut self, edit: crate::edit::Edit) {
        if self.input.apply(edit) {
            self.note_composer_activity();
            self.refresh_completions();
        }
    }

    pub fn flush_streaming(&mut self) {
        self.commit_reasoning();

        if self.streaming.is_empty() {
            return;
        }

        let text = std::mem::take(&mut self.streaming);

        self.streaming_generation = self.streaming_generation.wrapping_add(1);

        if !text.trim().is_empty() {
            self.entries.push(Entry::Assistant(text));
        }
    }

    /// Take the submitted prompt, if any.
    ///
    /// No transcript entry is written here: the driver emits
    /// [`AgentEvent::UserMessage`] once the prompt is journaled, so a failed
    /// submission never leaves a phantom line.
    pub fn submit(&mut self) -> Option<String> {
        let text = self.input.as_str().trim().to_owned();
        self.note_composer_activity();

        if text.is_empty() {
            self.clear_input();

            return None;
        }

        self.clear_input();
        self.scroll = 0;
        self.request_turn("requesting the model…");

        Some(text)
    }

    pub fn push_char(&mut self, character: char) {
        self.input.insert(character);
        self.note_composer_activity();
        self.refresh_completions();
    }

    pub fn newline(&mut self) {
        self.input.insert('\n');
        self.note_composer_activity();
        self.refresh_completions();
    }

    pub fn backspace(&mut self) {
        if self.input.backspace() {
            self.note_composer_activity();
            self.refresh_completions();
        }
    }

    /// Recompute command suggestions after an input edit. Editing always
    /// un-dismisses the popup offered by `Esc`.
    pub fn refresh_completions(&mut self) {
        #[cfg(test)]
        {
            self.completion_scans += 1;
        }

        // Borrowing the saved rows avoids cloning every session title on each
        // keystroke; the completion table only reads them.
        let sessions = self.resumable_sessions().collect::<Vec<_>>();

        self.completions = commands::suggestions(
            self.input.as_str(),
            &sessions,
            &self.skills,
            &self.files,
            &self.reasoning_levels,
            now_seconds(),
        );
        self.completion_dismissed = false;
        self.completion = if self.completions.is_empty() {
            None
        } else {
            Some(self.completion.unwrap_or(0).min(self.completions.len() - 1))
        };
    }

    /// How many completion scans this state has run, for tests that pin the
    /// cost of an edit.
    #[cfg(test)]
    pub fn completion_scans(&self) -> usize {
        self.completion_scans
    }

    pub fn dismiss_completions(&mut self) {
        self.completion_dismissed = true;
    }

    /// Whether the visible completion rows are `@` file mentions.
    pub fn completion_is_mention(&self) -> bool {
        self.completions
            .first()
            .is_some_and(commands::Completion::is_mention)
    }

    pub fn completion_visible(&self) -> bool {
        !self.completion_dismissed
            && !self.completions.is_empty()
            && !self.help
            && self.picker.is_none()
            && self.session_picker.is_none()
            && self.reasoning_picker.is_none()
            && self.question.is_none()
    }

    /// The row selected in the completion popup, if it is visible.
    pub fn selected_completion(&self) -> Option<commands::Completion> {
        if !self.completion_visible() {
            return None;
        }

        let position = self.completion?;

        self.completions.get(position).cloned()
    }

    pub fn move_completion(&mut self, down: bool) {
        if self.completions.is_empty() {
            return;
        }

        let last = self.completions.len() - 1;
        let position = self.completion.unwrap_or(0);

        self.completion = Some(if down {
            (position + 1).min(last)
        } else {
            position.saturating_sub(1)
        });
    }

    /// Replace the composer with the selected command. Returns whether a
    /// completion was applied.
    pub fn complete_selected(&mut self) -> bool {
        let Some(completion) = self.selected_completion() else {
            return false;
        };

        self.input.set_text(completion.completed_input());
        self.refresh_completions();

        true
    }

    /// Open the masked login prompt for a provider.
    pub fn open_login(&mut self, provider: &str) {
        self.login = Some(LoginModal {
            provider: provider.to_owned(),
            buffer: Editor::new(),
        });
    }

    pub fn login_push(&mut self, character: char) {
        if let Some(login) = &mut self.login {
            login.buffer.insert(character);
        }
    }

    pub fn login_backspace(&mut self) {
        if let Some(login) = &mut self.login {
            login.buffer.backspace();
        }
    }

    /// Apply one editing command to the masked login buffer.
    pub fn edit_login(&mut self, edit: crate::edit::Edit) {
        if let Some(login) = &mut self.login {
            login.buffer.apply(edit);
        }
    }

    /// Close the login prompt and hand back the typed key, if there was one.
    pub fn close_login(&mut self) -> Option<String> {
        let login = self.login.take()?;
        let buffer = login.buffer.as_str().to_owned();

        if buffer.is_empty() {
            None
        } else {
            Some(buffer)
        }
    }

    pub fn set_auth(&mut self, auth: crate::bridge::AuthSnapshot) {
        self.auth = auth;
    }

    /// Fold pending steering and queued follow-ups into the composer, oldest
    /// first, ahead of what the user already typed.
    ///
    /// Returns how many messages moved, or `None` when nothing was pending.
    pub fn restore_pending(&mut self) -> Option<usize> {
        if self.pending_steering.is_empty() && self.queue.is_empty() {
            return None;
        }

        let count = self.pending_steering.len() + self.queue.len();
        let mut texts = std::mem::take(&mut self.pending_steering);
        texts.extend(std::mem::take(&mut self.queue));
        let existing = self.input.as_str().to_owned();

        if !existing.trim().is_empty() {
            texts.push(existing);
        }

        self.input.set_text(texts.join("\n\n"));
        self.queue_paused = false;
        self.refresh_completions();
        self.scroll = 0;

        Some(count)
    }

    /// The reasoning levels this model offers, in display order.
    pub fn reasoning_options(&self) -> &[ReasoningEffort] {
        &self.reasoning_levels
    }

    pub fn reasoning_supported(&self, index: usize) -> bool {
        index < self.reasoning_levels.len()
    }

    pub fn move_reasoning_picker(&mut self, down: bool) {
        let Some(current) = self.reasoning_picker else {
            return;
        };

        let last = self.reasoning_levels.len().saturating_sub(1);
        self.reasoning_picker = Some(if down {
            (current + 1).min(last)
        } else {
            current.saturating_sub(1)
        });
    }

    /// The effort at one picker index.
    pub fn reasoning_at(&self, index: usize) -> Option<ReasoningEffort> {
        self.reasoning_levels.get(index).copied()
    }

    /// Index of the current effort, when the model offers it.
    pub fn reasoning_index(&self) -> usize {
        self.reasoning_levels
            .iter()
            .position(|level| *level == self.reasoning_effort)
            .unwrap_or(0)
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_add(lines).min(self.max_scroll);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.min(self.max_scroll).saturating_sub(lines);
    }
}

fn short_commit(commit: &str) -> &str {
    commit.get(..8).unwrap_or(commit)
}

pub fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Compact age of a unix-seconds stamp, used by the saved-session list.
pub fn format_age(at: u64, now: u64) -> String {
    let elapsed = now.saturating_sub(at);

    match elapsed {
        0..=59 => "just now".to_owned(),
        60..=3_599 => format!("{}m", elapsed / 60),
        3_600..=86_399 => format!("{}h", elapsed / 3_600),
        86_400..=604_799 => format!("{}d", elapsed / 86_400),
        _ => format!("{}w", elapsed / 604_800),
    }
}

/// Display-only progress is capped so a chatty command cannot grow the UI.
pub const TOOL_PROGRESS_CAP: usize = 256 * 1024;

/// A tool call that has started and not finished yet.
#[derive(Debug, Clone)]
pub struct ToolRun {
    pub call_id: String,
    pub tool: String,
    pub title: String,
    pub arguments: serde_json::Value,
    pub started: Instant,
    /// Output streamed so far; the final result replaces it.
    pub progress: String,
    pub truncated: bool,
    /// Revision of the streamed text, so the live card can cache its layout.
    pub generation: u64,
    pub preview: ToolPreview,
    /// Set when this Bash call launched a managed background process.
    pub process_id: Option<String>,
    /// The tool result, kept on the live card until the background process
    /// exits. Foreground calls commit their result immediately.
    pub result: Option<ToolRunResult>,
    /// Where the card sits in the transcript, fixed when the call started.
    pub placement: CardPlacement,
}

/// The authoritative result attached to a live background-process card.
#[derive(Debug, Clone)]
pub struct ToolRunResult {
    pub status: String,
    pub content: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default)]
struct PendingProcessOutput {
    text: String,
    truncated: bool,
}

/// One cell in the rendered transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Point {
    pub line: usize,
    pub column: usize,
}

impl Point {
    pub fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

impl PartialOrd for Point {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Point {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.line
            .cmp(&other.line)
            .then_with(|| self.column.cmp(&other.column))
    }
}

/// An in-app drag selection over rendered transcript lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Point,
    pub focus: Point,
    pub dragging: bool,
    /// Arrival time of the last click, for double-click detection.
    pub click_ms: u64,
    pub clicks: u8,
}

impl Selection {
    /// Anchor and focus in reading order.
    pub fn ordered(&self) -> (Point, Point) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    /// A selection that covers no text.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }
}

/// The short, human-facing tool label.
///
/// A `bash` call shows its whole command, which the card wraps over as many
/// header lines as it needs. A `recall` call shows its quoted query. Every
/// other tool keeps a path, name, or first command line elided to 60
/// characters.
fn tool_title(tool: &str, arguments: &serde_json::Value) -> String {
    // The header renders an MCP tool as `Server · tool`, from
    // `friendly_tool_name`, so naming the tool again here would print it twice
    // (`Exa · web_search_exa web_search_exa`). An MCP call has no path or
    // command to show in its place, so the title stays empty.
    if t8ful_mcp::split_model_name(tool).is_some() {
        return String::new();
    }

    if tool == "bash" {
        return arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
    }

    if tool == "recall" {
        let query = arguments
            .get("query")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim();

        if !query.is_empty() {
            return elide_chars(&format!("\"{query}\""), 60);
        }
    }

    let raw = arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            arguments
                .get("command")
                .and_then(serde_json::Value::as_str)
                .map(|command| command.lines().next().unwrap_or_default())
        })
        .or_else(|| arguments.get("name").and_then(serde_json::Value::as_str))
        // The five process tools have no path or command; their id is the one
        // thing that tells five wait/status calls apart.
        .or_else(|| {
            arguments
                .get("process_id")
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or_default()
        .trim();

    elide_chars(raw, 60)
}

fn tool_preview(tool: &str, arguments: &serde_json::Value) -> ToolPreview {
    match tool {
        "apply_patch" => {
            let patch = arguments
                .get("patch")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();

            if patch.trim().is_empty() {
                ToolPreview::default()
            } else {
                ToolPreview {
                    text: patch.to_owned(),
                    kind: PreviewKind::Diff,
                    language: None,
                }
            }
        }
        "edit_file" => edit_preview(arguments),
        "write_file" => {
            let content = arguments
                .get("content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let path = arguments.get("path").and_then(serde_json::Value::as_str);

            code_preview(content, path)
        }
        "spawn_agents" => spawn_agents_preview(arguments),
        _ => ToolPreview::default(),
    }
}

/// The prompts a `spawn_agents` call hands to its children.
///
/// A spawn card is otherwise just an id and a role, which tells a person nothing
/// about what was delegated; the prompt is the thing worth expanding.
fn spawn_agents_preview(arguments: &serde_json::Value) -> ToolPreview {
    let agents = arguments
        .get("agents")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();

    if agents.is_empty() {
        return ToolPreview::default();
    }

    let mut text = String::new();

    for (index, agent) in agents.iter().enumerate() {
        let role = agent
            .get("role")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("general");
        let isolation = agent
            .get("isolation")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("shared");
        let prompt = agent
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        if index > 0 {
            text.push_str("\n\n");
        }

        text.push_str(&format!("[{role} · {isolation}]\n{prompt}"));
    }

    ToolPreview {
        text,
        kind: PreviewKind::Code,
        language: None,
    }
}

fn preview_from_result(
    tool: &str,
    arguments: &serde_json::Value,
    fallback: &ToolPreview,
    status: ToolStatus,
    content: &str,
) -> ToolPreview {
    if matches!(
        status,
        ToolStatus::Denied | ToolStatus::NotExecuted | ToolStatus::Indeterminate
    ) {
        return ToolPreview::default();
    }

    match tool {
        "recall" if status == ToolStatus::Ok => recall_preview(content),
        "read_file" if status == ToolStatus::Ok => {
            let path = arguments.get("path").and_then(serde_json::Value::as_str);

            code_preview(content, path)
        }
        "bash" if looks_like_diff(content) => ToolPreview {
            text: content.to_owned(),
            kind: PreviewKind::Diff,
            language: None,
        },
        _ => fallback.clone(),
    }
}

/// A recall answer without the wrapper the model sees.
///
/// The `<recall scope="...">` element delimits the answer for the model; on
/// screen the card already names the tool, so the body opens with the scope and
/// continues with the hits.
fn recall_preview(content: &str) -> ToolPreview {
    let (scope, body) = match content.strip_prefix("<recall scope=\"") {
        Some(rest) => match rest.split_once('"') {
            Some((scope, rest)) => (
                scope,
                rest.split_once('\n')
                    .map(|(_, body)| body)
                    .unwrap_or_default(),
            ),
            None => ("", content),
        },
        None => ("", content),
    };
    // Truncation can leave the closing element off; the body is still usable.
    let body = body.strip_suffix("</recall>").unwrap_or(body).trim_end();

    if body.is_empty() {
        return ToolPreview::default();
    }

    let text = if scope.is_empty() {
        body.to_owned()
    } else {
        format!("scope: {scope}\n{body}")
    };

    ToolPreview {
        text,
        kind: PreviewKind::Code,
        language: None,
    }
}

fn preview_from_restored(
    tool: &str,
    arguments: &serde_json::Value,
    status: Option<ToolStatus>,
    content: &str,
) -> ToolPreview {
    let fallback = tool_preview(tool, arguments);

    match status {
        Some(status) => preview_from_result(tool, arguments, &fallback, status, content),
        None => fallback,
    }
}

fn code_preview(content: &str, path: Option<&str>) -> ToolPreview {
    if content.is_empty() {
        return ToolPreview::default();
    }

    ToolPreview {
        text: content.to_owned(),
        kind: PreviewKind::Code,
        language: path.and_then(language_for_path),
    }
}

fn edit_preview(arguments: &serde_json::Value) -> ToolPreview {
    let path = arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("file");
    let mut hunks = Vec::new();

    if let Some(edits) = arguments.get("edits").and_then(serde_json::Value::as_array) {
        for edit in edits {
            let old = edit
                .get("old_text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let new = edit
                .get("new_text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();

            if old.is_empty() && new.is_empty() {
                continue;
            }

            hunks.push((old, new));
        }
    } else {
        // Display-only fallback for a restored transcript recorded before the
        // `edits` array existed.
        let old = arguments
            .get("old_text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let new = arguments
            .get("new_text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        if !(old.is_empty() && new.is_empty()) {
            hunks.push((old, new));
        }
    }

    if hunks.is_empty() {
        return ToolPreview::default();
    }

    let mut text = format!("--- {path}\n+++ {path}\n");

    for (index, (old, new)) in hunks.iter().enumerate() {
        text.push_str(&format!("@@ edit {} @@\n", index + 1));

        for line in old.lines() {
            text.push('-');
            text.push_str(line);
            text.push('\n');
        }

        for line in new.lines() {
            text.push('+');
            text.push_str(line);
            text.push('\n');
        }
    }

    ToolPreview {
        text,
        kind: PreviewKind::Diff,
        language: None,
    }
}

fn looks_like_diff(text: &str) -> bool {
    text.lines().take(40).any(|line| {
        line.starts_with("diff --git ") || line.starts_with("@@ ") || line.starts_with("@@ -")
    })
}

fn language_for_path(path: &str) -> Option<String> {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)?
        .to_ascii_lowercase();
    let token = match extension.as_str() {
        "rs" => "rs",
        "toml" => "toml",
        "json" => "json",
        "md" | "markdown" => "md",
        "nix" => "nix",
        "py" => "py",
        "sh" | "bash" | "zsh" => "sh",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" => "cpp",
        "go" => "go",
        "js" | "mjs" | "cjs" => "js",
        "jsx" => "jsx",
        "ts" => "ts",
        "tsx" => "tsx",
        "yaml" | "yml" => "yaml",
        "html" => "html",
        "css" => "css",
        "lua" => "lua",
        "rb" => "rb",
        "java" => "java",
        "kt" => "kt",
        "swift" => "swift",
        "sql" => "sql",
        "xml" => "xml",
        other => other,
    };

    Some(token.to_owned())
}

fn elide_chars(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }

    let mut short: String = text.chars().take(width.saturating_sub(1)).collect();

    short.push('…');
    short
}

fn catalog_warning_entry(message: &str) -> Entry {
    Entry::Info(format!(
        "warning: model catalog unavailable ({message}); using fallback limits"
    ))
}

/// A restored notice, rebuilt as a card header and body.
///
/// Resume replays the conversation, and a notice arrives there as one message
/// carrying its wire envelope. Showing that text verbatim puts escaped tags and
/// `&amp;&amp;` in front of a person, so the envelope is decoded once here and the
/// plain `Info` row stays the fallback for anything this does not recognise.
fn notice_entry(text: &str) -> Option<Entry> {
    let row = t8ful_core::notice::parse_notice(text)?;

    Some(match row {
        t8ful_core::notice::NoticeRow::Process(notice) => {
            let mut title = format!("Process {} · {}", notice.process_id, notice.state);

            if let Some(code) = notice.exit_code {
                title.push_str(&format!(" (code {code})"));
            }

            if let Some(signal) = notice.signal {
                title.push_str(&format!(" (signal {signal})"));
            }

            title.push_str(&format!(
                " · {} · {}",
                crate::spinner::format_elapsed(Duration::from_millis(
                    notice.elapsed_ms.try_into().unwrap_or(u64::MAX)
                )),
                crate::images::human_bytes(notice.total_bytes),
            ));

            if notice.truncated {
                title.push_str(" · truncated");
            }

            let mut body = format!("command: {}", notice.command);

            if !notice.output_tail.is_empty() {
                body.push('\n');
                body.push_str(&notice.output_tail);
            }

            let status =
                if notice.exit_code.is_some_and(|code| code != 0) || notice.signal.is_some() {
                    "error"
                } else {
                    "ok"
                };

            Entry::Notice {
                title,
                body,
                status: status.to_owned(),
            }
        }
        t8ful_core::notice::NoticeRow::Agent(notice) => Entry::Notice {
            title: format!("Agent {} · {}", notice.agent, notice.status),
            body: notice.body,
            status: match notice.status.as_str() {
                "completed" => "ok",
                "failed" | "cancelled" => "error",
                _ => "info",
            }
            .to_owned(),
        },
    })
}

fn status_label(status: ToolStatus) -> String {
    match status {
        ToolStatus::Ok => "ok",
        ToolStatus::Error => "error",
        ToolStatus::Denied => "denied",
        ToolStatus::Indeterminate => "indeterminate",
        ToolStatus::NotExecuted => "not executed",
        ToolStatus::InputRequired => "input required",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use t8ful_core::event::{AgentEvent, HistoryEntry, HistoryRole};
    use t8ful_core::tool::ToolStatus;

    use super::{
        AppState, CardPlacement, Entry, Header, ToolPreview, UsageTotals, friendly_tool_name,
    };

    fn banners(state: &AppState) -> usize {
        state
            .entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Startup { .. }))
            .count()
    }

    fn state() -> AppState {
        AppState::new(Header {
            model: "openrouter/test".to_owned(),
            mode: t8ful_core::policy::Mode::Coding,
            session_id: "session-1".to_owned(),
            project_root: PathBuf::from("/work/project"),
            launch_dir: PathBuf::from("/work/project/sub"),
            version: "0.1.0".to_owned(),
            branch: Some("main".to_owned()),
        })
    }

    #[test]
    fn mcp_tool_names_render_as_a_server_and_tool() {
        for (id, expected) in [
            ("mcp__github__create_issue", "Github · create_issue"),
            ("mcp__linear__list_issues", "Linear · list_issues"),
            ("mcp__my-server__read_file", "My-Server · read_file"),
            ("mcp__a_b__tool", "A_B · tool"),
            // Not in the namespace: unchanged, and borrowed rather than copied.
            ("bash", "Bash"),
            ("read_file", "Read"),
            ("custom_tool", "custom_tool"),
        ] {
            assert_eq!(friendly_tool_name(id), expected, "{id}");
        }

        // A plain builtin is borrowed, so the common path allocates nothing.
        assert!(matches!(
            friendly_tool_name("bash"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn each_process_tool_keeps_its_own_label() {
        // All five used to collapse to `Process`, which made a card header say
        // nothing about which call it was.
        for (id, expected) in [
            ("process_output", "Process output"),
            ("process_status", "Process status"),
            ("process_list", "Process list"),
            ("process_wait", "Process wait"),
            ("process_stop", "Process stop"),
            // A server that happens to be called `process` stays distinct from
            // the builtins, which is the point of the namespace rendering.
            ("mcp__process__output", "Process · output"),
        ] {
            assert_eq!(friendly_tool_name(id), expected, "{id}");
        }
    }

    #[test]
    fn a_process_card_title_names_the_process_it_queried() {
        // The label says which call it is; the title says which process. Without
        // both, five wait/status calls are indistinguishable from each other.
        assert_eq!(
            super::tool_title(
                "process_status",
                &serde_json::json!({ "process_id": "process-2" })
            ),
            "process-2"
        );
        assert_eq!(
            super::tool_title("process_list", &serde_json::json!({})),
            ""
        );
        // A path still wins, so an ordinary tool title is unchanged.
        assert_eq!(
            super::tool_title(
                "read_file",
                &serde_json::json!({ "path": "src/lib.rs", "process_id": "process-2" })
            ),
            "src/lib.rs"
        );
    }

    #[test]
    fn every_new_session_seeds_the_startup_banner_once() {
        let mut state = state();

        state.apply_event(AgentEvent::HistoryRestored {
            entries: Vec::new(),
        });

        assert_eq!(banners(&state), 1);
        assert!(!state.banner_pending);

        // A second history replacement for the same session is not a new
        // session and must not seed the banner again.
        state.apply_event(AgentEvent::HistoryRestored {
            entries: Vec::new(),
        });

        assert_eq!(banners(&state), 0);

        // `/new` announces a different session id first, so the banner returns.
        state.apply_event(AgentEvent::SessionChanged {
            session_id: "session-2".to_owned(),
        });
        state.apply_event(AgentEvent::HistoryRestored {
            entries: Vec::new(),
        });

        assert_eq!(banners(&state), 1);
    }

    #[test]
    fn a_resumed_session_never_shows_the_banner() {
        let mut state = state();

        state.apply_event(AgentEvent::SessionChanged {
            session_id: "session-old".to_owned(),
        });
        state.apply_event(AgentEvent::HistoryRestored {
            entries: vec![HistoryEntry {
                arguments: None,
                role: HistoryRole::User,
                images: Vec::new(),
                text: "restored prompt".to_owned(),
                tool: None,
                status: None,
                truncated: false,
            }],
        });

        assert_eq!(banners(&state), 0);

        state.apply_event(AgentEvent::HistoryRestored {
            entries: Vec::new(),
        });

        assert_eq!(banners(&state), 0);
    }

    #[test]
    fn a_new_session_clears_the_previous_live_state() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "sleep 10" }),
        });
        state.apply_event(AgentEvent::UserMessage {
            text: "steer".to_owned(),
            source: t8ful_core::event::UserMessageSource::Steered,
            images: Vec::new(),
        });
        state.remember_steering("queued steer".to_owned());
        state.scroll = 3;
        state.busy = true;

        state.apply_event(AgentEvent::SessionChanged {
            session_id: "session-2".to_owned(),
        });

        assert!(state.tool_runs.is_empty());
        assert!(state.pending_steering().is_empty());
        assert!(state.selection.is_none());
        assert_eq!(state.scroll, 0);
        assert!(!state.busy);
        assert!(state.banner_pending);
    }

    #[test]
    fn reasoning_is_buffered_until_the_next_content_boundary() {
        let mut state = state();
        let before = state.reasoning_generation();

        state.apply_event(AgentEvent::ReasoningDelta {
            text: "because".to_owned(),
        });

        assert!(state.entries.is_empty());
        assert_eq!(state.reasoning(), "because");
        assert!(state.reasoning_generation() > before, "live block changed");

        state.apply_event(AgentEvent::TextDelta {
            text: "answer".to_owned(),
        });

        assert_eq!(state.entries, vec![Entry::Reasoning("because".to_owned())]);
        assert!(
            state.reasoning().is_empty(),
            "committed reasoning is drained"
        );
        assert_eq!(state.streaming, "answer");
    }

    #[test]
    fn reasoning_is_committed_before_a_tool_call_and_on_turn_finish() {
        let mut state = state();

        state.apply_event(AgentEvent::ReasoningDelta {
            text: "first".to_owned(),
        });
        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "read_file".to_owned(),
            arguments: serde_json::json!({ "path": "src/lib.rs" }),
        });

        assert_eq!(state.entries, vec![Entry::Reasoning("first".to_owned())]);

        state.apply_event(AgentEvent::ReasoningDelta {
            text: "second".to_owned(),
        });
        state.apply_event(AgentEvent::TurnFinished {
            turn: 1,
            reason: "stop".to_owned(),
        });

        assert_eq!(
            state.entries,
            vec![
                Entry::Reasoning("first".to_owned()),
                Entry::Reasoning("second".to_owned())
            ]
        );
        assert!(state.reasoning().is_empty());
    }

    #[test]
    fn restored_history_drops_live_reasoning() {
        let mut state = state();

        state.apply_event(AgentEvent::ReasoningDelta {
            text: "live".to_owned(),
        });
        state.apply_event(AgentEvent::HistoryRestored {
            entries: Vec::new(),
        });

        assert!(state.reasoning().is_empty());
        assert!(
            !state
                .entries
                .iter()
                .any(|entry| matches!(entry, Entry::Reasoning(_)))
        );
    }

    #[test]
    fn a_resumed_process_notice_restores_as_a_notice_card() {
        let mut state = state();
        let envelope = t8ful_core::notice::ProcessNotice {
            process_id: "process-1".to_owned(),
            command: "cd /work && cargo test 2>&1 | tail -5".to_owned(),
            state: "exited".to_owned(),
            exit_code: Some(0),
            signal: None,
            elapsed_ms: 30_500,
            total_bytes: 431,
            truncated: false,
            output_tail: "test result: ok".to_owned(),
        }
        .render();

        state.apply_event(AgentEvent::HistoryRestored {
            entries: vec![HistoryEntry {
                role: HistoryRole::Info,
                text: envelope,
                images: Vec::new(),
                tool: None,
                arguments: None,
                status: None,
                truncated: false,
            }],
        });

        assert_eq!(state.entries.len(), 1, "{:?}", state.entries);
        let Entry::Notice {
            title,
            body,
            status,
        } = &state.entries[0]
        else {
            panic!("a notice card: {:?}", state.entries[0]);
        };

        assert_eq!(status, "ok");
        assert!(title.contains("process-1"), "{title}");
        assert!(title.contains("exited"), "{title}");
        assert!(title.contains("code 0"), "{title}");
        assert!(title.contains("431 B"), "{title}");
        assert!(body.contains("cd /work && cargo test 2>&1"), "{body}");
        assert!(body.contains("test result: ok"), "{body}");
        assert!(!body.contains("&amp;"), "{body}");
        assert!(!body.contains("&gt;"), "{body}");
        assert!(!body.contains("<process_notice"), "{body}");
    }

    #[test]
    fn a_failed_process_notice_restores_with_an_error_status() {
        let mut state = state();
        let envelope = t8ful_core::notice::ProcessNotice {
            process_id: "process-2".to_owned(),
            command: "cargo bench".to_owned(),
            state: "exited".to_owned(),
            exit_code: Some(101),
            signal: None,
            elapsed_ms: 900,
            total_bytes: 12,
            truncated: false,
            output_tail: String::new(),
        }
        .render();

        state.apply_event(AgentEvent::HistoryRestored {
            entries: vec![HistoryEntry {
                role: HistoryRole::Info,
                text: envelope,
                images: Vec::new(),
                tool: None,
                arguments: None,
                status: None,
                truncated: false,
            }],
        });

        let Some(Entry::Notice { status, title, .. }) = state.entries.first() else {
            panic!("a notice card: {:?}", state.entries);
        };

        assert_eq!(status, "error");
        assert!(title.contains("(code 101)"), "{title}");
    }

    #[test]
    fn an_unrecognised_info_line_restores_as_information() {
        for text in [
            "warning: <process_notice> is mentioned inline".to_owned(),
            "restored 3 entries".to_owned(),
            "<process_notice id=\"process-1\">no closing tag".to_owned(),
        ] {
            let mut state = state();

            state.apply_event(AgentEvent::HistoryRestored {
                entries: vec![HistoryEntry {
                    role: HistoryRole::Info,
                    text: text.clone(),
                    images: Vec::new(),
                    tool: None,
                    arguments: None,
                    status: None,
                    truncated: false,
                }],
            });

            assert_eq!(state.entries, vec![Entry::Info(text.clone())], "{text}");
        }
    }

    #[test]
    fn a_whitespace_only_restored_message_occupies_no_entry() {
        let mut state = state();

        state.apply_event(AgentEvent::HistoryRestored {
            entries: vec![HistoryEntry {
                role: HistoryRole::Assistant,
                text: "\n\n".to_owned(),
                images: Vec::new(),
                tool: None,
                arguments: None,
                status: None,
                truncated: false,
            }],
        });

        assert!(state.entries.is_empty(), "{:?}", state.entries);
    }

    #[test]
    fn streaming_text_is_committed_once() {
        let mut state = state();

        state.apply_event(AgentEvent::TextDelta {
            text: "hel".to_owned(),
        });
        state.apply_event(AgentEvent::TextDelta {
            text: "lo".to_owned(),
        });

        assert_eq!(state.streaming, "hello");
        assert!(state.entries.is_empty());

        state.apply_event(AgentEvent::TextCompleted {
            text: "hello".to_owned(),
        });

        assert_eq!(state.entries, vec![Entry::Assistant("hello".to_owned())]);
    }

    #[test]
    fn a_finished_turn_flushes_pending_text() {
        let mut state = state();

        state.apply_event(AgentEvent::TextDelta {
            text: "partial".to_owned(),
        });
        state.apply_event(AgentEvent::TurnFinished {
            turn: 1,
            reason: "cancelled".to_owned(),
        });

        assert_eq!(state.entries, vec![Entry::Assistant("partial".to_owned())]);
        assert!(!state.busy);
    }

    #[test]
    fn page_scrolling_uses_the_viewport_and_clamps_both_ends() {
        let mut state = state();

        state.transcript_height = 19;
        state.max_scroll = 30;

        assert_eq!(state.page_step(), 19);

        state.scroll_up(state.page_step());
        assert_eq!(state.scroll, 19);

        state.scroll_up(99);
        assert_eq!(state.scroll, 30);

        // Reversing direction must move immediately, without consuming an
        // invisible excess offset.
        state.scroll_down(1);
        assert_eq!(state.scroll, 29);

        state.scroll = usize::MAX;
        state.scroll_down(1);
        assert_eq!(state.scroll, 29);

        state.scroll_down(99);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn page_step_is_at_least_one_in_a_tiny_terminal() {
        let mut state = state();

        assert_eq!(state.page_step(), 1);

        state.max_scroll = 0;
        state.scroll_up(10);

        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn catalog_updates_models_status_and_preserves_the_picker() {
        let mut state = state();

        state.apply_catalog(crate::bridge::CatalogSnapshot::Pending);
        assert!(state.catalog_pending());
        assert!(state.status.contains("loading model catalog"));

        state.apply_catalog(crate::bridge::CatalogSnapshot::Loaded {
            models: vec![row("vendor/one"), row("vendor/two")],
        });

        assert_eq!(
            state.models,
            vec![row("openrouter/test"), row("vendor/one"), row("vendor/two")]
        );
        assert!(!state.catalog_pending());
        assert_eq!(state.status, "ready");

        state.picker = Some(2);
        state.apply_catalog(crate::bridge::CatalogSnapshot::Loaded {
            models: vec![row("vendor/one"), row("vendor/two"), row("vendor/three")],
        });

        assert_eq!(state.picker, Some(2));
        assert_eq!(state.models[2].id, "vendor/two");

        state.apply_catalog(crate::bridge::CatalogSnapshot::Failed {
            message: "offline".to_owned(),
        });

        assert!(state.status.contains("fallback"));
        assert!(
            state
                .entries
                .iter()
                .any(|entry| matches!(entry, Entry::Info(text) if text.contains("offline")))
        );
    }

    #[test]
    fn the_status_line_reports_whichever_discovery_step_is_pending() {
        use crate::bridge::{CatalogSnapshot, McpSnapshot};

        // Applied in the order the interface applies its initial snapshots.
        // The catalog wins when both are pending, so the line does not depend
        // on which snapshot was applied last.
        let cases = [
            (false, false, "ready"),
            (false, true, "starting MCP servers…"),
            (true, false, "loading model catalog…"),
            (true, true, "loading model catalog…"),
        ];

        for (catalog_pending, mcp_pending, expected) in cases {
            let mut state = state();

            if catalog_pending {
                state.apply_catalog(CatalogSnapshot::Pending);
            } else {
                state.apply_catalog(CatalogSnapshot::Loaded { models: Vec::new() });
            }

            if mcp_pending {
                state.apply_mcp(McpSnapshot::Pending);
            } else {
                state.apply_mcp(McpSnapshot::Ready {
                    servers: 0,
                    tools: 0,
                });
            }

            assert_eq!(
                state.status, expected,
                "catalog pending {catalog_pending}, MCP pending {mcp_pending}"
            );
        }

        // Settling one of two pending steps must not claim "ready" over the
        // other one, whichever settles first.
        let mut state = state();

        state.apply_catalog(CatalogSnapshot::Pending);
        state.apply_mcp(McpSnapshot::Pending);
        state.apply_catalog(CatalogSnapshot::Loaded { models: Vec::new() });
        assert_eq!(state.status, "starting MCP servers…");

        let mut state = self::state();

        state.apply_catalog(CatalogSnapshot::Pending);
        state.apply_mcp(McpSnapshot::Pending);
        state.apply_mcp(McpSnapshot::Ready {
            servers: 1,
            tools: 2,
        });
        assert_eq!(state.status, "loading model catalog…");

        // A failed catalog is not erased when the last pending step settles:
        // the session runs on fallback limits and the line says so.
        let mut state = self::state();

        state.apply_catalog(CatalogSnapshot::Pending);
        state.apply_mcp(McpSnapshot::Pending);
        state.apply_catalog(CatalogSnapshot::Failed {
            message: "offline".to_owned(),
        });
        state.apply_mcp(McpSnapshot::Ready {
            servers: 0,
            tools: 0,
        });

        assert!(
            state.status.contains("fallback"),
            "a failed catalog was re-labelled ready: {}",
            state.status
        );
    }

    #[test]
    fn a_catalog_warning_survives_history_replacement() {
        let mut state = state();

        state.apply_catalog(crate::bridge::CatalogSnapshot::Failed {
            message: "provider offline".to_owned(),
        });
        state.apply_event(AgentEvent::HistoryRestored {
            entries: vec![HistoryEntry {
                arguments: None,
                role: HistoryRole::User,
                images: Vec::new(),
                text: "restored prompt".to_owned(),
                tool: None,
                status: None,
                truncated: false,
            }],
        });

        let warnings = state
            .entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Info(text) if text.contains("provider offline")))
            .count();

        assert_eq!(warnings, 1, "{:?}", state.entries);
        assert!(state.status.contains("fallback"));
    }

    #[test]
    fn set_models_deduplicates_without_losing_the_configured_model() {
        let mut state = state();

        state.set_models(vec![
            row("vendor/one"),
            row("vendor/one"),
            row("openrouter/test"),
        ]);

        assert_eq!(
            state.models,
            vec![row("vendor/one"), row("openrouter/test")]
        );
    }

    /// A catalog row with no provider, as a single-provider build produces.
    fn row(id: &str) -> crate::bridge::ModelRow {
        crate::bridge::ModelRow::new(id, None)
    }

    #[test]
    fn the_same_id_from_both_providers_stays_two_rows() {
        let mut state = state();

        state.header.model = "deepseek-v4.1-flash".to_owned();

        state.set_models(vec![
            crate::bridge::ModelRow::new("deepseek-v4.1-flash", Some("openrouter".to_owned())),
            crate::bridge::ModelRow::new("deepseek-v4.1-flash", Some("opencode-go".to_owned())),
        ]);

        assert_eq!(state.models.len(), 2);
        assert_eq!(state.models[0].label(), "deepseek-v4.1-flash [openrouter]");
        assert_eq!(state.models[1].label(), "deepseek-v4.1-flash [opencode-go]");
    }

    #[test]
    fn the_picker_filter_matches_the_provider_name_too() {
        let mut state = state();

        state.set_models(vec![
            crate::bridge::ModelRow::new("kimi-k3", Some("opencode-go".to_owned())),
            crate::bridge::ModelRow::new("vendor/sol", Some("openrouter".to_owned())),
        ]);
        state.picker = Some(0);
        type_filter(&mut state, "opencode");

        let rows = state.picker_rows();

        assert_eq!(rows.len(), 1);
        assert_eq!(state.models[rows[0]].id, "kimi-k3");
        assert_eq!(
            state.selected_model().map(|model| model.provider),
            Some(Some("opencode-go".to_owned()))
        );
    }

    fn type_filter(state: &mut AppState, text: &str) {
        for character in text.chars() {
            state.picker_push_char(character);
        }
    }

    #[test]
    fn streaming_and_history_revisions_track_content_changes() {
        let mut state = state();
        let before = state.streaming_generation();

        state.apply_event(AgentEvent::TextDelta {
            text: "hi".to_owned(),
        });
        assert!(state.streaming_generation() > before);

        let entries = state.entries_generation();

        state.apply_event(AgentEvent::HistoryRestored {
            entries: Vec::new(),
        });
        assert!(state.entries_generation() > entries);
    }

    #[test]
    fn background_progress_binds_to_the_bash_card_until_exit() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "make", "background": true }),
        });
        state.apply_event(AgentEvent::ProcessProgress {
            process_id: "process-1".to_owned(),
            chunk: "early\n".to_owned(),
            truncated: false,
            reset: false,
        });

        // Output can arrive before the tool result links the process id.
        assert!(state.entries.is_empty());

        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            status: ToolStatus::Ok,
            content: "started process-1 in the background".to_owned(),
            truncated: false,
            process_id: Some("process-1".to_owned()),
            images: Vec::new(),
        });

        assert!(state.entries.is_empty(), "the live card was duplicated");
        assert_eq!(state.tool_runs.len(), 1);
        assert_eq!(state.tool_runs[0].progress, "early\n");

        state.apply_event(AgentEvent::ProcessProgress {
            process_id: "process-1".to_owned(),
            chunk: "later\n".to_owned(),
            truncated: false,
            reset: false,
        });

        assert_eq!(state.tool_runs[0].progress, "early\nlater\n");
        assert!(state.entries.is_empty());

        state.apply_event(AgentEvent::ProcessStatus {
            process_id: "process-1".to_owned(),
            state: "exited".to_owned(),
            exit_code: Some(0),
        });

        assert!(state.tool_runs.is_empty());
        assert_eq!(state.entries.len(), 1);

        let Entry::Tool {
            status, content, ..
        } = &state.entries[0]
        else {
            panic!("expected a tool entry");
        };

        assert_eq!(status, "ok");
        assert!(content.contains("started process-1"), "{content}");
        assert!(content.contains("early\nlater\n"), "{content}");
    }

    #[test]
    fn a_fast_background_exit_commits_when_the_result_arrives() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "true", "background": true }),
        });
        state.apply_event(AgentEvent::ProcessProgress {
            process_id: "process-1".to_owned(),
            chunk: "quick\n".to_owned(),
            truncated: false,
            reset: false,
        });
        state.apply_event(AgentEvent::ProcessStatus {
            process_id: "process-1".to_owned(),
            state: "exited".to_owned(),
            exit_code: Some(0),
        });

        // The exit arrived before the result that links the card.
        assert!(state.tool_runs.len() == 1);
        assert!(state.entries.is_empty());

        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            status: ToolStatus::Ok,
            content: "started process-1".to_owned(),
            truncated: false,
            process_id: Some("process-1".to_owned()),
            images: Vec::new(),
        });

        assert!(state.tool_runs.is_empty());
        assert_eq!(state.entries.len(), 1);

        let Entry::Tool { content, .. } = &state.entries[0] else {
            panic!("expected a tool entry");
        };

        assert!(content.contains("quick\n"), "{content}");
    }

    #[test]
    fn a_turn_boundary_keeps_a_running_background_card_live() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "make", "background": true }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            status: ToolStatus::Ok,
            content: "started process-1".to_owned(),
            truncated: false,
            process_id: Some("process-1".to_owned()),
            images: Vec::new(),
        });
        state.apply_event(AgentEvent::TurnFinished {
            turn: 1,
            reason: "done".to_owned(),
        });

        assert_eq!(state.tool_runs.len(), 1);
        assert_eq!(state.tool_runs[0].process_id.as_deref(), Some("process-1"));
        assert!(state.entries.is_empty());
    }

    #[test]
    fn process_tool_results_are_ordinary_cards() {
        let mut state = state();

        for (call_id, tool) in [
            ("call-status", "process_status"),
            ("call-output", "process_output"),
            ("call-stop", "process_stop"),
        ] {
            state.apply_event(AgentEvent::ToolCallStarted {
                call_id: call_id.to_owned(),
                tool: tool.to_owned(),
                arguments: serde_json::json!({ "process_id": "process-1" }),
            });
            state.apply_event(AgentEvent::ToolResult {
                call_id: call_id.to_owned(),
                tool: tool.to_owned(),
                status: ToolStatus::Ok,
                content: format!("{tool} result"),
                truncated: false,
                process_id: Some("process-1".to_owned()),
                images: Vec::new(),
            });
        }

        assert!(
            state.tool_runs.is_empty(),
            "a process query is not a background Bash card"
        );
        assert_eq!(state.entries.len(), 3);
        assert!(
            state
                .entries
                .iter()
                .all(|entry| matches!(entry, Entry::Tool { .. })),
            "{:?}",
            state.entries
        );
    }

    #[test]
    fn a_card_records_the_position_where_its_call_started() {
        let mut state = state();

        state.apply_event(AgentEvent::UserMessage {
            text: "first".to_owned(),
            source: t8ful_core::event::UserMessageSource::Submitted,
            images: Vec::new(),
        });
        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "read_file".to_owned(),
            arguments: serde_json::json!({ "path": "src/lib.rs" }),
        });
        // An unrelated entry arrives while the call is still running.
        state.apply_event(AgentEvent::Warning {
            message: "slow provider".to_owned(),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "read_file".to_owned(),
            status: ToolStatus::Ok,
            content: "contents".to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        assert_eq!(state.entries.len(), 3, "{:?}", state.entries);

        let Entry::Tool { placement, .. } = &state.entries[2] else {
            panic!("expected the card to commit last: {:?}", state.entries);
        };

        assert_eq!(placement.anchor, 1, "the card belongs before the warning");
        assert_eq!(placement.seq, 0);
    }

    #[test]
    fn a_background_card_keeps_the_position_it_was_dispatched_at() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "make", "background": true }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            status: ToolStatus::Ok,
            content: "started process-1".to_owned(),
            truncated: false,
            process_id: Some("process-1".to_owned()),
            images: Vec::new(),
        });
        state.apply_event(AgentEvent::TextCompleted {
            text: "while it runs".to_owned(),
        });
        state.apply_event(AgentEvent::ProcessStatus {
            process_id: "process-1".to_owned(),
            state: "exited".to_owned(),
            exit_code: Some(0),
        });

        assert!(state.tool_runs.is_empty());
        assert_eq!(state.entries.len(), 2, "{:?}", state.entries);

        let Entry::Tool { placement, .. } = &state.entries[1] else {
            panic!("expected the card to commit last: {:?}", state.entries);
        };

        assert_eq!(placement.anchor, 0, "the card stays before the text");
    }

    #[test]
    fn parallel_cards_keep_their_dispatch_order_when_they_finish_out_of_order() {
        let mut state = state();

        for call_id in ["call-1", "call-2"] {
            state.apply_event(AgentEvent::ToolCallStarted {
                call_id: call_id.to_owned(),
                tool: "read_file".to_owned(),
                arguments: serde_json::json!({ "path": "src/lib.rs" }),
            });
        }

        for call_id in ["call-2", "call-1"] {
            state.apply_event(AgentEvent::ToolResult {
                call_id: call_id.to_owned(),
                tool: "read_file".to_owned(),
                status: ToolStatus::Ok,
                content: format!("{call_id} contents"),
                truncated: false,
                process_id: None,
                images: Vec::new(),
            });
        }

        let placements = state
            .entries
            .iter()
            .map(|entry| entry.placement().expect("a card"))
            .collect::<Vec<_>>();

        assert_eq!(placements[0].anchor, placements[1].anchor);
        assert_eq!(
            placements[0].seq, 1,
            "the card that finished first still has the later dispatch"
        );
        assert_eq!(placements[1].seq, 0);
    }

    #[test]
    fn a_session_change_drops_unbound_process_output() {
        let mut state = state();

        state.apply_event(AgentEvent::ProcessProgress {
            process_id: "process-1".to_owned(),
            chunk: "stale\n".to_owned(),
            truncated: false,
            reset: false,
        });
        state.apply_event(AgentEvent::SessionChanged {
            session_id: "another-session".to_owned(),
        });
        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "make", "background": true }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            status: ToolStatus::Ok,
            content: "started process-1".to_owned(),
            truncated: false,
            process_id: Some("process-1".to_owned()),
            images: Vec::new(),
        });

        assert_eq!(state.tool_runs[0].progress, "");
    }

    fn image_input(media_type: &str, path: &str) -> t8ful_core::content::ImageInput {
        t8ful_core::content::ImageInput::new(
            media_type,
            std::path::PathBuf::from(path),
            640,
            480,
            1536,
        )
    }

    #[test]
    fn an_attachment_stays_in_the_tray_until_the_driver_confirms_it() {
        let mut state = state();
        let image = image_input("image/png", "/state/images/one.png");

        state.apply_event(AgentEvent::AttachmentAdded {
            path: image.path.display().to_string(),
            media_type: image.media_type.clone(),
            width: image.width,
            height: image.height,
            bytes: image.bytes,
        });

        assert_eq!(state.pending_attachments, vec![image.clone()]);
        assert!(state.entries.is_empty());

        state.apply_event(AgentEvent::UserMessage {
            text: "look at this".to_owned(),
            source: t8ful_core::event::UserMessageSource::Submitted,
            images: vec![image],
        });

        assert!(state.pending_attachments.is_empty());
        assert_eq!(
            state.entries,
            vec![
                Entry::User("look at this".to_owned()),
                Entry::Images(vec![image_input("image/png", "/state/images/one.png")]),
            ]
        );
    }

    #[test]
    fn an_image_only_restored_message_keeps_its_position_and_is_not_requeued() {
        let mut state = state();
        let image = image_input("image/webp", "/state/images/restored.webp");

        state.apply_event(AgentEvent::HistoryRestored {
            entries: vec![
                HistoryEntry {
                    role: t8ful_core::event::HistoryRole::User,
                    text: String::new(),
                    images: vec![image.clone()],
                    tool: None,
                    arguments: None,
                    status: None,
                    truncated: false,
                },
                HistoryEntry {
                    role: t8ful_core::event::HistoryRole::Assistant,
                    text: "described".to_owned(),
                    images: Vec::new(),
                    tool: None,
                    arguments: None,
                    status: None,
                    truncated: false,
                },
            ],
        });

        assert_eq!(
            state.entries,
            vec![
                Entry::Images(vec![image]),
                Entry::Assistant("described".to_owned()),
            ]
        );
        assert!(state.pending_attachments.is_empty());
    }

    #[test]
    fn tool_results_become_transcript_entries() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            status: ToolStatus::Denied,
            content: "denied by the user".to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        assert_eq!(
            state.entries,
            vec![Entry::Tool {
                call_id: "call-1".to_owned(),
                name: "bash".to_owned(),
                status: "denied".to_owned(),
                content: "denied by the user".to_owned(),
                truncated: false,
                title: String::new(),
                preview: ToolPreview::default(),
                duration: None,
                placement: CardPlacement { anchor: 0, seq: 0 },
                images: Vec::new(),
            }]
        );
    }

    #[test]
    fn a_late_file_index_refreshes_an_open_mention_popup() {
        let mut state = state();

        state.set_input("look at @src/ma".to_owned());
        state.refresh_completions();
        assert!(state.completions.is_empty());

        state.apply_files(crate::bridge::ProjectFilesSnapshot {
            loaded: true,
            files: vec!["src/main.rs".to_owned()],
        });

        assert!(state.files_loaded());
        assert!(state.completion_is_mention());
        assert_eq!(state.completions.len(), 1);
        assert_eq!(
            state.completions[0].completed_input(),
            "look at @src/main.rs"
        );

        // Cursor motion re-renders the popup without rescanning project files.
        let scans = state.completion_scans();

        state.edit_input(crate::edit::Edit::LineStart);
        state.edit_input(crate::edit::Edit::WordLeft);
        state.edit_input(crate::edit::Edit::Left);

        assert_eq!(state.completion_scans(), scans);

        state.edit_input(crate::edit::Edit::Insert('x'));

        assert_eq!(state.completion_scans(), scans + 1);
    }

    #[test]
    fn a_skill_loaded_event_becomes_a_skill_tool_card() {
        let mut state = state();

        state.apply_event(AgentEvent::SkillLoaded {
            name: "review".to_owned(),
            content: "SKILL-BODY".to_owned(),
            truncated: false,
        });

        assert_eq!(
            state.entries,
            vec![Entry::Tool {
                call_id: "skill:review".to_owned(),
                name: "skill".to_owned(),
                status: "ok".to_owned(),
                content: "SKILL-BODY".to_owned(),
                truncated: false,
                title: "review".to_owned(),
                preview: ToolPreview::default(),
                duration: None,
                placement: CardPlacement { anchor: 0, seq: 0 },
                images: Vec::new(),
            }]
        );
        assert_eq!(state.status, "skill review loaded");
    }

    #[test]
    fn a_restored_skill_history_entry_keeps_its_card_title() {
        let mut state = state();

        state.apply_event(AgentEvent::HistoryRestored {
            entries: vec![HistoryEntry {
                role: HistoryRole::Tool,
                images: Vec::new(),
                text: "<skill name=\"review\">body</skill>".to_owned(),
                tool: Some("skill".to_owned()),
                arguments: Some(serde_json::json!({ "name": "review" })),
                status: Some(ToolStatus::Ok),
                truncated: false,
            }],
        });

        let Entry::Tool {
            name,
            title,
            status,
            content,
            ..
        } = &state.entries[0]
        else {
            panic!("expected a tool entry");
        };

        assert_eq!(name, "skill");
        assert_eq!(title, "review");
        assert_eq!(status, "ok");
        assert_eq!(content, "<skill name=\"review\">body</skill>");
    }

    #[test]
    fn a_tool_run_appears_at_dispatch_and_grows_with_progress() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "cargo test" }),
        });

        assert_eq!(state.tool_runs.len(), 1);
        assert!(state.entries.is_empty(), "the card is live, not committed");
        assert_eq!(state.tool_runs[0].title, "cargo test");

        let generation = state.tool_runs[0].generation;

        state.apply_event(AgentEvent::ToolProgress {
            call_id: "call-1".to_owned(),
            chunk: "compile".to_owned(),
            truncated: false,
        });

        assert_eq!(state.tool_runs[0].progress, "compile");
        assert!(state.tool_runs[0].generation > generation);
        assert!(state.entries.is_empty());

        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            status: ToolStatus::Ok,
            content: "[exit code 0, 1.0s]\ncompile done".to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        assert!(state.tool_runs.is_empty(), "the result replaces the card");
        assert_eq!(state.entries.len(), 1);
        assert!(matches!(
            &state.entries[0],
            Entry::Tool { title, .. } if title == "cargo test"
        ));
    }

    #[test]
    fn streamed_progress_is_capped() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "yes" }),
        });

        let chunk = "x".repeat(64 * 1024);

        for _ in 0..8 {
            state.apply_event(AgentEvent::ToolProgress {
                call_id: "call-1".to_owned(),
                chunk: chunk.clone(),
                truncated: false,
            });
        }

        assert_eq!(state.tool_runs[0].progress.len(), super::TOOL_PROGRESS_CAP);
        assert!(!state.tool_runs[0].truncated);

        state.apply_event(AgentEvent::ToolProgress {
            call_id: "call-1".to_owned(),
            chunk: "more".to_owned(),
            truncated: true,
        });

        assert_eq!(state.tool_runs[0].progress.len(), super::TOOL_PROGRESS_CAP);
        assert!(state.tool_runs[0].truncated);
    }

    #[test]
    fn a_steered_message_replaces_its_pending_line_exactly_once() {
        let mut state = state();

        state.remember_steering("look at the tests".to_owned());
        state.apply_event(AgentEvent::UserMessage {
            text: "look at the tests".to_owned(),
            source: t8ful_core::event::UserMessageSource::Steered,
            images: Vec::new(),
        });

        assert!(state.pending_steering().is_empty());
        assert_eq!(
            state.entries,
            vec![Entry::User("look at the tests".to_owned())]
        );

        state.apply_event(AgentEvent::TurnFinished {
            turn: 1,
            reason: "stop".to_owned(),
        });

        assert_eq!(
            state.entries,
            vec![Entry::User("look at the tests".to_owned())],
            "the delivered message is not duplicated"
        );
    }

    #[test]
    fn a_finished_turn_drops_undelivered_steering() {
        let mut state = state();

        state.remember_steering("too late".to_owned());
        state.apply_event(AgentEvent::TurnFinished {
            turn: 1,
            reason: "stop".to_owned(),
        });

        assert!(state.pending_steering().is_empty());
    }

    #[test]
    fn restoring_pending_messages_folds_steering_and_follow_ups_into_the_composer() {
        let mut state = state();

        state.remember_steering("steer".to_owned());
        state.queue = vec!["first".to_owned(), "second".to_owned()];
        state.queue_paused = true;
        state.set_input("draft".to_owned());

        assert_eq!(state.restore_pending(), Some(3));
        assert_eq!(state.input(), "steer\n\nfirst\n\nsecond\n\ndraft");
        assert!(state.pending_steering().is_empty());
        assert!(state.queue.is_empty());
        assert!(!state.queue_paused);
    }

    #[test]
    fn restoring_nothing_keeps_the_composer_untouched() {
        let mut state = state();

        state.set_input("draft".to_owned());

        assert_eq!(state.restore_pending(), None);
        assert_eq!(state.input(), "draft");
    }

    #[test]
    fn tool_results_pair_with_their_start_event_for_a_title_and_duration() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "read_file".to_owned(),
            arguments: serde_json::json!({ "path": "src/lib.rs" }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "read_file".to_owned(),
            status: ToolStatus::Ok,
            content: "contents".to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        let Entry::Tool {
            title,
            duration,
            call_id,
            ..
        } = &state.entries[0]
        else {
            panic!("expected a tool entry");
        };

        assert_eq!(call_id, "call-1");
        assert_eq!(title, "src/lib.rs");
        assert!(duration.is_some());
        assert!(state.tool_runs.is_empty());
    }

    #[test]
    fn submit_trims_and_ignores_blank_input() {
        let mut state = state();

        state.set_input("   ".to_owned());

        assert!(state.submit().is_none());
        assert!(!state.busy);

        state.set_input("  explain this  ".to_owned());

        assert_eq!(state.submit().as_deref(), Some("explain this"));
        assert!(state.busy);
        assert!(state.input().is_empty());
        assert!(
            state.entries.is_empty(),
            "the driver echoes the journaled prompt"
        );
    }

    #[test]
    fn usage_totals_keep_unknown_values_unknown() {
        let mut totals = UsageTotals::default();

        totals.record(Some(10), None, None, None, Some("0.0005"));
        totals.record(Some(5), Some(2), Some(3), None, None);

        assert_eq!(totals.input_tokens, Some(15));
        assert_eq!(totals.output_tokens, Some(2));
        assert_eq!(totals.cache_read_tokens, Some(3));
        assert!(totals.cost_usd.starts_with("0.0005"));
        // The hit rate and context come from the latest request only. The cached
        // tokens are part of the input, so they are not counted twice: the
        // context is that prompt plus its output, exactly as
        // `Usage::context_tokens` counts it for compaction.
        assert_eq!(totals.cache_hit_rate, Some(3.0 / 5.0));
        assert_eq!(totals.context_used, Some(7));

        totals.reset_context();
        assert!(totals.context_used.is_none());
    }

    fn usage_record(
        input: Option<u64>,
        output: Option<u64>,
        cost: Option<&str>,
    ) -> t8ful_core::journal::UsageRecord {
        t8ful_core::journal::UsageRecord {
            request_id: "request-1".to_owned(),
            model: "vendor/model".to_owned(),
            agent: "root".to_owned(),
            kind: "turn".to_owned(),
            usage: t8ful_core::usage::Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: input.map(|input| input / 2),
                cost_usd: cost.map(str::to_owned),
                ..t8ful_core::usage::Usage::default()
            },
        }
    }

    #[test]
    fn journaled_usage_replays_into_resumed_totals() {
        let records = vec![
            usage_record(Some(100), Some(10), Some("0.25")),
            usage_record(Some(40), Some(5), Some("0.5")),
        ];
        let totals = UsageTotals::from_records(&records);

        assert_eq!(totals.input_tokens, Some(140));
        assert_eq!(totals.output_tokens, Some(15));
        assert_eq!(totals.cache_read_tokens, Some(70));
        assert_eq!(totals.cost_usd, "0.750000");
        // The latest request decides the rate and the context size.
        assert_eq!(totals.cache_hit_rate, Some(0.5));
        assert_eq!(totals.context_used, Some(45));

        // A record with no provider counts keeps the totals unknown instead of
        // inventing a zero.
        let unknown = UsageTotals::from_records(&[usage_record(None, None, None)]);

        assert_eq!(unknown.input_tokens, None);
        assert_eq!(unknown.output_tokens, None);
        assert!(unknown.cost_usd.is_empty());
    }

    #[test]
    fn a_session_change_clears_the_previous_usage_totals() {
        let mut state = state();

        state.apply_event(AgentEvent::UsageRecorded {
            request_id: "request-1".to_owned(),
            model: "vendor/model".to_owned(),
            agent: "root".to_owned(),
            kind: "turn".to_owned(),
            input_tokens: Some(100),
            output_tokens: Some(10),
            cost_usd: Some("0.25".to_owned()),
            generation_id: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
        });

        assert_eq!(state.usage.input_tokens, Some(100));

        state.apply_event(AgentEvent::SessionChanged {
            session_id: "another-session".to_owned(),
        });

        assert_eq!(state.usage, UsageTotals::default());
    }

    #[test]
    fn input_editing_supports_multiline_and_backspace() {
        let mut state = state();

        state.push_char('a');
        state.newline();
        state.push_char('b');

        assert_eq!(state.input(), "a\nb");

        state.backspace();

        assert_eq!(state.input(), "a\n");
    }

    #[test]
    fn a_presented_plan_becomes_exactly_one_plan_entry() {
        let mut state = state();
        let plan = "# Ship it";

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "present_plan".to_owned(),
            arguments: serde_json::json!({ "plan": plan }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "present_plan".to_owned(),
            status: ToolStatus::Ok,
            content: "the plan was stored for review".to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });
        state.apply_event(AgentEvent::PlanPresented {
            plan: plan.to_owned(),
        });

        assert_eq!(
            state.entries,
            vec![Entry::Plan {
                body: plan.to_owned(),
                mode: t8ful_core::policy::Mode::Coding,
                placement: CardPlacement { anchor: 0, seq: 0 },
            }]
        );
        assert!(
            !state
                .entries
                .iter()
                .any(|entry| matches!(entry, Entry::Tool { .. } | Entry::Info(_)))
        );
    }

    #[test]
    fn a_live_plan_in_plan_mode_offers_the_hand_off() {
        let mut state = state();

        state.header.mode = t8ful_core::policy::Mode::Plan;
        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "present_plan".to_owned(),
            arguments: serde_json::json!({ "plan": "# Ship it" }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "present_plan".to_owned(),
            status: ToolStatus::Ok,
            content: "the plan was stored for review".to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        assert_eq!(
            state.plan_choice,
            Some(super::PlanChoice {
                plan: "# Ship it".to_owned(),
                highlight: 0,
            })
        );

        state.set_input("reply with changes");
        assert!(state.plan_choice.is_none());
    }

    #[test]
    fn a_plan_offers_no_hand_off_in_coding_mode_or_on_resume() {
        let mut coding = state();

        coding.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "present_plan".to_owned(),
            arguments: serde_json::json!({ "plan": "# Ship it" }),
        });
        coding.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "present_plan".to_owned(),
            status: ToolStatus::Ok,
            content: String::new(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        assert!(coding.plan_choice.is_none());

        let mut restored = state();

        restored.header.mode = t8ful_core::policy::Mode::Plan;
        restored.apply_event(AgentEvent::HistoryRestored {
            entries: vec![HistoryEntry {
                role: HistoryRole::Tool,
                images: Vec::new(),
                text: "the plan was stored for review".to_owned(),
                tool: Some("present_plan".to_owned()),
                arguments: Some(serde_json::json!({ "plan": "# Restored" })),
                status: Some(ToolStatus::Ok),
                truncated: false,
            }],
        });

        assert!(restored.plan_choice.is_none());
    }

    #[test]
    fn moving_back_restores_answers_and_custom_drafts() {
        let mut progress = super::QuestionProgress::begin(two_questions());

        assert!(!progress.back());
        assert!(!progress.forward());

        assert!(progress.choose(0).is_none());
        assert_eq!(progress.index, 1);
        assert_eq!(progress.answers().len(), 1);

        assert!(progress.back());
        assert_eq!(progress.index, 0);
        assert_eq!(progress.highlight, 0);
        assert_eq!(progress.answers().len(), 1);

        assert!(progress.forward());
        assert_eq!(progress.index, 1);

        let mut progress = super::QuestionProgress::begin(multi_first_questions());

        progress.toggle_option(0);
        progress.toggle_custom();
        progress.custom.set_text("extra");
        progress.submit_custom();
        assert!(progress.done().is_none());
        assert_eq!(progress.index, 1);

        assert!(progress.back());
        assert_eq!(progress.index, 0);
        assert_eq!(progress.checked, vec![0]);
        assert_eq!(progress.custom_draft.as_deref(), Some("extra"));
        assert_eq!(progress.highlight, 0);

        assert!(progress.forward());
        assert_eq!(progress.index, 1);
    }

    #[test]
    fn multi_select_checks_custom_text_and_empty_drafts_are_stored() {
        let mut progress = super::QuestionProgress::begin(multi_question());

        progress.toggle_option(1);
        progress.toggle_option(0);

        assert_eq!(progress.checked, vec![0, 1]);
        assert_eq!(progress.index, 0);
        assert!(progress.answers().is_empty());

        progress.toggle_option(0);

        assert_eq!(progress.checked, vec![1]);

        let mut progress = super::QuestionProgress::begin(multi_question());

        assert!(progress.done().is_none());
        assert!(progress.answers().is_empty());

        progress.toggle_option(1);
        progress.toggle_custom();
        progress.custom.set_text("and benches");
        assert!(progress.submit_custom().is_none());
        assert_eq!(progress.custom_draft.as_deref(), Some("and benches"));
        assert!(!progress.is_custom());

        let answers = progress.done().expect("answers");

        assert_eq!(answers[0].selected, vec![1]);
        assert_eq!(answers[0].custom.as_deref(), Some("and benches"));
        assert_eq!(
            answers[0].text(&multi_question().questions[0]),
            "Wide; wrote: and benches"
        );

        let mut progress = super::QuestionProgress::begin(multi_question());

        progress.toggle_option(0);
        progress.toggle_custom();
        progress.custom.set_text("draft");
        progress.submit_custom();
        progress.toggle_custom();
        progress.custom.clear();
        progress.submit_custom();

        assert!(progress.custom_draft.is_none());
        assert!(progress.done().is_some());
    }

    #[test]
    fn review_and_single_question_submission_finish_the_request() {
        let mut progress = super::QuestionProgress::begin(two_questions());

        assert!(progress.choose(0).is_none());
        assert_eq!(progress.index, 1);
        assert!(progress.choose(0).is_none());
        assert!(progress.review());
        assert_eq!(progress.index, 2);
        assert_eq!(progress.answers().len(), 2);
        assert_eq!(progress.submit_review().expect("review").len(), 2);

        assert!(progress.back());
        assert_eq!(progress.index, 1);
        assert!(!progress.review());

        let mut progress =
            super::QuestionProgress::begin(single_question("Scope", "How wide?", false));

        let answers = progress.choose(1).expect("answers");

        assert_eq!(answers[0].selected, vec![1]);
        assert!(!progress.review());
    }

    fn two_questions() -> t8ful_core::question::QuestionRequest {
        use t8ful_core::question::{Question, QuestionOption, QuestionRequest};

        QuestionRequest {
            questions: vec![
                Question {
                    header: "Scope".to_owned(),
                    question: "How wide?".to_owned(),
                    options: vec![
                        QuestionOption {
                            label: "Narrow".to_owned(),
                            description: "one crate".to_owned(),
                            preview: None,
                        },
                        QuestionOption {
                            label: "Wide".to_owned(),
                            description: "workspace".to_owned(),
                            preview: None,
                        },
                    ],
                    multi_select: false,
                },
                Question {
                    header: "Tests".to_owned(),
                    question: "Which suite?".to_owned(),
                    options: vec![QuestionOption {
                        label: "Fast".to_owned(),
                        description: "unit only".to_owned(),
                        preview: None,
                    }],
                    multi_select: false,
                },
            ],
        }
    }

    fn single_question(
        header: &str,
        prompt: &str,
        multi_select: bool,
    ) -> t8ful_core::question::QuestionRequest {
        use t8ful_core::question::{Question, QuestionOption, QuestionRequest};

        QuestionRequest {
            questions: vec![Question {
                header: header.to_owned(),
                question: prompt.to_owned(),
                options: vec![
                    QuestionOption {
                        label: "Narrow".to_owned(),
                        description: "one crate".to_owned(),
                        preview: None,
                    },
                    QuestionOption {
                        label: "Wide".to_owned(),
                        description: "workspace".to_owned(),
                        preview: None,
                    },
                ],
                multi_select,
            }],
        }
    }

    fn multi_question() -> t8ful_core::question::QuestionRequest {
        single_question("Scope", "How wide?", true)
    }

    fn multi_first_questions() -> t8ful_core::question::QuestionRequest {
        let mut request = two_questions();

        request.questions[0].multi_select = true;
        request
    }

    #[test]
    fn a_failed_present_plan_keeps_the_ordinary_tool_card() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "present_plan".to_owned(),
            arguments: serde_json::json!({ "plan": "not reached" }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "present_plan".to_owned(),
            status: ToolStatus::Error,
            content: "the plan must not be empty".to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        assert_eq!(state.entries.len(), 1);
        assert!(matches!(
            &state.entries[0],
            Entry::Tool { name, status, .. } if name == "present_plan" && status == "error"
        ));
    }

    #[test]
    fn only_a_user_switch_draws_a_mode_divider() {
        let mut state = state();

        state.apply_event(AgentEvent::ModeChanged {
            mode: t8ful_core::policy::Mode::Plan,
            previous: None,
        });

        assert!(state.entries.is_empty());
        assert_eq!(state.header.mode, t8ful_core::policy::Mode::Plan);

        state.apply_event(AgentEvent::ModeChanged {
            mode: t8ful_core::policy::Mode::Coding,
            previous: Some(t8ful_core::policy::Mode::Plan),
        });

        assert_eq!(
            state.entries,
            vec![Entry::ModeChange {
                from: t8ful_core::policy::Mode::Plan,
                to: t8ful_core::policy::Mode::Coding,
            }]
        );
    }

    #[test]
    fn a_restored_present_plan_entry_becomes_a_plan_block() {
        let mut state = state();

        state.apply_event(AgentEvent::HistoryRestored {
            entries: vec![HistoryEntry {
                role: HistoryRole::Tool,
                images: Vec::new(),
                text: "the plan was stored for review".to_owned(),
                tool: Some("present_plan".to_owned()),
                arguments: Some(serde_json::json!({ "plan": "# Restored\n\nBody." })),
                status: Some(ToolStatus::Ok),
                truncated: false,
            }],
        });

        assert_eq!(
            state.entries,
            vec![Entry::Plan {
                body: "# Restored\n\nBody.".to_owned(),
                mode: t8ful_core::policy::Mode::Coding,
                placement: CardPlacement::default(),
            }]
        );
    }

    #[test]
    fn patch_calls_carry_their_unified_diff_as_a_preview() {
        let mut state = state();
        let patch = "@@ -1 +1 @@\n-old\n+new";

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "apply_patch".to_owned(),
            arguments: serde_json::json!({ "patch": patch }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "apply_patch".to_owned(),
            status: ToolStatus::Ok,
            content: "applied patch to 1 file(s): src/lib.rs (7 bytes)".to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        let Entry::Tool { preview, .. } = &state.entries[0] else {
            panic!("expected a tool entry");
        };

        assert_eq!(preview.kind, super::PreviewKind::Diff);
        assert_eq!(preview.text, patch);
    }

    #[test]
    fn a_recall_card_shows_the_query_and_hides_the_model_wrapper() {
        let mut state = state();
        let content = "<recall scope=\"all\">\n1 matching message from 22 sessions. Only \
                      prompts and assistant replies are searched.\n\n- abc · user · 2h ago · \
                      coding · /work\n  title: Steering\n  «steering» «recall»\n\nResume one \
                      with `t8ful --resume <id>`.\n</recall>";

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "recall".to_owned(),
            arguments: serde_json::json!({ "query": "steering recall", "scope": "all" }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "recall".to_owned(),
            status: ToolStatus::Ok,
            content: content.to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        let Entry::Tool { title, preview, .. } = &state.entries[0] else {
            panic!("expected a tool entry");
        };

        assert_eq!(title, "\"steering recall\"");
        assert_eq!(preview.kind, super::PreviewKind::Code);
        assert!(preview.text.starts_with("scope: all\n"), "{}", preview.text);
        assert!(!preview.text.contains("<recall"), "{}", preview.text);
        assert!(!preview.text.contains("</recall>"), "{}", preview.text);
        assert!(
            preview.text.contains("«steering» «recall»"),
            "{}",
            preview.text
        );
    }

    #[test]
    fn read_results_use_the_path_extension_for_highlighting() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "read_file".to_owned(),
            arguments: serde_json::json!({ "path": "src/lib.rs" }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "read_file".to_owned(),
            status: ToolStatus::Ok,
            content: "fn main() {}".to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        let Entry::Tool { preview, .. } = &state.entries[0] else {
            panic!("expected a tool entry");
        };

        assert_eq!(preview.kind, super::PreviewKind::Code);
        assert_eq!(preview.language.as_deref(), Some("rs"));
        assert_eq!(preview.text, "fn main() {}");
    }

    #[test]
    fn diff_like_bash_output_becomes_a_diff_preview() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            arguments: serde_json::json!({ "command": "git diff" }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "bash".to_owned(),
            status: ToolStatus::Ok,
            content: "diff --git a/a b/a\n@@ -1 +1 @@\n-old\n+new".to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        let Entry::Tool { preview, .. } = &state.entries[0] else {
            panic!("expected a tool entry");
        };

        assert_eq!(preview.kind, super::PreviewKind::Diff);
        assert!(preview.text.contains("@@ -1 +1 @@"));
    }

    #[test]
    fn denied_calls_do_not_show_an_argument_preview() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "edit_file".to_owned(),
            arguments: serde_json::json!({
                "path": "src/lib.rs",
                "edits": [{ "old_text": "old", "new_text": "new" }]
            }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "edit_file".to_owned(),
            status: ToolStatus::Denied,
            content: "denied by the user".to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        let Entry::Tool { preview, .. } = &state.entries[0] else {
            panic!("expected a tool entry");
        };

        assert!(preview.is_empty());
    }

    #[test]
    fn edit_calls_become_a_small_old_new_diff() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "edit_file".to_owned(),
            arguments: serde_json::json!({
                "path": "src/lib.rs",
                "edits": [{ "old_text": "let a = 1;", "new_text": "let a = 2;" }]
            }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "edit_file".to_owned(),
            status: ToolStatus::Ok,
            content: "replaced 1 occurrence in src/lib.rs".to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        let Entry::Tool { preview, .. } = &state.entries[0] else {
            panic!("expected a tool entry");
        };

        assert_eq!(preview.kind, super::PreviewKind::Diff);
        assert!(preview.text.contains("@@ edit 1 @@"));
        assert!(preview.text.contains("-let a = 1;"));
        assert!(preview.text.contains("+let a = 2;"));
    }

    #[test]
    fn multiple_edits_render_as_separate_hunks() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "edit_file".to_owned(),
            arguments: serde_json::json!({
                "path": "src/lib.rs",
                "edits": [
                    { "old_text": "one", "new_text": "two" },
                    { "old_text": "three", "new_text": "four" }
                ]
            }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "edit_file".to_owned(),
            status: ToolStatus::Ok,
            content: "applied 2 edits (2 replacements)".to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        let Entry::Tool { preview, .. } = &state.entries[0] else {
            panic!("expected a tool entry");
        };

        assert!(preview.text.contains("@@ edit 1 @@\n-one\n+two"));
        assert!(preview.text.contains("@@ edit 2 @@\n-three\n+four"));
    }

    #[test]
    fn a_restored_legacy_edit_still_renders_a_preview() {
        let mut state = state();

        state.apply_event(AgentEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            tool: "edit_file".to_owned(),
            arguments: serde_json::json!({
                "path": "src/lib.rs",
                "old_text": "old",
                "new_text": "new"
            }),
        });
        state.apply_event(AgentEvent::ToolResult {
            call_id: "call-1".to_owned(),
            tool: "edit_file".to_owned(),
            status: ToolStatus::Ok,
            content: "applied 1 edit (1 replacement)".to_owned(),
            truncated: false,
            process_id: None,
            images: Vec::new(),
        });

        let Entry::Tool { preview, .. } = &state.entries[0] else {
            panic!("expected a tool entry");
        };

        assert!(preview.text.contains("@@ edit 1 @@\n-old\n+new"));
    }

    #[test]
    fn compaction_status_uses_real_token_counts() {
        let mut state = state();

        state.apply_event(AgentEvent::CompactionStarted {
            estimated_tokens: 412_000,
            context_limit: 1_048_576,
        });

        assert_eq!(state.status, "compacting context (412k/1.0M tokens)");
    }
}

#[cfg(test)]
mod session_tests {
    use super::{SessionSnapshot, SessionSummary};

    fn summary(id: &str, title: &str, last_message_at: u64) -> SessionSummary {
        SessionSummary {
            id: id.to_owned(),
            title: title.to_owned(),
            last_message_at,
            project: "project".to_owned(),
            resumable: true,
        }
    }

    #[test]
    fn apply_sessions_tracks_the_first_scan() {
        let mut state = super::AppState::new(super::Header {
            model: "vendor/model".to_owned(),
            mode: t8ful_core::policy::Mode::Coding,
            session_id: "session-1".to_owned(),
            project_root: std::path::PathBuf::from("/work/project"),
            launch_dir: std::path::PathBuf::from("/work/project"),
            version: "0.1.0".to_owned(),
            branch: None,
        });

        assert!(!state.sessions_loaded());

        state.apply_sessions(SessionSnapshot::Pending);
        assert!(!state.sessions_loaded());

        state.apply_sessions(SessionSnapshot::Loaded {
            sessions: vec![summary("session-1", "current", 1)],
        });
        assert!(state.sessions_loaded());
        assert_eq!(state.sessions.len(), 1);
    }
}

/// Where the current reasoning level list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningSource {
    /// Derived from the capability flag; a published list may replace it.
    Default,
    /// Published by the driver for the selected model.
    Model,
}

/// Move an unsupported effort to the nearest level the model offers.
fn clamp_effort(levels: &[ReasoningEffort], effort: ReasoningEffort) -> ReasoningEffort {
    if levels.contains(&effort) || levels.is_empty() {
        return effort;
    }

    let start = ReasoningEffort::ALL
        .iter()
        .position(|candidate| *candidate == effort)
        .unwrap_or_default();

    ReasoningEffort::ALL
        .iter()
        .skip(start)
        .chain(ReasoningEffort::ALL.iter().take(start).rev())
        .find(|candidate| levels.contains(candidate))
        .copied()
        .unwrap_or(ReasoningEffort::Default)
}
