use clap::{Args, Parser, Subcommand};

use crate::stack::ScopeMode;

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
    #[command(after_help = "\
Examples:
  ez init
  ez init --trunk main
  ez init --trunk develop")]
    Init {
        /// Trunk branch name (auto-detected if not provided)
        #[arg(long)]
        trunk: Option<String>,
    },

    /// Create a new stacked branch (worktree by default)
    #[command(after_help = "\
Examples:
  ez create feat/auth
  ez create feat/auth --scope 'src/auth/**'
  ez create feat/auth --hook setup-node
  ez create feat/auth -m \"add auth types\"
  ez create feat/auth -am \"add auth types\"
  ez create feat/auth -Am \"add auth types and new files\"
  ez create feat/auth --from main
  ez create feat/auth --no-worktree")]
    Create {
        /// Name for the new branch
        name: String,

        /// Commit selected changes with this message
        #[arg(short, long)]
        message: Option<String>,

        /// Stage all tracked changes before committing (requires -m)
        #[arg(short = 'a', long, requires = "message")]
        all: bool,

        /// Stage all changes, including untracked files, before committing (requires -m)
        #[arg(
            short = 'A',
            long = "all-files",
            requires = "message",
            conflicts_with = "all"
        )]
        all_files: bool,

        /// Create the branch from this base instead of the current branch (cannot combine with -m)
        #[arg(long, alias = "on", conflicts_with = "message")]
        from: Option<String>,

        /// Create a branch only (no worktree)
        #[arg(long)]
        no_worktree: bool,

        /// Scope pattern for files this branch is intended to touch (repeatable)
        #[arg(long)]
        scope: Vec<String>,

        /// Scope enforcement mode when scope is configured
        #[arg(long, requires = "scope", value_enum)]
        scope_mode: Option<ScopeMode>,

        /// Run a specific post-create hook, or list available hooks (--hook without a name)
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        hook: Option<String>,
    },

    /// Commit selected changes and auto-restack children
    #[command(after_help = "\
Examples:
  ez commit -m \"fix: typo\" -- src/main.rs
  ez commit -m \"feat: add parser\" -- src/parser.rs src/ast.rs
  ez commit -am \"feat: add parser\"
  ez commit -Am \"feat: add parser and new fixture\"
  git add -p
  ez commit -m \"fix: keep intended hunks only\"
  ez commit -m \"feat: add parser\" -m \"Implements recursive descent.\"
  ez commit -m \"chore: format\" --if-changed")]
    Commit {
        /// Commit message (repeat -m for multi-paragraph, like git)
        #[arg(short, long, required = true)]
        message: Vec<String>,

        /// Stage all tracked changes before committing
        #[arg(short, long)]
        all: bool,

        /// Stage all changes, including untracked files, before committing
        #[arg(short = 'A', long = "all-files", conflicts_with = "all")]
        all_files: bool,

        /// No-op (exit 0) if there is nothing to commit
        #[arg(long)]
        if_changed: bool,

        /// Stage only these paths before committing
        #[arg(last = true)]
        paths: Vec<String>,
    },

    /// Amend the current commit and auto-restack children
    #[command(after_help = "\
Examples:
  ez amend
  ez amend -m \"better message\"
  ez amend -a")]
    Amend {
        /// New commit message (keeps existing if not provided)
        #[arg(short, long)]
        message: Option<String>,

        /// Stage all changes before amending
        #[arg(short, long)]
        all: bool,
    },

    /// Push the current branch and create/update its PR
    #[command(after_help = "\
Examples:
  ez push
  ez push --title \"feat: add auth\" --body \"Adds login/logout.\"
  ez push --draft
  ez push --stack
  ez push -am \"feat: add auth\"
  ez push -Am \"feat: add auth and new snapshots\"")]
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

        /// Stage all tracked changes before committing (requires -m)
        #[arg(short = 'a', long = "all", requires = "message")]
        stage_all: bool,

        /// Stage all changes, including untracked files, before committing (requires -m)
        #[arg(
            short = 'A',
            long = "all-files",
            requires = "message",
            conflicts_with = "stage_all"
        )]
        stage_all_files: bool,

        /// Commit with this message before pushing
        #[arg(short = 'm', long)]
        message: Option<String>,
    },

    /// Push and create/update PRs for the entire stack
    #[command(after_help = "\
