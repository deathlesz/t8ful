//! Stored API credentials for the configured providers.
//!
//! `auth.json` lives in the t8ful state directory, is created owner-only, and is
//! written atomically. The environment variable for a provider always wins over
//! a stored key so a shell can override what `/login` saved, and saving or
//! forgetting one provider leaves the other untouched.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::{StatePaths, ensure_private_dir, ensure_private_file};

pub const AUTH_FILE: &str = "auth.json";

/// The provider id `auth.json` and `/login` use for OpenRouter.
pub const OPENROUTER: &str = "openrouter";
/// The provider id `auth.json` and `/login` use for OpenCode Go.
pub const OPENCODE_GO: &str = t8ful_opencode::PROVIDER_ID;

const AUTH_VERSION: u32 = 1;

/// Where the active credential came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AuthSource {
    /// The provider's environment variable is set.
    Environment,
    /// The key was saved by `/login`.
    Stored,
    /// No credential is configured.
    #[default]
    Missing,
}

/// The credential in effect for one provider.
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

/// The on-disk document. Deliberately not `Debug`: it holds the raw keys.
///
/// The `version` stays `1` for the OpenCode Go addition: an older file simply
/// lacks the field and reads as "nothing stored for that provider".
#[derive(Default, Serialize, Deserialize)]
struct AuthFile {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    openrouter: Option<ProviderAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    opencode_go: Option<ProviderAuth>,
}

#[derive(Serialize, Deserialize)]
struct ProviderAuth {
    api_key: String,
}

impl AuthFile {
    fn slot(&mut self, provider: &str) -> Option<&mut Option<ProviderAuth>> {
        match normalize(provider)? {
            OPENROUTER => Some(&mut self.openrouter),
            _ => Some(&mut self.opencode_go),
        }
    }

    fn stored(&self, provider: &str) -> Option<&ProviderAuth> {
        match normalize(provider)? {
            OPENROUTER => self.openrouter.as_ref(),
            _ => self.opencode_go.as_ref(),
        }
    }
}

/// Canonicalize a provider name, or `None` when it names no known provider.
///
/// `opencode` and `opencode_go` are accepted as spellings of `opencode-go` so a
/// hand-typed `/login opencode` does not silently do nothing.
fn normalize(provider: &str) -> Option<&'static str> {
    let trimmed = provider.trim();

    if trimmed.eq_ignore_ascii_case(OPENROUTER) {
        return Some(OPENROUTER);
    }

    if trimmed.eq_ignore_ascii_case(OPENCODE_GO)
        || trimmed.eq_ignore_ascii_case("opencode")
        || trimmed.eq_ignore_ascii_case("opencode_go")
    {
        return Some(OPENCODE_GO);
    }

    None
}

/// The environment variable that overrides a stored key for one provider.
pub fn environment_variable(provider: &str) -> Option<&'static str> {
    match normalize(provider)? {
        OPENROUTER => Some("OPENROUTER_API_KEY"),
        _ => Some("OPENCODE_GO_API_KEY"),
    }
}

/// Whether a provider name is one t8ful supports.
pub fn is_known_provider(provider: &str) -> bool {
    normalize(provider).is_some()
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

    #[error("unknown provider `{0}`; use openrouter or opencode-go")]
    UnknownProvider(String),
}

/// The credentials file inside the t8ful state directory.
pub fn path(paths: &StatePaths) -> PathBuf {
    paths.root.join(AUTH_FILE)
}

