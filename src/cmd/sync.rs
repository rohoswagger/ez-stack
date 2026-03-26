use anyhow::Result;

use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run(dry_run: bool, autostash: bool, force: bool) -> Result<()> {
    let state = StackState::load()?;

    if dry_run {
        ui::header("Sync preview (--dry-run, no changes will be made)");
        ui::info(&format!("Would fetch from `{}`", state.remote));
        ui::info(&format!(
            "Would update `{}` to latest remote (no checkout needed)",
            state.trunk
        ));

        let dry_worktree_map: std::collections::HashMap<String, String> = git::worktree_list()
            .unwrap_or_default()
            .into_iter()
            .filter(|wt| wt.path.contains("/.worktrees/"))
            .filter_map(|wt| wt.branch.map(|b| (b, wt.path)))
            .collect();

        let managed_branches: Vec<String> = state.branches.keys().cloned().collect();
        for branch_name in &managed_branches {
            let meta = state.get_branch(branch_name)?;
            if meta.pr_number.is_some() {
                ui::info(&format!("Would check if PR for `{branch_name}` is merged"));
                if let Some(wt_path) = dry_worktree_map.get(branch_name.as_str()) {
                    ui::info(&format!("  → Would remove worktree at `{wt_path}`"));
                }
            }
        }

        let order = state.topo_order();
        let mut any_restack = false;
        for branch_name in &order {
            let meta = state.get_branch(branch_name)?;
            let parent = &meta.parent;
            let stored_head = &meta.parent_head;
            if let Ok(current_tip) = git::rev_parse(parent) {
                if current_tip != *stored_head {
                    ui::info(&format!("Would restack `{branch_name}` onto `{parent}`"));
                    any_restack = true;
                }
            }
        }

        if !any_restack {
            ui::info("No restacking needed based on current local state");
        }

        ui::hint("Run `ez sync` (without --dry-run) to apply these changes");
        return Ok(());
    }

    // Autostash: stash before any mutations.
    let stashed = if autostash {
        let did_stash = git::stash_push()?;
        if did_stash {
            ui::info("Stashed uncommitted changes (--autostash)");
        }
        did_stash
    } else {
        false
    };

    let result = run_sync_inner(force);

    if stashed {
        if let Err(e) = git::stash_pop() {
            ui::warn(&format!("Failed to pop autostash: {e}"));
        } else {
            ui::info("Restored stashed changes");
        }
    }

    result
}

