mod cli;
mod cmd;
mod dev;
mod error;
#[allow(dead_code)]
mod git;
#[allow(dead_code)]
mod github;
mod hooks;
mod scope;
mod stack;
mod stack_body;
#[cfg(test)]
mod test_support;
mod ui;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands, ScopeCommands, SkillCommands, WorktreeCommands};
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

    // Check if this is a command that should show the first-run hint.
    let is_setup_command = matches!(cli.command, Commands::Setup { .. } | Commands::ShellInit);

    match run(cli) {
        Ok(()) => {
            // First-run hint: prompt to run `ez setup` once.
            if !is_setup_command && !cmd::setup::is_setup_done() {
                ui::hint("Shell not configured — run `ez setup` for PATH and worktree auto-cd");
            }
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
        Commands::Adopt { pr, branches } => cmd::adopt::run(pr, &branches),
        Commands::Create {
            name,
            message,
            all,
            all_files,
            from,
            no_worktree,
            scope,
            scope_mode,
            hook,
        } => cmd::create::run(
            &name,
            message.as_deref(),
            all,
            all_files,
            from.as_deref(),
            no_worktree,
            &scope,
            scope_mode,
            hook.as_deref(),
        ),
        Commands::Commit {
            message,
            all,
            all_files,
            if_changed,
            paths,
        } => {
            // Join repeated -m flags with double newline (matching git behavior).
            let full_message = message.join("\n\n");
            cmd::commit::run(&full_message, all, all_files, if_changed, &paths)
        }
        Commands::Amend { message, all } => cmd::amend::run(message.as_deref(), all),
        Commands::Push {
            draft,
            title,
            body,
            body_file,
            base,
            stack,
            stage_all,
            stage_all_files,
            message,
        } => cmd::push::run(
            draft,
            title.as_deref(),
            body.as_deref(),
            body_file.as_deref(),
            base.as_deref(),
            stack,
            stage_all,
            stage_all_files,
            message.as_deref(),
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
        Commands::Switch { name } => cmd::checkout::run(name.as_deref()),
        Commands::List { json } => cmd::list::run(json),
        Commands::Log { json } => cmd::log::run(json),
        Commands::Status { json } => cmd::status::run(json),
        Commands::Diff { stat, name_only } => cmd::diff::run(stat, name_only),
        Commands::Parent => cmd::parent::run(),
        Commands::Delete { branch, force, yes } => cmd::delete::run(branch.as_deref(), force, yes),
        Commands::Move { onto } => cmd::move_branch::run(&onto),
        Commands::Merge { method, yes, stack } => cmd::merge::run(&method, yes, stack),
        Commands::PrEdit {
            title,
            body,
            body_file,
        } => cmd::pr_edit::run(title.as_deref(), body.as_deref(), body_file.as_deref()),
        Commands::Draft => cmd::draft::run(false),
        Commands::Ready => cmd::draft::run(true),
        Commands::PrLink => cmd::pr_link::run(),
        Commands::Pr => cmd::pr_view::run(),
        Commands::Update { version, check } => cmd::update::run(version.as_deref(), check),
        Commands::Setup { yes } => cmd::setup::run(yes),
        Commands::Scope(args) => match args.command {
            ScopeCommands::Show => cmd::scope::show(),
            ScopeCommands::Add { mode, patterns } => cmd::scope::add(&patterns, mode),
            ScopeCommands::Set { mode, patterns } => cmd::scope::set(&patterns, mode),
            ScopeCommands::Clear => cmd::scope::clear(),
        },
        Commands::Skill(args) => match args.command {
            SkillCommands::Install => cmd::skill::install(),
            SkillCommands::Uninstall => cmd::skill::uninstall(),
        },
        Commands::ShellInit => cmd::shell_init::run(),
        Commands::Worktree(args) => match args.command {
            WorktreeCommands::Create { name, from } => {
                cmd::worktree::create(&name, from.as_deref())
            }
            WorktreeCommands::Delete { name, force, yes } => {
                cmd::delete::run(Some(&name), force, yes)
            }
            WorktreeCommands::List => cmd::list::run(false),
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
