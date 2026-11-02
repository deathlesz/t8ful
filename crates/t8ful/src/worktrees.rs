//! Git worktree creation for isolated child agents.
//!
//! Every operation shells out to `git` rather than reimplementing the index, so
//! the result is exactly what a person running `git worktree add` would get. The
//! parent's working tree is never touched: no stash, no reset, no commit.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use t8ful_core::subagent::{AgentId, Worktree};
use t8ful_core::worktree::{WorktreeError, Worktrees};

/// Paths in the parent tree that a worktree from `HEAD` would not contain.
#[derive(Debug, Default, PartialEq, Eq)]
struct TreeStatus {
    /// Staged, unstaged, or untracked changes: these block the spawn.
    blocked: Vec<String>,
    /// Ignored files: reported, but never a reason to refuse.
    ignored: Vec<String>,
}

/// Split `git status --porcelain --ignored` output into its two kinds.
fn parse_status(listed: &str) -> TreeStatus {
    let mut status = TreeStatus::default();

    for line in listed.lines().filter(|line| !line.trim().is_empty()) {
        let path = line.chars().skip(3).collect::<String>();

        if path.trim().is_empty() {
            continue;
        }

        if line.starts_with("!!") {
            status.ignored.push(path);
        } else {
            status.blocked.push(path);
        }
    }

    status
}

fn summarize(paths: &[String]) -> String {
    let shown = paths
        .iter()
        .take(10)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let more = paths.len().saturating_sub(10);

    match more {
        0 => shown,
        extra => format!("{shown} and {extra} more"),
    }
}

/// Creates worktrees under a state directory.
#[derive(Debug, Clone)]
pub struct GitWorktrees {
    /// Repository the parent works in, resolved once at startup.
    repo: PathBuf,
    /// Directory this session's worktrees live under.
    root: PathBuf,
    /// Session id, used in branch names so two sessions never collide.
    session: String,
}

impl GitWorktrees {
    /// Resolve the repository containing `project` and prepare the session root.
    ///
    /// Fails with `NotUsable` when `project` is not in a repository, so an
    /// isolated spawn refuses instead of silently sharing the tree.
    pub fn discover(
        project: &Path,
        state_root: &Path,
        session: &str,
    ) -> Result<Self, WorktreeError> {
        let repo = git_output(project, &["rev-parse", "--show-toplevel"])
            .map(PathBuf::from)
            .map_err(|error| {
                WorktreeError::NotUsable(format!(
                    "{} is not inside a Git repository ({error}); an isolated agent needs one, or \
                     run it with isolation \"shared\"",
                    project.display()
                ))
            })?;

        // `HEAD` must resolve to a commit. An unborn branch has no commit to
        // base a worktree on, and inventing one would change the repository.
        git_output(&repo, &["rev-parse", "--verify", "HEAD"]).map_err(|error| {
            WorktreeError::NotUsable(format!(
                "{} has no commit to start a worktree from ({error}); make an initial commit, or \
                 run this agent with isolation \"shared\"",
                repo.display()
            ))
        })?;

        Ok(Self {
            root: t8ful_core::worktree::session_root(state_root, session),
            repo,
            session: session.to_owned(),
        })
    }

