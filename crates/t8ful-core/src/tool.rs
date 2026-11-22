use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::content::ImageInput;
use crate::ids::CallId;
use crate::policy::Mode;
use crate::provider::ReasoningEffort;
use crate::question::QuestionRouter;
use crate::readscope::ReadScope;
use crate::sandbox::Sandbox;

/// Model-visible description of a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing accepted arguments.
    pub parameters: serde_json::Value,
}

/// What a tool can do to the world, used by the central policy gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    ReadOnly,
    Mutating,
    Process,
    /// A control tool that returns information to the user without touching the
    /// workspace (questions, plan presentation).
    Control,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: CallId,
    pub name: String,
    pub arguments: serde_json::Value,
}

impl ToolCall {
    pub fn new(
        id: impl Into<CallId>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Error,
    Denied,
    /// The call may or may not have run; it must never be replayed automatically.
    Indeterminate,
    /// The call was not started because the turn ended first.
    NotExecuted,
    /// The call needs an interactive answer that was unavailable.
    InputRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: CallId,
    pub name: String,
    pub status: ToolStatus,
    pub content: String,
    pub truncated: bool,
    /// Images this result carries, stored in session-owned attachment storage.
    ///
    /// The bytes never travel inside `content`: a chat-completions `tool`
    /// message is text, so the transport hoists these into a user message as
    /// `image_url` parts. `serde(default)` keeps journals written before this
    /// field existed loading unchanged.
    #[serde(default)]
    pub images: Vec<ImageInput>,
}

impl ToolResult {
    pub fn new(
        call: &ToolCall,
        status: ToolStatus,
        content: impl Into<String>,
        truncated: bool,
    ) -> Self {
        Self {
            call_id: call.id.clone(),
            name: call.name.clone(),
            status,
            content: content.into(),
            truncated,
            images: Vec::new(),
        }
    }

    /// Attach images the result carries for the model to see.
    pub fn with_images(mut self, images: Vec<ImageInput>) -> Self {
        self.images = images;
        self
    }

    pub fn denied(call: &ToolCall, reason: impl Into<String>) -> Self {
        Self::new(call, ToolStatus::Denied, reason, false)
    }

    pub fn not_executed(call: &ToolCall, reason: impl Into<String>) -> Self {
        Self::new(call, ToolStatus::NotExecuted, reason, false)
    }
}

/// Streaming output callback handed to a running tool.
///
/// Progress is display-only: the tool's returned output stays authoritative, and
/// a reporter that has reached its byte budget simply drops later chunks.
type ProgressCallback = dyn Fn(&[u8]) + Send + Sync;

#[derive(Clone)]
pub struct ProgressReporter {
    callback: Arc<ProgressCallback>,
}

impl ProgressReporter {
    pub fn new(callback: impl Fn(&[u8]) + Send + Sync + 'static) -> Self {
        Self {
            callback: Arc::new(callback),
        }
    }

    pub fn report(&self, bytes: &[u8]) {
        (self.callback)(bytes);
    }
}

impl fmt::Debug for ProgressReporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProgressReporter")
            .field("has_progress", &true)
            .finish()
    }
}

/// Incremental UTF-8 decoder for streamed tool output.
///
/// Reads hand over arbitrary byte chunks, and stdout and stderr can split a
/// multibyte character across two reads. The decoder holds an incomplete tail
/// until the next chunk instead of replacing it with U+FFFD; genuinely invalid
/// bytes still become the replacement character.
#[derive(Debug, Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode as much as possible and keep an incomplete trailing sequence.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);

        let mut output = String::new();

        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    output.push_str(text);
                    self.pending.clear();

                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();

                    if valid > 0 {
                        output.push_str(
                            std::str::from_utf8(&self.pending[..valid])
                                .expect("the decoder reported this prefix as valid"),
                        );
                    }

                    match error.error_len() {
                        Some(invalid) => {
                            output.push('\u{FFFD}');
                            self.pending.drain(..valid + invalid);
                        }
                        None => {
                            // An incomplete sequence at the end: keep it for the
                            // next chunk rather than emitting a broken character.
                            self.pending.drain(..valid);

                            break;
                        }
                    }
                }
            }
        }

        output
    }

    /// Flush whatever is buffered, replacing a still-incomplete tail.
    pub fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }

        let tail = String::from_utf8_lossy(&self.pending).into_owned();

        self.pending.clear();

        tail
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// A [`ProgressReporter`] wrapper with its own incremental decoder.
///
/// Each output stream gets one of these, so stdout holding a split character
/// until its next read never absorbs bytes from stderr.
pub struct Utf8Progress {
    reporter: ProgressReporter,
    stream: Utf8Stream,
}

