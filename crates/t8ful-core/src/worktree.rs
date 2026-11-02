//! Git worktree creation for isolated child agents.
//!
//! Core defines the interface only. The real implementation lives in the
//! composition root (`t8ful`) because it shells out to Git, and core deliberately
//! spawns no processes.

use std::path::PathBuf;

use async_trait::async_trait;

use crate::subagent::{AgentId, Worktree};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorktreeError {
    /// The project is not inside a Git repository with a usable `HEAD`.
    #[error("{0}")]
    NotUsable(String),

    /// The parent tree has local changes, so a worktree from `HEAD` would not
    /// contain them.
    ///
    /// This is an error rather than a question: coding mode is autonomous and
    /// has no approval surface to ask through.
    #[error(
        "the working tree has local changes that a worktree started from HEAD would not contain: \
         {paths}. Commit, stash, or discard them first, or run this agent with isolation \"shared\""
    )]
    Dirty { paths: String },

    #[error("cannot create a worktree: {0}")]
    Failed(String),
}

/// Creates and removes the Git worktrees isolated children run in.
#[async_trait]
pub trait Worktrees: Send + Sync {
    /// Create one worktree for `agent`, based on the repository's `HEAD`.
    async fn create(&self, agent: &AgentId) -> Result<Worktree, WorktreeError>;

    /// Remove a worktree t8ful created.
    ///
    /// Only called when the user discards work, which t8ful never does on its
    /// own; it exists so a future command can, not because the agent loop needs
    /// it.
    async fn remove(&self, worktree: &Worktree) -> Result<(), WorktreeError>;
}

/// Worktree source used when isolation is disabled or unavailable.
///
/// Every request fails with an actionable error rather than silently falling
/// back to a shared tree: an isolated child that quietly ran in the parent's
/// working directory would be the exact corruption isolation exists to prevent.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoWorktrees;

#[async_trait]
impl Worktrees for NoWorktrees {
    async fn create(&self, _agent: &AgentId) -> Result<Worktree, WorktreeError> {
        Err(WorktreeError::NotUsable(
            "worktrees are unavailable in this session; run the agent with isolation \
             \"shared\" or start t8ful from a Git repository"
                .to_owned(),
        ))
    }

    async fn remove(&self, _worktree: &Worktree) -> Result<(), WorktreeError> {
        Ok(())
    }
}

/// Where a session keeps the worktrees it created.
///
/// Deliberately under the state directory, not the project: a worktree inside the
/// project would be visible to its own build and to a project-wide walk.
pub fn session_root(state_root: &std::path::Path, session: &str) -> PathBuf {
    state_root.join("worktrees").join(session)
}

#[cfg(test)]
mod tests {
    use super::{NoWorktrees, WorktreeError, Worktrees, session_root};
    use crate::subagent::AgentId;

    #[tokio::test]
    async fn a_missing_worktree_source_fails_instead_of_sharing_the_tree() {
        let error = NoWorktrees
            .create(&AgentId::new("a1"))
            .await
            .expect_err("refused");

        match error {
            WorktreeError::NotUsable(message) => {
                assert!(message.contains("isolation \"shared\""), "{message}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn a_dirty_tree_error_names_the_paths_and_the_escape_hatch() {
        let error = WorktreeError::Dirty {
            paths: "crates/t8ful/src/agent.rs".to_owned(),
        };

        let text = error.to_string();

        assert!(text.contains("crates/t8ful/src/agent.rs"));
        assert!(text.contains("isolation \"shared\""));
        assert!(text.contains("Commit, stash, or discard"));
    }

    #[test]
    fn worktrees_live_under_the_state_directory_not_the_project() {
        let root = session_root(std::path::Path::new("/state/t8ful"), "s1");

        assert_eq!(root, std::path::Path::new("/state/t8ful/worktrees/s1"));
    }
}
