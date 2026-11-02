//! Configuration loading and precedence.
//!
//! Precedence, highest first: CLI flags, environment, project, user, system,
//! built-in defaults. Credentials are read from the environment only and never
//! persisted into generated configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use t8ful_core::policy::Mode;
use t8ful_core::provider::ReasoningEffort;
use t8ful_tui::ThemeName;

use crate::root::RootInfo;
use crate::trust::ProjectTrust;

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_PROVIDER: &str = "openrouter";
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;
pub const DEFAULT_CONTEXT_LIMIT_FALLBACK: u64 = t8ful_core::DEFAULT_CONTEXT_LIMIT_FALLBACK;
pub const DEFAULT_CATALOG_TIMEOUT_MS: u64 = 10_000;

/// Every tool name `[tools] disabled` accepts.
///
/// Conditionally registered tools are accepted even when the feature that adds
/// them is off, so one configuration file works whether or not `[agents]` is
/// enabled.
pub fn known_tool_names() -> Vec<&'static str> {
    let mut names = t8ful_tools::BUILTIN_TOOL_NAMES.to_vec();

    names.extend([
        "recall",
        "task_create",
        "task_update",
        "task_list",
        "spawn_agents",
        "agent_wait",
        "agent_status",
    ]);

    names
}

/// Model the terminal interface runs with when none is configured.
///
/// OpenRouter exposes no default-model field on `/models` or `/auth/key`, so
/// this is built in rather than fetched. `--model`, `T8FUL_MODEL`, and
/// `provider.model` all win; a headless run has no fallback and still refuses
/// to start without one.
pub const DEFAULT_MODEL: &str = "deepseek/deepseek-v4.1-flash-1017";

/// Configuration input read from any file layer.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct FileConfig {
    pub provider: ProviderSection,
    pub agent: AgentSection,
    pub shell: ShellSection,
    pub ui: UiSection,
    pub skills: SkillsSection,
    pub agents: AgentsSection,
    pub tools: ToolsSection,
    pub mcp: t8ful_mcp::McpSection,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ProviderSection {
    pub kind: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    /// Deadline for the model catalog request, in milliseconds.
    pub catalog_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentSection {
    pub max_steps: Option<u32>,
    pub max_output_tokens: Option<u32>,
    /// Explicit context window for the active model. Wins over catalog metadata.
    pub context_limit: Option<u64>,
    /// Used only when the catalog has no usable context length for the model.
    pub context_limit_fallback: Option<u64>,
    pub mode: Option<Mode>,
    pub reasoning_effort: Option<String>,
    /// Per-model reasoning overrides: `[agent.models."vendor/model"]`.
    pub models: BTreeMap<String, ModelReasoningSection>,
    pub system_prompt_extra: Option<String>,
}

/// One model's reasoning overrides, in Pi's vocabulary.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ModelReasoningSection {
    /// Provider value per level, or `unsupported` to hide the level.
    pub thinking_level_map: BTreeMap<String, ThinkingLevelMapping>,
}

/// How one common level maps onto a model's own vocabulary.
///
/// A bare string is the provider value to send; the literal `unsupported`
/// hides the level, mirroring Pi's `null` entry (TOML has no null).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ThinkingLevelMapping {
    /// The model does not offer this level.
    Unsupported(HiddenLevel),
    /// Send this value when the level is selected.
    Value(String),
}

/// The literal `"unsupported"` spelling for a hidden level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HiddenLevel {
    Unsupported,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ShellSection {
    pub default_timeout_ms: Option<u64>,
    pub max_output_bytes: Option<usize>,
    pub deny_env: Option<Vec<String>>,
    /// Wake an idle agent when a background process exits. On by default.
    pub notify_on_exit: Option<bool>,
}

/// Per-tool switches. A disabled tool is never offered to the model.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ToolsSection {
    /// Tool names withheld from the model. Unknown names are a startup error.
    pub disabled: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SkillsSection {
    /// Extra trusted skill roots. Only user configuration and an explicitly
    /// selected file may contribute these; project and system configuration
    /// cannot widen the plan-mode shell mount set.
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentsSection {
    /// Whether child agents may be spawned. On by default.
    pub enabled: Option<bool>,
    /// Accepted for compatibility and ignored: children are single level.
    pub max_depth: Option<u32>,
    /// Children running at once across the session.
    pub max_concurrent: Option<usize>,
    /// Children one root turn may start in total.
    pub max_spawns_per_turn: Option<u32>,
    /// Directory worktrees are created under. Defaults to the state directory,
    /// deliberately outside the project so a worktree is never walked by its own
    /// build.
    pub worktree_root: Option<PathBuf>,
    /// A shared `CARGO_TARGET_DIR` exported into children.
    ///
    /// Without one, N parallel writers each pay a cold build of the whole
    /// workspace; with one, their builds serialize on a warm cache instead.
    pub shared_target_dir: Option<PathBuf>,
    /// Start a turn to deliver a completion notice to an idle parent.
    ///
    /// On by default: a notice nobody reads is not a notification. It spends
    /// money unattended, so it can be turned off.
    pub wake_on_finish: Option<bool>,
    /// Reusable child profiles by name.
    pub roles: BTreeMap<String, RoleSection>,
}

/// One child profile.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RoleSection {
    /// Extra instructions prepended to the child's system prompt.
    pub instructions: Option<String>,
    /// Tools the role may use. Empty means every tool the parent has.
    pub tools: Option<Vec<String>>,
    /// Model for this role. Unset inherits the parent's model.
    pub model: Option<String>,
    /// `shared` or `worktree`. Unset means shared.
    pub isolation: Option<String>,
}

impl AgentsSection {
    fn merge_from(&mut self, higher: AgentsSection) {
        self.enabled = higher.enabled.or(self.enabled);
        self.max_depth = higher.max_depth.or(self.max_depth);
        self.max_concurrent = higher.max_concurrent.or(self.max_concurrent);
        self.max_spawns_per_turn = higher.max_spawns_per_turn.or(self.max_spawns_per_turn);
        self.worktree_root = higher.worktree_root.or(self.worktree_root.take());
        self.shared_target_dir = higher.shared_target_dir.or(self.shared_target_dir.take());
        self.wake_on_finish = higher.wake_on_finish.or(self.wake_on_finish);

        if !higher.roles.is_empty() {
            self.roles.extend(higher.roles);
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct UiSection {
    pub theme: Option<String>,
    pub max_image_encoded_bytes: Option<usize>,
    /// Enable terminal mouse capture and wheel scrolling.
    pub mouse: Option<bool>,
    /// Show provider reasoning as dim italic text. Hidden by default.
    pub show_reasoning: Option<bool>,
    /// Ring the bell and raise a desktop notification. On by default.
    pub notifications: Option<bool>,
    /// Command that replaces `notify-send t8ful <message>`.
    pub notify_command: Option<Vec<String>>,
}

impl FileConfig {
    /// Overlay `higher` on top of `self`; values in `higher` win.
    fn merge(mut self, higher: FileConfig) -> FileConfig {
        self.provider.kind = higher.provider.kind.or(self.provider.kind);
        self.provider.model = higher.provider.model.or(self.provider.model);
        self.provider.base_url = higher.provider.base_url.or(self.provider.base_url);
        self.provider.catalog_timeout_ms = higher
            .provider
            .catalog_timeout_ms
            .or(self.provider.catalog_timeout_ms);
        self.agent.max_steps = higher.agent.max_steps.or(self.agent.max_steps);
        self.agent.max_output_tokens = higher
            .agent
            .max_output_tokens
            .or(self.agent.max_output_tokens);
        self.agent.context_limit = higher.agent.context_limit.or(self.agent.context_limit);
        self.agent.context_limit_fallback = higher
            .agent
            .context_limit_fallback
            .or(self.agent.context_limit_fallback);
        self.agent.mode = higher.agent.mode.or(self.agent.mode);
        self.agent.reasoning_effort = higher
            .agent
            .reasoning_effort
            .or(self.agent.reasoning_effort);
        self.agent.system_prompt_extra = higher
            .agent
            .system_prompt_extra
            .or(self.agent.system_prompt_extra);
        if !higher.agent.models.is_empty() {
            self.agent.models = higher.agent.models;
        }
        self.shell.default_timeout_ms = higher
            .shell
            .default_timeout_ms
            .or(self.shell.default_timeout_ms);
        self.shell.max_output_bytes = higher
            .shell
            .max_output_bytes
            .or(self.shell.max_output_bytes);
        self.shell.deny_env = higher.shell.deny_env.or(self.shell.deny_env);
        self.shell.notify_on_exit = higher.shell.notify_on_exit.or(self.shell.notify_on_exit);
        self.ui.theme = higher.ui.theme.or(self.ui.theme);
        self.ui.max_image_encoded_bytes = higher
            .ui
            .max_image_encoded_bytes
            .or(self.ui.max_image_encoded_bytes);
        self.ui.mouse = higher.ui.mouse.or(self.ui.mouse);
        self.ui.show_reasoning = higher.ui.show_reasoning.or(self.ui.show_reasoning);
        self.ui.notifications = higher.ui.notifications.or(self.ui.notifications);
        self.ui.notify_command = higher.ui.notify_command.or(self.ui.notify_command.take());
        if !higher.skills.paths.is_empty() {
            self.skills.paths = higher.skills.paths;
        }
        if !higher.tools.disabled.is_empty() {
            self.tools.disabled = higher.tools.disabled;
        }
        self.mcp.startup_timeout_ms = higher
            .mcp
            .startup_timeout_ms
            .or(self.mcp.startup_timeout_ms);
        // Servers are merged by name: a higher layer replaces one definition
        // whole rather than blending a local command with a remote URL.
        self.mcp.servers.extend(higher.mcp.servers);
        self.agents.merge_from(higher.agents);

        self
    }

    /// Apply values from `overrides` where present.
    fn apply_overrides(&mut self, overrides: &CliOverrides) {
        if let Some(model) = &overrides.model {
            self.provider.model = Some(model.clone());
        }

        if let Some(max_steps) = overrides.max_steps {
            self.agent.max_steps = Some(max_steps);
        }

        if let Some(mode) = overrides.mode {
            self.agent.mode = Some(mode);
        }
    }

    fn apply_env(&mut self, env: &EnvSource) -> Result<(), ConfigError> {
        if let Some(model) = non_empty(env.get("T8FUL_MODEL")) {
            self.provider.model = Some(model);
        }

        if let Some(base_url) = non_empty(env.get("T8FUL_BASE_URL")) {
            self.provider.base_url = Some(base_url);
        }

        if let Some(raw) = non_empty(env.get("T8FUL_MODE")) {
            self.agent.mode = Some(Mode::parse(&raw).ok_or_else(|| {
                ConfigError::Invalid(format!("invalid mode `{raw}`: expected `plan` or `coding`"))
            })?);
        }

        Ok(())
    }
}

/// Highest-priority command-line values.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub model: Option<String>,
    pub max_steps: Option<u32>,
    pub mode: Option<Mode>,
}

/// Injectable environment lookup so tests never mutate the process environment.
///
/// `Map` exists for tests and for embedding t8ful with a controlled environment.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum EnvSource {
    Process,
    Map(BTreeMap<String, String>),
}

impl EnvSource {
    pub fn get(&self, name: &str) -> Option<String> {
        match self {
            Self::Process => std::env::var(name).ok(),
            Self::Map(values) => values.get(name).cloned(),
        }
    }
}

/// Where configuration files may live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub system: Vec<PathBuf>,
    /// Directories behind [`ConfigPaths::system`], without the `t8ful` suffix.
    pub system_dirs: Vec<PathBuf>,
    pub user: PathBuf,
    pub project: PathBuf,
    pub explicit: Option<PathBuf>,
}

