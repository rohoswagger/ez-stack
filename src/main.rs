mod cli;
mod cmd;
mod error;
#[allow(dead_code)]
mod git;
#[allow(dead_code)]
mod github;
mod stack;
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
        Commands::Create { name, message, all } => cmd::create::run(&name, message.as_deref(), all),
        Commands::Commit { message, all } => cmd::commit::run(&message, all),
        Commands::Amend { message, all } => cmd::amend::run(message.as_deref(), all),
        Commands::Push { draft, title, body, body_file, base } => {
            cmd::push::run(draft, title.as_deref(), body.as_deref(), body_file.as_deref(), base.as_deref())
        }
        Commands::Submit { draft, title, body, body_file } => {
            cmd::submit::run(draft, title.as_deref(), body.as_deref(), body_file.as_deref())
        }
        Commands::Sync { dry_run } => cmd::sync::run(dry_run),
        Commands::Restack => cmd::restack::run(),
        Commands::Up => cmd::navigate::up(),
        Commands::Down => cmd::navigate::down(),
        Commands::Top => cmd::navigate::top(),
        Commands::Bottom => cmd::navigate::bottom(),
        Commands::Checkout => cmd::checkout::run(),
        Commands::Log => cmd::log::run(),
        Commands::Status => cmd::status::run(),
        Commands::Delete { branch, force } => cmd::delete::run(branch.as_deref(), force),
        Commands::Move { onto } => cmd::move_branch::run(&onto),
        Commands::Merge { method } => cmd::merge::run(&method),
    }
}
