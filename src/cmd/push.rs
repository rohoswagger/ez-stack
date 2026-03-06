use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run(
    draft: bool,
    title: Option<&str>,
    body: Option<&str>,
    body_file: Option<&str>,
    base_override: Option<&str>,
) -> Result<()> {
    let mut state = StackState::load()?;
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current.clone()));
    }

    let remote = &state.remote.clone();

    let resolved_body: Option<String> = match body_file {
        Some(path) => Some(github::body_from_file(path)?),
        None => body.map(|s| s.to_string()),
    };

    let parent = if let Some(b) = base_override {
        b.to_string()
    } else {
        state.get_branch(&current)?.parent.clone()
    };

    // Push the branch with force-with-lease.
    let sp = ui::spinner(&format!("Pushing `{current}`..."));
    git::fetch_branch(remote, &current)?;
    git::push(remote, &current, true)?;
    sp.finish_and_clear();
    ui::info(&format!("Pushed `{current}`"));

    // Create or update the PR.
    let pr_url = push_or_update_pr(&mut state, &current, &parent, draft, title, resolved_body.as_deref())?;

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
    title_override: Option<&str>,
    body_override: Option<&str>,
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
            let derived_title = commits
                .first()
                .map(|(_, msg)| msg.clone())
                .unwrap_or_else(|| branch.to_string());

            let title = title_override.unwrap_or(&derived_title);
            let default_body = "Part of a stack managed by `ez`.";
            let body = body_override.unwrap_or(default_body);

            let pr = github::create_pr(title, body, parent, branch, draft)?;
            state.get_branch_mut(branch)?.pr_number = Some(pr.number);
            ui::info(&format!("Created PR #{}: {}", pr.number, pr.url));
            pr.url
        }
    };

    Ok(pr_url)
}
