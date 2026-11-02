//! Transport construction: a sandboxed or direct child for stdio, and a
//! Streamable HTTP worker for a remote server.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use http::{HeaderName, HeaderValue};
use rmcp::model::{ClientConfig, ProtocolVersion};
use rmcp::service::{ClientInitializeError, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleClient};
use t8ful_core::sandbox::{Sandbox, SandboxedCommand};
use t8ful_core::tool::ToolContext;
use tokio::process::{Child, Command};

use crate::config::{Env, ServerConfig, Transport};

/// A client session that speaks MCP to one server.
pub type Client = RunningService<RoleClient, ClientConfig>;

/// How a session is established: probe for stateless discovery, then fall back.
///
/// `Auto` sends `server/discover` first and, when the peer answers with a
/// JSON-RPC error that is not a modern-era rejection, retries with the legacy
/// `initialize` handshake on the same connection. The preferred version is the
/// only one that defines discovery; the legacy fallback is pinned to the newest
/// version that still defines `initialize`, because a `2026-07-28` peer has no
/// handshake to fall back to.
fn lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        legacy_version: Some(ProtocolVersion::V_2025_11_25),
    }
}

/// How a stdio server is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnMode {
    /// Run the configured command directly, with the filtered environment.
    Direct,
    /// Run it inside the planning sandbox.
    Sandboxed,
}

/// A started server: the client session and the child it owns, if any.
pub struct Started {
    pub client: Client,
    /// The stdio child. Kept so shutdown can kill a process that ignored
    /// `close()`, and so `Drop` cannot leave a server running.
    pub child: Option<Child>,
}

/// Start one server and complete the MCP handshake.
pub async fn connect(
    config: &ServerConfig,
    mode: SpawnMode,
    ctx: &ToolContext,
    env: &dyn Env,
) -> Result<Started, String> {
    match &config.transport {
        Transport::Stdio { command, cwd } => {
            connect_stdio(config, command, cwd, mode, ctx, env).await
        }
        Transport::Http {
            url,
            bearer_token_env,
            headers,
        } => {
            // `http_credentials` reads both values through `env`; the names are
            // taken from the configuration there, so nothing is threaded here.
            let _ = (bearer_token_env, headers);
            connect_http(config, url, env).await
        }
    }
}

async fn connect_stdio(
    config: &ServerConfig,
    command: &[String],
    cwd: &Path,
    mode: SpawnMode,
    ctx: &ToolContext,
    env: &dyn Env,
) -> Result<Started, String> {
    let program = command
        .first()
        .ok_or_else(|| format!("server `{}` has an empty command", config.name))?;
    let args = &command[1..];

    let mut child = match mode {
        SpawnMode::Direct => spawn_direct(config, program, args, cwd, env)?,
        SpawnMode::Sandboxed => spawn_sandboxed(config, program, args, cwd, ctx, env).await?,
    };

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("server `{}` has no stdin", config.name))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("server `{}` has no stdout", config.name))?;
    let transport = AsyncRwTransport::<RoleClient, _, _>::new_client(stdout, stdin);

    let client = ClientConfig::default()
        .serve_with_lifecycle(transport, lifecycle())
        .await
        .map_err(|error| describe_initialize_error(config, error))?;

    Ok(Started {
        client,
        child: Some(child),
    })
}

fn spawn_direct(
    config: &ServerConfig,
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &dyn Env,
) -> Result<Child, String> {
    let values = config
        .stdio_env(env)
        .map_err(|error| error.for_server(&config.name).to_string())?;
    let mut command = Command::new(program);

    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(values.iter())
        // A server that outlives its transport would leak a process.
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    command.spawn().map_err(|error| {
        format!(
            "server `{}` could not start `{program}`: {error}",
            config.name
        )
    })
}

async fn spawn_sandboxed(
    config: &ServerConfig,
    program: &str,
    args: &[String],
    cwd: &Path,
    ctx: &ToolContext,
    env: &dyn Env,
) -> Result<Child, String> {
    let sandbox = ctx.sandbox.as_ref().ok_or_else(|| {
        format!(
            "server `{}` is not started in plan mode because the planning sandbox is unavailable; \
             refusing to run a planning server on the host",
            config.name
        )
    })?;

    // D07: a credential known to t8ful is withheld from a local planning
    // server, even when the server's own configuration lists it. Plan mode runs
    // on behalf of a reader.
    let mut values = std::collections::BTreeMap::new();

    for (name, value) in config
        .stdio_env(env)
        .map_err(|error| error.for_server(&config.name).to_string())?
    {
        if config
            .credential_variables()
            .iter()
            .any(|variable| *variable == name)
        {
            continue;
        }

        values.insert(name, value);
    }

    let command = SandboxedCommand {
        program: resolve_program(program),
        args: args.to_vec(),
        cwd: cwd.to_path_buf(),
        env: values,
        read_only_paths: mounts(ctx),
    };

    let started = sandbox
        .spawn(command)
        .await
        .map_err(|error| describe_sandbox_error(config, sandbox.as_ref(), &error))?;

    Ok(started.child)
}

/// Resolve the program the way the plan-mode shell does, so a sandboxed server
/// finds `npx` on `PATH` instead of failing on a bare name.
fn resolve_program(program: &str) -> String {
    let path = Path::new(program);

    if path.is_absolute() || program.contains('/') {
        return program.to_owned();
    }

    let Some(dirs) = std::env::var_os("PATH") else {
        return program.to_owned();
    };

    for directory in std::env::split_paths(&dirs) {
        let candidate = directory.join(program);

        if candidate.is_file() {
            return candidate.display().to_string();
        }
    }

    program.to_owned()
}

