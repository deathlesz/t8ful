//! Resolved MCP server configuration.
//!
//! [`ServerConfig`] holds no secrets. A credential appears here only as the
//! *name* of an environment variable, which the hub reads at spawn time. The
//! `Debug` implementation prints those names and nothing else, so a server
//! configuration can be logged or journaled without leaking a token.
//!
//! Values are read through an injected [`Env`] rather than `std::env` directly,
//! so a test can supply a fixed map instead of mutating the process
//! environment.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// Where a server's environment values come from.
pub trait Env: Send + Sync {
    /// The value of `name`, or `None` when it is not set or not UTF-8.
    fn get(&self, name: &str) -> Option<String>;

    /// The value of `name`, or the reason it cannot be used.
    fn require(&self, name: &str) -> Result<String, ConfigError> {
        self.get(name).ok_or_else(|| ConfigError::MissingVariable {
            variable: name.to_owned(),
        })
    }
}

/// The real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// A fixed environment, for tests and for embedding with a controlled one.
#[derive(Debug, Clone, Default)]
pub struct MapEnv(BTreeMap<String, String>);

impl MapEnv {
    pub fn new(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        Self(entries.into_iter().collect())
    }

    pub fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        )
    }
}

impl Env for MapEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

/// Whether a server's tools may run while planning.
///
/// `read-only` is a grant, so it can only come from configuration that is
/// allowed to widen authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanTrust {
    /// Denied in plan mode with an explanation naming the setting.
    #[default]
    Mutating,
    /// Allowed in plan mode.
    ReadOnly,
}

impl PlanTrust {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "mutating" => Some(Self::Mutating),
            "read-only" | "read_only" | "readonly" => Some(Self::ReadOnly),
            _ => None,
        }
    }

    pub fn is_read_only(self) -> bool {
        self == Self::ReadOnly
    }
}

/// One credential the hub must read from the environment at spawn time.
///
/// Holds a variable name and never a value.
#[derive(Clone, PartialEq, Eq)]
pub struct EnvName(String);

impl EnvName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Read the value through `env`.
    pub fn value(&self, env: &dyn Env) -> Result<String, ConfigError> {
        env.require(&self.0)
    }
}

impl fmt::Debug for EnvName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Only the name: `EnvName` never holds a value.
        f.debug_tuple("EnvName").field(&self.0).finish()
    }
}

/// Where a server lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// A child process speaking newline-delimited JSON-RPC on its stdio.
    Stdio {
        command: Vec<String>,
        /// Resolved against the project root when relative.
        cwd: PathBuf,
    },
    /// A remote Streamable HTTP endpoint.
    Http {
        url: String,
        /// The bearer token, by variable name.
        bearer_token_env: Option<EnvName>,
        /// Extra headers, header name to variable name.
        headers: BTreeMap<String, EnvName>,
    },
}

/// One resolved server.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub name: String,
    pub transport: Transport,
    /// Variables the server's own process receives, by name.
    pub pass_env: Vec<EnvName>,
    pub plan: PlanTrust,
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("name", &self.name)
            .field("transport", &self.transport)
            .field("pass_env", &self.pass_env)
            .field("plan", &self.plan)
            .finish()
    }
}

impl ServerConfig {
    pub fn is_remote(&self) -> bool {
        matches!(self.transport, Transport::Http { .. })
    }

    /// Every environment variable name this server's credentials come from.
    ///
    /// Used to withhold them from ordinary shell commands: a token that only an
    /// MCP server should see must not be inherited by every `bash` call.
    pub fn credential_variables(&self) -> Vec<&str> {
        let mut names = self
            .pass_env
            .iter()
            .map(EnvName::as_str)
            .collect::<Vec<_>>();

        if let Transport::Http {
            bearer_token_env,
            headers,
            ..
        } = &self.transport
        {
            if let Some(name) = bearer_token_env {
                names.push(name.as_str());
            }

            names.extend(headers.values().map(EnvName::as_str));
        }

        names
    }

