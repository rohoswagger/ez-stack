use anyhow::Result;

use crate::cmd::mutation_guard;
use crate::cmd::mutation_guard::StageMode;
use crate::cmd::restack;
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

    // Auto-restack the whole subtree so every descendant stays on top of the new
    // HEAD — not just direct children (which would leave grandchildren detached).
    let current_root = git::repo_root()?;
    let restacked_count =
        restack::cascade_restack(&mut state, &current, &current_root, &current, "commit")?;

    // Restacking may have left us on a descendant branch; return to the original.
    if restacked_count > 0 {
        git::checkout(&current)?;
    }

    state.save()?;

    if restacked_count > 0 {
        ui::info(&format!("Restacked {restacked_count} branch(es)"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    fn should_skip_commit(if_changed: bool, has_staged: bool) -> bool {
        if_changed && !has_staged
    }

    #[test]
    fn test_if_changed_semantics() {
        // if_changed=true, nothing staged → should skip (return early)
        assert!(should_skip_commit(true, false));
        // if_changed=true, something staged → should commit
        assert!(!should_skip_commit(true, true));
        // if_changed=false, nothing staged → NothingToCommit error (existing behavior)
        assert!(!should_skip_commit(false, false));
    }
}
