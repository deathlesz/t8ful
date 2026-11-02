//! Project root discovery.
//!
//! The root is the Git worktree root when the launch directory is inside a
//! repository, and the launch directory otherwise. Git failures other than
//! "not a repository" are reported instead of being mistaken for a non-Git
//! directory.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSource {
    Git,
    LaunchDir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootInfo {
    pub path: PathBuf,
    pub source: RootSource,
}

#[derive(Debug, thiserror::Error)]
pub enum RootError {
    #[error("git could not be started in {launch}: {detail}")]
    GitUnavailable { launch: PathBuf, detail: String },

    #[error("git root discovery failed in {launch}: {detail}")]
    GitFailed { launch: PathBuf, detail: String },

    #[error("git reported an empty repository root for {launch}")]
    EmptyRoot { launch: PathBuf },

    #[error("cannot resolve {path}: {detail}")]
    Unresolvable { path: PathBuf, detail: String },

    #[error(
        "refusing project root {root} for launch {launch}: {reason}; check `core.worktree` in \
         the repository's Git configuration if the repository was moved"
    )]
    UnusableRoot {
        launch: PathBuf,
        root: PathBuf,
        reason: String,
    },
}

/// Discover the project root for `launch` using `git` from `PATH`.
pub fn discover(launch: &Path) -> Result<RootInfo, RootError> {
    let git = find_in_path("git");

    discover_with(launch, git.as_deref())
}

/// Discovery with an explicit git program, so tests can simulate a missing or
/// failing git.
///
/// The launch directory and the reported root are canonicalized, and the root
/// must contain the launch directory, must not be `/`, and must carry a `.git`
/// entry. A `[core] worktree` redirect that violates this is rejected instead
/// of silently adopting an unrelated tree as the project.
pub fn discover_with(launch: &Path, git: Option<&Path>) -> Result<RootInfo, RootError> {
    let launch = launch
        .canonicalize()
        .map_err(|error| RootError::Unresolvable {
            path: launch.to_path_buf(),
            detail: error.to_string(),
        })?;

    if launch == Path::new("/") {
        return Err(RootError::UnusableRoot {
            launch: launch.clone(),
            root: launch,
            reason: "the filesystem root is not an acceptable project root".to_owned(),
        });
    }

    let Some(git) = git else {
        tracing::warn!(
            target: "t8ful::root",
            "git was not found on PATH; using the launch directory as the project root"
        );

        return Ok(RootInfo {
            path: launch,
            source: RootSource::LaunchDir,
        });
    };

    let output = std::process::Command::new(git)
        .arg("-C")
        .arg(&launch)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| RootError::GitUnavailable {
            launch: launch.clone(),
            detail: error.to_string(),
        })?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();

        if trimmed.is_empty() {
            return Err(RootError::EmptyRoot {
                launch: launch.clone(),
            });
        }

        let reported = PathBuf::from(trimmed);
        let root = reported
            .canonicalize()
            .map_err(|error| RootError::Unresolvable {
                path: reported.clone(),
                detail: error.to_string(),
            })?;

        validate_git_root(&launch, &root)?;

        return Ok(RootInfo {
            path: root,
            source: RootSource::Git,
        });
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();

    if detail.contains("not a git repository") {
        return Ok(RootInfo {
            path: launch,
            source: RootSource::LaunchDir,
        });
    }

    Err(RootError::GitFailed {
        launch,
        detail: if detail.is_empty() {
            format!("git exited with status {}", output.status)
        } else {
            detail.to_owned()
        },
    })
}

/// Reject a Git-reported root that does not describe the launch repository.
fn validate_git_root(launch: &Path, root: &Path) -> Result<(), RootError> {
    let reason = if root == Path::new("/") {
        Some("the filesystem root is not an acceptable project root")
    } else if !launch.starts_with(root) {
        Some("the launch directory is not inside the reported root")
    } else if std::fs::symlink_metadata(root.join(".git")).is_err() {
        Some("the reported root has no `.git` entry")
    } else {
        None
    };

    match reason {
        Some(reason) => Err(RootError::UnusableRoot {
            launch: launch.to_path_buf(),
            root: root.to_path_buf(),
            reason: reason.to_owned(),
        }),
        None => Ok(()),
    }
}

/// The current branch of the repository containing `launch`, when it can be
/// determined.
///
/// This is display metadata, so every failure is reported as `None`: a missing
/// git, a directory outside a repository, and a detached `HEAD` all have no
/// branch name to show.
pub fn branch(launch: &Path) -> Option<String> {
    let git = find_in_path("git")?;

    branch_with(launch, &git)
}

/// Branch detection with an explicit git program.
pub fn branch_with(launch: &Path, git: &Path) -> Option<String> {
    let output = std::process::Command::new(git)
        .arg("-C")
        .arg(launch)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();

    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

/// Look up an executable in `PATH`.
pub fn find_in_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;

    find_in_paths(program, std::env::split_paths(&path))
}

