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
        "rebase conflict on branch `{0}` — resolve conflicts, then run `ez restack` to continue"
    )]
    RebaseConflict(String),

    #[error(
        "no staged changes to commit\n  → Stage files with `git add <files>`, or use `ez commit -am \"msg\"` to stage all"
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
