//! Model Context Protocol clients for t8ful.
//!
//! Two transports are supported: a local child process speaking
//! newline-delimited JSON-RPC on its stdio, and a remote Streamable HTTP
//! endpoint. Both are driven through `rmcp`; this crate adds the parts t8ful
//! owns: resolved credential-free configuration, the model-visible naming
//! scheme, the plan-mode trust decision, bounded result rendering, and the
//! [`McpHub`] lifecycle that starts every server concurrently and shuts them
//! all down again.
//!
//! MCP resources, prompts, sampling, elicitation, and tasks are deliberately
//! absent. A server that demands interactive OAuth fails with an actionable
//! error naming the variable to set instead.

pub mod client;
pub mod config;
pub mod hub;
pub mod name;
pub mod render;
pub mod text;
pub mod tool;
pub mod transport;

pub use config::{
    ConfigError, DEFAULT_STARTUP_TIMEOUT, EnvName, PlanTrust, ServerConfig, Transport,
};
pub use hub::{McpHub, ServerState, untrusted_message};
pub use name::{MAX_NAME_LEN, NAMESPACE, is_mcp_name, model_name, split_model_name};
pub use tool::McpTool;

use std::collections::BTreeMap;
use std::path::Path;

/// One `[mcp.servers.<name>]` entry as it appears in a configuration file.
///
/// This is the parsed shape only. Resolution into a [`ServerConfig`] validates
/// the transport shape, resolves `cwd` against the project root, and reports
/// every credential as a name.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct ServerSection {
    /// Program and arguments for a local server.
    pub command: Option<Vec<String>>,
    /// Working directory, relative to the project root unless absolute.
    pub cwd: Option<String>,
    /// Environment variables the server receives, by name.
    pub pass_env: Vec<String>,
    /// Remote endpoint for a Streamable HTTP server.
    pub url: Option<String>,
    /// Bearer token, by environment variable name.
    pub bearer_token_env: Option<String>,
    /// Extra headers: header name to environment variable name.
    pub headers: BTreeMap<String, String>,
    /// `read-only` or `mutating`.
    pub plan: Option<String>,
}

/// The whole `[mcp]` section.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct McpSection {
    /// Per-server startup deadline, in milliseconds.
    pub startup_timeout_ms: Option<u64>,
    pub servers: BTreeMap<String, ServerSection>,
}

/// A resolved `[mcp]` section: no secrets, and validated server shapes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedMcp {
    pub startup_timeout: std::time::Duration,
    pub servers: Vec<ServerConfig>,
}

/// Resolve the `[mcp]` section against the project root.
///
/// `warnings` receives one line per server that was dropped or whose `plan`
/// grant could not be honoured. A server that is missing a transport, or that
/// has both, is a hard error: it is a configuration mistake, not a runtime
/// condition, and silently ignoring it would leave the model without tools the
/// user believes are configured.
pub fn resolve(
    section: McpSection,
    project_root: &Path,
    warnings: &mut Vec<String>,
) -> Result<ResolvedMcp, ConfigError> {
    let startup_timeout = match section.startup_timeout_ms {
        Some(0) => {
            return Err(ConfigError::server(
                "mcp",
                "`mcp.startup_timeout_ms` must be at least 1",
            ));
        }
        Some(milliseconds) => std::time::Duration::from_millis(milliseconds),
        None => DEFAULT_STARTUP_TIMEOUT,
    };

    let mut servers = Vec::with_capacity(section.servers.len());

    for (name, raw) in section.servers {
        let server = resolve_server(&name, raw, project_root, warnings)?;

        servers.push(server);
    }

    Ok(ResolvedMcp {
        startup_timeout,
        servers,
    })
}

