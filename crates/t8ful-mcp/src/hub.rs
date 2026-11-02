//! Starting, tracking, and stopping every configured MCP server.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use t8ful_core::policy::Mode;
use t8ful_core::tool::{Tool, ToolContext, ToolRegistry};
use tokio::process::Child;

use crate::client::ClientHandle;
use crate::config::{Env, PlanTrust, ProcessEnv, ServerConfig, Transport};
use crate::name;
use crate::tool::McpTool;
use crate::transport::{self, Client, SpawnMode};

/// How long a server gets to answer `close()` before it is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// What happened to one configured server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    /// Started and listed its tools.
    Ready { tools: usize },
    /// Configured, but not started in this run, with the reason.
    Skipped { reason: String },
    /// Start, handshake, or discovery failed.
    Failed { message: String },
}

/// A running server and everything needed to stop it.
struct Running {
    /// `None` once shutdown has taken it.
    client: Option<Client>,
    handle: ClientHandle,
    child: Option<Child>,
}

impl Running {
    /// Close the session, then kill a stdio child that ignored the close.
    ///
    /// A remote server has no child; closing the session is enough.
    async fn stop(&mut self) {
        self.handle.close();

        if let Some(mut client) = self.client.take() {
            let _ = client.close_with_timeout(SHUTDOWN_GRACE).await;
        }

        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

/// Every configured server, plus the tools they contribute.
///
/// Dropping the hub shuts every server down, so a session switch or a failed
/// build cannot leave a server process behind.
#[derive(Default)]
pub struct McpHub {
    /// Per-server outcome, in server-name order.
    servers: BTreeMap<String, ServerState>,
    /// Per-server live state, for servers that started.
    running: BTreeMap<String, Running>,
    /// The tools this hub contributes, in discovery order.
    tool_list: Vec<McpTool>,
    /// Model-visible name to the server's own tool name.
    remote_names: BTreeMap<String, String>,
    /// Model-visible name to server name.
    owners: BTreeMap<String, String>,
}

impl McpHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start every configured server concurrently and return once all settle.
    ///
    /// A server that fails becomes [`ServerState::Failed`]; the hub keeps the
    /// rest. `warnings` receives one line per server that did not start, so a
    /// frontend can show them in the transcript.
    pub async fn start(
        configs: Vec<ServerConfig>,
        ctx: &ToolContext,
        startup_timeout: Duration,
        warnings: &mut Vec<String>,
    ) -> Self {
        Self::start_with_env(
            configs,
            ctx,
            startup_timeout,
            Arc::new(ProcessEnv),
            warnings,
        )
        .await
    }

    /// [`McpHub::start`] with an injected environment.
    ///
    /// Credentials are read through `env`, so a test supplies a fixed map
    /// instead of mutating the process environment.
    pub async fn start_with_env(
        configs: Vec<ServerConfig>,
        ctx: &ToolContext,
        startup_timeout: Duration,
        env: Arc<dyn Env>,
        warnings: &mut Vec<String>,
    ) -> Self {
        let mut hub = Self::new();

        if configs.is_empty() {
            return hub;
        }

        // Decide the spawn mode once, before starting anything: a refusal here
        // is a configuration or mode decision, not a startup failure.
        let mut to_start = Vec::new();

        for config in configs {
            match spawn_mode(&config, ctx) {
                Ok(mode) => to_start.push((config, mode)),
                Err(reason) => {
                    warnings.push(reason.clone());
                    hub.servers
                        .insert(config.name.clone(), ServerState::Skipped { reason });
                }
            }
        }

        // Every server starts at once, so the whole startup is bounded by one
        // deadline rather than one per server.
        let started = start_all(to_start, ctx, startup_timeout, env).await;

        for (config, outcome) in started {
            match outcome {
                Ok((running, definitions)) => {
                    let accepted = hub.add_tools(&config, &running, definitions, warnings);

                    hub.running.insert(config.name.clone(), running);
                    hub.servers
                        .insert(config.name.clone(), ServerState::Ready { tools: accepted });
                }
                Err(message) => {
                    warnings.push(message.clone());
                    hub.servers
                        .insert(config.name.clone(), ServerState::Failed { message });
                }
            }
        }

        hub
    }

    /// Register one server's tools and return how many were accepted.
    fn add_tools(
        &mut self,
        config: &ServerConfig,
        running: &Running,
        definitions: Vec<rmcp::model::Tool>,
        warnings: &mut Vec<String>,
    ) -> usize {
        let mut accepted = 0;

        for definition in definitions {
            let model_name = self.unique_name(&config.name, definition.name.as_ref(), warnings);
            let tool = McpTool::new(
                model_name.clone(),
                config,
                &definition,
                running.handle.clone(),
            );

            self.remote_names
                .insert(model_name.clone(), tool.remote_name().to_owned());
            self.owners.insert(model_name, config.name.clone());
            self.tool_list.push(tool);
            accepted += 1;
        }

        accepted
    }

    /// A model-visible name no other tool has taken.
    fn unique_name(&self, server: &str, tool: &str, warnings: &mut Vec<String>) -> String {
        let base = name::model_name(server, tool);

        if self.is_free(&base) {
            return base;
        }

        for counter in 2..1000u32 {
            let suffix = format!("_{counter}");
            let candidate = if base.len() + suffix.len() <= name::MAX_NAME_LEN {
                format!("{base}{suffix}")
            } else {
                let cut = name::MAX_NAME_LEN.saturating_sub(suffix.len());
                format!("{}{suffix}", &base[..cut])
            };

            if self.is_free(&candidate) {
                warnings.push(format!(
                    "MCP tool `{tool}` from server `{server}` collides with an existing tool name; \
                     it is offered as `{candidate}`"
                ));

                return candidate;
            }
        }

        base
    }

    fn is_free(&self, candidate: &str) -> bool {
        !self.remote_names.contains_key(candidate)
            && !self
                .tool_list
                .iter()
                .any(|tool| tool.model_name() == candidate)
    }

    /// The tools this hub contributes.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tool_list
            .iter()
            .map(|tool| Arc::new(tool.clone()) as Arc<dyn Tool>)
            .collect()
    }