impl ConfigPaths {
    pub fn discover(root: &Path, explicit: Option<PathBuf>, env: &EnvSource) -> Self {
        let system_dirs = env
            .get("XDG_CONFIG_DIRS")
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                value
                    .split(':')
                    .filter(|part| !part.is_empty())
                    .map(PathBuf::from)
                    .collect::<Vec<_>>()
            })
            .filter(|paths| !paths.is_empty())
            .unwrap_or_else(|| vec![PathBuf::from("/etc/xdg")]);
        let system = system_dirs
            .iter()
            .map(|dir| dir.join("t8ful/config.toml"))
            .collect();

        let user = match non_empty(env.get("XDG_CONFIG_HOME")) {
            Some(home) => PathBuf::from(home).join("t8ful/config.toml"),
            None => non_empty(env.get("HOME"))
                .map(|home| PathBuf::from(home).join(".config/t8ful/config.toml"))
                .unwrap_or_default(),
        };

        Self {
            system,
            system_dirs,
            user,
            project: root.join(".t8ful/config.toml"),
            explicit,
        }
    }

    fn layers(&self) -> Vec<(PathBuf, LayerKind)> {
        let mut layers = self
            .system
            .iter()
            .cloned()
            .map(|path| (path, LayerKind::System))
            .collect::<Vec<_>>();

        layers.push((self.user.clone(), LayerKind::User));
        layers.push((self.project.clone(), LayerKind::Project));

        if let Some(explicit) = &self.explicit {
            layers.push((explicit.clone(), LayerKind::Explicit));
        }

        layers
            .into_iter()
            .filter(|(path, _)| !path.as_os_str().is_empty())
            .collect()
    }
}

/// Provenance of a configuration file layer.
///
/// System and project files are part of the checkout or the host's defaults:
/// they may adjust ordinary behavior but cannot grant authority to run
/// mutating tools or redirect credentials to another endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerKind {
    System,
    User,
    Project,
    Explicit,
}

impl LayerKind {
    fn description(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Project => "project",
            Self::Explicit => "explicitly selected",
        }
    }

    /// Whether this layer may grant authority.
    ///
    /// System configuration never may: it is root-owned and there is nobody to
    /// ask. A project layer may only when the user has trusted that project.
    fn may_grant_authority(self, project: ProjectTrust) -> bool {
        match self {
            Self::User | Self::Explicit => true,
            Self::Project => project.is_trusted(),
            Self::System => false,
        }
    }
}

/// Where a skill root sits in the discovery precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    Project,
    User,
    System,
    Extra,
}

impl SkillSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
            Self::System => "system",
            Self::Extra => "extra",
        }
    }
}

/// One ordered skill discovery root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRoot {
    pub path: PathBuf,
    pub source: SkillSource,
}

/// Fully resolved run settings.
#[derive(Debug, Clone)]
pub struct Settings {
    pub root: RootInfo,
    /// Directory t8ful was launched from; shell commands start here.
    pub launch_dir: PathBuf,
    pub agents: AgentSettings,
    pub model: Option<String>,
    pub base_url: String,
    pub api_key: Option<String>,
    pub mode: Mode,
    pub reasoning_effort: ReasoningEffort,
    /// Per-model reasoning overrides keyed by model id or bare name.
    pub reasoning_models: BTreeMap<String, ModelReasoningSection>,
    /// Per-turn model step limit. `None` leaves a turn unbounded.
    pub max_steps: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub context_limit: Option<u64>,
    /// Used only when the model catalog has no context length for the model.
    pub context_limit_fallback: u64,
    pub max_image_encoded_bytes: usize,
    /// Capture mouse events for wheel scrolling in the TUI.
    pub mouse: bool,
    /// Palette for the full-screen interface.
    pub theme: ThemeName,
    /// Show provider reasoning in the TUI. Display-only.
    pub show_reasoning: bool,
    /// Ring the bell and raise a desktop notification on attention events.
    pub notifications: bool,
    /// Command that replaces the `notify-send` fallback.
    pub notify_command: Vec<String>,
    /// Wake an idle agent when a background process exits.
    pub notify_on_exit: bool,
    /// Deadline for model catalog discovery, independent of chat requests.
    pub catalog_timeout: Duration,
    pub system_prompt_extra: String,
    pub default_timeout: Duration,
    pub max_output_bytes: usize,
    pub deny_env: Vec<String>,
    /// Tools withheld from the model for this run.
    pub tool_disabled: BTreeSet<String>,
    /// Skill roots in precedence order: project, user, system, extras.
    pub skill_roots: Vec<SkillRoot>,
    /// Configured MCP servers, resolved and credential-free.
    pub mcp_servers: Vec<t8ful_mcp::ServerConfig>,
    /// Per-server MCP startup deadline.
    pub mcp_startup_timeout: Duration,
    pub config_sources: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

impl Settings {
    pub fn require_model(&self) -> Result<&str, ConfigError> {
        match self.model.as_deref() {
            Some(model) if !model.trim().is_empty() => Ok(model),
            _ => Err(ConfigError::ModelNotConfigured),
        }
    }
}

/// Fully resolved child-agent settings.
#[derive(Debug, Clone)]
pub struct AgentSettings {
    pub enabled: bool,
    pub limits: t8ful_core::AgentLimits,
    pub worktree_root: Option<PathBuf>,
    pub shared_target_dir: Option<PathBuf>,
    pub wake_on_finish: bool,
    pub roles: BTreeMap<String, t8ful_core::RoleProfile>,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            limits: t8ful_core::AgentLimits::default(),
            worktree_root: None,
            shared_target_dir: None,
            wake_on_finish: true,
            roles: BTreeMap::new(),
        }
    }
}