    /// The repository the parent works in.
    #[cfg(test)]
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// What the parent's working tree holds relative to `HEAD`.
    ///
    /// A worktree starts from `HEAD`, so a blocked change would leave the
    /// parent's work behind. Any ignored change is missing from the worktree
    /// too, but a stale build directory is the ordinary state of a checkout, so
    /// it is reported instead of refusing every spawn.
    async fn tree_status(&self) -> Result<TreeStatus, WorktreeError> {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["status", "--porcelain", "--ignored"])
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|error| WorktreeError::Failed(format!("cannot run git status: {error}")))?;

        if !output.status.success() {
            return Err(WorktreeError::Failed(format!(
                "git status failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        Ok(parse_status(&String::from_utf8_lossy(&output.stdout)))
    }

    /// The branch name for one child.
    fn branch_for(&self, agent: &AgentId) -> String {
        format!("t8ful/{}/{}", self.session, agent.as_str())
    }
}

#[async_trait]
impl Worktrees for GitWorktrees {
    async fn create(&self, agent: &AgentId) -> Result<Worktree, WorktreeError> {
        let status = self.tree_status().await?;

        if !status.blocked.is_empty() {
            let ignored = match status.ignored.is_empty() {
                true => String::new(),
                false => format!(
                    "; ignored paths are not copied either: {}",
                    summarize(&status.ignored)
                ),
            };

            return Err(WorktreeError::Dirty {
                paths: format!("{}{ignored}", summarize(&status.blocked)),
            });
        }

        let base_commit = git_output(&self.repo, &["rev-parse", "HEAD"])
            .map_err(|error| WorktreeError::Failed(format!("cannot resolve HEAD: {error}")))?;
        let branch = self.branch_for(agent);
        let path = self.root.join(agent.as_str());

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                WorktreeError::Failed(format!(
                    "cannot create the worktree directory {}: {error}",
                    parent.display()
                ))
            })?;
        }

        // A stale worktree from an interrupted run must not block a new one, and
        // must not be silently reused: its branch may hold someone else's work.
        if path.exists() {
            return Err(WorktreeError::Failed(format!(
                "{} already exists; a previous worktree was left there. Inspect it with \
                 `git -C {} status` and remove it with `git worktree remove {}` before retrying",
                path.display(),
                path.display(),
                path.display()
            )));
        }

        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "add", "-b"])
            .arg(&branch)
            .arg(&path)
            .arg(&base_commit)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|error| {
                WorktreeError::Failed(format!("cannot run git worktree add: {error}"))
            })?;

        if !output.status.success() {
            return Err(WorktreeError::Failed(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        Ok(Worktree {
            path,
            branch: Some(branch),
            base_commit: Some(base_commit),
            ignored_paths: status.ignored,
        })
    }

    async fn remove(&self, worktree: &Worktree) -> Result<(), WorktreeError> {
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["worktree", "remove", "--force"])
            .arg(&worktree.path)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|error| {
                WorktreeError::Failed(format!("cannot run git worktree remove: {error}"))
            })?;

        if !output.status.success() {
            return Err(WorktreeError::Failed(format!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        Ok(())
    }
}

/// Run one git command and return its trimmed stdout.
///
/// Blocking on purpose: this is used only during setup, and `discover` runs
/// before the executor takes over.
fn git_output(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use t8ful_core::subagent::AgentId;
    use t8ful_core::worktree::{WorktreeError, Worktrees};

    use super::GitWorktrees;

    /// Run one git command with the developer's global configuration ignored.
    ///
    /// A signed-commit policy, a commit template, or a hooks path in the real
    /// `~/.gitconfig` would otherwise decide whether these tests pass.
    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .status()
            .expect("git runs");

        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// A repository with one commit, so `HEAD` resolves.
    ///
    /// `target/` is ignored from the first commit, so a test can put build
    /// output in the parent without turning the tree dirty.
    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");

        git(dir.path(), &["init", "--initial-branch=main"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("README.md"), "hello\n").expect("write");
        std::fs::write(dir.path().join(".gitignore"), "target/\n").expect("write");
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "initial"]);

        dir
    }

    fn build_output(repo: &Path) {
        std::fs::create_dir_all(repo.join("target/debug")).expect("create target");
        std::fs::write(repo.join("target/debug/artifact"), "build\n").expect("write");
    }

    #[tokio::test]
    async fn a_clean_repository_produces_a_worktree_on_its_own_branch() {
        let repo = repository();
        let state = tempfile::tempdir().expect("state");
        let trees = GitWorktrees::discover(repo.path(), state.path(), "s1").expect("discovered");

        let worktree = trees.create(&AgentId::new("a1")).await.expect("created");

        assert!(worktree.path.exists());
        assert_eq!(worktree.branch.as_deref(), Some("t8ful/s1/a1"));
        assert!(worktree.base_commit.is_some());
        assert!(worktree.path.join("README.md").exists());
        assert!(worktree.inspect_command().starts_with("git -C "));

        // The branch really exists in the parent repository.
        let branches = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["branch", "--list", "t8ful/s1/a1"])
            .output()
            .expect("branches");

        assert!(String::from_utf8_lossy(&branches.stdout).contains("t8ful/s1/a1"));
    }

    #[tokio::test]
    async fn a_dirty_tree_refuses_and_names_the_paths() {
        let repo = repository();
        let state = tempfile::tempdir().expect("state");

        std::fs::write(repo.path().join("README.md"), "changed\n").expect("modify");
        std::fs::write(repo.path().join("new.txt"), "untracked\n").expect("write");
        build_output(repo.path());

        let trees = GitWorktrees::discover(repo.path(), state.path(), "s1").expect("discovered");
        let error = trees
            .create(&AgentId::new("a1"))
            .await
            .expect_err("refused");

        match error {
            WorktreeError::Dirty { paths } => {
                assert!(paths.contains("README.md"), "{paths}");
                assert!(paths.contains("new.txt"), "{paths}");
                assert!(
                    paths.contains("ignored paths are not copied either")
                        && paths.contains("target"),
                    "the refusal must report the ignored paths too: {paths}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }

        // Nothing was stashed or committed on the parent's behalf.
        let status = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["status", "--porcelain"])
            .output()
            .expect("status");

        assert!(String::from_utf8_lossy(&status.stdout).contains("README.md"));
    }

    #[tokio::test]
    async fn ignored_paths_are_reported_without_blocking_the_spawn() {
        let repo = repository();
        let state = tempfile::tempdir().expect("state");

        build_output(repo.path());

        let trees = GitWorktrees::discover(repo.path(), state.path(), "s1").expect("discovered");
        let worktree = trees
            .create(&AgentId::new("a1"))
            .await
            .expect("ignored build output must not refuse an isolated spawn");

        assert!(
            worktree
                .ignored_paths
                .iter()
                .any(|path| path.contains("target")),
            "the parent's ignored paths have to be reported: {:?}",
            worktree.ignored_paths
        );
        assert!(worktree.path.exists());
        assert!(
            !worktree.path.join("target").exists(),
            "a worktree starts from HEAD and does not copy ignored files"
        );
    }

    #[tokio::test]
    async fn a_repository_without_a_commit_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");

        git(dir.path(), &["init", "--initial-branch=main"]);

        let state = tempfile::tempdir().expect("state");

        match GitWorktrees::discover(dir.path(), state.path(), "s1") {
            Err(WorktreeError::NotUsable(message)) => {
                assert!(message.contains("no commit"), "{message}");
                assert!(message.contains("isolation \"shared\""), "{message}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_plain_directory_is_refused_with_the_escape_hatch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = tempfile::tempdir().expect("state");

        match GitWorktrees::discover(dir.path(), state.path(), "s1") {
            Err(WorktreeError::NotUsable(message)) => {
                assert!(message.contains("not inside a Git repository"), "{message}");
                assert!(message.contains("isolation \"shared\""), "{message}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_existing_worktree_path_is_not_reused() {
        let repo = repository();
        let state = tempfile::tempdir().expect("state");
        let trees = GitWorktrees::discover(repo.path(), state.path(), "s1").expect("discovered");

        trees.create(&AgentId::new("a1")).await.expect("created");

        let error = trees
            .create(&AgentId::new("a1"))
            .await
            .expect_err("refused");

        assert!(error.to_string().contains("already exists"), "{error}");
        assert!(error.to_string().contains("git worktree remove"), "{error}");
    }

    #[tokio::test]
    async fn removing_a_worktree_leaves_the_parent_alone() {
        let repo = repository();
        let state = tempfile::tempdir().expect("state");
        let trees = GitWorktrees::discover(repo.path(), state.path(), "s1").expect("discovered");

        let worktree = trees.create(&AgentId::new("a1")).await.expect("created");

        trees.remove(&worktree).await.expect("removed");

        assert!(!worktree.path.exists());
        assert!(repo.path().join("README.md").exists());
    }

    #[tokio::test]
    async fn a_worktree_child_can_write_without_touching_the_parent() {
        let repo = repository();
        let state = tempfile::tempdir().expect("state");
        let trees = GitWorktrees::discover(repo.path(), state.path(), "s1").expect("discovered");
        let worktree = trees.create(&AgentId::new("a1")).await.expect("created");

        std::fs::write(worktree.path.join("new.txt"), "child work\n").expect("write");

        assert!(worktree.path.join("new.txt").exists());
        assert!(!repo.path().join("new.txt").exists(), "parent is untouched");
    }

    #[tokio::test]
    async fn each_child_gets_a_distinct_branch_and_path() {
        let repo = repository();
        let state = tempfile::tempdir().expect("state");
        let trees = GitWorktrees::discover(repo.path(), state.path(), "s1").expect("discovered");

        let first = trees.create(&AgentId::new("a1")).await.expect("first");
        let second = trees.create(&AgentId::new("a2")).await.expect("second");

        assert_ne!(first.path, second.path);
        assert_ne!(first.branch, second.branch);
        assert_eq!(second.branch.as_deref(), Some("t8ful/s1/a2"));
    }

    #[tokio::test]
    async fn a_worktree_in_a_subdirectory_resolves_to_the_repository_root() {
        let repo = repository();
        let nested = repo.path().join("crates/deep");
        std::fs::create_dir_all(&nested).expect("nested");

        let state = tempfile::tempdir().expect("state");
        let trees = GitWorktrees::discover(&nested, state.path(), "s1").expect("discovered");

        assert_eq!(
            std::fs::canonicalize(trees.repo()).expect("canonical"),
            std::fs::canonicalize(repo.path()).expect("canonical")
        );
    }
}
