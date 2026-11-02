//! XDG state directories for sessions, attachments, and the usage ledger.

use std::path::PathBuf;

use crate::config::EnvSource;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePaths {
    pub root: PathBuf,
    pub sessions: PathBuf,
    pub usage: PathBuf,
}

impl StatePaths {
    pub fn discover(env: &EnvSource) -> Self {
        let root = non_empty(env.get("XDG_STATE_HOME"))
            .map(PathBuf::from)
            .or_else(|| {
                non_empty(env.get("HOME")).map(|home| PathBuf::from(home).join(".local/state"))
            })
            .unwrap_or_else(|| PathBuf::from("."))
            .join("t8ful");

        Self {
            sessions: root.join("sessions"),
            usage: root.join("usage"),
            root,
        }
    }

    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.sessions.join(id)
    }

    pub fn journal_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("journal.jsonl")
    }

    pub fn attachment_dir(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("attachments")
    }

    pub fn usage_ledger(&self) -> PathBuf {
        self.usage.join("ledger.jsonl")
    }

    pub fn usage_lock(&self) -> PathBuf {
        self.usage.join("lock")
    }
}

/// Create a directory with private permissions.
pub fn ensure_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    set_mode(path, 0o700)
}

/// Create or tighten a private file.
/// Create or tighten a private file.
///
/// Creates the file when it does not exist; tightening the mode of a missing
/// file fails, which would break every caller that creates its file lazily.
pub fn ensure_private_file(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }

    if !path.exists() {
        use std::os::unix::fs::OpenOptionsExt;

        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
    }

    set_mode(path, 0o600)
}

pub fn open_private_append(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .mode(0o600)
        .open(path)?;

    set_mode(path, 0o600)?;

    Ok(file)
}

fn set_mode(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{StatePaths, ensure_private_dir};
    use crate::config::EnvSource;

    #[test]
    fn state_paths_follow_xdg_state_home() {
        let env = EnvSource::Map(BTreeMap::from([(
            "XDG_STATE_HOME".to_owned(),
            "/state".to_owned(),
        )]));
        let paths = StatePaths::discover(&env);

        assert_eq!(paths.root, std::path::PathBuf::from("/state/t8ful"));
        assert_eq!(
            paths.usage_ledger(),
            std::path::PathBuf::from("/state/t8ful/usage/ledger.jsonl")
        );
    }

    #[test]
    fn private_directories_use_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("state");
        ensure_private_dir(&nested).unwrap();

        assert_eq!(
            std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
