//! Environment filtering for shell commands.
//!
//! Known t8ful and MCP credentials are withheld from ordinary shell commands. This
//! reduces accidental leaks; it is **not** a security boundary for unsandboxed
//! coding commands, and other inherited values may still contain secrets.

use std::ffi::{OsStr, OsString};

use tokio::process::Command;

/// Credential variables that are always withheld, whatever the configuration says.
pub const ALWAYS_DENIED: &[&str] = &["OPENROUTER_API_KEY", "OPENCODE_GO_API_KEY"];

/// Deny list for one run.
#[derive(Debug, Clone, Default)]
pub struct EnvFilter {
    denied: Vec<OsString>,
}

impl EnvFilter {
    pub fn new(extra: &[String]) -> Self {
        let mut denied = ALWAYS_DENIED.iter().map(OsString::from).collect::<Vec<_>>();

        for name in extra {
            let name = name.trim();

            if name.is_empty() {
                continue;
            }

            let name = OsString::from(name);

            if !denied.contains(&name) {
                denied.push(name);
            }
        }

        Self { denied }
    }

    pub fn denies(&self, name: &OsStr) -> bool {
        self.denied.iter().any(|denied| denied == name)
    }

    pub fn denied_names(&self) -> Vec<String> {
        self.denied
            .iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect()
    }

    /// Environment entries the child is allowed to inherit.
    pub fn environment(&self) -> Vec<(OsString, OsString)> {
        std::env::vars_os()
            .filter(|(name, _)| !self.denies(name))
            .collect()
    }

    /// Apply the filter to an existing command.
    pub fn apply(&self, command: &mut Command) {
        command.env_clear();
        command.envs(self.environment());
    }
}

#[cfg(test)]
mod tests {
    use super::EnvFilter;

    #[test]
    fn always_denies_provider_credentials() {
        let filter = EnvFilter::new(&[]);

        assert!(filter.denies(std::ffi::OsStr::new("OPENROUTER_API_KEY")));
        assert!(filter.denies(std::ffi::OsStr::new("OPENCODE_GO_API_KEY")));
        assert!(!filter.denies(std::ffi::OsStr::new("PATH")));
    }

    #[test]
    fn accepts_configured_names_without_duplicates() {
        let filter = EnvFilter::new(&[
            "GITHUB_TOKEN".to_owned(),
            " GITHUB_TOKEN ".to_owned(),
            "OPENROUTER_API_KEY".to_owned(),
            String::new(),
        ]);

        assert_eq!(
            filter.denied_names(),
            vec![
                "OPENROUTER_API_KEY".to_owned(),
                "OPENCODE_GO_API_KEY".to_owned(),
                "GITHUB_TOKEN".to_owned()
            ]
        );
    }

    #[test]
    fn filters_inherited_environment() {
        let filter = EnvFilter::new(&[]);
        let inherited = filter.environment();
        let names = inherited
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(!names.iter().any(|name| name == "OPENROUTER_API_KEY"));
        assert!(!names.iter().any(|name| name == "OPENCODE_GO_API_KEY"));
    }
}