Examples:
  ez submit
  ez submit --draft")]
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
    #[command(after_help = "\
Examples:
  ez sync
  ez sync --autostash
  ez sync --dry-run
  ez sync --force")]
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

    /// Fetch trunk, refresh it locally, and rebase stale branches onto their latest parent tips
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
    #[command(
        alias = "checkout",
        after_help = "\
Examples:
  ez switch feat/auth
  ez switch 42"
    )]
    Switch {
        /// Branch name or PR number to switch to directly
        name: Option<String>,
    },

    /// Show the visual stack tree with PR status
    #[command(after_help = "\
Examples:
  ez log
  ez log --json")]
    Log {
        /// Output stack as JSON to stdout
        #[arg(long)]
        json: bool,
    },

    /// Show current branch info and stack position
    #[command(after_help = "\
Examples:
  ez status
  ez status --json")]
    Status {
        /// Output status as JSON to stdout
        #[arg(long)]
        json: bool,
    },

    /// List all local branches, including untracked ones, with PRs, worktree paths, and working tree state
    #[command(
        alias = "branch",
        after_help = "\
Examples:
  ez list
  ez list --json
  ez branch"
    )]
    List {
        /// Output as JSON to stdout
        #[arg(long)]
        json: bool,
    },

    /// Show diff of current branch vs its parent (what the PR reviewer sees)
    #[command(after_help = "\
Examples:
  ez diff
  ez diff --stat
  ez diff --name-only")]
    Diff {
        /// Show only the diffstat summary
        #[arg(long)]
        stat: bool,

        /// Show only changed file names
        #[arg(long)]
        name_only: bool,
    },

    /// Print the parent branch name to stdout
    #[command(after_help = "\
Examples:
  ez parent
  git diff $(ez parent)...HEAD --stat")]
    Parent,

    /// Delete a branch (and its worktree if present), stop listeners on its dev port, and reparent its children
    #[command(after_help = "\
Examples:
  ez delete
  ez delete feat/old-branch
  ez delete --force
  ez delete --yes")]
    Delete {
        /// Branch to delete (defaults to current branch)
        branch: Option<String>,

        /// Force delete even if not merged
        #[arg(short, long)]
        force: bool,

        /// Skip confirmation when deleting a worktree you are inside
        #[arg(short, long)]
        yes: bool,
    },

    /// Move (reparent) the current branch onto another branch
    #[command(after_help = "\
Examples:
  ez move --onto main
  ez move --onto feat/base")]
    Move {
        /// New parent branch
        #[arg(long)]
        onto: String,
    },

    /// Merge the bottom PR of the current stack via GitHub
    #[command(after_help = "\
Examples:
  ez merge
  ez merge --yes
  ez merge --stack --yes
  ez merge --method squash
  ez merge --method rebase")]
    Merge {
        /// Merge method: merge, squash, or rebase
        #[arg(long, default_value = "squash")]
        method: String,

        /// Skip confirmation prompt (for agents and scripts)
        #[arg(short, long)]
        yes: bool,

        /// Merge the current linear stack bottom-to-top
        #[arg(long)]
        stack: bool,
    },

    /// Edit the PR for the current branch
    #[command(after_help = "\
Examples:
  ez pr-edit
  ez pr-edit --title \"new title\" --body \"updated body\"")]
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
    #[command(after_help = "\
Examples:
  ez pr-link
  open $(ez pr-link)")]
    PrLink,

    /// Open the current branch's PR in the browser
    Pr,

    /// Update ez to the latest version
    #[command(after_help = "\
Examples:
  ez update
  ez update --check
  ez update --version v0.1.12")]
    Update {
        /// Install a specific version (e.g., v0.1.11)
        #[arg(long)]
        version: Option<String>,

        /// Check for updates without installing
        #[arg(long)]
        check: bool,
    },

    /// Configure shell integration (PATH + auto-cd for worktrees)
    #[command(after_help = "\
