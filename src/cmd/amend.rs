use anyhow::{Result, bail};

use crate::cmd::rebase_conflict;
use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

pub fn run(message: Option<&str>, all: bool) -> Result<()> {
    let mut state = StackState::load()?;
    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }
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
        bail!(EzError::UserMessage(
            "no staged changes to amend\n  → Stage files with `git add <files>`, or use `ez amend -a` to stage all".to_string()
        ));
    }

    let before = git::rev_parse("HEAD")?;

    git::commit_amend(message)?;

    let after = git::rev_parse("HEAD")?;
    let short_after = &after[..after.len().min(7)];
    ui::success(&format!("Amended commit {short_after}"));

    // Show diff stat so agents can verify what was amended.
    let (files, ins, del) = git::diff_stat_numbers();
    if let Ok(stat) = git::show_stat_head() {
        let stat = stat.trim();
        if !stat.is_empty() {
            eprintln!("{stat}");
        }
    }

    // Emit receipt.
    ui::receipt(&serde_json::json!({
        "cmd": "amend",
        "branch": current,
        "before": &before[..before.len().min(7)],
        "after": short_after,
        "files_changed": files,
        "insertions": ins,
        "deletions": del,
    }));

    // Auto-restack children of the current branch.
    let current_head = after;
    let children = state.children_of(&current);

    let current_root = git::repo_root()?;

    for child_name in &children {
        // Guard FIRST — before extracting old_parent_head.
        if let Ok(Some(_wt_path)) = git::branch_checked_out_elsewhere(child_name, &current_root) {
            ui::info(&format!("Skipped `{child_name}` (in worktree)"));
            continue;
        }

        let old_parent_head = state.get_branch(child_name)?.parent_head.clone();

        let sp = ui::spinner(&format!("Restacking `{child_name}`..."));
        let outcome = git::rebase_onto(&current_head, &old_parent_head, child_name)?;
        sp.finish_and_clear();

        match outcome {
            git::RebaseOutcome::RebasingComplete => {
                let child = state.get_branch_mut(child_name)?;
                child.parent_head = current_head.clone();
                ui::info(&format!("Restacked `{child_name}`"));
            }
            git::RebaseOutcome::Conflict(conflict) => {
                git::checkout(&current)?;
                state.save()?;
                rebase_conflict::report("amend", child_name, &current, &conflict, "ez restack");
                bail!(EzError::RebaseConflict(child_name.clone()));
            }
        }
    }

    // Return to the original branch after restacking (only if we may have moved).
    if !children.is_empty() {
        git::checkout(&current)?;
    }

    state.save()?;
    Ok(())
}
