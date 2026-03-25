mod cli;
mod cmd;
mod error;
#[allow(dead_code)]
mod git;
#[allow(dead_code)]
mod github;
mod stack;
mod stack_body;
mod ui;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands, WorktreeCommands};
use std::time::Instant;

fn exit_code_for(e: &anyhow::Error) -> i32 {
    use crate::error::EzError;
    if let Some(ez) = e.downcast_ref::<EzError>() {
        match ez {
            EzError::GhError(_) => 2,
            EzError::RebaseConflict(_) => 3,
            EzError::StaleRemoteRef(_) => 4,
            EzError::OnTrunk
            | EzError::BranchNotInStack(_)
            | EzError::AlreadyAtTop
            | EzError::AlreadyAtBottom
            | EzError::BranchAlreadyExists(_)
            | EzError::AlreadyInitialized
            | EzError::NotInitialized
            | EzError::UserMessage(_) => 5,
            EzError::NothingToCommit | EzError::UnstagedChanges => 6,
            _ => 1,
        }
    } else {
        1
    }
}

fn main() {
    let start = Instant::now();

    // Use try_parse so clap errors go through our formatting instead of raw stderr.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            handle_clap_error(e, start);
            return;
        }
    };

    match run(cli) {
        Ok(()) => {
            ui::exit_status(0, start.elapsed());
        }
        Err(e) => {
            let code = exit_code_for(&e);
            ui::error(&format!("{e:#}"));
            ui::exit_status(code, start.elapsed());
            std::process::exit(code);
        }
    }
}

fn handle_clap_error(e: clap::Error, start: Instant) {
    use clap::error::ErrorKind;

    match e.kind() {
        // Help, version, and bare `ez` / `ez worktree`: show help, exit 0.
        // These are Level 0/1 discovery — agents call them to learn available commands.
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayVersion
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            print!("{e}");
            ui::exit_status(0, start.elapsed());
        }
        // Missing required arg: rewrite as ez-style hint.
        ErrorKind::MissingRequiredArgument => {
            ui::error(&format!("{e}"));
            ui::hint("Run `ez <command> --help` for usage details");
            ui::exit_status(5, start.elapsed());
            std::process::exit(5);
        }
        // Unknown subcommand or invalid arg.
        ErrorKind::InvalidSubcommand | ErrorKind::UnknownArgument => {
            ui::error(&format!("{e}"));
            ui::hint("Run `ez --help` to see available commands");
            ui::exit_status(5, start.elapsed());
            std::process::exit(5);
        }
        // Everything else: print clap's message with our exit line.
        _ => {
            ui::error(&format!("{e}"));
            ui::exit_status(5, start.elapsed());
            std::process::exit(5);
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Init { trunk } => cmd::init::run(trunk),
        Commands::Create {
            name,
            message,
            all,
            from,
        } => cmd::create::run(&name, message.as_deref(), all, from.as_deref()),
        Commands::Commit {
            message,
            all,
            if_changed,
            paths,
        } => {
            // Join repeated -m flags with double newline (matching git behavior).
            let full_message = message.join("\n\n");
            cmd::commit::run(&full_message, all, if_changed, &paths)
        }
        Commands::Amend { message, all } => cmd::amend::run(message.as_deref(), all),
        Commands::Push {
            draft,
            title,
            body,
            body_file,
            base,
            stack,
        } => cmd::push::run(
            draft,
            title.as_deref(),
            body.as_deref(),
            body_file.as_deref(),
            base.as_deref(),
            stack,
        ),
        Commands::Submit {
            draft,
            title,
            body,
            body_file,
        } => cmd::submit::run(
            draft,
            title.as_deref(),
            body.as_deref(),
            body_file.as_deref(),
        ),
        Commands::Sync {
            dry_run,
            autostash,
            force,
        } => cmd::sync::run(dry_run, autostash, force),
        Commands::Restack => cmd::restack::run(),
        Commands::Up => cmd::navigate::up(),
        Commands::Down => cmd::navigate::down(),
        Commands::Top => cmd::navigate::top(),
        Commands::Bottom => cmd::navigate::bottom(),
        Commands::Checkout { name } => cmd::checkout::run(name.as_deref()),
        Commands::Log { json } => cmd::log::run(json),
        Commands::Status { json } => cmd::status::run(json),
        Commands::Diff { stat, name_only } => cmd::diff::run(stat, name_only),
        Commands::Parent => cmd::parent::run(),
        Commands::Delete { branch, force } => cmd::delete::run(branch.as_deref(), force),
        Commands::Move { onto } => cmd::move_branch::run(&onto),
        Commands::Merge { method } => cmd::merge::run(&method),
        Commands::PrEdit {
            title,
            body,
            body_file,
        } => cmd::pr_edit::run(title.as_deref(), body.as_deref(), body_file.as_deref()),
        Commands::Draft => cmd::draft::run(false),
        Commands::Ready => cmd::draft::run(true),
        Commands::PrLink => cmd::pr_link::run(),
        Commands::Pr => cmd::pr_view::run(),
        Commands::Worktree(args) => match args.command {
            WorktreeCommands::Create { name, from } => {
                cmd::worktree::create(&name, from.as_deref())
            }
            WorktreeCommands::Delete { name, force } => cmd::worktree::delete(&name, force),
            WorktreeCommands::List => cmd::worktree::list(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EzError;

    #[test]
    fn test_exit_codes() {
        assert_eq!(
            exit_code_for(&EzError::RebaseConflict("x".into()).into()),
            3
        );
        assert_eq!(
            exit_code_for(&EzError::StaleRemoteRef("x".into()).into()),
            4
        );
        assert_eq!(exit_code_for(&EzError::OnTrunk.into()), 5);
        assert_eq!(exit_code_for(&EzError::GhError("x".into()).into()), 2);
        assert_eq!(exit_code_for(&EzError::NothingToCommit.into()), 6);
        assert_eq!(exit_code_for(&EzError::UnstagedChanges.into()), 6);
        assert_eq!(exit_code_for(&anyhow::anyhow!("generic")), 1);
    }
}