    /// The environment a stdio child receives.
    ///
    /// The child does not inherit t8ful's environment: only `PATH` (so a
    /// `command` such as `npx` resolves), `HOME` and `TMPDIR` (so a package
    /// cache resolves), plus the configured names. A listed variable that is
    /// absent is an error rather than a silent omission.
    pub fn stdio_env(&self, env: &dyn Env) -> Result<BTreeMap<String, String>, ConfigError> {
        let mut values = BTreeMap::new();

        for inherited in ["PATH", "HOME", "TMPDIR"] {
            if let Some(value) = env.get(inherited) {
                values.insert(inherited.to_owned(), value);
            }
        }

        for name in &self.pass_env {
            values.insert(name.as_str().to_owned(), name.value(env)?);
        }

        Ok(values)
    }

    /// The bearer token and headers for a remote server, read now.
    pub fn http_credentials(
        &self,
        env: &dyn Env,
    ) -> Result<(Option<String>, BTreeMap<String, String>), ConfigError> {
        let Transport::Http {
            bearer_token_env,
            headers,
            ..
        } = &self.transport
        else {
            return Ok((None, BTreeMap::new()));
        };

        let token = match bearer_token_env {
            Some(name) => Some(name.value(env)?),
            None => None,
        };
        let mut resolved = BTreeMap::new();

        for (header, name) in headers {
            resolved.insert(header.clone(), name.value(env)?);
        }

        Ok((token, resolved))
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("mcp server `{server}`: {message}")]
    Server { server: String, message: String },

    #[error(
        "environment variable `{variable}` is not set; it is required by a server's configuration"
    )]
    MissingVariable { variable: String },
}

impl ConfigError {
    pub fn server(server: &str, message: impl Into<String>) -> Self {
        Self::Server {
            server: server.to_owned(),
            message: message.into(),
        }
    }

    /// The error as it should appear for one server, naming it.
    pub fn for_server(self, server: &str) -> Self {
        match self {
            Self::MissingVariable { variable } => Self::Server {
                server: server.to_owned(),
                message: format!(
                    "environment variable `{variable}` is not set; it is required by the \
                     server's configuration"
                ),
            },
            other => other,
        }
    }
}

/// The default startup deadline for the whole server set.
///
/// A legacy server is discovered by `server/discover` failing first: `rmcp`
/// gives that probe 10 s before it treats the peer as legacy, and the
/// `initialize` handshake runs after it on the same connection. The deadline
/// therefore has to cover the probe plus the handshake, or a slow legacy server
/// would be reported as a timeout instead of connecting. A server that is
/// unreachable or refuses the connection still fails immediately on its
/// transport error; only one that connects and never answers waits this long.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(12);

#[cfg(test)]
mod tests {
    use super::{
        ConfigError, Env, EnvName, MapEnv, PlanTrust, ProcessEnv, ServerConfig, Transport,
    };
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn stdio(name: &str, pass_env: &[&str]) -> ServerConfig {
        ServerConfig {
            name: name.to_owned(),
            transport: Transport::Stdio {
                command: vec!["true".to_owned()],
                cwd: PathBuf::from("/"),
            },
            pass_env: pass_env.iter().map(|name| EnvName::new(*name)).collect(),
            plan: PlanTrust::default(),
        }
    }

    fn http(name: &str, token: Option<&str>, headers: &[(&str, &str)]) -> ServerConfig {
        ServerConfig {
            name: name.to_owned(),
            transport: Transport::Http {
                url: "https://example.invalid/mcp".to_owned(),
                bearer_token_env: token.map(EnvName::new),
                headers: headers
                    .iter()
                    .map(|(header, variable)| ((*header).to_owned(), EnvName::new(*variable)))
                    .collect::<BTreeMap<_, _>>(),
            },
            pass_env: Vec::new(),
            plan: PlanTrust::default(),
        }
    }

    #[test]
    fn plan_trust_parses_both_spellings() {
        assert_eq!(PlanTrust::parse("read-only"), Some(PlanTrust::ReadOnly));
        assert_eq!(PlanTrust::parse(" read_only "), Some(PlanTrust::ReadOnly));
        assert_eq!(PlanTrust::parse("MUTATING"), Some(PlanTrust::Mutating));
        assert_eq!(PlanTrust::parse("read"), None);
        assert_eq!(PlanTrust::default(), PlanTrust::Mutating);
    }

