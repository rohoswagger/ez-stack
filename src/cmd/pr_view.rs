use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run() -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current.clone()));
    }

    let pr_number = state.get_branch(&current)?.pr_number.ok_or_else(|| {
        EzError::UserMessage(format!(
            "No PR found for `{current}` — run `ez push` to create one first"
        ))
    })?;

    ui::success(&format!("Opened PR for `{current}`"));
    github::open_pr_in_browser(&pr_number.to_string(), state.repo.as_deref())?;
    Ok(())
}
