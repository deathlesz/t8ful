//! Root project instructions (`AGENTS.md`).
//!
//! Only the root file is loaded; nested or global instruction files are ignored.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::config::ConfigError;

/// Instructions are truncated (not rejected) beyond this size.
pub const MAX_INSTRUCTION_BYTES: usize = 128 * 1024;

pub const INSTRUCTION_FILE: &str = "AGENTS.md";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instructions {
    pub path: Option<PathBuf>,
    pub text: String,
    pub truncated: bool,
    /// Set when an instruction symlink was ignored. The frontends print it.
    pub warning: Option<String>,
}

impl Instructions {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Load `<root>/AGENTS.md` when present.
///
/// A symlink is followed only when its canonical target is a regular file that
/// stays inside the canonical project root. An in-project link keeps working;
/// an escaping or non-regular target is ignored with a warning so a repository
/// cannot pull an arbitrary host file into the system prompt.
pub fn load(root: &Path) -> Result<Instructions, ConfigError> {
    let path = root.join(INSTRUCTION_FILE);

    let canonical_root = match root.canonicalize() {
        Ok(root) => root,
        Err(source) if source.kind() == ErrorKind::NotFound => {
            return Ok(Instructions::default());
        }
        Err(source) => {
            return Err(ConfigError::Instructions { path, source });
        }
    };

    let target = match path.canonicalize() {
        Ok(target) => target,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(Instructions::default());
        }
        Err(source) => {
            return Err(ConfigError::Instructions { path, source });
        }
    };

    let ignored = if !target.starts_with(&canonical_root) {
        Some("its symlink target is outside the project root")
    } else if !std::fs::metadata(&target)
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
    {
        Some("its target is not a regular file")
    } else {
        None
    };

    if let Some(reason) = ignored {
        let warning = format!("ignoring project instructions {}: {reason}", path.display());

        tracing::warn!(target: "t8ful::instructions", "{warning}");

        return Ok(Instructions {
            path: Some(path),
            warning: Some(warning),
            ..Instructions::default()
        });
    }

    let bytes = match std::fs::read(&target) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(Instructions::default());
        }
        Err(source) => {
            return Err(ConfigError::Instructions { path, source });
        }
    };

    let truncated = bytes.len() > MAX_INSTRUCTION_BYTES;
    let bytes = if truncated {
        let mut limit = MAX_INSTRUCTION_BYTES;

        while limit > 0 && std::str::from_utf8(&bytes[..limit]).is_err() {
            limit -= 1;
        }

        &bytes[..limit]
    } else {
        &bytes[..]
    };

    let text = String::from_utf8_lossy(bytes).into_owned();

    if truncated {
        tracing::warn!(
            target: "t8ful::instructions",
            path = %path.display(),
            limit = MAX_INSTRUCTION_BYTES,
            "project instructions were truncated"
        );
    }

    Ok(Instructions {
        path: Some(path),
        text,
        truncated,
        warning: None,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::{MAX_INSTRUCTION_BYTES, load};

    #[test]
    fn missing_instructions_are_not_an_error() {
        let dir = TempDir::new().unwrap();
        let instructions = load(dir.path()).expect("load");

        assert!(instructions.is_empty());
        assert!(instructions.path.is_none());
    }

    #[test]
    fn loads_the_root_instruction_file_only() {
        let dir = TempDir::new().unwrap();

        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(dir.path().join("nested/AGENTS.md"), "nested rules").unwrap();

        let instructions = load(dir.path()).expect("load");

        assert_eq!(instructions.text, "root rules");
        assert!(!instructions.truncated);
    }

    #[test]
    fn oversized_instructions_are_truncated_with_a_flag() {
        let dir = TempDir::new().unwrap();

        std::fs::write(
            dir.path().join("AGENTS.md"),
            "a".repeat(MAX_INSTRUCTION_BYTES + 10),
        )
        .unwrap();

        let instructions = load(dir.path()).expect("load");

        assert!(instructions.truncated);
        assert_eq!(instructions.text.len(), MAX_INSTRUCTION_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn instruction_symlinks_must_stay_inside_the_root() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("docs")).unwrap();

        std::fs::write(dir.path().join("secret.txt"), "OUT-OF-ROOT").unwrap();
        std::fs::write(project.join("docs/AGENTS.md"), "IN-PROJECT-LINK").unwrap();

        for (name, link, expect_text, expect_warning) in [
            (
                "absolute escape",
                dir.path().join("secret.txt"),
                "",
                Some("outside the project root"),
            ),
            (
                "relative escape",
                std::path::PathBuf::from("../secret.txt"),
                "",
                Some("outside the project root"),
            ),
            (
                "in-project link",
                std::path::PathBuf::from("docs/AGENTS.md"),
                "IN-PROJECT-LINK",
                None,
            ),
            (
                "directory target",
                std::path::PathBuf::from("docs"),
                "",
                Some("not a regular file"),
            ),
        ] {
            let instruction = project.join("AGENTS.md");
            let _ = std::fs::remove_file(&instruction);
            symlink(link, &instruction).unwrap();

            let loaded = load(&project).expect("load");

            assert_eq!(loaded.text, expect_text, "{name}");
            assert_eq!(
                loaded.path.as_deref(),
                Some(instruction.as_path()),
                "{name}: the requested path is reported"
            );
            match expect_warning {
                Some(reason) => {
                    let warning = loaded.warning.expect("a warning");

                    assert!(warning.contains(reason), "{name}: {warning}");
                    assert!(warning.contains("AGENTS.md"), "{name}: {warning}");
                }
                None => assert!(loaded.warning.is_none(), "{name}"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_instructions_are_reported() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("AGENTS.md");

        std::fs::write(&path, "secret rules").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Running as root would bypass the permission bits; skip in that case.
        if std::fs::read(&path).is_ok() {
            return;
        }

        let error = load(dir.path()).expect_err("unreadable file");

        assert!(error.to_string().contains("AGENTS.md"), "{error}");
    }
}
