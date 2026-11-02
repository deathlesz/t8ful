//! Stored API credentials for the OpenRouter provider.
//!
//! `auth.json` lives in the t8ful state directory, is created owner-only, and is
//! written atomically. The environment variable always wins over a stored key
//! so a shell can override what `/login` saved.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::{StatePaths, ensure_private_dir, ensure_private_file};

pub const AUTH_FILE: &str = "auth.json";

const AUTH_VERSION: u32 = 1;

/// Where the active credential came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSource {
    /// `OPENROUTER_API_KEY` is set.
    Environment,
    /// The key was saved by `/login`.
    Stored,
    /// No credential is configured.
    Missing,
}

/// The credential in effect for this process.
#[derive(Clone, PartialEq, Eq)]
pub struct Auth {
    pub key: Option<String>,
    pub source: AuthSource,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("key", &self.key.as_ref().map(|_| "***"))
            .field("source", &self.source)
            .finish()
    }
}

/// The on-disk document. Deliberately not `Debug`: it holds the raw key.
#[derive(Default, Serialize, Deserialize)]
struct AuthFile {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    openrouter: Option<ProviderAuth>,
}

#[derive(Serialize, Deserialize)]
struct ProviderAuth {
    api_key: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{path} is not a readable credentials file ({source}); delete it and run /login again")]
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

/// The credentials file inside the t8ful state directory.
pub fn path(paths: &StatePaths) -> PathBuf {
    paths.root.join(AUTH_FILE)
}

/// Resolve the credential in effect: the environment wins over a stored key.
pub fn resolve(paths: &StatePaths, environment: Option<&str>) -> Result<Auth, AuthError> {
    if let Some(key) = environment.filter(|key| !key.trim().is_empty()) {
        return Ok(Auth {
            key: Some(key.to_owned()),
            source: AuthSource::Environment,
        });
    }

    match stored_key(&path(paths))? {
        Some(key) => Ok(Auth {
            key: Some(key),
            source: AuthSource::Stored,
        }),
        None => Ok(Auth {
            key: None,
            source: AuthSource::Missing,
        }),
    }
}

/// Read the stored OpenRouter key, if any.
pub fn stored_key(path: &Path) -> Result<Option<String>, AuthError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AuthError::Read {
                path: path.to_owned(),
                source: error,
            });
        }
    };
    let file: AuthFile = serde_json::from_slice(&bytes).map_err(|error| AuthError::Malformed {
        path: path.to_owned(),
        source: Box::new(error),
    })?;

    if file.version != AUTH_VERSION {
        return Err(AuthError::Malformed {
            path: path.to_owned(),
            source: format!(
                "unsupported credentials version {} (expected {AUTH_VERSION})",
                file.version
            )
            .into(),
        });
    }

    Ok(file
        .openrouter
        .map(|provider| provider.api_key)
        .filter(|key| !key.trim().is_empty()))
}

/// Save the OpenRouter key owner-only and atomically.
pub fn save(path: &Path, api_key: &str) -> Result<(), AuthError> {
    let payload = serde_json::to_vec_pretty(&AuthFile {
        version: AUTH_VERSION,
        openrouter: Some(ProviderAuth {
            api_key: api_key.to_owned(),
        }),
    })
    .map_err(|error| AuthError::Write {
        path: path.to_owned(),
        source: std::io::Error::other(error),
    })?;

    write_atomically(path, payload)
}

/// Replace the credentials file owner-only and atomically.
///
/// The temporary file is renamed over the target, so a reader sees either the
/// old document or the new one and never a half-written key. Every writer uses
/// this sequence: `/logout` truncating the live file in place would leave an
/// unparseable credential file (a startup error) if the process died mid-write.
fn write_atomically(path: &Path, payload: Vec<u8>) -> Result<(), AuthError> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent).map_err(|source| AuthError::Write {
            path: parent.to_owned(),
            source,
        })?;
    }

    let temporary = path.with_extension("json.tmp");

    std::fs::write(&temporary, payload).map_err(|source| AuthError::Write {
        path: temporary.clone(),
        source,
    })?;
    ensure_private_file(&temporary).map_err(|source| AuthError::Write {
        path: temporary.clone(),
        source,
    })?;
    std::fs::rename(&temporary, path).map_err(|source| {
        let _ = std::fs::remove_file(&temporary);

        AuthError::Write {
            path: path.to_owned(),
            source,
        }
    })?;

    Ok(())
}