impl Utf8Progress {
    pub fn new(reporter: ProgressReporter) -> Self {
        Self {
            reporter,
            stream: Utf8Stream::new(),
        }
    }

    pub fn report(&mut self, bytes: &[u8]) {
        let text = self.stream.push(bytes);

        if !text.is_empty() {
            self.reporter.report(text.as_bytes());
        }
    }

    /// Flush an incomplete tail when the stream ends.
    pub fn finish(&mut self) {
        let tail = self.stream.finish();

        if !tail.is_empty() {
            self.reporter.report(tail.as_bytes());
        }
    }
}

/// Execution environment shared by every tool in a run.
#[derive(Clone)]
pub struct ToolContext {
    pub project_root: PathBuf,
    pub launch_dir: PathBuf,
    pub default_timeout: Duration,
    pub max_output_bytes: usize,
    pub deny_env: Vec<String>,
    /// Variables the caller wants set for every shell command in this context.
    ///
    /// Used by isolated children to inherit a shared build cache; never derived
    /// from model input. Extras are withheld when `deny_env` denies the name,
    /// so an extra can add a variable but never reintroduce a denied one.
    pub extra_env: Vec<(String, String)>,
    /// The agent's mode at dispatch time.
    ///
    /// The dispatching agent fills the live values into every copy it hands to a
    /// tool, so a tool that starts work on the agent's behalf reads the mode
    /// that applies now rather than the one this context was constructed with.
    pub mode: Mode,
    /// The agent's model, window, and reasoning level at dispatch time.
    ///
    /// Filled for the same reason as `mode`: a child agent must not inherit the
    /// model the parent has since switched away from.
    pub model: String,
    pub context_limit: Option<u64>,
    /// The output budget that goes with the window above, so a child is not
    /// handed a live window with the budget of the model the parent left.
    pub max_output_tokens: Option<u32>,
    pub reasoning_effort: ReasoningEffort,
    pub sandbox: Option<Arc<dyn Sandbox>>,
    /// Read-only mount roots used by plan-mode shell execution.
    pub read_scope: Option<ReadScope>,
    pub questions: Option<Arc<dyn QuestionRouter>>,
    pub max_image_encoded_bytes: usize,
    /// Set by the agent loop so long-running tools can stream display-only
    /// output while they work.
    pub progress: Option<ProgressReporter>,
}

impl ToolContext {
    pub fn new(project_root: PathBuf, launch_dir: PathBuf) -> Self {
        Self {
            project_root,
            launch_dir,
            default_timeout: Duration::from_secs(120),
            max_output_bytes: 256 * 1024,
            deny_env: Vec::new(),
            extra_env: Vec::new(),
            mode: Mode::Coding,
            model: String::new(),
            context_limit: None,
            max_output_tokens: None,
            reasoning_effort: ReasoningEffort::Default,
            sandbox: None,
            read_scope: None,
            questions: None,
            max_image_encoded_bytes: crate::DEFAULT_MAX_IMAGE_ENCODED_BYTES,
            progress: None,
        }
    }

    /// Report a display-only output chunk to the agent loop, if it asked.
    pub fn report_progress(&self, bytes: &[u8]) {
        if let Some(progress) = &self.progress {
            progress.report(bytes);
        }
    }
}

