use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ez",
    about = "Stacked PRs for GitHub — manage dependent branches with ease",
    version,
    after_help = "Run `ez <command> --help` for more information on a specific command."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize ez in the current git repository
    Init {
        /// Trunk branch name (auto-detected if not provided)
        #[arg(long)]
        trunk: Option<String>,
    },

    /// Create a new stacked branch
    Create {
        /// Name for the new branch
        name: String,

        /// Commit staged changes with this message
        #[arg(short, long)]
        message: Option<String>,

        /// Stage all tracked changes before committing (requires -m)
        #[arg(short = 'a', long, requires = "message")]
        all: bool,

        /// Create the branch from this base instead of the current branch (cannot combine with -m)
        #[arg(long, alias = "on", conflicts_with = "message")]
        from: Option<String>,
    },

    /// Commit staged changes and auto-restack children
    Commit {
        /// Commit message (repeat -m for multi-paragraph, like git)
        #[arg(short, long, required = true)]
        message: Vec<String>,

        /// Stage all changes before committing
        #[arg(short, long)]
        all: bool,

        /// No-op (exit 0) if there is nothing to commit
        #[arg(long)]
        if_changed: bool,

        /// Stage only these paths before committing
        #[arg(last = true)]
        paths: Vec<String>,
    },

    /// Amend the current commit and auto-restack children
    Amend {
        /// New commit message (keeps existing if not provided)
        #[arg(short, long)]
        message: Option<String>,

        /// Stage all changes before amending
        #[arg(short, long)]
        all: bool,
    },

    /// Push the current branch and create/update its PR
    Push {
        /// Create a draft PR
        #[arg(long)]
        draft: bool,

        /// PR title (defaults to first commit message)
        #[arg(long)]
        title: Option<String>,

        /// PR body text
        #[arg(long)]
        body: Option<String>,

        /// PR body from file
        #[arg(long)]
        body_file: Option<String>,

        /// Override the PR base branch
        #[arg(long)]
        base: Option<String>,

        /// Push all branches in the stack (equivalent to ez submit)
        #[arg(long)]
        stack: bool,
    },

    /// Push and create/update PRs for the entire stack
    Submit {
        /// Create draft PRs
        #[arg(long)]
        draft: bool,

        /// PR title (defaults to first commit message)
        #[arg(long)]
        title: Option<String>,

        /// PR body text
        #[arg(long)]
        body: Option<String>,

        /// PR body from file
        #[arg(long)]
        body_file: Option<String>,
    },

    /// Fetch trunk, detect merged PRs, clean up, and restack
    Sync {
        /// Show what sync would do without making changes
        #[arg(long)]
        dry_run: bool,

        /// Stash uncommitted changes before sync and restore after
        #[arg(long)]
        autostash: bool,

        /// Force-remove worktrees and branches even if they have uncommitted changes
        #[arg(long)]
        force: bool,
    },

    /// Rebase children onto the current branch tip
    Restack,

    /// Move up one branch in the stack
    Up,

    /// Move down one branch in the stack (toward trunk)
    Down,

    /// Move to the top of the stack
    Top,

    /// Move to the bottom of the stack (first branch above trunk)
    Bottom,

    /// Switch to a branch by name or PR number (interactive if no argument)
    Checkout {
        /// Branch name or PR number to check out directly
        name: Option<String>,
    },

    /// Show the visual stack tree with PR status
    Log {
        /// Output stack as JSON to stdout
        #[arg(long)]
        json: bool,
    },

    /// Show current branch info and stack position
    Status {
        /// Output status as JSON to stdout
        #[arg(long)]
        json: bool,
    },

    /// Show diff of current branch vs its parent (what the PR reviewer sees)
    Diff {
        /// Show only the diffstat summary
        #[arg(long)]
        stat: bool,

        /// Show only changed file names
        #[arg(long)]
        name_only: bool,
    },

    /// Print the parent branch name to stdout
    Parent,

    /// Delete a branch and reparent its children
    Delete {
        /// Branch to delete (defaults to current branch)
        branch: Option<String>,

        /// Force delete even if not merged
        #[arg(short, long)]
        force: bool,
    },

    /// Move (reparent) the current branch onto another branch
    Move {
        /// New parent branch
        #[arg(long)]
        onto: String,
    },

    /// Merge the bottom PR of the current stack via GitHub
    Merge {
        /// Merge method: merge, squash, or rebase
        #[arg(long, default_value = "squash")]
        method: String,
    },

    /// Edit the PR for the current branch
    PrEdit {
        /// New PR title
        #[arg(long)]
        title: Option<String>,

        /// New PR body text
        #[arg(long)]
        body: Option<String>,

        /// New PR body from file
        #[arg(long)]
        body_file: Option<String>,
    },

    /// Mark the current branch's PR as a draft
    Draft,

    /// Mark the current branch's PR as ready for review
    Ready,

    /// Print the PR URL for the current branch to stdout
    PrLink,

    /// Open the current branch's PR in the browser
    Pr,

    /// Manage git worktrees
    Worktree(WorktreeArgs),
}

#[derive(Args)]
pub struct WorktreeArgs {
    #[command(subcommand)]
    pub command: WorktreeCommands,
}

#[derive(Subcommand)]
pub enum WorktreeCommands {
    /// Create a stacked branch and check it out in a new worktree at .worktrees/<name>
    Create {
        /// Name for the branch and worktree directory
        name: String,

        /// Base branch to stack on (defaults to current branch)
        #[arg(long, alias = "on")]
        from: Option<String>,
    },

    /// Remove a worktree and its branch from the stack
    Delete {
        /// Worktree name (directory under .worktrees/)
        name: String,

        /// Force-remove even if the worktree has uncommitted changes
        #[arg(short, long)]
        force: bool,
    },

    /// List all worktrees with their name, branch, and path
    List,
}
