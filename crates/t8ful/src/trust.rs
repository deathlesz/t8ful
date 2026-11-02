//! Per-project trust decisions.
//!
//! A project's configuration may define MCP servers, select the provider
//! endpoint, widen the skill mount set, or choose the notifier command. None of
//! that is safe to take from a directory the user merely opened, so the project
//! layer is restricted by default and only applies in full once the user has
//! trusted that project.
//!
//! Trust is recorded per canonical project root in `trust.json`, owner-only and
//! written atomically, and it is bound to the hash of the project configuration
//! it was granted for. A later change to that file asks again: otherwise a
//! `git pull` could quietly add a server that runs at startup under a decision
//! the user made about different content.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::{StatePaths, ensure_private_dir, ensure_private_file};

pub const TRUST_FILE: &str = "trust.json";

const TRUST_VERSION: u32 = 1;

/// Whether a project's configuration may be applied in full for this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProjectTrust {
    /// Project configuration is restricted to ordinary settings.
    #[default]
    Restricted,
    /// The user trusted this project's configuration.
    Trusted,
}

impl ProjectTrust {
    pub fn is_trusted(self) -> bool {
        self == Self::Trusted
    }
}

/// What the store knows about one project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustState {
    /// The project was trusted and its configuration is unchanged since.
    Trusted,
    /// No decision was recorded for this project.
    Absent,
    /// The project was trusted, but its configuration has changed since.
    Changed,
}

impl TrustState {
    pub fn is_trusted(self) -> bool {
        self == Self::Trusted
    }
}

/// One recorded decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustEntry {
    /// When the decision was recorded, in Unix seconds.
    pub trusted_at: u64,
    /// Hash of the project configuration file as it was when trusted.
    ///
    /// `None` records that the project was trusted while it had no
    /// configuration file at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_sha256: Option<String>,
}

/// The on-disk document.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustFile {
    version: u32,
    #[serde(default)]
    projects: BTreeMap<String, TrustEntry>,
}