    #[test]
    fn a_missing_credential_names_the_variable_and_never_a_value() {
        let server = stdio("filesystem", &["T8FUL_TEST_ABSENT_VARIABLE"]);
        let error = server
            .stdio_env(&MapEnv::default())
            .expect_err("the variable is absent");
        let message = error.for_server("filesystem").to_string();

        assert!(message.contains("T8FUL_TEST_ABSENT_VARIABLE"), "{message}");
        assert!(message.contains("filesystem"), "{message}");
    }

    #[test]
    fn a_passed_variable_reaches_only_its_own_child_environment() {
        let env = MapEnv::from_pairs(&[
            ("T8FUL_TEST_MCP_TOKEN", "s3cret-value"),
            ("T8FUL_TEST_OTHER_TOKEN", "other-value"),
        ]);
        let values = stdio("github", &["T8FUL_TEST_MCP_TOKEN"])
            .stdio_env(&env)
            .expect("the variable is set");

        assert_eq!(
            values.get("T8FUL_TEST_MCP_TOKEN").map(String::as_str),
            Some("s3cret-value")
        );
        // The other token is not in this server's environment at all.
        assert!(!values.contains_key("T8FUL_TEST_OTHER_TOKEN"), "{values:?}");
    }

    #[test]
    fn the_child_environment_is_not_the_parent_environment() {
        let env = MapEnv::from_pairs(&[
            ("PATH", "/usr/bin"),
            ("T8FUL_TEST_UNRELATED_SECRET", "leak-me"),
        ]);
        let values = stdio("local", &[]).stdio_env(&env).expect("no error");

        assert_eq!(values.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert!(
            !values.contains_key("T8FUL_TEST_UNRELATED_SECRET"),
            "{values:?}"
        );
    }

    #[test]
    fn credential_variables_cover_the_bearer_token_and_every_header() {
        let server = http("linear", Some("LINEAR_TOKEN"), &[("X-Org", "LINEAR_ORG")]);

        assert_eq!(
            server.credential_variables(),
            vec!["LINEAR_TOKEN", "LINEAR_ORG"]
        );
    }

    #[test]
    fn debug_prints_variable_names_and_no_values() {
        let env = MapEnv::from_pairs(&[("T8FUL_TEST_DEBUG_TOKEN", "do-not-print-me")]);
        let server = http("linear", Some("T8FUL_TEST_DEBUG_TOKEN"), &[]);
        // The value is reachable only through `http_credentials`.
        let (token, _) = server.http_credentials(&env).expect("set");
        let rendered = format!("{server:?}");

        assert_eq!(token.as_deref(), Some("do-not-print-me"));
        assert!(rendered.contains("T8FUL_TEST_DEBUG_TOKEN"), "{rendered}");
        assert!(!rendered.contains("do-not-print-me"), "{rendered}");
    }

    #[test]
    fn http_credentials_are_read_on_demand() {
        let env = MapEnv::from_pairs(&[
            ("T8FUL_TEST_BEARER", "bearer-value"),
            ("T8FUL_TEST_HEADER", "header-value"),
        ]);
        let (token, headers) = http(
            "remote",
            Some("T8FUL_TEST_BEARER"),
            &[("X-Org", "T8FUL_TEST_HEADER")],
        )
        .http_credentials(&env)
        .expect("both are set");

        assert_eq!(token.as_deref(), Some("bearer-value"));
        assert_eq!(
            headers.get("X-Org").map(String::as_str),
            Some("header-value")
        );
    }

    #[test]
    fn a_stdio_server_has_no_http_credentials() {
        let (token, headers) = stdio("local", &[])
            .http_credentials(&MapEnv::default())
            .expect("no error");

        assert!(token.is_none());
        assert!(headers.is_empty());
    }

    #[test]
    fn an_absent_bearer_token_is_an_error_naming_it() {
        let server = http("remote", Some("T8FUL_TEST_ABSENT_BEARER"), &[]);
        let error = server
            .http_credentials(&MapEnv::default())
            .expect_err("absent");

        assert!(
            matches!(&error, ConfigError::MissingVariable { variable } if variable == "T8FUL_TEST_ABSENT_BEARER"),
            "{error:?}"
        );
    }

    #[test]
    fn the_process_environment_is_readable() {
        // `PATH` is set for any process the test harness starts.
        assert!(ProcessEnv.get("PATH").is_some());
        assert!(ProcessEnv.get("T8FUL_TEST_DEFINITELY_ABSENT").is_none());
    }
}