fn find_in_paths(program: &str, paths: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    for directory in paths {
        let candidate = directory.join(program);

        if is_executable(&candidate) {
            return Some(candidate);
        }
    }

    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use super::{RootSource, branch_with, discover_with, find_in_path};

    /// A syntactically valid author address assembled in parts so no tool can
    /// confuse it for a real credential or contact.
    const TEST_ADDRESS: &str = concat!("t8ful", "@", "t8ful.invalid");

    fn git(dir: &Path, args: &[&str]) {
        // Isolate the fixture from the developer's git configuration: no signing
        // keys, hooks, or host templates may influence these repositories.
        let status = Command::new("git")
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t8ful")
            .env("GIT_AUTHOR_EMAIL", TEST_ADDRESS)
            .env("GIT_COMMITTER_NAME", "t8ful")
            .env("GIT_COMMITTER_EMAIL", TEST_ADDRESS)
            .status()
            .expect("run git");

        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "--quiet"]);
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        git(dir, &["add", "README.md"]);
        git(dir, &["commit", "--quiet", "-m", "initial"]);
    }

    #[test]
    fn non_git_directory_falls_back_to_the_launch_directory() {
        let dir = TempDir::new().unwrap();
        let root = discover_with(dir.path(), find_in_path("git").as_deref()).expect("root");

        assert_eq!(root.path, dir.path());
        assert_eq!(root.source, RootSource::LaunchDir);
    }

    #[test]
    fn nested_launch_directory_uses_the_repository_root() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);

        let nested = repo.join("crates/inner");
        std::fs::create_dir_all(&nested).unwrap();

        let root = discover_with(&nested, find_in_path("git").as_deref()).expect("root");

        assert_eq!(
            root.path.canonicalize().unwrap(),
            repo.canonicalize().unwrap()
        );
        assert_eq!(root.source, RootSource::Git);
    }

    #[test]
    fn linked_worktree_resolves_to_its_own_root() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);

        let worktree = dir.path().join("worktree");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                worktree.to_str().unwrap(),
                "-b",
                "feature",
            ],
        );

        let nested = worktree.join("sub");
        std::fs::create_dir_all(&nested).unwrap();

        let root = discover_with(&nested, find_in_path("git").as_deref()).expect("root");

        assert_eq!(
            root.path.canonicalize().unwrap(),
            worktree.canonicalize().unwrap()
        );
        assert_eq!(root.source, RootSource::Git);
    }

    #[test]
    fn nested_repositories_resolve_to_the_inner_repository() {
        let dir = TempDir::new().unwrap();
        let outer = dir.path().join("outer");
        std::fs::create_dir(&outer).unwrap();
        init_repo(&outer);

        let inner = outer.join("inner");
        std::fs::create_dir(&inner).unwrap();
        init_repo(&inner);

        let root = discover_with(&inner, find_in_path("git").as_deref()).expect("root");

        assert_eq!(
            root.path.canonicalize().unwrap(),
            inner.canonicalize().unwrap()
        );
    }

    #[test]
    fn a_core_worktree_redirect_outside_the_repository_is_rejected() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);

        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        git(
            &repo,
            &["config", "core.worktree", outside.to_str().unwrap()],
        );

        let error = discover_with(&repo, find_in_path("git").as_deref())
            .expect_err("a redirected root must be refused");

        assert!(error.to_string().contains("core.worktree"), "{error}");
        assert!(
            error.to_string().contains("not inside the reported root"),
            "{error}"
        );
    }

    #[test]
    fn a_core_worktree_redirect_to_the_filesystem_root_is_rejected() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);

        git(&repo, &["config", "core.worktree", "/"]);

        let error = discover_with(&repo, find_in_path("git").as_deref())
            .expect_err("the filesystem root must be refused");

        assert!(error.to_string().contains("core.worktree"), "{error}");
        assert!(error.to_string().contains("filesystem root"), "{error}");
    }

    #[test]
    fn the_current_branch_is_reported_for_a_repository() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);

        let git_path = find_in_path("git").expect("git");
        let branch = branch_with(&repo, &git_path).expect("branch");

        assert!(!branch.is_empty());
        assert_ne!(branch, "HEAD");

        git(&repo, &["checkout", "--quiet", "-b", "feature/pi"]);

        assert_eq!(branch_with(&repo, &git_path).as_deref(), Some("feature/pi"));
    }

    #[test]
    fn branch_detection_outside_a_repository_is_none() {
        let dir = TempDir::new().unwrap();
        let git = find_in_path("git").expect("git");

        assert_eq!(branch_with(dir.path(), &git), None);
    }

    #[test]
    fn a_detached_head_has_no_branch() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        git(&repo, &["checkout", "--quiet", "--detach", "HEAD"]);

        let git = find_in_path("git").expect("git");

        assert_eq!(branch_with(&repo, &git), None);
    }

    #[test]
    fn missing_git_falls_back_with_a_warning() {
        let dir = TempDir::new().unwrap();
        let root = discover_with(dir.path(), None).expect("root");

        assert_eq!(root.path, dir.path());
        assert_eq!(root.source, RootSource::LaunchDir);
    }

    #[test]
    fn a_broken_git_is_reported_instead_of_being_treated_as_no_repository() {
        let dir = TempDir::new().unwrap();
        let fake_git = dir.path().join("git");

        std::fs::write(
            &fake_git,
            "#!/bin/sh\necho 'fatal: detected dubious ownership' >&2\nexit 128\n",
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(&fake_git, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let error = discover_with(dir.path(), Some(&fake_git)).expect_err("hard failure");

        assert!(error.to_string().contains("dubious ownership"), "{error}");
    }
}