impl Default for TrustFile {
    fn default() -> Self {
        Self {
            version: TRUST_VERSION,
            projects: BTreeMap::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("{path} is not a readable trust file ({source}); delete it to start over")]
    Malformed {
        path: PathBuf,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("cannot save {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// The trust file inside the t8ful state directory.
pub fn path(paths: &StatePaths) -> PathBuf {
    paths.root.join(TRUST_FILE)
}

/// Hash a project's configuration file, or `None` when it does not exist.
///
/// A project with no configuration file cannot require trust, and `None` is
/// distinct from the hash of an empty file so adding one is a real change.
pub fn config_hash(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;

    Some(digest(&bytes))
}

/// SHA-256 of `bytes`, as lower-case hex.
fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    // `sha2` 0.11's output type no longer implements `LowerHex`, so the digest
    // is formatted byte by byte. The result is the same 64 hex characters that
    // 0.10's `{:x}` produced, so a recorded `trust.json` keeps matching.
    let hash = Sha256::digest(bytes);
    let mut hex = String::with_capacity(hash.len() * 2);

    for byte in hash {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }

    hex
}

/// Read the whole document, treating a missing file as empty.
fn read(path: &Path) -> Result<TrustFile, TrustError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(TrustFile::default()),
        Err(source) => {
            return Err(TrustError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };

    serde_json::from_slice(&bytes).map_err(|error| TrustError::Malformed {
        path: path.to_owned(),
        source: Box::new(error),
    })
}

/// Write the document owner-only and atomically.
fn write(path: &Path, file: &TrustFile) -> Result<(), TrustError> {
    let payload = serde_json::to_vec_pretty(file).map_err(|error| TrustError::Write {
        path: path.to_owned(),
        source: std::io::Error::other(error),
    })?;

    if let Some(parent) = path.parent() {
        ensure_private_dir(parent).map_err(|source| TrustError::Write {
            path: parent.to_owned(),
            source,
        })?;
    }

    // Rename over the target so a reader sees the old document or the new one,
    // never a half-written one. A decision the user made must not be lost to a
    // crash mid-write, because losing it means asking again at random.
    let temporary = path.with_extension("json.tmp");

    std::fs::write(&temporary, payload).map_err(|source| TrustError::Write {
        path: temporary.clone(),
        source,
    })?;
    ensure_private_file(&temporary).map_err(|source| TrustError::Write {
        path: temporary.clone(),
        source,
    })?;
    std::fs::rename(&temporary, path).map_err(|source| {
        let _ = std::fs::remove_file(&temporary);

        TrustError::Write {
            path: path.to_owned(),
            source,
        }
    })?;

    Ok(())
}

/// The trust key for a project root.
///
/// The root is canonicalized when it can be, so a symlinked or relative launch
/// path names the same entry as the real directory. A root that cannot be
/// canonicalized is used as given rather than skipped: refusing to record a
/// decision would make the prompt reappear forever.
pub fn project_key(root: &Path) -> String {
    std::fs::canonicalize(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Whether `root` is trusted for the configuration currently on disk.
pub fn state(
    paths: &StatePaths,
    root: &Path,
    config: Option<&str>,
) -> Result<TrustState, TrustError> {
    let file = read(&path(paths))?;
    let key = project_key(root);

    let Some(entry) = file.projects.get(&key) else {
        return Ok(TrustState::Absent);
    };

    match (&entry.config_sha256, config) {
        (None, None) => Ok(TrustState::Trusted),
        (Some(recorded), Some(current)) if recorded == current => Ok(TrustState::Trusted),
        // A file appeared, disappeared, or changed content since the decision.
        _ => Ok(TrustState::Changed),
    }
}

/// Record a decision for `root` against the configuration currently on disk.
pub fn record(
    paths: &StatePaths,
    root: &Path,
    config: Option<&str>,
    now: u64,
) -> Result<(), TrustError> {
    let mut file = read(&path(paths))?;

    file.version = TRUST_VERSION;
    file.projects.insert(
        project_key(root),
        TrustEntry {
            trusted_at: now,
            config_sha256: config.map(str::to_owned),
        },
    );

    write(&path(paths), &file)
}

/// Forget a decision, returning whether one was recorded.
pub fn revoke(paths: &StatePaths, root: &Path) -> Result<bool, TrustError> {
    let mut file = read(&path(paths))?;
    let removed = file.projects.remove(&project_key(root)).is_some();

    if removed {
        write(&path(paths), &file)?;
    }

    Ok(removed)
}

/// Every recorded project, in path order.
pub fn list(paths: &StatePaths) -> Result<BTreeMap<String, TrustEntry>, TrustError> {
    Ok(read(&path(paths))?.projects)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::{
        ProjectTrust, TrustError, TrustState, config_hash, digest, list, path, project_key, record,
        revoke, state,
    };
    use crate::state::StatePaths;

    fn paths() -> (TempDir, StatePaths) {
        let dir = TempDir::new().expect("temp dir");
        let paths = StatePaths::discover(&crate::config::EnvSource::Map(
            std::collections::BTreeMap::from([(
                "XDG_STATE_HOME".to_owned(),
                dir.path().display().to_string(),
            )]),
        ));

        (dir, paths)
    }

    #[test]
    fn a_project_with_no_decision_is_absent() {
        let (_dir, paths) = paths();

        assert_eq!(
            state(&paths, Path::new("/work/repo"), Some("abc")).expect("state"),
            TrustState::Absent
        );
        assert!(!TrustState::Absent.is_trusted());
    }

    #[test]
    fn a_recorded_decision_is_reused_while_the_configuration_is_unchanged() {
        let (_dir, paths) = paths();

        record(&paths, Path::new("/work/repo"), Some("abc"), 1_700_000_000).expect("record");

        assert_eq!(
            state(&paths, Path::new("/work/repo"), Some("abc")).expect("state"),
            TrustState::Trusted
        );
        assert!(TrustState::Trusted.is_trusted());
    }

    #[test]
    fn changing_the_configuration_asks_again() {
        let (_dir, paths) = paths();

        record(&paths, Path::new("/work/repo"), Some("abc"), 1_700_000_000).expect("record");

        // Different content, a file that appeared, and a file that vanished all
        // mean the decision was made about something else.
        for current in [Some("def"), None] {
            assert_eq!(
                state(&paths, Path::new("/work/repo"), current).expect("state"),
                TrustState::Changed,
                "{current:?}"
            );
        }

        record(&paths, Path::new("/work/repo"), None, 1_700_000_001).expect("record");

        assert_eq!(
            state(&paths, Path::new("/work/repo"), Some("abc")).expect("state"),
            TrustState::Changed
        );
        assert_eq!(
            state(&paths, Path::new("/work/repo"), None).expect("state"),
            TrustState::Trusted
        );
    }

    #[test]
    fn trust_does_not_extend_to_a_sibling_with_a_shared_prefix() {
        let (_dir, paths) = paths();

        record(&paths, Path::new("/work/repo"), Some("abc"), 1).expect("record");

        assert_eq!(
            state(&paths, Path::new("/work/repo-other"), Some("abc")).expect("state"),
            TrustState::Absent
        );
        assert_eq!(
            state(&paths, Path::new("/work/repo/nested"), Some("abc")).expect("state"),
            TrustState::Absent
        );
    }

    #[test]
    fn revoking_removes_only_that_project() {
        let (_dir, paths) = paths();

        record(&paths, Path::new("/work/one"), None, 1).expect("record");
        record(&paths, Path::new("/work/two"), None, 2).expect("record");

        assert!(revoke(&paths, Path::new("/work/one")).expect("revoke"));
        assert!(!revoke(&paths, Path::new("/work/one")).expect("revoke twice"));

        let listed = list(&paths).expect("list");

        assert_eq!(listed.len(), 1);
        assert!(listed.contains_key("/work/two"));
    }

    #[test]
    fn the_file_is_private_and_written_without_leaving_a_temporary() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, paths) = paths();

        record(&paths, Path::new("/work/repo"), None, 1).expect("record");

        let file = path(&paths);
        let mode = std::fs::metadata(&file)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o600, "{mode:o}");
        assert!(!file.with_extension("json.tmp").exists());

        let parent = file.parent().expect("parent");
        let parent_mode = std::fs::metadata(parent)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(parent_mode, 0o700, "{parent_mode:o}");
    }

    #[test]
    fn a_malformed_file_names_the_path() {
        let (_dir, paths) = paths();

        std::fs::create_dir_all(paths.root.clone()).expect("state dir");
        std::fs::write(path(&paths), b"{ not json").expect("write");

        let error = state(&paths, Path::new("/work/repo"), None).expect_err("malformed");

        assert!(matches!(error, TrustError::Malformed { .. }), "{error:?}");
        assert!(error.to_string().contains("trust.json"), "{error}");
    }

    #[test]
    fn a_canonical_root_and_its_symlink_share_one_entry() {
        let (dir, paths) = paths();
        let real = dir.path().join("real");
        let link = dir.path().join("link");

        std::fs::create_dir_all(&real).expect("real dir");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        record(&paths, &real, None, 1).expect("record");

        assert_eq!(
            state(&paths, &link, None).expect("state"),
            TrustState::Trusted
        );
        assert_eq!(project_key(&real), project_key(&link));
    }

    #[test]
    fn a_missing_configuration_has_no_hash() {
        let dir = TempDir::new().expect("temp dir");

        assert_eq!(config_hash(&dir.path().join("absent.toml")), None);
        assert_eq!(
            config_hash(&dir.path().join("absent.toml")),
            None,
            "hashing twice is stable"
        );
    }

    #[test]
    fn a_present_configuration_hashes_to_its_contents() {
        let dir = TempDir::new().expect("temp dir");
        let file = dir.path().join("config.toml");

        std::fs::write(&file, b"a").expect("write");
        let first = config_hash(&file).expect("hash");

        std::fs::write(&file, b"b").expect("write");
        let second = config_hash(&file).expect("hash");

        assert_ne!(first, second);
        assert_eq!(first, digest(b"a"));
    }

    #[test]
    fn the_default_is_restricted() {
        assert_eq!(ProjectTrust::default(), ProjectTrust::Restricted);
        assert!(ProjectTrust::Trusted.is_trusted());
        assert!(!ProjectTrust::Restricted.is_trusted());
    }
}