Examples:
  ez setup
  ez setup --yes")]
    Setup {
        /// Skip confirmation (for agents and scripts)
        #[arg(short, long)]
        yes: bool,
    },

    /// Manage the current branch's scope configuration
    Scope(ScopeArgs),

    /// Install or manage the ez-workflow skill for AI agents
    Skill(SkillArgs),

    /// Print shell integration code (used by `ez setup` internally)
    #[command(after_help = "\
Examples:
  eval \"$(ez shell-init)\"")]
    ShellInit,

    /// View and update ez settings for the current repo
    Config(ConfigArgs),

    /// Manage git worktrees
    Worktree(WorktreeArgs),
}

#[derive(Args)]
pub struct ScopeArgs {
    #[command(subcommand)]
    pub command: ScopeCommands,
}

#[derive(Subcommand)]
pub enum ScopeCommands {
    /// Show the current branch's configured scope
    #[command(after_help = "\
Examples:
  ez scope show")]
    Show,

    /// Add one or more patterns to the current branch's scope
    #[command(after_help = "\
Examples:
  ez scope add 'src/auth/**'
  ez scope add --mode strict 'tests/auth/**'")]
    Add {
        /// Update scope enforcement mode while adding patterns
        #[arg(long, value_enum)]
        mode: Option<ScopeMode>,

        /// Scope patterns to add
        #[arg(required = true)]
        patterns: Vec<String>,
    },

    /// Replace the current branch's scope with new patterns
    #[command(after_help = "\
Examples:
  ez scope set 'src/auth/**' 'tests/auth/**'
  ez scope set --mode strict 'src/auth/**'")]
    Set {
        /// Set scope enforcement mode while replacing patterns
        #[arg(long, value_enum)]
        mode: Option<ScopeMode>,

        /// Scope patterns to set
        #[arg(required = true)]
        patterns: Vec<String>,
    },

    /// Clear the current branch's scope configuration
    #[command(after_help = "\
Examples:
  ez scope clear")]
    Clear,
}

#[derive(Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommands,
}

#[derive(Subcommand)]
pub enum ConfigCommands {
    /// List all config settings
    #[command(after_help = "\
Examples:
  ez config list")]
    List,

    /// Get the value of a config key
    #[command(after_help = "\
Examples:
  ez config get trunk
  ez config get remote")]
    Get {
        /// Config key to read
        key: String,
    },

    /// Set a config key to a new value
    #[command(after_help = "\
Examples:
  ez config set trunk develop
  ez config set remote fork
  ez config set default_from dev
  ez config set repo owner/name")]
    Set {
        /// Config key to update
        key: String,

        /// New value
        value: String,
    },
}

#[derive(Args)]
pub struct WorktreeArgs {
    #[command(subcommand)]
    pub command: WorktreeCommands,
}

#[derive(Subcommand)]
pub enum WorktreeCommands {
    /// Create a stacked branch and check it out in a new worktree at .worktrees/<name>
    #[command(after_help = "\
Examples:
  cd $(ez worktree create feat/auth)
  cd $(ez worktree create feat/auth --from main)")]
    Create {
        /// Name for the branch and worktree directory
        name: String,

        /// Base branch to stack on (defaults to current branch)
        #[arg(long, alias = "on")]
        from: Option<String>,
    },

    /// Remove a worktree and its branch from the stack
    #[command(after_help = "\
Examples:
  ez worktree delete feat/auth
  ez worktree delete feat/auth --force
  cd $(ez worktree delete feat/auth --yes)")]
    Delete {
        /// Worktree name (directory under .worktrees/)
        name: String,

        /// Force-remove even if the worktree has uncommitted changes
        #[arg(short, long)]
        force: bool,

        /// Skip confirmation when deleting the worktree you are currently in
        #[arg(short, long)]
        yes: bool,
    },

    /// List all worktrees with their name, branch, and path
    List,
}

#[derive(Args)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub command: SkillCommands,
}

#[derive(Subcommand)]
pub enum SkillCommands {
    /// Install the ez-workflow skill into this repo's .agents/skills/ with agent-specific symlinks
    #[command(after_help = "\
Examples:
  ez skill install")]
    Install,

