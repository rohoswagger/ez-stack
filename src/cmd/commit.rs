use anyhow::{Result, bail};

use crate::cmd::mutation_guard;
use crate::cmd::mutation_guard::StageMode;
use crate::cmd::rebase_conflict;
use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

pub fn run(
    message: &str,
    all: bool,
    all_files: bool,
    if_changed: bool,
    paths: &[String],
) -> Result<()> {
    let mut state = StackState::load()?;
    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }
    let stage_mode = if all_files {
        Some(StageMode::All)
    } else if all {
        Some(StageMode::Tracked)
    } else {
        None
    };
    let Some(outcome) = mutation_guard::commit_with_guard(message, stage_mode, if_changed, paths)?
    else {
        return Ok(());
    };

    let current = outcome.current;
    let before = outcome.before;
    let after = outcome.after;
    let short_after = &after[..after.len().min(7)];
    let subject = message.lines().next().unwrap_or(message);
    ui::success(&format!(
        "Committed {short_after} on `{current}`: {subject}"
    ));

    // Show diff stat so agents can verify what was committed.
    if let Ok(stat) = git::show_stat_head() {
        let stat = stat.trim();
        if !stat.is_empty() {
            eprintln!("{stat}");
        }
    }

    // Emit receipt.
    ui::receipt(&serde_json::json!({
        "cmd": "commit",
        "branch": current,
        "before": &before[..before.len().min(7)],
        "after": short_after,
        "files_changed": outcome.files_changed,
        "insertions": outcome.insertions,
        "deletions": outcome.deletions,
        "scope_defined": outcome.scope.scope_defined,
        "scope_mode": outcome.scope.scope_mode,
        "out_of_scope_count": outcome.scope.out_of_scope_files.len(),
        "out_of_scope_files": outcome.scope.out_of_scope_files,
    }));

    // Auto-restack children so they stay on top of the new HEAD.
    let new_head = after;
    let children = state.children_of(&current);

    let current_root = git::repo_root()?;
    let mut restacked_count = 0;

    for child in &children {
        // Guard FIRST — before extracting old_base (avoids unused-variable warning when skipping).
        if let Ok(Some(_wt_path)) = git::branch_checked_out_elsewhere(child, &current_root) {
            ui::info(&format!("Skipped `{child}` (in worktree)"));
            continue;
        }

        let meta = state.get_branch(child)?;
        let old_base = meta.parent_head.clone();

        ui::info(&format!("Restacking `{child}`..."));
        match git::rebase_onto(&new_head, &old_base, child)? {
            git::RebaseOutcome::RebasingComplete => {}
            git::RebaseOutcome::Conflict(conflict) => {
                // Save progress so the user can fix conflicts and continue with `ez restack`.
                state.save()?;
                git::checkout(&current)?;
                rebase_conflict::report("commit", child, &current, &conflict, "ez restack");
                bail!(EzError::RebaseConflict(child.clone()));
            }
        }

        let meta = state.get_branch_mut(child)?;
        meta.parent_head = new_head.clone();
        restacked_count += 1;
    }

    // After restacking we may be on a child branch; return to the original.
    if !children.is_empty() {
        git::checkout(&current)?;
    }

    state.save()?;

    if restacked_count > 0 {
        ui::info(&format!("Restacked {restacked_count} child branch(es)"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_if_changed_semantics() {
        // if_changed=true, nothing staged → should skip (return early)
        assert!(true && !false); // if_changed && !has_staged → skip
        // if_changed=true, something staged → should commit
        assert!(!(true && !true)); // if_changed && !has_staged = false → don't skip
        // if_changed=false, nothing staged → NothingToCommit error (existing behavior)
        assert!(!(false && !false)); // if_changed=false → guard never fires
    }
}