/// Read-scope roots for the sandbox, resolved and de-duplicated.
fn mounts(ctx: &ToolContext) -> Vec<PathBuf> {
    let mut roots = vec![ctx.project_root.clone()];

    if let Some(scope) = &ctx.read_scope {
        roots.extend(scope.roots().iter().cloned());
    }

    let mut unique: Vec<PathBuf> = Vec::new();

    for root in roots {
        if let Ok(canonical) = std::fs::canonicalize(&root)
            && !unique.contains(&canonical)
        {
            unique.push(canonical);
        }
    }

    unique
}

fn describe_sandbox_error(
    config: &ServerConfig,
    sandbox: &dyn Sandbox,
    error: &t8ful_core::sandbox::SandboxError,
) -> String {
    format!(
        "server `{}` could not start in the {} planning sandbox ({error}); plan-mode servers run \
         without network access and without writable paths, so a package-manager launcher such as \
         `npx` usually fails here. Configure this server with `url` instead to reach it remotely, \
         or use it in coding mode.",
        config.name,
        sandbox.name()
    )
}

async fn connect_http(config: &ServerConfig, url: &str, env: &dyn Env) -> Result<Started, String> {
    let (token, header_values) = config
        .http_credentials(env)
        .map_err(|error| error.for_server(&config.name).to_string())?;
    let mut custom_headers = HashMap::new();

    for (name, value) in header_values {
        let header = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            format!(
                "server `{}` has an invalid header name `{name}`: {error}",
                config.name
            )
        })?;
        let value = HeaderValue::from_str(&value).map_err(|_| {
            format!(
                "server `{}` has a header value for `{name}` that is not a valid HTTP header \
                 value; check the variable it comes from",
                config.name
            )
        })?;

        custom_headers.insert(header, value);
    }

    // A configured token becomes `Authorization: Bearer …`; an absent one must
    // not send an empty bearer header, so the two shapes are built separately.
    let transport_config = match token {
        Some(token) => StreamableHttpClientTransportConfig::with_uri(url)
            .auth_header(token)
            .custom_headers(custom_headers),
        None => StreamableHttpClientTransportConfig::with_uri(url).custom_headers(custom_headers),
    };
    // rmcp's `from_config` builds a reqwest client that loads the *system* CA
    // store and `expect`s on failure, which panics in a minimal container or a
    // Nix build sandbox with no `/etc/ssl/certs`. Building the client here with
    // the bundled Mozilla roots makes a remote server behave the same way
    // everywhere, which is how the provider client already works.
    let http = tls_client().map_err(|error| {
        format!(
            "server `{}` could not create an HTTP client: {error}",
            config.name
        )
    })?;
    let transport = StreamableHttpClientTransport::with_client(http, transport_config);

    let client = ClientConfig::default()
        .serve_with_lifecycle(transport, lifecycle())
        .await
        .map_err(|error| describe_initialize_error(config, error))?;

    Ok(Started {
        client,
        child: None,
    })
}

/// A reqwest client with deterministic TLS trust.
///
/// The bundled Mozilla root set is used instead of the host certificate store,
/// so a remote MCP server works the same in a development shell, a Nix build
/// sandbox, and a minimal container.
fn tls_client() -> Result<reqwest::Client, String> {
    let mut roots = rustls::RootCertStore::empty();

    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .build()
        .map_err(|error| error.to_string())
}

/// Turn an initialization failure into an actionable message.
fn describe_initialize_error(config: &ServerConfig, error: ClientInitializeError) -> String {
    let transport = match &config.transport {
        Transport::Stdio { .. } => "stdio",
        Transport::Http { .. } => "streamable HTTP",
    };
    let detail = match &error {
        ClientInitializeError::TransportError { error, .. } => error.to_string(),
        // The variant's own message names neither cause, and the interesting
        // one is almost always the failed `initialize` after a legacy probe.
        ClientInitializeError::LegacyFallbackFailed { discover, fallback } => format!(
            "server/discover failed ({discover}) and the legacy initialize fallback failed \
             ({fallback})"
        ),
        other => other.to_string(),
    };

    if looks_like_oauth(&detail) {
        return format!(
            "server `{}` requires interactive OAuth, which t8ful does not implement; configure a \
             static token with `bearer_token_env` instead ({detail})",
            config.name
        );
    }

    format!(
        "server `{}` failed to initialize over {transport}: {detail}",
        config.name
    )
}

/// Whether a failure is the server demanding an interactive OAuth flow.
fn looks_like_oauth(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();

    lower.contains("authrequired")
        || lower.contains("www-authenticate")
        || lower.contains("insufficient_scope")
        || lower.contains("unauthorized")
        || lower.contains("401")
}

#[cfg(test)]
mod tests {
    use super::looks_like_oauth;

    #[test]
    fn oauth_style_failures_are_recognized() {
        for detail in [
            "AuthRequired(AuthRequiredError { www_authenticate_header: \"Bearer\" })",
            "HTTP 401: Unauthorized",
            "insufficient_scope",
        ] {
            assert!(looks_like_oauth(detail), "{detail}");
        }

        for detail in [
            "connection refused",
            "Transport closed",
            "JSON-RPC error: bad",
        ] {
            assert!(!looks_like_oauth(detail), "{detail}");
        }
    }
}
