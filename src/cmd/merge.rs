use anyhow::{Result, bail};

use crate::cmd::rebase_conflict;
use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run(method: &str) -> Result<()> {
    let mut state = StackState::load()?;
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current.clone()));
    }

    // Find the bottom branch of the stack (closest to trunk).
    let bottom = state.stack_bottom(&current);
    let meta = state.get_branch(&bottom)?;
    let pr_number = meta.pr_number;

    let pr_number = match pr_number {
        Some(n) => n,
        None => bail!(EzError::UserMessage(format!(
            "Branch `{bottom}` has no associated PR — run `ez submit` first"
        ))),
    };

    // Confirm with the user.
    let pr_info = github::get_pr_status(&bottom)?;
    let title = pr_info
        .as_ref()
        .map(|p| p.title.as_str())
        .unwrap_or("(unknown)");

    if !ui::confirm(&format!("Merge PR #{pr_number} for `{bottom}` ({title})?")) {
        ui::info("Aborted");
        return Ok(());
    }

    // Merge via GitHub.
    let sp = ui::spinner(&format!("Merging PR #{pr_number}..."));
    github::merge_pr(pr_number, method)?;
    sp.finish_and_clear();
    ui::info(&format!("Merged PR #{pr_number} for `{bottom}`"));

    // Reparent children of the merged branch to trunk.
    let trunk = state.trunk.clone();
    let remote = state.remote.clone();

    let children = state.reparent_children_preserving_parent_head(&bottom, &trunk)?;
    for child_name in &children {
        ui::info(&format!("Reparented `{child_name}` onto `{trunk}`"));

        // Update the PR base on GitHub if the child has a PR.
        if let Some(child_pr) = state.get_branch(child_name)?.pr_number
            && let Err(e) = github::update_pr_base(child_pr, &trunk)
        {
            ui::warn(&format!("Failed to update PR base for `{child_name}`: {e}"));
        }
    }

    // Remove the merged branch from state.
    state.remove_branch(&bottom);

    // Delete local branch if it still exists.
    // (gh merge --delete-branch may have already removed the remote branch)
    if git::branch_exists(&bottom) {
        // If we're on the merged branch, checkout trunk first.
        let current_now = git::current_branch()?;
        if current_now == bottom {
            git::checkout(&trunk)?;
        }
        let _ = git::delete_branch(&bottom, true);
    }

    // Fetch to get the merged trunk.
    let sp = ui::spinner("Fetching latest changes...");
    git::fetch(&remote)?;
    sp.finish_and_clear();

    // Restack remaining branches in topological order.
    let order = state.topo_order();
    let mut restacked = 0;
    let current_root = git::repo_root()?;

    for branch_name in &order {
        let meta = state.get_branch(branch_name)?;
        let parent = meta.parent.clone();
        let stored_parent_head = meta.parent_head.clone();

        let current_parent_tip = if state.is_trunk(&parent) {
            git::rev_parse(&format!("{remote}/{parent}"))?
        } else {
            git::rev_parse(&parent)?
        };

        if current_parent_tip == stored_parent_head {
            continue;
        }

        if let Ok(Some(_wt_path)) = git::branch_checked_out_elsewhere(branch_name, &current_root) {
            ui::warn(&format!("Skipped `{branch_name}` (in worktree)"));
            continue;
        }

        let sp = ui::spinner(&format!("Restacking `{branch_name}` onto `{parent}`..."));
        let outcome = git::rebase_onto(&current_parent_tip, &stored_parent_head, branch_name)?;
        sp.finish_and_clear();

        match outcome {
            git::RebaseOutcome::RebasingComplete => {
                let meta = state.get_branch_mut(branch_name)?;
                meta.parent_head = current_parent_tip;
                restacked += 1;
                ui::info(&format!("Restacked `{branch_name}` onto `{parent}`"));
            }
            git::RebaseOutcome::Conflict(conflict) => {
                state.save()?;
                rebase_conflict::report("merge", branch_name, &parent, &conflict, "ez restack");
                bail!(EzError::RebaseConflict(branch_name.clone()));
            }
        }
    }

    // Checkout the next branch in the stack, or trunk if none remain.
    let current_now = git::current_branch()?;
    if !state.is_managed(&current_now) && !state.is_trunk(&current_now) {
        if let Some(next) = children.first().filter(|c| state.is_managed(c)) {
            git::checkout(next)?;
            ui::info(&format!("Checked out `{next}`"));
        } else {
            git::checkout(&trunk)?;
            ui::info(&format!("Checked out `{trunk}`"));
        }
    }

    state.save()?;

    if restacked > 0 {
        ui::info(&format!("Restacked {restacked} branch(es)"));
    }
    ui::success("Merge complete");

    ui::receipt(&serde_json::json!({
        "cmd": "merge",
        "branch": bottom,
        "pr_number": pr_number,
        "method": method,
    }));

    Ok(())
}
