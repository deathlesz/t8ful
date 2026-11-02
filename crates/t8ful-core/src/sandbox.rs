use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::tool::Tool;

/// A command that must run inside an isolating sandbox.
///
/// The contract deliberately has no network switch and no arbitrary writable
/// paths: sandboxed planning commands never get network access, and the only
/// writable location is the private scratch space provided by the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    /// Paths the process may read but not modify. The backend pins these mounts
    /// so a rename in the parent cannot redirect them.
    pub read_only_paths: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct SandboxedProcess {
    pub child: tokio::process::Child,
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("no sandbox backend is available: {0}")]
    Unavailable(String),

    #[error("sandbox failed to start the command: {0}")]
    Launch(String),

    #[error("sandbox cannot enforce the requested boundary: {0}")]
    Unsupported(String),
}

/// Process-isolation boundary used by plan mode.
///
/// Callers must treat every error as a refusal to run: there is deliberately no
/// unsandboxed fallback.
#[async_trait]
pub trait Sandbox: Send + Sync {
    fn name(&self) -> &'static str;

    async fn spawn(&self, command: SandboxedCommand) -> Result<SandboxedProcess, SandboxError>;
}

/// External tool providers (MCP servers in Phase 3) expose tools through this
/// boundary so the core policy gate sees them like built-in tools.
#[async_trait]
pub trait ToolSource: Send + Sync {
    fn namespace(&self) -> &str;

    async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>, String>;
}