fn resolve_server(
    name: &str,
    raw: ServerSection,
    project_root: &Path,
    warnings: &mut Vec<String>,
) -> Result<ServerConfig, ConfigError> {
    if name.trim().is_empty() {
        return Err(ConfigError::server("", "a server name must not be empty"));
    }

    let plan = match raw.plan.as_deref() {
        None => PlanTrust::Mutating,
        Some(value) => PlanTrust::parse(value).ok_or_else(|| {
            ConfigError::server(
                name,
                format!("unknown `plan` value `{value}`; expected `read-only` or `mutating`"),
            )
        })?,
    };

    let transport = match (raw.command, raw.url) {
        (Some(_), Some(_)) => {
            return Err(ConfigError::server(
                name,
                "`command` and `url` are mutually exclusive; a server is either local or remote",
            ));
        }
        (None, None) => {
            return Err(ConfigError::server(
                name,
                "one of `command` or `url` is required",
            ));
        }
        (Some(command), None) => {
            if command.is_empty() || command[0].trim().is_empty() {
                return Err(ConfigError::server(
                    name,
                    "`command` must name a program to run",
                ));
            }

            if raw.bearer_token_env.is_some() || !raw.headers.is_empty() {
                warnings.push(format!(
                    "MCP server `{name}`: `bearer_token_env` and `headers` only apply to a remote \
                     server and are ignored for a local one"
                ));
            }

            let cwd = match raw.cwd.as_deref().map(str::trim) {
                Some("") | None => project_root.to_path_buf(),
                Some(relative) => {
                    let path = Path::new(relative);

                    if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        project_root.join(path)
                    }
                }
            };

            Transport::Stdio { command, cwd }
        }
        (None, Some(url)) => {
            if url.trim().is_empty() {
                return Err(ConfigError::server(name, "`url` must not be empty"));
            }

            if raw.cwd.is_some() {
                warnings.push(format!(
                    "MCP server `{name}`: `cwd` only applies to a local server and is ignored"
                ));
            }

            Transport::Http {
                url: url.trim().to_owned(),
                bearer_token_env: raw.bearer_token_env.map(EnvName::new),
                headers: raw
                    .headers
                    .into_iter()
                    .map(|(header, variable)| (header, EnvName::new(variable)))
                    .collect(),
            }
        }
    };

    let pass_env = raw
        .pass_env
        .into_iter()
        .map(|variable| EnvName::new(variable.trim().to_owned()))
        .filter(|variable| !variable.as_str().is_empty())
        .collect::<Vec<_>>();
    let remote = matches!(transport, Transport::Http { .. });

    if remote && !pass_env.is_empty() {
        warnings.push(format!(
            "MCP server `{name}`: `pass_env` only applies to a local server and is ignored"
        ));
    }

    Ok(ServerConfig {
        name: name.to_owned(),
        transport,
        // A remote server has no process environment to pass into.
        pass_env: if remote { Vec::new() } else { pass_env },
        plan,
    })
}

#[cfg(test)]
mod tests {
    use super::{McpSection, PlanTrust, Transport, resolve};
    use std::path::Path;

    fn section(toml_text: &str) -> McpSection {
        toml::from_str(toml_text).expect("the section parses")
    }

    fn resolve_text(text: &str) -> (super::ResolvedMcp, Vec<String>) {
        let mut warnings = Vec::new();
        let resolved = resolve(section(text), Path::new("/work"), &mut warnings)
            .expect("the section resolves");

        (resolved, warnings)
    }

    #[test]
    fn a_stdio_server_resolves_its_cwd_against_the_project_root() {
        let (resolved, _) = resolve_text(
            r#"
            [servers.filesystem]
            command = ["npx", "-y", "server-filesystem", "/work"]
            cwd = "sub"
            pass_env = ["HOME"]
            "#,
        );
        let server = &resolved.servers[0];

        assert_eq!(server.name, "filesystem");
        assert_eq!(
            server.transport,
            Transport::Stdio {
                command: vec![
                    "npx".to_owned(),
                    "-y".to_owned(),
                    "server-filesystem".to_owned(),
                    "/work".to_owned(),
                ],
                cwd: std::path::PathBuf::from("/work/sub"),
            }
        );
        assert_eq!(server.plan, PlanTrust::Mutating);
        assert!(!server.is_remote());
    }

    #[test]
    fn a_remote_server_keeps_credentials_as_names() {
        let (resolved, _) = resolve_text(
            r#"
            [servers.linear]
            url = "https://mcp.linear.app/mcp"
            bearer_token_env = "LINEAR_TOKEN"
            plan = "read-only"

            [servers.linear.headers]
            "X-Org" = "LINEAR_ORG"
            "#,
        );
        let server = &resolved.servers[0];

        assert!(server.is_remote());
        assert_eq!(server.plan, PlanTrust::ReadOnly);
        assert_eq!(
            server.credential_variables(),
            vec!["LINEAR_TOKEN", "LINEAR_ORG"]
        );
    }