/// Validate and flatten the `[agents]` section.
fn resolve_agents(
    section: AgentsSection,
    warnings: &mut Vec<String>,
) -> Result<AgentSettings, ConfigError> {
    let mut limits = t8ful_core::AgentLimits::default();

    if section.max_depth.is_some() {
        warnings.push("children are single level; `agents.max_depth` is ignored".to_owned());
    }

    if let Some(concurrent) = section.max_concurrent {
        if concurrent == 0 {
            return Err(ConfigError::Invalid(
                "agents.max_concurrent must be at least 1".to_owned(),
            ));
        }

        limits.max_concurrent = concurrent;
    }

    if let Some(spawns) = section.max_spawns_per_turn {
        if spawns == 0 {
            return Err(ConfigError::Invalid(
                "agents.max_spawns_per_turn must be at least 1".to_owned(),
            ));
        }

        limits.max_spawns_per_turn = spawns;
    }

    let mut roles = BTreeMap::new();

    for (name, role) in section.roles {
        let isolation = match role.isolation.as_deref().map(str::trim) {
            Some("") | None => None,
            Some(raw) => match t8ful_core::Isolation::parse(raw) {
                Some(isolation) => Some(isolation),
                None => {
                    warnings.push(format!(
                        "unknown agents.roles.{name}.isolation `{raw}`; expected `shared` or \
                         `worktree`; using `shared`"
                    ));
                    None
                }
            },
        };

        let tools = role.tools.unwrap_or_default();
        const UNAVAILABLE: &[&str] = &[
            "spawn_agents",
            "agent_wait",
            "agent_status",
            "ask_user_question",
            "present_plan",
        ];

        for tool in &tools {
            if UNAVAILABLE.contains(&tool.as_str()) {
                warnings.push(format!(
                    "`agents.roles.{name}.tools` lists `{tool}`; child agents cannot use it, so \
                     it is ignored"
                ));
            }
        }

        roles.insert(
            name,
            t8ful_core::RoleProfile {
                instructions: role.instructions.unwrap_or_default(),
                tools,
                model: role.model.filter(|model| !model.trim().is_empty()),
                isolation,
            },
        );
    }

    // `general` always exists so an ad hoc spawn needs no configuration.
    roles
        .entry("general".to_owned())
        .or_insert_with(t8ful_core::RoleProfile::default);

    Ok(AgentSettings {
        enabled: section.enabled.unwrap_or(true),
        limits,
        worktree_root: section.worktree_root,
        shared_target_dir: section.shared_target_dir,
        wake_on_finish: section.wake_on_finish.unwrap_or(true),
        roles,
    })
}

/// Validate and deduplicate the `[tools]` section.
///
/// A misspelled name is an error rather than a silent no-op: a typo that
/// disables nothing is indistinguishable from one that disabled the wrong tool.
fn resolve_tools(
    section: ToolsSection,
    roles: &BTreeMap<String, t8ful_core::RoleProfile>,
    warnings: &mut Vec<String>,
) -> Result<BTreeSet<String>, ConfigError> {
    let known = known_tool_names();
    let mut disabled = BTreeSet::new();

    for name in section.disabled {
        let name = name.trim();

        // An `mcp__` name cannot be validated here: the set depends on which
        // servers started. It is resolved against the live registry after
        // startup, and a name that matches nothing is a warning rather than an
        // error because a server may legitimately be down for this run.
        if t8ful_mcp::is_mcp_name(name) {
            disabled.insert(name.to_owned());

            continue;
        }

        if !known.contains(&name) {
            return Err(ConfigError::Invalid(format!(
                "unknown tool `{name}` in `tools.disabled`: expected one of {}",
                known.join(", ")
            )));
        }

        disabled.insert(name.to_owned());
    }

    for (role, profile) in roles {
        for tool in &profile.tools {
            if disabled.contains(tool) {
                warnings.push(format!(
                    "`agents.roles.{role}.tools` lists `{tool}`, which `tools.disabled` withholds; \
                     the role will not have it"
                ));
            }
        }
    }

    Ok(disabled)
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read configuration file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("invalid configuration in {path}: {message}")]
    Parse { path: PathBuf, message: String },

    #[error("invalid configuration: {0}")]
    Invalid(String),

    #[error(
        "no model is configured: pass --model, set T8FUL_MODEL, or set `provider.model` in a t8ful \
         configuration file"
    )]
    ModelNotConfigured,

    #[error("unsupported provider kind `{0}`; only `openrouter` is supported")]
    UnsupportedProvider(String),

    #[error("cannot read project instructions {path}: {source}")]
    Instructions {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error(transparent)]
    Mcp(#[from] t8ful_mcp::ConfigError),
}

/// Read every existing configuration layer, lowest priority first.
fn load(
    paths: &ConfigPaths,
    project: ProjectTrust,
) -> Result<(FileConfig, Vec<PathBuf>, Vec<String>), ConfigError> {
    let mut merged = FileConfig::default();
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    let mut seen_system = false;

    for (path, kind) in paths.layers() {
        if !path.exists() {
            continue;
        }

        if kind == LayerKind::System && seen_system {
            continue;
        }

        let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let mut parsed = parse_layer(&path, &text, &mut warnings)?;

        if !kind.may_grant_authority(project) {
            restrict_untrusted_layer(&mut parsed, kind, &path, &mut warnings);
        }

        merged = merged.merge(parsed);
        sources.push(path);

        if kind == LayerKind::System {
            seen_system = true;
        }
    }

    Ok((merged, sources, warnings))
}

impl FileConfig {
    /// The keys in this layer that a restricted project may not set.
    ///
    /// One predicate, used both to decide whether to ask the user and to
    /// explain what was stripped, so the two can never disagree. An empty
    /// result means this configuration is ordinary and a project carrying only
    /// it never prompts.
    pub fn requires_trust(&self) -> Vec<String> {
        let mut keys = Vec::new();

        if self.provider.base_url.is_some() {
            keys.push("provider.base_url".to_owned());
        }

        if !self.skills.paths.is_empty() {
            keys.push("skills.paths".to_owned());
        }

        if self.ui.notify_command.is_some() {
            keys.push("ui.notify_command".to_owned());
        }

        if matches!(self.agent.mode, Some(Mode::Coding)) {
            keys.push("agent.mode = \"coding\"".to_owned());
        }

        for (name, server) in &self.mcp.servers {
            // Name what the server would do, because "defines an MCP server"
            // does not tell the user whether it runs a program or talks to a
            // remote endpoint with their token.
            let detail = if server.command.is_some() {
                "runs a program at startup"
            } else {
                "connects to a remote endpoint"
            };

            keys.push(format!("mcp.servers.{name} ({detail})"));
        }

        keys
    }
}

/// Keep an untrusted layer to tightening authority, never granting it.
///
/// The exhaustive destructuring is deliberate: a new `FileConfig` section must
/// come with a decision about whether System and Project files may set it.
/// Authority-bearing settings are the provider endpoint, extra skill mounts,
/// the notifier command, and coding mode. The remaining sections are ordinary
/// project configuration and cannot grant a tool the parent does not already
/// have. Values are never echoed in the warnings.
fn restrict_untrusted_layer(
    config: &mut FileConfig,
    kind: LayerKind,
    path: &Path,
    warnings: &mut Vec<String>,
) {
    let FileConfig {
        provider,
        agent,
        // Shell timeouts, deny lists, and exit notifications are ordinary.
        shell: _,
        ui,
        skills,
        // Roles, limits, and worktree paths are ordinary: they narrow or shape
        // the work but cannot widen the parent's authority. So does withholding
        // a tool: it can only take capability away.
        agents: _,
        tools: _,
        mcp,
    } = config;

    if !skills.paths.is_empty() {
        warnings.push(format!(
            "ignoring `skills.paths` from {} configuration {}: only user configuration, an \
             explicitly selected --config file, or a trusted project may widen the plan-mode \
             shell mount set",
            kind.description(),
            path.display()
        ));
        skills.paths.clear();
    }

    if provider.base_url.is_some() {
        warnings.push(format!(
            "ignoring `provider.base_url` from {} configuration {}: only user configuration, \
             T8FUL_BASE_URL, an explicitly selected --config file, or a trusted project may select \
             the provider endpoint",
            kind.description(),
            path.display()
        ));
        provider.base_url = None;
    }

    if ui.notify_command.is_some() {
        warnings.push(format!(
            "ignoring `ui.notify_command` from {} configuration {}: only user configuration, an \
             explicitly selected --config file, or a trusted project may select a host command",
            kind.description(),
            path.display()
        ));
        ui.notify_command = None;
    }

    // A server definition selects a program to run and a URL to receive
    // credentials, which is the same class of setting as `provider.base_url`
    // and `skills.paths`. An untrusted layer may not add or override one.
    if !mcp.servers.is_empty() {
        warnings.push(format!(
            "ignoring {} MCP server definition(s) from {} configuration {}: only user \
             configuration, an explicitly selected --config file, or a trusted project may define \
             an MCP server",
            mcp.servers.len(),
            kind.description(),
            path.display()
        ));
        mcp.servers.clear();
    }

    // A lower-trust layer may tighten to plan mode, but selecting coding mode
    // is a grant. Dropping the value lets a user or system plan default stand.
    if matches!(agent.mode, Some(Mode::Coding)) {
        warnings.push(format!(
            "ignoring `agent.mode = \"coding\"` from {} configuration {}: untrusted configuration \
             may only select plan mode; pass --mode coding, set T8FUL_MODE=coding, trust the \
             project, or select it in user configuration",
            kind.description(),
            path.display()
        ));
        agent.mode = None;
    }
}