impl fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolContext")
            .field("project_root", &self.project_root)
            .field("launch_dir", &self.launch_dir)
            .field("default_timeout", &self.default_timeout)
            .field("max_output_bytes", &self.max_output_bytes)
            .field("deny_env", &self.deny_env)
            .field("extra_env", &self.extra_env.len())
            .field("mode", &self.mode)
            .field("model", &self.model)
            .field("context_limit", &self.context_limit)
            .field("reasoning_effort", &self.reasoning_effort)
            .field(
                "sandbox",
                &self.sandbox.as_ref().map(|sandbox| sandbox.name()),
            )
            .field("read_scope", &self.read_scope)
            .field("has_questions", &self.questions.is_some())
            .field("max_image_encoded_bytes", &self.max_image_encoded_bytes)
            .field("has_progress", &self.progress.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub truncated: bool,
    pub metadata: Option<serde_json::Value>,
    /// Images the tool returned, already in session-owned storage.
    ///
    /// The agent copies them onto the `ToolResult`, which is what the transport
    /// reads when it builds the request.
    pub images: Vec<ImageInput>,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            truncated: false,
            metadata: None,
            images: Vec::new(),
        }
    }

    pub fn with_truncation(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }

    pub fn with_images(mut self, images: Vec<ImageInput>) -> Self {
        self.images = images;
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments for tool: {0}")]
    InvalidArguments(String),

    #[error("{0}")]
    Failed(String),

    #[error("tool cancelled")]
    Cancelled,

    /// The tool needs a user answer, but no interactive channel is available.
    #[error("{0}")]
    InputRequired(String),
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    /// A tool-specific explanation for the policy gate's denial.
    ///
    /// Used when the generic plan-mode message would be misleading, so the
    /// model and the transcript see why this particular call was refused.
    fn denial_reason(&self) -> Option<String> {
        None
    }

    async fn execute(
        &self,
        call: &ToolCall,
        context: &ToolContext,
    ) -> Result<ToolOutput, ToolError>;
}

/// Name-keyed collection of available tools.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.insert(tool.spec().name, Arc::new(tool));
    }

    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.spec().name, tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|tool| tool.spec()).collect()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// Remove the named tools and report how many were present.
    ///
    /// Withholding happens at registration rather than at dispatch: the model
    /// receives `specs()`, so a disabled tool is never offered and no call can
    /// arrive for it.
    pub fn without(&mut self, disabled: &BTreeSet<String>) -> usize {
        let mut removed = 0;

        for name in disabled {
            if self.tools.remove(name).is_some() {
                removed += 1;
            }
        }

        removed
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.names())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use async_trait::async_trait;

    use super::{
        Tool, ToolCall, ToolContext, ToolError, ToolOutput, ToolRegistry, ToolSpec, Utf8Stream,
    };

    struct Named(&'static str);

    #[async_trait]
    impl Tool for Named {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.to_owned(),
                description: String::new(),
                parameters: serde_json::json!({ "type": "object" }),
            }
        }

        async fn execute(
            &self,
            _call: &ToolCall,
            _context: &ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("ok"))
        }
    }

    #[test]
    fn withholding_removes_the_named_tools_and_counts_them() {
        let mut registry = ToolRegistry::new();

        registry.register(Named("keep"));
        registry.register(Named("drop"));
        registry.register(Named("other"));

        let removed = registry.without(&BTreeSet::from(["drop".to_owned(), "absent".to_owned()]));

        assert_eq!(removed, 1);
        assert_eq!(registry.names(), vec!["keep", "other"]);
        assert!(registry.get("keep").is_some());
        assert!(registry.get("drop").is_none());
        assert!(registry.specs().iter().all(|spec| spec.name != "drop"));
    }

    #[test]
    fn a_split_multibyte_character_is_decoded_once() {
        let mut stream = Utf8Stream::new();

        assert_eq!(stream.push(&[0xc3]), "");
        assert!(stream.has_pending());
        assert_eq!(stream.push(&[0xa9]), "é");
        assert!(!stream.has_pending());
        assert_eq!(stream.finish(), "");
    }

    #[test]
    fn invalid_bytes_become_one_replacement_character() {
        let mut stream = Utf8Stream::new();

        assert_eq!(stream.push(b"a\xffb"), "a\u{FFFD}b");
        assert!(!stream.has_pending());
    }

    #[test]
    fn an_incomplete_tail_is_replaced_when_the_stream_ends() {
        let mut stream = Utf8Stream::new();

        assert_eq!(stream.push(b"ok\xe2\x82"), "ok");
        assert_eq!(stream.finish(), "\u{FFFD}");
        assert!(!stream.has_pending());
    }
}
