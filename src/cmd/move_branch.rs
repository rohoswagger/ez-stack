use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run(onto: &str) -> Result<()> {
    let mut state = StackState::load()?;
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current.clone()));
    }

    // The --onto target must be trunk or a managed branch.
    if !state.is_trunk(onto) && !state.is_managed(onto) {
        bail!(EzError::UserMessage(format!(
            "Target branch `{onto}` is not trunk or a managed branch"
        )));
    }

    // Prevent moving onto self.
    if onto == current {
        bail!(EzError::UserMessage(
            "Cannot move a branch onto itself".to_string()
        ));
    }

    // Prevent moving onto a descendant (would create a cycle).
    let path = state.path_to_trunk(onto);
    if path.contains(&current) {
        bail!(EzError::UserMessage(format!(
            "Cannot move `{current}` onto `{onto}` — `{onto}` is a descendant of `{current}`"
        )));
    }

    let meta = state.get_branch(&current)?;
    let old_parent_head = meta.parent_head.clone();
    let pr_number = meta.pr_number;

    let new_parent_head = git::rev_parse(onto)?;

    // Rebase current branch onto the new parent.
    let sp = ui::spinner(&format!("Rebasing `{current}` onto `{onto}`..."));
    let ok = git::rebase_onto(&new_parent_head, &old_parent_head, &current)?;
    sp.finish_and_clear();

    if !ok {
        bail!(EzError::RebaseConflict(current.clone()));
    }

    // Update branch metadata.
    let meta = state.get_branch_mut(&current)?;
    meta.parent = onto.to_string();
    meta.parent_head = new_parent_head;

    // Update PR base if a PR exists.
    if let Some(pr) = pr_number {
        let base = if state.is_trunk(onto) {
            state.trunk.clone()
        } else {
            onto.to_string()
        };
        if let Err(e) = github::update_pr_base(pr, &base) {
            ui::warn(&format!("Failed to update PR base: {e}"));
        }
    }

    // Restack children — they need to be rebased onto the new tip of current branch.
    let new_tip = git::rev_parse(&current)?;
    let children = state.children_of(&current);
    let mut restacked = 0;
    let current_root = git::repo_root()?;

    for child_name in &children {
        if let Ok(Some(wt_path)) = git::branch_checked_out_elsewhere(child_name, &current_root) {
            ui::warn(&format!(
                "`{child_name}` is checked out in worktree `{wt_path}` — skipping restack (run `ez restack` in that worktree)"
            ));
            continue;
        }

        let child = state.get_branch(child_name)?;
        let child_parent_head = child.parent_head.clone();

        if child_parent_head == new_tip {
            continue;
        }

        let sp = ui::spinner(&format!("Restacking `{child_name}` onto `{current}`..."));
        let ok = git::rebase_onto(&new_tip, &child_parent_head, child_name)?;
        sp.finish_and_clear();

        if ok {
            let child = state.get_branch_mut(child_name)?;
            child.parent_head = new_tip.clone();
            restacked += 1;
            ui::success(&format!("Restacked `{child_name}` onto `{current}`"));
        } else {
            state.save()?;
            ui::hint("Resolve the conflicts manually, then run `ez restack` again.");
            bail!(EzError::RebaseConflict(child_name.clone()));
        }
    }

    // Checkout the current branch again (rebase may have left us on the last restacked child).
    git::checkout(&current)?;

    state.save()?;

    ui::success(&format!("Moved `{current}` onto `{onto}`"));
    if restacked > 0 {
        ui::info(&format!("Restacked {restacked} child branch(es)"));
    }

    Ok(())
}