/// Parse one configuration file into its sections.
///
/// Public so the trust prompt can inspect a project's file before deciding
/// whether to apply it: the prompt must list what would be granted without
/// granting any of it first.
pub fn parse_layer(
    path: &Path,
    text: &str,
    warnings: &mut Vec<String>,
) -> Result<FileConfig, ConfigError> {
    // `toml 1.x` no longer parses a whole document through `Value::from_str`;
    // go through the document parser explicitly.
    let value = toml::from_str::<toml::Value>(text).map_err(|error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;

    if let Some(table) = value.as_table() {
        for key in table.keys() {
            if !matches!(
                key.as_str(),
                "provider" | "agent" | "shell" | "ui" | "skills" | "agents" | "tools" | "mcp"
            ) {
                warnings.push(format!(
                    "unknown configuration section `{key}` in {}",
                    path.display()
                ));
            }
        }
    }

    toml::from_str::<FileConfig>(text).map_err(|error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

/// Resolve settings for one run.
pub fn resolve(
    root: RootInfo,
    launch_dir: PathBuf,
    paths: &ConfigPaths,
    cli: &CliOverrides,
    env: &EnvSource,
    require_model: bool,
    project: ProjectTrust,
) -> Result<Settings, ConfigError> {
    let project_root = root.path.clone();
    let (mut merged, config_sources, mut warnings) = load(paths, project)?;

    merged.apply_env(env)?;
    merged.apply_overrides(cli);

    let provider_kind = non_empty(merged.provider.kind)
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_owned())
        .to_ascii_lowercase();

    if provider_kind != DEFAULT_PROVIDER {
        return Err(ConfigError::UnsupportedProvider(provider_kind));
    }

    let model = non_empty(merged.provider.model);
    let base_url = non_empty(merged.provider.base_url)
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned())
        .trim_end_matches('/')
        .to_owned();

    if sends_credentials_in_plaintext(&base_url) {
        warnings.push(
            "provider.base_url uses plaintext HTTP to a non-loopback host; the API key will be \
             sent unencrypted"
                .to_owned(),
        );
    }

    let reasoning_effort = match merged.agent.reasoning_effort.as_deref() {
        Some(raw) => ReasoningEffort::parse(raw).ok_or_else(|| {
            ConfigError::Invalid(format!(
                "invalid reasoning effort `{raw}`: expected `default`, `off`, `minimal`, `low`, `medium`, \
                 `high`, `xhigh`, or `max`"
            ))
        })?,
        None => ReasoningEffort::Default,
    };

    let max_steps = merged.agent.max_steps;

    if max_steps == Some(0) {
        return Err(ConfigError::Invalid(
            "agent.max_steps must be at least 1".to_owned(),
        ));
    }

    let context_limit = merged.agent.context_limit;

    if context_limit == Some(0) {
        return Err(ConfigError::Invalid(
            "agent.context_limit must be at least 1".to_owned(),
        ));
    }

    let context_limit_fallback = merged
        .agent
        .context_limit_fallback
        .unwrap_or(DEFAULT_CONTEXT_LIMIT_FALLBACK);

    if context_limit_fallback == 0 {
        return Err(ConfigError::Invalid(
            "agent.context_limit_fallback must be at least 1".to_owned(),
        ));
    }

    let max_image_encoded_bytes = merged
        .ui
        .max_image_encoded_bytes
        .unwrap_or(t8ful_core::DEFAULT_MAX_IMAGE_ENCODED_BYTES);

    if max_image_encoded_bytes == 0 {
        return Err(ConfigError::Invalid(
            "ui.max_image_encoded_bytes must be at least 1".to_owned(),
        ));
    }

    let theme = match merged.ui.theme.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => match ThemeName::parse(raw) {
            Some(name) => name,
            None => {
                warnings.push(format!(
                    "unknown ui.theme `{raw}`; expected `pi` or `ansi`; using `pi`"
                ));

                ThemeName::Pi
            }
        },
        _ => ThemeName::Pi,
    };

    let catalog_timeout_ms = merged
        .provider
        .catalog_timeout_ms
        .unwrap_or(DEFAULT_CATALOG_TIMEOUT_MS);

    if catalog_timeout_ms == 0 {
        return Err(ConfigError::Invalid(
            "provider.catalog_timeout_ms must be at least 1".to_owned(),
        ));
    }

    let max_output_bytes = merged
        .shell
        .max_output_bytes
        .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);

    if max_output_bytes == 0 {
        return Err(ConfigError::Invalid(
            "shell.max_output_bytes must be at least 1".to_owned(),
        ));
    }

    let skill_roots = skill_roots(paths, &project_root, &merged.skills.paths);
    let agents = resolve_agents(merged.agents, &mut warnings)?;
    let tool_disabled = resolve_tools(merged.tools, &agents.roles, &mut warnings)?;
    let resolved_mcp = t8ful_mcp::resolve(merged.mcp, &project_root, &mut warnings)?;
    // Every name an MCP server's credentials come from is withheld from
    // ordinary shell commands: a token only a server should see must not be
    // inherited by every `bash` call.
    let mut deny_env = merged.shell.deny_env.unwrap_or_default();

    for server in &resolved_mcp.servers {
        for variable in server.credential_variables() {
            if !deny_env.iter().any(|name| name == variable) {
                deny_env.push(variable.to_owned());
            }
        }
    }

    let mcp_servers = resolved_mcp.servers;
    let mcp_startup_timeout = resolved_mcp.startup_timeout;

    let settings = Settings {
        agents,
        root,
        launch_dir,
        model,
        base_url,
        api_key: non_empty(env.get("OPENROUTER_API_KEY")),
        mode: merged.agent.mode.unwrap_or(Mode::Coding),
        reasoning_effort,
        reasoning_models: merged.agent.models,
        max_steps,
        max_output_tokens: merged.agent.max_output_tokens,
        context_limit,
        context_limit_fallback,
        max_image_encoded_bytes,
        mouse: merged.ui.mouse.unwrap_or(true),
        theme,
        show_reasoning: merged.ui.show_reasoning.unwrap_or(false),
        notifications: merged.ui.notifications.unwrap_or(true),
        notify_command: merged.ui.notify_command.unwrap_or_default(),
        notify_on_exit: merged.shell.notify_on_exit.unwrap_or(true),
        catalog_timeout: Duration::from_millis(catalog_timeout_ms),
        system_prompt_extra: merged.agent.system_prompt_extra.unwrap_or_default(),
        default_timeout: Duration::from_millis(
            merged
                .shell
                .default_timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS),
        ),
        max_output_bytes,
        deny_env,
        tool_disabled,
        skill_roots,
        mcp_servers,
        mcp_startup_timeout,
        config_sources,
        warnings: std::mem::take(&mut warnings),
    };

    if require_model {
        settings.require_model()?;
    }

    Ok(settings)
}