    #[test]
    fn a_server_with_both_transports_is_rejected() {
        let mut warnings = Vec::new();
        let error = resolve(
            section(
                r#"
                [servers.both]
                command = ["true"]
                url = "https://example.invalid/mcp"
                "#,
            ),
            Path::new("/work"),
            &mut warnings,
        )
        .expect_err("both is an error");

        assert!(error.to_string().contains("both"), "{error}");
    }

    #[test]
    fn a_server_with_neither_transport_is_rejected() {
        let mut warnings = Vec::new();
        let error = resolve(
            section("[servers.nothing]\npass_env = []\n"),
            Path::new("/work"),
            &mut warnings,
        )
        .expect_err("neither is an error");

        assert!(error.to_string().contains("required"), "{error}");
    }

    #[test]
    fn an_unknown_plan_value_is_rejected() {
        let mut warnings = Vec::new();
        let error = resolve(
            section("[servers.s]\ncommand = [\"true\"]\nplan = \"harmless\"\n"),
            Path::new("/work"),
            &mut warnings,
        )
        .expect_err("unknown plan");

        assert!(error.to_string().contains("harmless"), "{error}");
    }

    #[test]
    fn transport_specific_keys_are_ignored_with_a_warning() {
        let (resolved, warnings) = resolve_text(
            r#"
            [servers.local]
            command = ["true"]
            bearer_token_env = "TOKEN"

            [servers.remote]
            url = "https://example.invalid/mcp"
            cwd = "sub"
            pass_env = ["HOME"]
            "#,
        );

        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("bearer_token_env"))
        );
        assert!(warnings.iter().any(|warning| warning.contains("pass_env")));
        assert!(warnings.iter().any(|warning| warning.contains("`cwd`")));
        assert!(resolved.servers[1].pass_env.is_empty());
    }

    #[test]
    fn an_empty_startup_timeout_is_rejected() {
        let mut warnings = Vec::new();
        let error = resolve(
            section("startup_timeout_ms = 0\n"),
            Path::new("/work"),
            &mut warnings,
        )
        .expect_err("zero timeout");

        assert!(error.to_string().contains("startup_timeout_ms"), "{error}");
    }

    #[test]
    fn the_nix_module_shape_round_trips() {
        // Exactly what `nix/modules/settings.nix` renders for the documented
        // example, so a module regression shows up here as well.
        let text = r#"
            [mcp]
            startup_timeout_ms = 2500

            [mcp.servers.filesystem]
            command = ["npx", "-y", "srv", "/work"]
            pass_env = ["HOME"]
            plan = "read-only"

            [mcp.servers.linear]
            bearer_token_env = "LINEAR_TOKEN"
            url = "https://mcp.linear.app/mcp"
        "#;
        // The rendered file wraps the section in `[mcp]`, so it is parsed the
        // way `FileConfig` parses it rather than as a bare section.
        #[derive(serde::Deserialize)]
        struct Wrapper {
            mcp: McpSection,
        }

        let parsed = toml::from_str::<Wrapper>(text).expect("the rendered file parses");
        let mut warnings = Vec::new();
        let resolved = resolve(parsed.mcp, Path::new("/work"), &mut warnings)
            .expect("the rendered file resolves");

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            resolved.startup_timeout,
            std::time::Duration::from_millis(2500)
        );
        assert_eq!(resolved.servers.len(), 2);

        let filesystem = resolved
            .servers
            .iter()
            .find(|server| server.name == "filesystem")
            .expect("filesystem");
        let linear = resolved
            .servers
            .iter()
            .find(|server| server.name == "linear")
            .expect("linear");

        assert_eq!(filesystem.plan, PlanTrust::ReadOnly);
        assert!(linear.is_remote());
        assert_eq!(linear.credential_variables(), vec!["LINEAR_TOKEN"]);
    }

    #[test]
    fn the_default_timeout_applies_when_unset() {
        let (resolved, _) = resolve_text("");

        assert_eq!(resolved.startup_timeout, super::DEFAULT_STARTUP_TIMEOUT);
    }
}
