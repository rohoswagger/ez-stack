use anyhow::{Result, bail};

use crate::cmd::rebase_conflict;
use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

pub fn run() -> Result<()> {
    let mut state = StackState::load()?;
    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }
    let original_branch = git::current_branch()?;
    let current_root = git::repo_root()?;

    ui::info(&format!("Fetching from `{}`...", state.remote));
    git::fetch(&state.remote)?;
    match git::update_branch_to_latest_remote(
        &state.remote,
        &state.trunk,
        &original_branch,
        &current_root,
    ) {
        Ok(true) => ui::info(&format!("Updated `{}` to latest", state.trunk)),
        Ok(false) => {}
        Err(e) => ui::warn(&format!("Could not update `{}` — {e}", state.trunk)),
    }

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
        if let Ok(Some(_wt_path)) = git::branch_checked_out_elsewhere(branch_name, &current_root) {
            ui::warn(&format!("Skipped `{branch_name}` (in worktree)"));
            skipped += 1;
            continue;
        }

        // Branch is stale — rebase onto the new parent tip.
        let before_sha = git::rev_parse(branch_name).unwrap_or_default();

        let sp = ui::spinner(&format!("Restacking `{branch_name}` onto `{parent}`..."));
        let outcome = git::rebase_onto(&current_parent_tip, &stored_parent_head, branch_name)?;
        sp.finish_and_clear();

        match outcome {
            git::RebaseOutcome::RebasingComplete => {
                let meta = state.get_branch_mut(branch_name)?;
                meta.parent_head = current_parent_tip;
                restacked += 1;
                ui::info(&format!("Restacked `{branch_name}` onto `{parent}`"));

                // Auto-drop commits whose patches are already upstream.
                let mut redundant_count: u64 = 0;
                if let Ok(cherry) = git::cherry(&parent, branch_name) {
                    let redundant: Vec<&str> =
                        cherry.lines().filter(|l| l.starts_with("- ")).collect();
                    if !redundant.is_empty() {
                        redundant_count = redundant.len() as u64;
                        ui::info(&format!(
                            "Dropping {redundant_count} redundant commit(s) from `{branch_name}` (already in `{parent}`)",
                        ));
                        match git::rebase(&parent, branch_name) {
                            Ok(true) => {
                                ui::info(&format!(
                                    "Dropped redundant commits from `{branch_name}`"
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

                let after_sha = git::rev_parse(branch_name).unwrap_or_default();
                ui::receipt(&serde_json::json!({
                    "cmd": "restack",
                    "branch": branch_name,
                    "action": "restacked",
                    "parent": parent,
                    "before": &before_sha[..before_sha.len().min(7)],
                    "after": &after_sha[..after_sha.len().min(7)],
                    "redundant_commits": redundant_count,
                }));
            }
            git::RebaseOutcome::Conflict(conflict) => {
                git::checkout(&original_branch)?;
                state.save()?;
                rebase_conflict::report("restack", branch_name, &parent, &conflict, "ez restack");
                bail!(EzError::RebaseConflict(branch_name.clone()));
            }
        }
    }

    // Return to the original branch.
    git::checkout(&original_branch)?;

    state.save()?;

    if restacked == 0 && skipped == 0 {
        ui::info("All branches are up to date — nothing to restack");
    }

    if restacked > 0 {
        ui::success(&format!("Restacked {restacked} branch(es)"));
    }

    Ok(())
}