/// Remove the stored key, leaving any other providers untouched.
pub fn remove(path: &Path) -> Result<(), AuthError> {
    let file = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(AuthError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    let mut auth: AuthFile =
        serde_json::from_slice(&file).map_err(|error| AuthError::Malformed {
            path: path.to_owned(),
            source: Box::new(error),
        })?;

    if auth.openrouter.take().is_none() {
        return Ok(());
    }

    let payload = serde_json::to_vec_pretty(&auth).map_err(|error| AuthError::Write {
        path: path.to_owned(),
        source: std::io::Error::other(error),
    })?;

    write_atomically(path, payload)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::{AuthError, AuthSource, path, remove, resolve, save, stored_key};
    use crate::config::EnvSource;
    use crate::state::StatePaths;

    fn paths(dir: &tempfile::TempDir) -> StatePaths {
        StatePaths::discover(&EnvSource::Map(std::collections::BTreeMap::from([(
            "XDG_STATE_HOME".to_owned(),
            dir.path().display().to_string(),
        )])))
    }

    #[test]
    fn a_saved_key_round_trips_with_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let file = path(&paths);

        save(&file, "sk-or-stored").expect("save");

        assert_eq!(
            stored_key(&file).expect("read").as_deref(),
            Some("sk-or-stored")
        );

        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        let parent = std::fs::metadata(paths.root.as_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o600);
        assert_eq!(parent, 0o700);
    }

    #[test]
    fn saving_again_overwrites_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let file = path(&paths);

        save(&file, "first").expect("save");
        save(&file, "second").expect("save");

        assert_eq!(stored_key(&file).expect("read").as_deref(), Some("second"));
        assert!(!file.with_extension("json.tmp").exists());
    }

    #[test]
    fn the_environment_wins_over_a_stored_key() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);

        save(&path(&paths), "sk-or-stored").expect("save");

        assert_eq!(
            resolve(&paths, Some("sk-or-env")).expect("resolve"),
            super::Auth {
                key: Some("sk-or-env".to_owned()),
                source: AuthSource::Environment,
            }
        );
        assert_eq!(
            resolve(&paths, None).expect("resolve"),
            super::Auth {
                key: Some("sk-or-stored".to_owned()),
                source: AuthSource::Stored,
            }
        );
        assert_eq!(
            resolve(&paths, Some("  ")).expect("resolve"),
            super::Auth {
                key: Some("sk-or-stored".to_owned()),
                source: AuthSource::Stored,
            }
        );
    }

    #[test]
    fn a_missing_file_means_no_credential() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);

        assert_eq!(
            resolve(&paths, None).expect("resolve"),
            super::Auth {
                key: None,
                source: AuthSource::Missing,
            }
        );
    }

    #[test]
    fn a_malformed_file_names_the_path_and_suggests_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let file = path(&paths);

        crate::state::ensure_private_dir(&paths.root).unwrap();
        std::fs::write(&file, "{not json").unwrap();

        let error = stored_key(&file).expect_err("malformed");
        let message = format!("{error}");

        assert!(matches!(error, AuthError::Malformed { .. }));
        assert!(message.contains("auth.json"), "{message}");
        assert!(message.contains("delete it"), "{message}");
    }

    #[test]
    fn removal_forgets_only_the_openrouter_key() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let file = path(&paths);

        save(&file, "sk-or-stored").expect("save");
        remove(&file).expect("remove");

        // A readable document, not a truncated one: `stored_key` fails on a
        // malformed file, and the key itself must be gone.
        assert_eq!(stored_key(&file).expect("read"), None);
        assert!(file.exists());

        let document = std::fs::read_to_string(&file).expect("read the file");

        assert!(!document.contains("api_key"), "{document}");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!file.with_extension("json.tmp").exists());

        remove(&file).expect("removal is idempotent");
    }

    #[test]
    fn the_debug_representation_never_reveals_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let auth = resolve(&paths(&dir), Some("sk-or-secret")).expect("resolve");

        assert!(!format!("{auth:?}").contains("sk-or-secret"));
    }
}