    /// Register every server tool into a registry.
    ///
    /// `mcp__` names cannot collide with a builtin, because no builtin uses the
    /// namespace; the check is still made so a future builtin cannot be silently
    /// overwritten by a server that happened to pick the same name.
    pub fn register_into(&self, registry: &mut ToolRegistry) {
        for tool in self.tools() {
            let name = tool.spec().name;

            if registry.get(&name).is_some() {
                tracing::warn!(
                    target: "t8ful::mcp",
                    "MCP tool `{name}` would replace a registered tool of the same name; skipping it"
                );

                continue;
            }

            registry.register_arc(tool);
        }
    }

    pub fn state(&self, server: &str) -> Option<&ServerState> {
        self.servers.get(server)
    }

    pub fn states(&self) -> &BTreeMap<String, ServerState> {
        &self.servers
    }

    /// The server's own tool name for a model-visible one.
    pub fn remote_name(&self, model_name: &str) -> Option<&str> {
        self.remote_names.get(model_name).map(String::as_str)
    }

    pub fn server_of(&self, model_name: &str) -> Option<&str> {
        self.owners.get(model_name).map(String::as_str)
    }

    /// Every model-visible name, for `[tools] disabled` validation.
    pub fn tool_names(&self) -> Vec<&str> {
        self.remote_names.keys().map(String::as_str).collect()
    }

    /// Stop one server and withdraw its tools.
    pub async fn stop(&mut self, server: &str) {
        if let Some(mut running) = self.running.remove(server) {
            running.stop().await;
        }

        self.tool_list.retain(|tool| tool.server() != server);
        self.owners.retain(|_, owner| owner != server);
        self.remote_names
            .retain(|model_name, _| self.owners.contains_key(model_name));

        if let Some(state) = self.servers.get_mut(server) {
            *state = ServerState::Skipped {
                reason: format!("server `{server}` was stopped"),
            };
        }
    }

