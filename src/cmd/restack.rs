use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

pub fn run() -> Result<()> {
    let mut state = StackState::load()?;
    let original_branch = git::current_branch()?;
    let current_root = git::repo_root()?;

    let order = state.topo_order();
    let mut restacked = 0;
    let mut skipped = 0;

    for branch_name in &order {
        let meta = state.get_branch(branch_name)?;
        let parent = meta.parent.clone();
        let stored_parent_head = meta.parent_head.clone();

        let current_parent_tip = git::rev_parse(&parent)?;

        if current_parent_tip == stored_parent_head {
            continue;
        }

        // Guard: skip branches checked out in another worktree.
        if let Ok(Some(wt_path)) = git::branch_checked_out_elsewhere(branch_name, &current_root) {
            ui::warn(&format!(
                "`{branch_name}` is checked out in worktree `{wt_path}` — run `ez restack` in that worktree"
            ));
            skipped += 1;
            continue;
        }

        // Branch is stale — rebase onto the new parent tip.
        let sp = ui::spinner(&format!("Restacking `{branch_name}` onto `{parent}`..."));
        let ok = git::rebase_onto(&current_parent_tip, &stored_parent_head, branch_name)?;
        sp.finish_and_clear();

        if ok {
            let meta = state.get_branch_mut(branch_name)?;
            meta.parent_head = current_parent_tip;
            restacked += 1;
            ui::success(&format!("Restacked `{branch_name}` onto `{parent}`"));

            // Auto-drop commits whose patches are already upstream.
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
            git::checkout(&original_branch)?;
            state.save()?;
            ui::hint("Resolve the conflicts manually, then run `ez restack` again.");
            bail!(EzError::RebaseConflict(branch_name.clone()));
        }
    }

    // Return to the original branch.
    git::checkout(&original_branch)?;

    state.save()?;

    if restacked == 0 && skipped == 0 {
        ui::info("All branches are up to date — nothing to restack");
    }

    Ok(())
}
