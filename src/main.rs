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
use cli::{Cli, Commands};

fn main() {
    let cli = Cli::parse();

    if let Err(e) = run(cli) {
        ui::error(&format!("{e:#}"));
        std::process::exit(1);
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
        } => cmd::commit::run(&message, all, if_changed),
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
        Commands::Sync { dry_run, autostash } => cmd::sync::run(dry_run, autostash),
        Commands::Restack => cmd::restack::run(),
        Commands::Up => cmd::navigate::up(),
        Commands::Down => cmd::navigate::down(),
        Commands::Top => cmd::navigate::top(),
        Commands::Bottom => cmd::navigate::bottom(),
        Commands::Checkout { name } => cmd::checkout::run(name.as_deref()),
        Commands::Log { json } => cmd::log::run(json),
        Commands::Status { json } => cmd::status::run(json),
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
    }
}
