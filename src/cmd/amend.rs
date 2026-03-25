use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

pub fn run(message: Option<&str>, all: bool) -> Result<()> {
    let mut state = StackState::load()?;
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current.clone()));
    }

    if all {
        git::add_all()?;
    }

    if !all && !git::has_staged_changes()? {
        bail!(EzError::NothingToCommit);
    }

    git::commit_amend(message)?;
    ui::success("Amended commit");

    // Show diff stat so agents can verify what was amended.
    if let Ok(stat) = git::show_stat_head() {
        let stat = stat.trim();
        if !stat.is_empty() {
            eprintln!("{stat}");
        }
    }

    // Auto-restack children of the current branch.
    let current_head = git::rev_parse("HEAD")?;
    let children = state.children_of(&current);

    let current_root = git::repo_root()?;
    let mut restacked_count = 0;

    for child_name in &children {
        // Guard FIRST — before extracting old_parent_head.
        if let Ok(Some(wt_path)) = git::branch_checked_out_elsewhere(child_name, &current_root) {
            ui::info(&format!(
                "`{child_name}` is in worktree `{wt_path}` — run `ez restack` there to update it"
            ));
            continue;
        }

        let old_parent_head = state.get_branch(child_name)?.parent_head.clone();

        let sp = ui::spinner(&format!("Restacking `{child_name}`..."));
        let ok = git::rebase_onto(&current_head, &old_parent_head, child_name)?;
        sp.finish_and_clear();

        if ok {
            let child = state.get_branch_mut(child_name)?;
            child.parent_head = current_head.clone();
            restacked_count += 1;
            ui::info(&format!("Restacked `{child_name}`"));
        } else {
            git::checkout(&current)?;
            state.save()?;
            ui::hint("Resolve conflicts, then run `ez restack`");
            bail!(EzError::RebaseConflict(child_name.clone()));
        }
    }

    // Return to the original branch after restacking (only if we may have moved).
    if !children.is_empty() {
        git::checkout(&current)?;
    }

    state.save()?;
    if restacked_count > 0 {
        ui::success(&format!(
            "Amended `{current}` and restacked {restacked_count} child branch(es)"
        ));
    } else {
        ui::success(&format!("Amended `{current}`"));
    }
    Ok(())
}