    /// Remove the ez-workflow skill from this repo
    #[command(after_help = "\
Examples:
  ez skill uninstall")]
    Uninstall,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_commit_with_paths_after_double_dash() {
        let cli = Cli::try_parse_from([
            "ez",
            "commit",
            "-m",
            "fix: parser",
            "--",
            "src/main.rs",
            "src/lib.rs",
        ])
        .expect("parse commit");

        match cli.command {
            Commands::Commit { message, paths, .. } => {
                assert_eq!(message, vec!["fix: parser".to_string()]);
                assert_eq!(
                    paths,
                    vec!["src/main.rs".to_string(), "src/lib.rs".to_string()]
                );
            }
            _ => panic!("expected commit command"),
        }
    }

    #[test]
    fn parses_create_scope_mode_and_from_alias() {
        let cli = Cli::try_parse_from([
            "ez",
            "create",
            "feat/auth",
            "--on",
            "main",
            "--scope",
            "src/auth/**",
            "--scope-mode",
            "strict",
        ])
        .expect("parse create");

        match cli.command {
            Commands::Create {
                from,
                scope,
                scope_mode,
                ..
            } => {
                assert_eq!(from.as_deref(), Some("main"));
                assert_eq!(scope, vec!["src/auth/**".to_string()]);
                assert_eq!(scope_mode, Some(ScopeMode::Strict));
            }
            _ => panic!("expected create command"),
        }
    }

    #[test]
    fn parses_create_all_files_combined_short_flags() {
        let cli = Cli::try_parse_from(["ez", "create", "feat/auth", "-Am", "feat: add files"])
            .expect("parse create -Am");

        match cli.command {
            Commands::Create {
                message,
                all,
                all_files,
                ..
            } => {
                assert_eq!(message.as_deref(), Some("feat: add files"));
                assert!(!all);
                assert!(all_files);
            }
            _ => panic!("expected create command"),
        }
    }

    #[test]
    fn parses_commit_all_files_combined_short_flags() {
        let cli = Cli::try_parse_from(["ez", "commit", "-Am", "feat: add new files"])
            .expect("parse commit -Am");

        match cli.command {
            Commands::Commit {
                message,
                all,
                all_files,
                ..
            } => {
                assert_eq!(message, vec!["feat: add new files".to_string()]);
                assert!(!all);
                assert!(all_files);
            }
            _ => panic!("expected commit command"),
        }
    }

    #[test]
    fn parses_branch_alias_to_list() {
        let cli = Cli::try_parse_from(["ez", "branch"]).expect("parse branch alias");
        match cli.command {
            Commands::List { json } => assert!(!json),
            _ => panic!("expected list command"),
        }
    }

    #[test]
    fn parses_push_all_files_combined_short_flags() {
        let cli = Cli::try_parse_from(["ez", "push", "-Am", "feat: ship new files"])
            .expect("parse push -Am");

        match cli.command {
            Commands::Push {
                message,
                stage_all,
                stage_all_files,
                ..
            } => {
                assert_eq!(message.as_deref(), Some("feat: ship new files"));
                assert!(!stage_all);
                assert!(stage_all_files);
            }
            _ => panic!("expected push command"),
        }
    }

    #[test]
    fn parses_worktree_delete_yes_flag() {
        let cli =
            Cli::try_parse_from(["ez", "worktree", "delete", "feat/auth", "--yes", "--force"])
                .expect("parse worktree delete");

        match cli.command {
            Commands::Worktree(WorktreeArgs {
                command: WorktreeCommands::Delete { name, force, yes },
            }) => {
                assert_eq!(name, "feat/auth");
                assert!(force);
                assert!(yes);
            }
            _ => panic!("expected worktree delete command"),
        }
    }

    #[test]
    fn parses_merge_yes_and_stack_flags() {
        let cli = Cli::try_parse_from(["ez", "merge", "--yes", "--stack", "--method", "rebase"])
            .expect("parse merge");

        match cli.command {
            Commands::Merge { method, yes, stack } => {
                assert_eq!(method, "rebase");
                assert!(yes);
                assert!(stack);
            }
            _ => panic!("expected merge command"),
        }
    }
}