    /// Stop every server.
    pub async fn stop_all(&mut self) {
        for (_, mut running) in std::mem::take(&mut self.running) {
            running.stop().await;
        }

        self.tool_list.clear();
        self.remote_names.clear();
        self.owners.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.tool_list.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tool_list.len()
    }
}

impl Drop for McpHub {
    fn drop(&mut self) {
        // `Drop` cannot await. Closing the handle makes later calls fail
        // immediately, and killing the child guarantees the process group does
        // not outlive the hub; the session's own drop guard cancels its task.
        for running in self.running.values_mut() {
            running.handle.close();

            if let Some(child) = running.child.as_mut() {
                let _ = child.start_kill();
            }
        }
    }
}

/// One started server plus its tool list.
type StartOutcome = Result<(Running, Vec<rmcp::model::Tool>), String>;

/// Whether this server may start now, and how.
fn spawn_mode(config: &ServerConfig, ctx: &ToolContext) -> Result<SpawnMode, String> {
    if ctx.mode != Mode::Plan {
        return Ok(SpawnMode::Direct);
    }

    match &config.transport {
        Transport::Stdio { .. } => {
            if ctx.sandbox.is_none() {
                return Err(format!(
                    "MCP server `{}` was not started in plan mode because the planning sandbox is \
                     unavailable; refusing to run a planning server on the host",
                    config.name
                ));
            }

            Ok(SpawnMode::Sandboxed)
        }
        Transport::Http { .. } => {
            if config.plan == PlanTrust::ReadOnly {
                Ok(SpawnMode::Direct)
            } else {
                Err(untrusted_message(&config.name))
            }
        }
    }
}

/// The actionable refusal shared by the gate and the plan-mode skip.
pub fn untrusted_message(server: &str) -> String {
    format!(
        "MCP server `{server}` is not trusted for plan mode; set `mcp.servers.{server}.plan = \
         \"read-only\"` in user configuration if its tools are safe to run while planning"
    )
}

/// Start every server concurrently under one deadline.
async fn start_all(
    targets: Vec<(ServerConfig, SpawnMode)>,
    ctx: &ToolContext,
    startup_timeout: Duration,
    env: Arc<dyn Env>,
) -> Vec<(ServerConfig, StartOutcome)> {
    let mut handles = Vec::with_capacity(targets.len());

    for (config, mode) in targets {
        let ctx = ctx.clone();
        let env = Arc::clone(&env);

        handles.push(tokio::spawn(async move {
            let outcome = start_one(&config, mode, &ctx, startup_timeout, env.as_ref()).await;

            (config, outcome)
        }));
    }

    let mut started = Vec::with_capacity(handles.len());

    for handle in handles {
        match handle.await {
            Ok(value) => started.push(value),
            Err(error) => {
                // A panic in a startup task must not take the hub with it.
                tracing::error!(target: "t8ful::mcp", "an MCP startup task panicked: {error}");
            }
        }
    }

    started
}

async fn start_one(
    config: &ServerConfig,
    mode: SpawnMode,
    ctx: &ToolContext,
    startup_timeout: Duration,
    env: &dyn Env,
) -> StartOutcome {
    let started =
        match tokio::time::timeout(startup_timeout, transport::connect(config, mode, ctx, env))
            .await
        {
            Ok(Ok(started)) => started,
            Ok(Err(message)) => return Err(message),
            Err(_) => {
                return Err(format!(
                    "MCP server `{}` did not finish initializing within {} ms",
                    config.name,
                    startup_timeout.as_millis()
                ));
            }
        };

    let transport::Started { client, child } = started;
    let handle = ClientHandle::new(&client);

    let definitions = match tokio::time::timeout(startup_timeout, client.list_all_tools()).await {
        Ok(Ok(tools)) => tools,
        Ok(Err(error)) => {
            return Err(format!(
                "MCP server `{}` failed to list its tools: {error}",
                config.name
            ));
        }
        Err(_) => {
            return Err(format!(
                "MCP server `{}` did not answer `tools/list` within {} ms",
                config.name,
                startup_timeout.as_millis()
            ));
        }
    };

    Ok((
        Running {
            client: Some(client),
            handle,
            child,
        },
        definitions,
    ))
}
