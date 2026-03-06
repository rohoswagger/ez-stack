use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run(draft: bool) -> Result<()> {
    let mut state = StackState::load()?;
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current.clone()));
    }

    let remote = &state.remote.clone();
    let parent = state.get_branch(&current)?.parent.clone();

    // Push the branch with force-with-lease.
    let sp = ui::spinner(&format!("Pushing `{current}`..."));
    git::fetch_branch(remote, &current)?;
    git::push(remote, &current, true)?;
    sp.finish_and_clear();
    ui::info(&format!("Pushed `{current}`"));

    // Create or update the PR.
    let pr_url = push_or_update_pr(&mut state, &current, &parent, draft)?;

    state.save()?;
    ui::success(&format!("PR: {pr_url}"));
    Ok(())
}

/// Push-or-update logic shared with the `submit` command.
///
/// Returns the PR URL.
pub fn push_or_update_pr(
    state: &mut StackState,
    branch: &str,
    parent: &str,
    draft: bool,
) -> Result<String> {
    let existing_pr = github::get_pr_status(branch)?;

    let pr_url = match existing_pr {
        Some(pr) => {
            // Update the base branch if the parent has changed.
            github::update_pr_base(pr.number, parent)?;
            state.get_branch_mut(branch)?.pr_number = Some(pr.number);
            ui::info(&format!("Updated PR #{} base to `{parent}`", pr.number));
            pr.url
        }
        None => {
            // Derive the PR title from the first commit on this branch.
            let range = format!("{parent}..{branch}");
            let commits = git::log_oneline(&range, 1)?;
            let title = commits
                .first()
                .map(|(_, msg)| msg.clone())
                .unwrap_or_else(|| branch.to_string());

            let body = "Part of a stack managed by `ez`.";

            let pr = github::create_pr(&title, body, parent, branch, draft)?;
            state.get_branch_mut(branch)?.pr_number = Some(pr.number);
            ui::info(&format!("Created PR #{}: {}", pr.number, pr.url));
            pr.url
        }
    };

    Ok(pr_url)
}