/// Resolve the credential in effect for one provider: the environment wins over
/// a stored key.
pub fn resolve(
    paths: &StatePaths,
    provider: &str,
    environment: Option<&str>,
) -> Result<Auth, AuthError> {
    if let Some(key) = environment.filter(|key| !key.trim().is_empty()) {
        return Ok(Auth {
            key: Some(key.to_owned()),
            source: AuthSource::Environment,
        });
    }

    match stored_key(&path(paths), provider)? {
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

/// Read the stored key for one provider, if any.
pub fn stored_key(path: &Path, provider: &str) -> Result<Option<String>, AuthError> {
    let normalized = normalize(provider)
        .ok_or_else(|| AuthError::UnknownProvider(provider.trim().to_owned()))?;
    let file = read_document(path)?;
    let Some(file) = file else {
        return Ok(None);
    };

    Ok(file
        .stored(normalized)
        .map(|stored| stored.api_key.clone())
        .filter(|key| !key.trim().is_empty()))
}

/// Read and validate the document, or `None` when the file does not exist.
fn read_document(path: &Path) -> Result<Option<AuthFile>, AuthError> {
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

    Ok(Some(file))
}

/// Save one provider's key owner-only and atomically, leaving the others intact.
pub fn save(path: &Path, provider: &str, api_key: &str) -> Result<(), AuthError> {
    let mut file = read_document(path)?.unwrap_or_default();

    file.version = AUTH_VERSION;

    let Some(slot) = file.slot(provider) else {
        return Err(AuthError::UnknownProvider(provider.trim().to_owned()));
    };

    *slot = Some(ProviderAuth {
        api_key: api_key.to_owned(),
    });

    let payload = serde_json::to_vec_pretty(&file).map_err(|error| AuthError::Write {
        path: path.to_owned(),
        source: std::io::Error::other(error),
    })?;

    write_atomically(path, payload)
}

/// Remove one provider's stored key, leaving any other providers untouched.
pub fn remove(path: &Path, provider: &str) -> Result<(), AuthError> {
    let Some(mut file) = read_document(path)? else {
        return Ok(());
    };

    let Some(slot) = file.slot(provider) else {
        return Err(AuthError::UnknownProvider(provider.trim().to_owned()));
    };

    if slot.take().is_none() {
        return Ok(());
    }

    let payload = serde_json::to_vec_pretty(&file).map_err(|error| AuthError::Write {
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::{
        AuthError, AuthSource, OPENCODE_GO, OPENROUTER, path, remove, resolve, save, stored_key,
    };
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

        save(&file, OPENROUTER, "sk-or-stored").expect("save");

        assert_eq!(
            stored_key(&file, OPENROUTER).expect("read").as_deref(),
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

        save(&file, OPENROUTER, "first").expect("save");
        save(&file, OPENROUTER, "second").expect("save");

        assert_eq!(
            stored_key(&file, OPENROUTER).expect("read").as_deref(),
            Some("second")
        );
        assert!(!file.with_extension("json.tmp").exists());
    }

    #[test]
    fn the_environment_wins_over_a_stored_key() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);

        save(&path(&paths), OPENROUTER, "sk-or-stored").expect("save");

        assert_eq!(
            resolve(&paths, OPENROUTER, Some("sk-or-env")).expect("resolve"),
            super::Auth {
                key: Some("sk-or-env".to_owned()),
                source: AuthSource::Environment,
            }
        );
        assert_eq!(
            resolve(&paths, OPENROUTER, None).expect("resolve"),
            super::Auth {
                key: Some("sk-or-stored".to_owned()),
                source: AuthSource::Stored,
            }
        );
        assert_eq!(
            resolve(&paths, OPENROUTER, Some("  ")).expect("resolve"),
            super::Auth {
                key: Some("sk-or-stored".to_owned()),
                source: AuthSource::Stored,
            }
        );
    }

    #[test]
    fn the_go_environment_variable_wins_over_a_stored_go_key() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);

        save(&path(&paths), OPENCODE_GO, "oc_sk_stored").expect("save");

        assert_eq!(
            resolve(&paths, OPENCODE_GO, Some("oc_sk_env")).expect("resolve"),
            super::Auth {
                key: Some("oc_sk_env".to_owned()),
                source: AuthSource::Environment,
            }
        );
        // An OpenRouter variable says nothing about the Go credential.
        assert_eq!(
            resolve(&paths, OPENCODE_GO, None).expect("resolve"),
            super::Auth {
                key: Some("oc_sk_stored".to_owned()),
                source: AuthSource::Stored,
            }
        );
    }

    #[test]
    fn a_missing_file_means_no_credential() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);

        assert_eq!(
            resolve(&paths, OPENROUTER, None).expect("resolve"),
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

        let error = stored_key(&file, OPENROUTER).expect_err("malformed");
        let message = format!("{error}");

        assert!(matches!(error, AuthError::Malformed { .. }));
        assert!(message.contains("auth.json"), "{message}");
        assert!(message.contains("delete it"), "{message}");
    }

    #[test]
    fn removal_forgets_only_the_named_provider() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let file = path(&paths);

        save(&file, OPENROUTER, "sk-or-stored").expect("save");
        save(&file, OPENCODE_GO, "oc_sk_stored").expect("save");
        remove(&file, OPENROUTER).expect("remove");

        // A readable document, not a truncated one: `stored_key` fails on a
        // malformed file, and the key itself must be gone.
        assert_eq!(stored_key(&file, OPENROUTER).expect("read"), None);
        assert_eq!(
            stored_key(&file, OPENCODE_GO).expect("read").as_deref(),
            Some("oc_sk_stored")
        );
        assert!(file.exists());

        let document = std::fs::read_to_string(&file).expect("read the file");

        assert!(!document.contains("sk-or-stored"), "{document}");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!file.with_extension("json.tmp").exists());

        remove(&file, OPENROUTER).expect("removal is idempotent");
        remove(&file, OPENCODE_GO).expect("remove");
        assert_eq!(stored_key(&file, OPENCODE_GO).expect("read"), None);
    }

    #[test]
    fn a_file_written_before_go_existed_still_reads_its_openrouter_key() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(&dir);
        let file = path(&paths);

        crate::state::ensure_private_dir(&paths.root).unwrap();
        std::fs::write(
            &file,
            "{\n  \"version\": 1,\n  \"openrouter\": {\n    \"api_key\": \"sk-or-old\"\n  }\n}\n",
        )
        .unwrap();

        assert_eq!(
            stored_key(&file, OPENROUTER).expect("read").as_deref(),
            Some("sk-or-old")
        );
        assert_eq!(stored_key(&file, OPENCODE_GO).expect("read"), None);

        // Saving Go keeps the old OpenRouter key in place.
        save(&file, OPENCODE_GO, "oc_sk_new").expect("save");

        assert_eq!(
            stored_key(&file, OPENROUTER).expect("read").as_deref(),
            Some("sk-or-old")
        );
        assert_eq!(
            stored_key(&file, OPENCODE_GO).expect("read").as_deref(),
            Some("oc_sk_new")
        );
    }

    #[test]
    fn an_unknown_provider_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = path(&paths(&dir));

        assert!(matches!(
            save(&file, "anthropic", "key"),
            Err(AuthError::UnknownProvider(_))
        ));
        assert!(matches!(
            stored_key(&file, "anthropic"),
            Err(AuthError::UnknownProvider(_))
        ));
    }

    #[test]
    fn the_debug_representation_never_reveals_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let auth = resolve(&paths(&dir), OPENROUTER, Some("sk-or-secret")).expect("resolve");

        assert!(!format!("{auth:?}").contains("sk-or-secret"));
    }
}