fn run_sync_inner(force: bool) -> Result<()> {
    let mut state = StackState::load()?;
    let original_branch = git::current_branch()?;
    let original_root = git::repo_root()?;

    // Fetch from remote.
    let sp = ui::spinner(&format!("Fetching from `{}`...", state.remote));
    git::fetch(&state.remote)?;
    sp.finish_and_clear();
    ui::success(&format!("Fetched from `{}`", state.remote));

    // Update trunk to latest. git fetch above already refreshed origin/<trunk>.
    // Only attempt to fast-forward local trunk when it is strictly behind the remote-tracking
    // ref — skip silently if local is equal, ahead, or diverged (nothing safe to do).
    let remote_tracking = format!("{}/{}", state.remote, state.trunk);
    let trunk_is_behind = git::is_ancestor(&state.trunk, &remote_tracking)
        && !git::is_ancestor(&remote_tracking, &state.trunk);
    if trunk_is_behind {
        if original_branch == state.trunk {
            // Currently on trunk: fast-forward via merge (fetch refupdate won't update HEAD).
            let remote_ref = format!("{}/{}", state.remote, state.trunk);
            match git::fast_forward_merge(&remote_ref) {
                Ok(()) => ui::success(&format!("Updated `{}` to latest", state.trunk)),
                Err(e) => ui::warn(&format!("Could not update `{}` — {e}", state.trunk)),
            }
        } else {
            // Not on trunk: update ref directly without checkout.
            match git::fetch_refupdate(&state.remote, &state.trunk) {
                Ok(()) => ui::success(&format!("Updated `{}` to latest", state.trunk)),
                Err(e) => ui::warn(&format!("Could not update `{}` — {e}", state.trunk)),
            }
        }
    }

    // Build branch→worktree map for pruning merged branches.
    // Only include worktrees under .worktrees/ — the main worktree must never be removed.
    let worktree_map: std::collections::HashMap<String, String> = git::worktree_list()
        .unwrap_or_default()
        .into_iter()
        .filter(|wt| wt.path.contains("/.worktrees/"))
        .filter_map(|wt| wt.branch.map(|b| (b, wt.path)))
        .collect();

    // Detect merged PRs and clean up.
    let managed_branches: Vec<String> = state.branches.keys().cloned().collect();
    let mut cleaned = Vec::new();

    for branch_name in &managed_branches {
        let meta = state.get_branch(branch_name)?;
        let pr_number = meta.pr_number;

        // Auto-clean branches that no longer exist locally (deleted outside of ez).
        if !git::branch_exists(branch_name) {
            let parent = state.get_branch(branch_name)?.parent.clone();
            let children = state.children_of(branch_name);
            if let Ok(parent_tip) = git::rev_parse(&parent) {
                for child_name in &children {
                    if let Ok(child) = state.get_branch_mut(child_name) {
                        child.parent = parent.clone();
                        child.parent_head = parent_tip.clone();
                    }
                }
            }
            state.remove_branch(branch_name);
            ui::success(&format!(
                "Cleaned up `{branch_name}` (branch no longer exists locally)"
            ));
            cleaned.push(branch_name.clone());
            continue;
        }

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

        // Remove worktree for this branch (if any).
        if let Some(wt_path) = worktree_map.get(branch_name.as_str()) {
            let result = if force {
                git::worktree_remove_force(wt_path)
            } else {
                git::worktree_remove(wt_path)
            };
            match result {
                Ok(()) => ui::success(&format!("Removed worktree at `{wt_path}`")),
                Err(e) => ui::warn(&format!(
                    "Could not remove worktree at `{wt_path}`: {e}\n  Hint: use `ez sync --force` to discard uncommitted changes"
                )),
            }
        }

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

        // Skip branches that no longer exist (shouldn't happen after cleanup above, but be safe).
        if !git::branch_exists(branch_name) {
            continue;
        }

        let current_parent_tip = git::rev_parse(&parent)?;

        if current_parent_tip == stored_parent_head {
            continue;
        }

        // Guard: skip branches checked out in another worktree.
        if let Ok(Some(wt_path)) = git::branch_checked_out_elsewhere(branch_name, &original_root) {
            ui::warn(&format!(
                "`{branch_name}` is checked out in worktree `{wt_path}` — skipping restack (run `ez restack` in that worktree)"
            ));
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

            // Post-restack: use `git cherry` to detect commits whose patches
            // are already upstream (landed via a different path). If found,
            // run a plain `git rebase` which auto-drops them via patch-id matching.
            if let Ok(cherry) = git::cherry(&parent, branch_name) {
                let redundant: Vec<&str> = cherry.lines().filter(|l| l.starts_with("- ")).collect();
                if !redundant.is_empty() {
                    ui::info(&format!(
                        "Dropping {} redundant commit(s) from `{branch_name}` (already in `{parent}`)",
                        redundant.len()
                    ));
                    match git::rebase(&parent, branch_name) {
                        Ok(true) => {
                            ui::success(&format!(
                                "Cleaned up `{branch_name}` — dropped redundant commits"
                            ));
                        }
                        Ok(false) => {
                            ui::warn(&format!(
                                "Could not auto-drop redundant commits from `{branch_name}` (conflict)"
                            ));
                            ui::hint(&format!(
                                "Run `git rebase {parent}` on `{branch_name}` manually and skip redundant commits"
                            ));
                        }
                        Err(e) => {
                            ui::warn(&format!(
                                "Could not clean up redundant commits from `{branch_name}`: {e}"
                            ));
                        }
                    }
                }
            }
        } else {
            // Save progress so the user can fix and continue.
            state.save()?;
            ui::hint("Resolve the conflicts manually, then run `ez restack` to continue.");
            anyhow::bail!(crate::error::EzError::RebaseConflict(branch_name.clone()));
        }
    }

    // Return to original branch if it still exists.
    // If it was cleaned up (merged), fall back to trunk — but trunk might be in another worktree.
    if git::branch_exists(&original_branch) {
        git::checkout(&original_branch)?;
    } else {
        match git::checkout(&state.trunk) {
            Ok(()) => ui::info(&format!(
                "Previous branch `{original_branch}` was cleaned up — switched to `{}`",
                state.trunk
            )),
            Err(_) => ui::warn(&format!(
                "Previous branch `{original_branch}` was cleaned up. Switch to another branch manually \
                 (trunk may be checked out in another worktree)."
            )),
        }
    }

    state.save()?;

    if cleaned.is_empty() && restacked == 0 {
        ui::info("Everything is up to date");
    }

    // Prune stale worktree admin entries.
    let _ = git::worktree_prune();

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn run_signatures_compile() {
        // Verifies the public API is correct at compile time.
        let f: fn(bool, bool, bool) -> anyhow::Result<()> = super::run;
        let _ = std::mem::size_of_val(&f);
    }
}