/// Build the ordered root list, keeping the highest-precedence source when two
/// entries name the same directory.
fn skill_roots(paths: &ConfigPaths, project: &Path, extras: &[PathBuf]) -> Vec<SkillRoot> {
    let user = paths
        .user
        .parent()
        .map(|parent| parent.join("skills"))
        .unwrap_or_default();
    let system = paths
        .system_dirs
        .first()
        .map(|dir| dir.join("t8ful/skills"))
        .unwrap_or_default();
    let mut candidates = vec![
        SkillRoot {
            path: project.join(".t8ful/skills"),
            source: SkillSource::Project,
        },
        SkillRoot {
            path: user,
            source: SkillSource::User,
        },
        SkillRoot {
            path: system,
            source: SkillSource::System,
        },
    ];

    candidates.extend(extras.iter().map(|path| SkillRoot {
        path: if path.is_absolute() {
            path.clone()
        } else {
            project.join(path)
        },
        source: SkillSource::Extra,
    }));

    let mut roots: Vec<SkillRoot> = Vec::with_capacity(candidates.len());

    for mut candidate in candidates {
        if candidate.path.as_os_str().is_empty() {
            continue;
        }

        candidate.path = t8ful_tools::paths::normalize(&candidate.path);

        if roots.iter().any(|root| root.path == candidate.path) {
            continue;
        }

        roots.push(candidate);
    }

    roots
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

/// Whether `url` is plain HTTP to something other than this machine.
///
/// Loopback HTTP stays usable for local fixtures; a user-chosen remote HTTP
/// endpoint is permitted but warned about, because the bearer token would
/// travel in clear text.
fn sends_credentials_in_plaintext(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("http://") else {
        return false;
    };

    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match host_port.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or(bracketed),
        None => host_port.split(':').next().unwrap_or(host_port),
    };

    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return false;
    }

    match host.parse::<std::net::IpAddr>() {
        Ok(address) => !address.is_loopback(),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use t8ful_core::policy::Mode;
    use t8ful_core::provider::ReasoningEffort;
    use tempfile::TempDir;

    use super::{CliOverrides, ConfigError, ConfigPaths, DEFAULT_BASE_URL, EnvSource, resolve};
    use crate::root::{RootInfo, RootSource};

    struct Fixture {
        dir: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: TempDir::new().expect("temp dir"),
            }
        }

        fn paths(&self) -> ConfigPaths {
            ConfigPaths {
                system: vec![self.dir.path().join("system/t8ful/config.toml")],
                system_dirs: vec![self.dir.path().join("system")],
                user: self.dir.path().join("user/t8ful/config.toml"),
                project: self.dir.path().join("project/.t8ful/config.toml"),
                explicit: None,
            }
        }

        fn root(&self) -> RootInfo {
            RootInfo {
                path: self.dir.path().join("project"),
                source: RootSource::Git,
            }
        }

        fn write(&self, path: &Path, body: &str) {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create config dir");
            }

            std::fs::write(path, body).expect("write config");
        }

        fn env(&self) -> EnvSource {
            EnvSource::Map(BTreeMap::from([
                ("HOME".to_owned(), self.dir.path().display().to_string()),
                ("OPENROUTER_API_KEY".to_owned(), "test-key".to_owned()),
            ]))
        }
    }

    fn resolve_with(
        fixture: &Fixture,
        cli: CliOverrides,
        env: &EnvSource,
        require_model: bool,
    ) -> Result<super::Settings, ConfigError> {
        resolve_with_trust(
            fixture,
            cli,
            env,
            require_model,
            super::ProjectTrust::Restricted,
        )
    }

    /// The keys the trust prompt would list for this fixture.
    fn project_keys(fixture: &Fixture) -> Result<Vec<String>, ConfigError> {
        let path = fixture.paths().project;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        let mut warnings = Vec::new();

        Ok(super::parse_layer(&path, &text, &mut warnings)?.requires_trust())
    }

    fn resolve_with_trust(
        fixture: &Fixture,
        cli: CliOverrides,
        env: &EnvSource,
        require_model: bool,
        project: super::ProjectTrust,
    ) -> Result<super::Settings, ConfigError> {
        resolve(
            fixture.root(),
            fixture.dir.path().join("project/sub"),
            &fixture.paths(),
            &cli,
            env,
            require_model,
            project,
        )
    }

    #[test]
    fn defaults_apply_when_no_file_exists() {
        let fixture = Fixture::new();
        let settings = resolve_with(
            &fixture,
            CliOverrides {
                model: Some("vendor/model".to_owned()),
                ..CliOverrides::default()
            },
            &fixture.env(),
            true,
        )
        .expect("settings");

        assert_eq!(settings.model.as_deref(), Some("vendor/model"));
        assert_eq!(settings.base_url, DEFAULT_BASE_URL);
        assert_eq!(settings.reasoning_effort, ReasoningEffort::Default);
        assert_eq!(settings.max_steps, None);
        assert!(settings.config_sources.is_empty());
    }

    #[test]
    fn project_overrides_user_overrides_system() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.system[0],
            "[provider]\nmodel = \"system/model\"\nbase_url = \"http://system\"\n[agent]\nmax_steps = 1\n",
        );
        fixture.write(
            &paths.user,
            "[provider]\nmodel = \"user/model\"\n[agent]\nmax_steps = 2\n",
        );
        fixture.write(&paths.project, "[agent]\nmax_steps = 3\n");

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");

        assert_eq!(settings.model.as_deref(), Some("user/model"));
        // A system file may not divert credentials to another endpoint.
        assert_eq!(settings.base_url, DEFAULT_BASE_URL);
        assert_eq!(settings.max_steps, Some(3));
        assert_eq!(settings.config_sources.len(), 3);
        assert!(
            settings
                .warnings
                .iter()
                .any(|warning| warning.contains("provider.base_url")),
            "{:?}",
            settings.warnings
        );
    }

    #[test]
    fn plaintext_http_warnings_cover_only_non_loopback_hosts() {
        use super::sends_credentials_in_plaintext;

        assert!(!sends_credentials_in_plaintext("https://example.com"));
        assert!(!sends_credentials_in_plaintext("http://localhost:8080"));
        assert!(!sends_credentials_in_plaintext("http://127.0.0.1:8080"));
        assert!(!sends_credentials_in_plaintext("http://127.0.0.2:8080"));
        assert!(!sends_credentials_in_plaintext("http://[::1]:8080"));
        assert!(sends_credentials_in_plaintext("http://example.com"));
        assert!(sends_credentials_in_plaintext(
            "http://user:key@198.51.100.7"
        ));
    }

    #[test]
    fn environment_overrides_project_values() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.project,
            "[provider]\nmodel = \"project/model\"\nbase_url = \"http://project\"\n",
        );

        let mut values = BTreeMap::from([
            ("HOME".to_owned(), fixture.dir.path().display().to_string()),
            ("T8FUL_MODEL".to_owned(), "env/model".to_owned()),
            ("T8FUL_BASE_URL".to_owned(), "http://env".to_owned()),
            ("T8FUL_APPROVAL_MODE".to_owned(), "autonomous".to_owned()),
        ]);
        values.insert("OPENROUTER_API_KEY".to_owned(), "env-key".to_owned());

        let settings = resolve_with(
            &fixture,
            CliOverrides::default(),
            &EnvSource::Map(values),
            true,
        )
        .expect("settings");

        assert_eq!(settings.model.as_deref(), Some("env/model"));
        assert_eq!(settings.base_url, "http://env");
        assert_eq!(settings.api_key.as_deref(), Some("env-key"));
        assert!(
            settings
                .warnings
                .iter()
                .all(|warning| !warning.contains("T8FUL_APPROVAL_MODE")),
            "removed approval settings have no special handling: {:?}",
            settings.warnings
        );
    }

    #[test]
    fn cli_overrides_everything_else() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.project,
            "[provider]\nmodel = \"project/model\"\n[agent]\nmax_steps = 3\n",
        );

        let settings = resolve_with(
            &fixture,
            CliOverrides {
                model: Some("cli/model".to_owned()),
                max_steps: Some(9),
                mode: None,
            },
            &fixture.env(),
            true,
        )
        .expect("settings");

        assert_eq!(settings.model.as_deref(), Some("cli/model"));
        assert_eq!(settings.max_steps, Some(9));
    }

    #[test]
    fn an_explicit_config_file_has_the_highest_file_priority() {
        let fixture = Fixture::new();
        let mut paths = fixture.paths();
        let explicit = fixture.dir.path().join("explicit.toml");

        fixture.write(&paths.project, "[provider]\nmodel = \"project/model\"\n");
        fixture.write(&explicit, "[provider]\nmodel = \"explicit/model\"\n");
        paths.explicit = Some(explicit);

        let settings = resolve(
            fixture.root(),
            fixture.dir.path().join("project/sub"),
            &paths,
            &CliOverrides::default(),
            &fixture.env(),
            true,
            super::ProjectTrust::Restricted,
        )
        .expect("settings");

        assert_eq!(settings.model.as_deref(), Some("explicit/model"));
    }

    #[test]
    fn a_missing_model_is_an_actionable_error() {
        let fixture = Fixture::new();

        let error = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect_err("model missing");

        assert!(matches!(error, ConfigError::ModelNotConfigured));
        assert!(error.to_string().contains("--model"));
    }

    #[test]
    fn the_models_command_does_not_require_a_model() {
        let fixture = Fixture::new();
        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), false)
            .expect("settings");

        assert!(settings.model.is_none());
    }

    #[test]
    fn malformed_configuration_names_the_file() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(&paths.project, "[provider\nmodel = 1\n");

        let error = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect_err("parse error");

        assert!(error.to_string().contains("config.toml"), "{error}");
    }

    #[test]
    fn unsupported_provider_kinds_are_rejected() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.project,
            "[provider]\nkind = \"other\"\nmodel = \"vendor/model\"\n",
        );

        let error = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect_err("unsupported provider");

        assert!(matches!(error, ConfigError::UnsupportedProvider(_)));
    }

    #[test]
    fn unknown_sections_are_reported_as_warnings() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.project,
            "[provider]\nmodel = \"vendor/model\"\n[tasks]\nparallel = true\n",
        );

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");

        assert_eq!(settings.warnings.len(), 1);
        assert!(settings.warnings[0].contains("tasks"));
    }

    #[test]
    fn the_approval_section_is_an_unknown_section() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.project,
            "[provider]\nmodel = \"vendor/model\"\n[approval]\nmode = \"yolo\"\n",
        );

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("the removed section does not fail the run");

        assert!(
            settings
                .warnings
                .iter()
                .any(|warning| warning.contains("unknown configuration section `approval`")),
            "{:?}",
            settings.warnings
        );
    }

    #[test]
    fn per_model_thinking_levels_come_from_configuration() {
        let fixture = Fixture::new();

        fixture.write(
            &fixture.paths().project,
            "[provider]\nmodel = \"vendor/model\"\n[agent.models.\"vendor/custom\".thinking_level_map]\nlow = \"lo\"\nhigh = \"hi\"\nminimal = \"unsupported\"\n",
        );

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");
        let section = settings
            .reasoning_models
            .get("vendor/custom")
            .expect("model section");

        assert_eq!(
            section.thinking_level_map.get("low"),
            Some(&super::ThinkingLevelMapping::Value("lo".to_owned()))
        );
        assert_eq!(
            section.thinking_level_map.get("high"),
            Some(&super::ThinkingLevelMapping::Value("hi".to_owned()))
        );
        assert_eq!(
            section.thinking_level_map.get("minimal"),
            Some(&super::ThinkingLevelMapping::Unsupported(
                super::HiddenLevel::Unsupported
            ))
        );
    }

    #[test]
    fn extended_reasoning_levels_parse_from_configuration() {
        let fixture = Fixture::new();

        fixture.write(
            &fixture.paths().project,
            "[provider]\nmodel = \"vendor/model\"\n[agent]\nreasoning_effort = \"xhigh\"\n",
        );

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");

        assert_eq!(settings.reasoning_effort, ReasoningEffort::XHigh);
    }

    #[test]
    fn reasoning_effort_is_read_from_agent_configuration() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.project,
            "[provider]\nmodel = \"vendor/model\"\n[agent]\nreasoning_effort = \"high\"\n",
        );

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");

        assert_eq!(settings.reasoning_effort, ReasoningEffort::High);
    }

    #[test]
    fn invalid_reasoning_effort_lists_the_valid_names() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.project,
            "[provider]\nmodel = \"vendor/model\"\n[agent]\nreasoning_effort = \"extreme\"\n",
        );

        let error = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect_err("invalid effort");
        let message = error.to_string();

        assert!(message.contains("extreme"), "{message}");

        for valid in ["default", "off", "low", "medium", "high"] {
            assert!(message.contains(valid), "{message}");
        }
    }

    #[test]
    fn project_configuration_can_disable_notifications_but_not_select_the_command() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(&paths.user, "[ui]\nnotify_command = [\"user-notifier\"]\n");
        fixture.write(
            &paths.project,
            "[ui]\nnotifications = false\nnotify_command = [\"bash\", \"-c\", \"printf pwned\"]\n\
             [shell]\nnotify_on_exit = false\n",
        );

        let settings = resolve_with(
            &fixture,
            CliOverrides {
                model: Some("vendor/model".to_owned()),
                ..CliOverrides::default()
            },
            &fixture.env(),
            true,
        )
        .expect("settings");

        assert!(!settings.notifications);
        assert_eq!(settings.notify_command, vec!["user-notifier"]);
        assert!(!settings.notify_on_exit);

        let warning = settings
            .warnings
            .iter()
            .find(|warning| warning.contains("ui.notify_command"))
            .expect("a warning about the ignored command");

        assert!(warning.contains("project configuration"), "{warning}");
        assert!(
            !warning.contains("pwned"),
            "the command must not be echoed: {warning}"
        );
        assert!(!warning.contains("printf"), "{warning}");
    }

    #[test]
    fn an_explicit_config_file_may_select_the_notify_command() {
        let fixture = Fixture::new();
        let mut paths = fixture.paths();
        let explicit = fixture.dir.path().join("explicit.toml");

        fixture.write(
            &explicit,
            "[ui]\nnotify_command = [\"explicit-notifier\"]\n",
        );
        paths.explicit = Some(explicit);

        let settings = resolve(
            fixture.root(),
            fixture.dir.path().join("project/sub"),
            &paths,
            &CliOverrides {
                model: Some("vendor/model".to_owned()),
                ..CliOverrides::default()
            },
            &fixture.env(),
            true,
            super::ProjectTrust::Restricted,
        )
        .expect("settings");

        assert_eq!(settings.notify_command, vec!["explicit-notifier"]);
    }

    #[test]
    fn untrusted_layers_may_tighten_but_not_leave_plan_mode() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.system[0],
            "[provider]\nmodel = \"vendor/model\"\n[agent]\nmode = \"plan\"\n",
        );
        fixture.write(&paths.project, "[agent]\nmode = \"coding\"\n");

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");

        assert_eq!(settings.mode, Mode::Plan);

        let warning = settings
            .warnings
            .iter()
            .find(|warning| warning.contains("agent.mode"))
            .expect("a warning about the ignored mode");

        assert!(warning.contains("project configuration"), "{warning}");
        assert!(warning.contains("coding"), "{warning}");
    }

    #[test]
    fn a_project_plan_mode_still_tightens_a_coding_default() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.user,
            "[provider]\nmodel = \"vendor/model\"\n[agent]\nmode = \"coding\"\n",
        );
        fixture.write(&paths.project, "[agent]\nmode = \"plan\"\n");

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");

        assert_eq!(settings.mode, Mode::Plan);
    }

    #[test]
    fn explicit_coding_still_grants_coding_over_a_project_plan() {
        let fixture = Fixture::new();
        let mut paths = fixture.paths();
        let explicit = fixture.dir.path().join("explicit.toml");

        fixture.write(&paths.project, "[agent]\nmode = \"plan\"\n");
        fixture.write(&explicit, "[agent]\nmode = \"coding\"\n");
        paths.explicit = Some(explicit);

        let settings = resolve(
            fixture.root(),
            fixture.dir.path().join("project/sub"),
            &paths,
            &CliOverrides {
                model: Some("vendor/model".to_owned()),
                ..CliOverrides::default()
            },
            &fixture.env(),
            true,
            super::ProjectTrust::Restricted,
        )
        .expect("settings");

        assert_eq!(settings.mode, Mode::Coding);
    }

    #[test]
    fn a_project_layer_cannot_grant_authority_but_still_configures_ordinary_settings() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.user,
            "[provider]\nmodel = \"vendor/model\"\n[skills]\npaths = [\"/user-extra\"]\n\
             [ui]\nnotify_command = [\"user-notifier\"]\n[agent]\nmode = \"plan\"\n",
        );
        fixture.write(
            &paths.project,
            "[provider]\nbase_url = \"http://project\"\n[skills]\npaths = [\"/project-extra\"]\n\
             [ui]\nnotify_command = [\"project-notifier\"]\n[agent]\nmode = \"coding\"\nmax_steps = 4\n\
             [agents]\nmax_concurrent = 2\n",
        );

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");

        assert_eq!(settings.base_url, DEFAULT_BASE_URL);
        assert_eq!(settings.notify_command, vec!["user-notifier"]);
        assert_eq!(settings.mode, Mode::Plan);
        assert!(
            settings
                .skill_roots
                .iter()
                .all(|root| root.path != Path::new("/project-extra"))
        );
        assert_eq!(settings.max_steps, Some(4));
        assert_eq!(settings.agents.limits.max_concurrent, 2);
    }

    #[test]
    fn a_trusted_project_applies_every_authority_bearing_key() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.project,
            "[provider]\nmodel = \"vendor/model\"\nbase_url = \"http://project.test\"\n\
             [skills]\npaths = [\"/project-extra\"]\n\
             [ui]\nnotify_command = [\"project-notifier\"]\n\
             [agent]\nmode = \"coding\"\n\
             [mcp.servers.injected]\ncommand = [\"true\"]\n",
        );

        let settings = resolve_with_trust(
            &fixture,
            CliOverrides::default(),
            &fixture.env(),
            true,
            super::ProjectTrust::Trusted,
        )
        .expect("settings");

        assert_eq!(settings.base_url, "http://project.test");
        assert_eq!(settings.notify_command, vec!["project-notifier"]);
        assert_eq!(settings.mode, Mode::Coding);
        assert!(
            settings
                .skill_roots
                .iter()
                .any(|root| root.path == Path::new("/project-extra")),
            "{:?}",
            settings.skill_roots
        );
        assert_eq!(settings.mcp_servers.len(), 1);
        assert_eq!(settings.mcp_servers[0].name, "injected");
        assert!(
            !settings
                .warnings
                .iter()
                .any(|warning| warning.contains("ignoring")),
            "{:?}",
            settings.warnings
        );
    }

    #[test]
    fn trust_does_not_extend_to_system_configuration() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(&paths.user, "[provider]\nmodel = \"vendor/model\"\n");
        fixture.write(
            &paths.system[0],
            "[provider]\nbase_url = \"http://system.test\"\n\
             [mcp.servers.system]\ncommand = [\"true\"]\n",
        );

        let settings = resolve_with_trust(
            &fixture,
            CliOverrides::default(),
            &fixture.env(),
            true,
            super::ProjectTrust::Trusted,
        )
        .expect("settings");

        assert_eq!(settings.base_url, DEFAULT_BASE_URL);
        assert!(
            settings.mcp_servers.is_empty(),
            "{:?}",
            settings.mcp_servers
        );
    }

    #[test]
    fn the_prompt_lists_only_authority_bearing_keys() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        // Ordinary project settings must not make a repository prompt.
        fixture.write(
            &paths.project,
            "[provider]\nmodel = \"vendor/model\"\n\
             [shell]\nmax_output_bytes = 4096\n\
             [agent]\nmax_steps = 4\n",
        );

        let startup_keys = project_keys(&fixture).expect("keys");

        assert!(startup_keys.is_empty(), "{startup_keys:?}");

        fixture.write(
            &paths.project,
            "[provider]\nbase_url = \"http://project.test\"\n\
             [mcp.servers.local]\ncommand = [\"true\"]\n\
             [mcp.servers.remote]\nurl = \"https://example.invalid/mcp\"\n",
        );

        let keys = project_keys(&fixture).expect("keys");

        assert_eq!(keys.len(), 3, "{keys:?}");
        assert!(
            keys.iter().any(|key| key == "provider.base_url"),
            "{keys:?}"
        );
        assert!(
            keys.iter()
                .any(|key| key.starts_with("mcp.servers.local") && key.contains("runs a program")),
            "{keys:?}"
        );
        assert!(
            keys.iter()
                .any(|key| key.starts_with("mcp.servers.remote") && key.contains("remote endpoint")),
            "{keys:?}"
        );
    }

    #[test]
    fn a_project_layer_cannot_define_an_mcp_server() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.user,
            "[provider]\nmodel = \"vendor/model\"\n[mcp.servers.trusted]\ncommand = [\"true\"]\n",
        );
        fixture.write(
            &paths.project,
            "[mcp.servers.injected]\ncommand = [\"curl\", \"http://evil.invalid\"]\n\
             [mcp.servers.trusted]\nurl = \"http://project.invalid/mcp\"\n",
        );

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");

        assert_eq!(settings.mcp_servers.len(), 1, "{:?}", settings.mcp_servers);
        assert_eq!(settings.mcp_servers[0].name, "trusted");
        // The user definition stands: the project may not replace it with a URL.
        assert!(!settings.mcp_servers[0].is_remote());
        assert!(
            settings
                .warnings
                .iter()
                .any(|warning| warning.contains("MCP server definition")),
            "{:?}",
            settings.warnings
        );
    }

    #[test]
    fn a_system_layer_cannot_define_an_mcp_server() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.system[0],
            "[provider]\nmodel = \"vendor/model\"\n[mcp.servers.system]\ncommand = [\"true\"]\n",
        );

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");

        assert!(
            settings.mcp_servers.is_empty(),
            "{:?}",
            settings.mcp_servers
        );
        assert!(
            settings
                .warnings
                .iter()
                .any(|warning| warning.contains("system")),
            "{:?}",
            settings.warnings
        );
    }

    #[test]
    fn mcp_credential_names_are_withheld_from_shell_commands() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.user,
            "[provider]\nmodel = \"vendor/model\"\n[shell]\ndeny_env = [\"OTHER\"]\n\
             [mcp.servers.local]\ncommand = [\"true\"]\npass_env = [\"GITHUB_TOKEN\"]\n\
             [mcp.servers.remote]\nurl = \"https://example.invalid/mcp\"\n\
             bearer_token_env = \"LINEAR_TOKEN\"\n\
             [mcp.servers.remote.headers]\n\"X-Org\" = \"LINEAR_ORG\"\n",
        );

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");

        for variable in ["OTHER", "GITHUB_TOKEN", "LINEAR_TOKEN", "LINEAR_ORG"] {
            assert!(
                settings.deny_env.iter().any(|name| name == variable),
                "{variable} is missing from {:?}",
                settings.deny_env
            );
        }

        // Every credential is present exactly once, whatever the layers said.
        let mut sorted = settings.deny_env.clone();

        sorted.sort();
        sorted.dedup();

        assert_eq!(
            sorted.len(),
            settings.deny_env.len(),
            "{:?}",
            settings.deny_env
        );
    }

    #[test]
    fn an_mcp_name_in_tools_disabled_is_accepted_without_static_validation() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.user,
            "[provider]\nmodel = \"vendor/model\"\n[tools]\ndisabled = [\"mcp__gh__issue\"]\n",
        );

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("an MCP name is not statically validated");

        assert!(
            settings.tool_disabled.contains("mcp__gh__issue"),
            "{:?}",
            settings.tool_disabled
        );
    }

    #[test]
    fn an_unknown_builtin_name_is_still_a_startup_error() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.user,
            "[provider]\nmodel = \"vendor/model\"\n[tools]\ndisabled = [\"no_such_tool\"]\n",
        );

        let error = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect_err("a misspelled builtin is an error");

        assert!(error.to_string().contains("no_such_tool"), "{error}");
    }

    #[test]
    fn project_configuration_can_select_a_theme_and_show_reasoning() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.project,
            "[ui]\ntheme = \"ANSI\"\nshow_reasoning = true\n",
        );

        let settings = resolve_with(
            &fixture,
            CliOverrides {
                model: Some("vendor/model".to_owned()),
                ..CliOverrides::default()
            },
            &fixture.env(),
            true,
        )
        .expect("settings");

        assert_eq!(settings.theme, t8ful_tui::ThemeName::Ansi);
        assert!(settings.show_reasoning);
    }

    #[test]
    fn an_unknown_theme_warns_and_falls_back_to_pi() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(&paths.project, "[ui]\ntheme = \"solarized\"\n");

        let settings = resolve_with(
            &fixture,
            CliOverrides {
                model: Some("vendor/model".to_owned()),
                ..CliOverrides::default()
            },
            &fixture.env(),
            true,
        )
        .expect("a bad theme name must not fail startup");

        assert_eq!(settings.theme, t8ful_tui::ThemeName::Pi);
        assert!(
            settings
                .warnings
                .iter()
                .any(|warning| warning.contains("ui.theme") && warning.contains("solarized")),
            "{:?}",
            settings.warnings
        );
    }

    #[test]
    fn project_configuration_can_disable_mouse_and_set_the_catalog_deadline() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.project,
            "[provider]\ncatalog_timeout_ms = 500\n[ui]\nmouse = false\n",
        );

        let settings = resolve_with(
            &fixture,
            CliOverrides {
                model: Some("vendor/model".to_owned()),
                ..CliOverrides::default()
            },
            &fixture.env(),
            true,
        )
        .expect("settings");

        assert!(!settings.mouse);
        assert_eq!(
            settings.catalog_timeout,
            std::time::Duration::from_millis(500)
        );
    }

    #[test]
    fn zero_catalog_timeout_is_rejected() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(&paths.project, "[provider]\ncatalog_timeout_ms = 0\n");

        let error = resolve_with(
            &fixture,
            CliOverrides {
                model: Some("vendor/model".to_owned()),
                ..CliOverrides::default()
            },
            &fixture.env(),
            true,
        )
        .expect_err("zero timeout");

        assert!(matches!(error, ConfigError::Invalid(_)));
        assert!(error.to_string().contains("catalog_timeout_ms"));
    }

    #[test]
    fn zero_max_steps_is_rejected() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(&paths.project, "[agent]\nmax_steps = 0\n");

        let error = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect_err("invalid steps");

        assert!(matches!(error, ConfigError::Invalid(_)));
    }

    #[test]
    fn context_limit_is_validated_and_loaded() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(&paths.project, "[agent]\ncontext_limit = 0\n");

        let error = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect_err("zero context limit");

        assert!(matches!(error, ConfigError::Invalid(_)));

        fixture.write(&paths.project, "[agent]\ncontext_limit = 1048576\n");

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), false)
            .expect("valid context limit");

        assert_eq!(settings.context_limit, Some(1_048_576));
    }

    #[test]
    fn config_paths_follow_xdg_variables() {
        let env = EnvSource::Map(BTreeMap::from([
            ("XDG_CONFIG_HOME".to_owned(), "/xdg/home".to_owned()),
            ("XDG_CONFIG_DIRS".to_owned(), "/first:/second".to_owned()),
        ]));

        let paths = ConfigPaths::discover(Path::new("/project"), None, &env);

        assert_eq!(
            paths.system,
            vec![
                PathBuf::from("/first/t8ful/config.toml"),
                PathBuf::from("/second/t8ful/config.toml")
            ]
        );
        assert_eq!(paths.user, PathBuf::from("/xdg/home/t8ful/config.toml"));
        assert_eq!(paths.project, PathBuf::from("/project/.t8ful/config.toml"));
        assert_eq!(
            paths.system_dirs,
            vec![PathBuf::from("/first"), PathBuf::from("/second")]
        );
    }

    #[test]
    fn skill_roots_follow_project_user_system_then_extras() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.user,
            "[skills]\npaths = [\"shared\", \"/opt/extra\"]\n",
        );
        fixture.write(&paths.project, "[provider]\nmodel = \"vendor/model\"\n");

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");
        let project = fixture.dir.path().join("project");

        assert_eq!(
            settings.skill_roots,
            vec![
                super::SkillRoot {
                    path: project.join(".t8ful/skills"),
                    source: super::SkillSource::Project,
                },
                super::SkillRoot {
                    path: fixture.dir.path().join("user/t8ful/skills"),
                    source: super::SkillSource::User,
                },
                super::SkillRoot {
                    path: fixture.dir.path().join("system/t8ful/skills"),
                    source: super::SkillSource::System,
                },
                super::SkillRoot {
                    path: project.join("shared"),
                    source: super::SkillSource::Extra,
                },
                super::SkillRoot {
                    path: PathBuf::from("/opt/extra"),
                    source: super::SkillSource::Extra,
                },
            ]
        );
    }

    #[test]
    fn a_duplicate_skill_root_keeps_the_higher_precedence_source() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        if let Some(parent) = paths.user.parent() {
            std::fs::create_dir_all(parent).expect("create user dir");
        }

        fixture.write(
            &paths.user,
            "[provider]\nmodel = \"vendor/model\"\n[skills]\npaths = [\"../../user/t8ful/skills\"]\n",
        );

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");

        assert_eq!(
            settings
                .skill_roots
                .iter()
                .filter(|root| root.path == fixture.dir.path().join("user/t8ful/skills"))
                .count(),
            1
        );
        assert_eq!(settings.skill_roots[1].source, super::SkillSource::User);
    }

    #[test]
    fn the_system_skill_root_follows_xdg_config_dirs() {
        let env = EnvSource::Map(BTreeMap::from([
            ("XDG_CONFIG_HOME".to_owned(), "/xdg/home".to_owned()),
            ("XDG_CONFIG_DIRS".to_owned(), "/first:/second".to_owned()),
        ]));
        let paths = ConfigPaths::discover(Path::new("/project"), None, &env);

        let roots = super::skill_roots(&paths, Path::new("/project"), &[]);

        assert_eq!(
            roots
                .iter()
                .map(|root| root.path.clone())
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from("/project/.t8ful/skills"),
                PathBuf::from("/xdg/home/t8ful/skills"),
                PathBuf::from("/first/t8ful/skills"),
            ]
        );
    }

    #[test]
    fn system_skill_paths_are_stripped_with_a_warning() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.system[0],
            "[provider]\nmodel = \"system/model\"\n[skills]\npaths = [\"/system-extra\"]\n",
        );
        fixture.write(&paths.user, "[provider]\nmodel = \"vendor/model\"\n");

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");

        assert!(
            settings
                .skill_roots
                .iter()
                .all(|root| root.path != Path::new("/system-extra"))
        );

        let warning = settings
            .warnings
            .iter()
            .find(|warning| warning.contains("skills.paths"))
            .expect("a warning about the stripped paths");

        assert!(
            warning.contains("system configuration"),
            "the warning must name the layer the paths came from: {warning}"
        );

        let claim = warning
            .split_once("only ")
            .map(|(_, claim)| claim)
            .expect("the warning must say which layers may grant the paths");

        assert!(
            !claim.contains("system"),
            "the gate strips system paths, so the message must not name system as allowed: \
             {warning}"
        );
        assert!(
            claim.contains("user") && claim.contains("explicitly selected"),
            "the message must name the sources that are still accepted: {warning}"
        );
    }

    #[test]
    fn project_skill_paths_are_stripped_with_a_warning() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(
            &paths.project,
            "[provider]\nmodel = \"vendor/model\"\n[skills]\npaths = [\"/escape\"]\n",
        );

        let settings = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect("settings");

        assert!(
            settings
                .skill_roots
                .iter()
                .all(|root| root.path != Path::new("/escape"))
        );
        assert!(
            settings
                .warnings
                .iter()
                .any(|warning| warning.contains("skills.paths"))
        );
    }

    /// Resolve settings from one project config body.
    fn agent_settings(contents: &str) -> crate::config::Settings {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(&paths.project, contents);

        resolve_with(
            &fixture,
            CliOverrides {
                model: Some("vendor/model".to_owned()),
                ..CliOverrides::default()
            },
            &fixture.env(),
            true,
        )
        .expect("settings")
    }

    #[test]
    fn agents_are_enabled_by_default_with_the_documented_limits() {
        let settings = agent_settings("");

        assert!(settings.agents.enabled);
        assert_eq!(settings.agents.limits.max_concurrent, 4);
        assert_eq!(settings.agents.limits.max_spawns_per_turn, 16);
        assert!(settings.agents.wake_on_finish);
        assert!(settings.agents.roles.contains_key("general"));
    }

    #[test]
    fn max_depth_is_accepted_but_ignored_with_a_warning() {
        let settings = agent_settings("[agents]\nmax_depth = 3\n");

        assert!(
            settings
                .warnings
                .iter()
                .any(|warning| warning.contains("single level") && warning.contains("max_depth")),
            "{:?}",
            settings.warnings
        );
        assert_eq!(settings.agents.limits, t8ful_core::AgentLimits::default());
    }

    #[test]
    fn the_agents_section_overrides_limits_and_the_wake() {
        let settings = agent_settings(
            "[agents]\nmax_concurrent = 6\nmax_spawns_per_turn = 30\n\
             wake_on_finish = false\n",
        );

        assert_eq!(settings.agents.limits.max_concurrent, 6);
        assert_eq!(settings.agents.limits.max_spawns_per_turn, 30);
        assert!(!settings.agents.wake_on_finish);
    }

    #[test]
    fn a_zero_limit_is_rejected_with_the_field_name() {
        for (field, contents) in [
            ("agents.max_concurrent", "[agents]\nmax_concurrent = 0\n"),
            (
                "agents.max_spawns_per_turn",
                "[agents]\nmax_spawns_per_turn = 0\n",
            ),
        ] {
            let fixture = Fixture::new();
            let paths = fixture.paths();

            fixture.write(&paths.project, contents);

            let error = resolve_with(
                &fixture,
                CliOverrides {
                    model: Some("vendor/model".to_owned()),
                    ..CliOverrides::default()
                },
                &fixture.env(),
                true,
            )
            .expect_err("zero limit");

            assert!(matches!(error, ConfigError::Invalid(_)), "{field}");
            assert!(error.to_string().contains(field), "{field}: {error}");
        }
    }

    #[test]
    fn a_role_carries_instructions_tools_model_and_isolation() {
        let settings = agent_settings(
            "[agents.roles.researcher]\ninstructions = \"Only read.\"\n\
             tools = [\"read_file\", \"bash\"]\nmodel = \"vendor/cheap\"\nisolation = \"shared\"\n",
        );

        let role = settings
            .agents
            .roles
            .get("researcher")
            .expect("the configured role");

        assert_eq!(role.instructions, "Only read.");
        assert_eq!(role.tools, vec!["read_file", "bash"]);
        assert_eq!(role.model.as_deref(), Some("vendor/cheap"));
        assert_eq!(role.isolation, Some(t8ful_core::Isolation::Shared));
        assert!(role.allows("read_file"));
        assert!(!role.allows("write_file"));
    }

    #[test]
    fn a_role_without_tools_may_use_anything() {
        let settings =
            agent_settings("[agents.roles.implementer]\ninstructions = \"Write code.\"\n");

        let role = settings.agents.roles.get("implementer").expect("role");

        assert!(role.allows("write_file"));
        assert!(role.tools.is_empty());
    }

    #[test]
    fn a_role_naming_an_unavailable_child_tool_is_warned_about() {
        let settings = agent_settings(
            "[agents.roles.researcher]\ntools = [\"read_file\", \"spawn_agents\"]\n",
        );

        assert!(
            settings.warnings.iter().any(|warning| {
                warning.contains("agents.roles.researcher.tools")
                    && warning.contains("spawn_agents")
                    && warning.contains("ignored")
            }),
            "{:?}",
            settings.warnings
        );
    }

    #[test]
    fn an_unknown_isolation_word_warns_and_falls_back_to_shared() {
        let settings = agent_settings("[agents.roles.writer]\nisolation = \"bubble\"\n");

        let role = settings.agents.roles.get("writer").expect("role");

        assert!(role.isolation.is_none(), "unset means shared");
        assert!(
            settings
                .warnings
                .iter()
                .any(|warning| warning.contains("isolation") && warning.contains("bubble")),
            "{:?}",
            settings.warnings
        );
    }

    #[test]
    fn explicitly_naming_general_does_not_duplicate_it() {
        let settings = agent_settings("[agents.roles.general]\ninstructions = \"Custom.\"\n");

        let role = settings.agents.roles.get("general").expect("role");

        assert_eq!(role.instructions, "Custom.");
    }

    #[test]
    fn worktree_and_target_paths_are_carried_through() {
        let settings = agent_settings(
            "[agents]\nworktree_root = \"/tmp/trees\"\n\
             shared_target_dir = \"/tmp/target\"\n",
        );

        assert_eq!(
            settings.agents.worktree_root.as_deref(),
            Some(std::path::Path::new("/tmp/trees"))
        );
        assert_eq!(
            settings.agents.shared_target_dir.as_deref(),
            Some(std::path::Path::new("/tmp/target"))
        );
    }

    #[test]
    fn tools_are_enabled_by_default() {
        let settings = agent_settings("");

        assert!(settings.tool_disabled.is_empty());
    }

    #[test]
    fn tools_disabled_parses_and_deduplicates() {
        let settings = agent_settings("[tools]\ndisabled = [\"bash\", \"recall\", \"bash\"]\n");

        assert_eq!(
            settings
                .tool_disabled
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["bash", "recall"]
        );
    }

    #[test]
    fn a_lower_layer_can_withhold_a_tool_and_a_higher_layer_replaces_the_list() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(&paths.system[0], "[tools]\ndisabled = [\"bash\"]\n");
        fixture.write(&paths.user, "[tools]\ndisabled = [\"edit_file\"]\n");

        let settings = resolve_with(
            &fixture,
            CliOverrides {
                model: Some("vendor/model".to_owned()),
                ..CliOverrides::default()
            },
            &fixture.env(),
            true,
        )
        .expect("settings");

        assert_eq!(
            settings
                .tool_disabled
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["edit_file"]
        );
    }

    #[test]
    fn a_project_layer_may_withhold_a_tool() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(&paths.project, "[tools]\ndisabled = [\"bash\"]\n");

        let settings = resolve_with(
            &fixture,
            CliOverrides {
                model: Some("vendor/model".to_owned()),
                ..CliOverrides::default()
            },
            &fixture.env(),
            true,
        )
        .expect("settings");

        assert!(settings.tool_disabled.contains("bash"));
        assert!(settings.warnings.is_empty(), "{:?}", settings.warnings);
    }

    #[test]
    fn an_unknown_tool_name_is_a_hard_error() {
        let fixture = Fixture::new();
        let paths = fixture.paths();

        fixture.write(&paths.project, "[tools]\ndisabled = [\"bsh\"]\n");

        let error = resolve_with(&fixture, CliOverrides::default(), &fixture.env(), true)
            .expect_err("unknown tool");

        let message = error.to_string();

        assert!(message.contains("`bsh`"), "{message}");
        assert!(message.contains("read_file"), "{message}");
        assert!(message.contains("recall"), "{message}");
    }

    #[test]
    fn a_role_naming_a_withheld_tool_warns() {
        let settings = agent_settings(
            "[tools]\ndisabled = [\"bash\"]\n\
             [agents.roles.writer]\ntools = [\"bash\", \"read_file\"]\n",
        );

        assert!(
            settings
                .warnings
                .iter()
                .any(|warning| warning.contains("agents.roles.writer") && warning.contains("bash")),
            "{:?}",
            settings.warnings
        );
    }
}
