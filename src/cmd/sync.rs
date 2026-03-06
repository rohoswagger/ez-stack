use anyhow::Result;

use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run(dry_run: bool) -> Result<()> {
    let mut state = StackState::load()?;
    let original_branch = git::current_branch()?;

    // Fetch from remote.
    let sp = ui::spinner(&format!("Fetching from `{}`...", state.remote));
    git::fetch(&state.remote)?;
    sp.finish_and_clear();
    ui::success(&format!("Fetched from `{}`", state.remote));

    // Fast-forward trunk to match remote.
    let remote_trunk = format!("{}/{}", state.remote, state.trunk);
    git::checkout(&state.trunk)?;
    if git::fast_forward_merge(&remote_trunk).is_err() {
        ui::warn("Could not fast-forward trunk — you may have local commits");
    } else {
        ui::success(&format!("Updated `{}` to latest", state.trunk));
    }

    if dry_run {
        ui::info("[dry-run] Would fast-forward trunk to latest remote");

        let order = state.topo_order();
        for branch_name in &order {
            let meta = state.get_branch(branch_name)?;
            if meta.pr_number.is_some() {
                ui::info(&format!("[dry-run] Would check PR status for `{branch_name}`"));
            }
            let parent = &meta.parent;
            let stored_head = &meta.parent_head;
            if let Ok(current_tip) = git::rev_parse(parent) {
                if current_tip != *stored_head {
                    ui::info(&format!("[dry-run] Would restack `{branch_name}` onto `{parent}`"));
                }
            }
        }

        ui::info("[dry-run] No changes made — rerun without --dry-run to apply");
        return Ok(());
    }

    // Detect merged PRs and clean up.
    let managed_branches: Vec<String> = state.branches.keys().cloned().collect();
    let mut cleaned = Vec::new();

    for branch_name in &managed_branches {
        let meta = state.get_branch(branch_name)?;
        let pr_number = meta.pr_number;

        // Only check branches that have a PR associated.
        let merged = if pr_number.is_some() {
            let sp = ui::spinner(&format!("Checking PR status for `{branch_name}`..."));
            let status = github::get_pr_status(branch_name)?;
            sp.finish_and_clear();
            status.is_some_and(|pr| pr.merged)
        } else {
            false
        };

        if !merged {
            continue;
        }

        // Reparent children to this branch's parent.
        let parent = state.get_branch(branch_name)?.parent.clone();
        let children = state.children_of(branch_name);
        let parent_tip = git::rev_parse(&parent)?;

        for child_name in &children {
            let child = state.get_branch_mut(child_name)?;
            child.parent = parent.clone();
            child.parent_head = parent_tip.clone();
        }

        // Remove from state.
        state.remove_branch(branch_name);

        // Delete local branch (ignore errors if already gone).
        let _ = git::delete_branch(branch_name, true);

        ui::success(&format!("Cleaned up merged branch: `{branch_name}`"));
        cleaned.push(branch_name.clone());
    }

    // Restack remaining branches.
    let order = state.topo_order();
    let mut restacked = 0;

    for branch_name in &order {
        let meta = state.get_branch(branch_name)?;
        let parent = meta.parent.clone();
        let stored_parent_head = meta.parent_head.clone();

        let current_parent_tip = git::rev_parse(&parent)?;

        if current_parent_tip == stored_parent_head {
            continue;
        }

        let sp = ui::spinner(&format!("Restacking `{branch_name}` onto `{parent}`..."));
        let ok = git::rebase_onto(&current_parent_tip, &stored_parent_head, branch_name)?;
        sp.finish_and_clear();

        if ok {
            let meta = state.get_branch_mut(branch_name)?;
            meta.parent_head = current_parent_tip;
            restacked += 1;
            ui::success(&format!("Restacked `{branch_name}` onto `{parent}`"));
        } else {
            // Save progress so the user can fix and continue.
            state.save()?;
            ui::hint("Resolve the conflicts manually, then run `ez restack` to continue.");
            anyhow::bail!(crate::error::EzError::RebaseConflict(branch_name.clone()));
        }
    }

    // Return to original branch if it still exists, otherwise trunk.
    if git::branch_exists(&original_branch) {
        git::checkout(&original_branch)?;
    } else {
        git::checkout(&state.trunk)?;
        ui::info(&format!(
            "Previous branch `{original_branch}` was cleaned up — switched to `{}`",
            state.trunk
        ));
    }

    state.save()?;

    // Summary.
    if !cleaned.is_empty() {
        ui::info(&format!("Cleaned up {} merged branch(es)", cleaned.len()));
    }
    if restacked > 0 {
        ui::info(&format!("Restacked {restacked} branch(es)"));
    }
    if cleaned.is_empty() && restacked == 0 {
        ui::info("Everything is up to date");
    }

    Ok(())
}
