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
    /// Initialize this repo for ez by writing repo-local stack metadata
    #[command(after_help = "\
What it changes:
  Writes repo-local ez metadata under .git/ez/. It does not edit your shell config.

Examples:
  ez init --yes
  ez init --trunk main --yes
  ez init --trunk main --rerere
  ez init                    # prints this help")]
    Init {
        /// Trunk branch name (auto-detected if not provided)
        #[arg(long)]
        trunk: Option<String>,

        /// Accept recommended repo defaults (for agents and scripts). Currently includes
        /// initializing with the detected trunk and enabling git rerere; more may be added later.
        #[arg(short, long)]
        yes: bool,

        /// Enable git rerere when initializing (included in `--yes`; use without `--yes` to opt in
        /// only to rerere alongside other flags)
        #[arg(long)]
        rerere: bool,
    },

    /// Adopt branches from GitHub PRs into the local stack
    #[command(after_help = "\
Examples:
  ez adopt
  ez adopt --pr 42
  ez adopt feat/auth feat/db")]
    Adopt {
        /// Adopt the chain for a specific PR number
        #[arg(long)]
        pr: Option<u64>,

        /// Specific branch names to adopt
        branches: Vec<String>,
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
  ez push --pr
  ez push --no-pr
  ez push --stack
  ez push -am \"feat: add auth\"
  ez push -Am \"feat: add auth and new snapshots\"")]
    Push {
        /// Create a draft PR
        #[arg(long, conflicts_with = "no_pr")]
        draft: bool,

        /// Override draft config to create a ready-for-review PR
        #[arg(long, conflicts_with = "no_pr")]
        no_draft: bool,

        /// Push the branch without creating or updating a PR
        #[arg(long, conflicts_with_all = ["draft", "no_draft", "title", "body", "body_file"])]
        no_pr: bool,

        /// Create/update a PR even when config no_pr is true
        #[arg(long, conflicts_with = "no_pr")]
        pr: bool,

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
  ez submit --draft

Note: --draft only affects newly created PRs. Existing PRs are not changed.
Use `ez ready` to undraft an existing PR.")]
    Submit {
        /// Create draft PRs (only affects new PRs, not existing ones)
        #[arg(long)]
        draft: bool,

        /// Override draft config to create ready-for-review PRs
        #[arg(long)]
        no_draft: bool,

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
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        onto: Option<String>,
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

    /// Configure this machine's shell integration for ez
    #[command(after_help = "\
What it changes:
  Updates your shell rc file so ez can manage PATH and auto-cd for worktrees.
  It does not initialize the current repo. Use `ez init` for repo setup.

Examples:
  ez setup --yes
  ez setup --interactive
  ez setup                   # prints this help")]
    Setup {
        /// Skip confirmation (for agents and scripts)
        #[arg(short, long, conflicts_with = "interactive")]
        yes: bool,

        /// Preview shell config edits and prompt before applying them
        #[arg(short, long)]
        interactive: bool,
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
  ez config set repo owner/name
  ez config set draft true
  ez config set no_pr true
  ez config set rerere true")]
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
    use clap::{CommandFactory, Parser};

    #[test]
    fn top_level_help_distinguishes_repo_init_from_machine_setup() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("Initialize this repo for ez"));
        assert!(help.contains("Configure this machine's shell integration"));
    }

    #[test]
    fn parses_init_yes_flag() {
        let cli = Cli::try_parse_from(["ez", "init", "--yes"]).expect("parse init --yes");

        match cli.command {
            Commands::Init { yes, trunk, rerere } => {
                assert!(yes);
                assert!(trunk.is_none());
                assert!(!rerere);
            }
            _ => panic!("expected init command"),
        }
    }

    #[test]
    fn parses_init_rerere_flag() {
        let cli = Cli::try_parse_from(["ez", "init", "--rerere"]).expect("parse init --rerere");

        match cli.command {
            Commands::Init { yes, rerere, .. } => {
                assert!(!yes);
                assert!(rerere);
            }
            _ => panic!("expected init command"),
        }
    }

    #[test]
    fn parses_init_yes_and_rerere_together() {
        let cli = Cli::try_parse_from(["ez", "init", "--yes", "--rerere"])
            .expect("parse init --yes --rerere");

        match cli.command {
            Commands::Init { yes, rerere, .. } => {
                assert!(yes);
                assert!(rerere);
            }
            _ => panic!("expected init command"),
        }
    }

    #[test]
    fn parses_init_trunk_and_rerere() {
        let cli = Cli::try_parse_from(["ez", "init", "--trunk", "develop", "--rerere"])
            .expect("parse init trunk rerere");

        match cli.command {
            Commands::Init { trunk, rerere, yes } => {
                assert_eq!(trunk.as_deref(), Some("develop"));
                assert!(rerere);
                assert!(!yes);
            }
            _ => panic!("expected init command"),
        }
    }

    #[test]
    fn parses_setup_interactive_flag() {
        let cli =
            Cli::try_parse_from(["ez", "setup", "--interactive"]).expect("parse setup interactive");

        match cli.command {
            Commands::Setup { yes, interactive } => {
                assert!(!yes);
                assert!(interactive);
            }
            _ => panic!("expected setup command"),
        }
    }

    #[test]
    fn setup_yes_conflicts_with_interactive() {
        let err = match Cli::try_parse_from(["ez", "setup", "--yes", "--interactive"]) {
            Ok(_) => panic!("yes and interactive should conflict"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

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
                no_pr,
                no_draft,
                ..
            } => {
                assert_eq!(message.as_deref(), Some("feat: ship new files"));
                assert!(!stage_all);
                assert!(stage_all_files);
                assert!(!no_pr);
                assert!(!no_draft);
            }
            _ => panic!("expected push command"),
        }
    }

    #[test]
    fn parses_push_no_pr_flag() {
        let cli = Cli::try_parse_from(["ez", "push", "--no-pr"]).expect("parse push --no-pr");

        match cli.command {
            Commands::Push {
                no_pr, pr, draft, ..
            } => {
                assert!(no_pr);
                assert!(!pr);
                assert!(!draft);
            }
            _ => panic!("expected push command"),
        }
    }

    #[test]
    fn parses_push_pr_flag() {
        let cli = Cli::try_parse_from(["ez", "push", "--pr"]).expect("parse push --pr");

        match cli.command {
            Commands::Push { pr, no_pr, .. } => {
                assert!(pr);
                assert!(!no_pr);
            }
            _ => panic!("expected push command"),
        }
    }

    #[test]
    fn push_no_pr_conflicts_with_draft() {
        let result = Cli::try_parse_from(["ez", "push", "--no-pr", "--draft"]);
        assert!(result.is_err(), "--no-pr and --draft should conflict");
    }

    #[test]
    fn push_pr_conflicts_with_no_pr() {
        let result = Cli::try_parse_from(["ez", "push", "--pr", "--no-pr"]);
        assert!(result.is_err(), "--pr and --no-pr should conflict");
    }

    #[test]
    fn parses_push_no_draft_flag() {
        let cli = Cli::try_parse_from(["ez", "push", "--no-draft"]).expect("parse push --no-draft");

        match cli.command {
            Commands::Push {
                no_draft, draft, ..
            } => {
                assert!(no_draft);
                assert!(!draft);
            }
            _ => panic!("expected push command"),
        }
    }

    #[test]
    fn parses_submit_no_draft_flag() {
        let cli =
            Cli::try_parse_from(["ez", "submit", "--no-draft"]).expect("parse submit --no-draft");

        match cli.command {
            Commands::Submit {
                no_draft, draft, ..
            } => {
                assert!(no_draft);
                assert!(!draft);
            }
            _ => panic!("expected submit command"),
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

    #[test]
    fn parses_move_onto_without_value_for_custom_error() {
        let cli = Cli::try_parse_from(["ez", "move", "--onto"])
            .expect("parse move with missing onto value");

        match cli.command {
            Commands::Move { onto } => assert_eq!(onto.as_deref(), Some("")),
            _ => panic!("expected move command"),
        }
    }
}
