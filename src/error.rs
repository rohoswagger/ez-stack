use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum EzError {
    #[error(
        "not a git repository (or any parent up to mount point)\n  → Run `git init` to create one, or `cd` into an existing repo"
    )]
    NotARepo,

    #[error("ez is not initialized in this repo — run `ez init` first")]
    NotInitialized,

    #[error("ez is already initialized in this repo — run `ez log` to see the current stack")]
    AlreadyInitialized,

    #[error("currently on trunk branch — create a stacked branch first with `ez create <name>`")]
    OnTrunk,

    #[error("branch `{0}` not found in stack metadata\n  → Run `ez log` to see tracked branches")]
    BranchNotInStack(String),

    #[error("branch `{0}` already exists — use `ez checkout {0}` to switch to it")]
    BranchAlreadyExists(String),

    #[error("no children to restack — nothing to do")]
    NoChildren,

    #[error("already at the top of the stack — use `ez log` to see the full stack")]
    AlreadyAtTop,

    #[error("already at the bottom of the stack — use `ez log` to see the full stack")]
    AlreadyAtBottom,

    #[error(
        "rebase conflict on branch `{0}` — see the conflict details above, then run `ez restack` after applying the resolution"
    )]
    RebaseConflict(String),

    #[error(
        "no changes selected for commit\n  → Preferred: `ez commit -m \"msg\" -- <paths>` for focused files\n  → Bulk tracked update: `ez commit -am \"msg\"`\n  → Bulk tracked + untracked update: `ez commit -Am \"msg\"`\n  → Partial hunks: `git add -p` then `ez commit -m \"msg\"`"
    )]
    NothingToCommit,

    #[error("unstaged or uncommitted changes — stash them first, or use `ez sync --autostash`")]
    UnstagedChanges,

    #[error("git command failed: {0}")]
    GitError(String),

    #[error(
        "push rejected: remote ref for `{0}` is stale\n  → Run: git fetch origin {0}\n  → Then retry: ez push"
    )]
    StaleRemoteRef(String),

    #[error("gh CLI error: {0}\n  → Check authentication: `gh auth status`")]
    GhError(String),

    #[error("{0}")]
    UserMessage(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_include_actionable_hints() {
        assert!(
            EzError::NotInitialized
                .to_string()
                .contains("run `ez init` first")
        );
        assert!(
            EzError::BranchNotInStack("feat/x".into())
                .to_string()
                .contains("stack metadata")
        );
        assert!(
            EzError::NothingToCommit
                .to_string()
                .contains("Preferred: `ez commit -m")
        );
        assert!(
            EzError::StaleRemoteRef("feat/x".into())
                .to_string()
                .contains("git fetch origin feat/x")
        );
        assert!(
            EzError::GhError("boom".into())
                .to_string()
                .contains("gh auth status")
        );
    }
}
