use anyhow::{Result, bail};
use std::collections::HashMap;

use crate::dev;
use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run(branch: Option<&str>, force: bool, yes: bool) -> Result<()> {
    let mut state = StackState::load()?;
    let current = git::current_branch()?;

    let target = branch.unwrap_or(&current).to_string();

    if state.is_trunk(&target) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&target) {
        bail!(EzError::BranchNotInStack(target.clone()));
    }

    // Build branch → worktree path map to detect if the target has a worktree.
    let worktree_map: HashMap<String, String> = git::worktree_list()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|wt| wt.branch.map(|b| (b, wt.path)))
        .collect();

    let has_worktree = worktree_map.contains_key(&target);

    if has_worktree {
        return delete_with_worktree(&mut state, &target, &current, force, yes, &worktree_map);
    }

    // No worktree — original branch-only delete path.

    // Worktree guard: if the target branch is checked out in another worktree, bail.
    let current_root = git::repo_root()?;
    if let Ok(Some(wt_path)) = git::branch_checked_out_elsewhere(&target, &current_root) {
        bail!(EzError::UserMessage(format!(
            "branch `{target}` is checked out in worktree `{wt_path}`\n  → Run `ez delete {target}` to remove it"
        )));
    }

    let meta = state.get_branch(&target)?;
    let parent = meta.parent.clone();
    let pr_number = meta.pr_number;

    // Reparent children.
    let children = state.reparent_children_preserving_parent_head(&target, &parent)?;
    for child_name in &children {
        ui::info(&format!("Reparented `{child_name}` onto `{parent}`"));
    }

    // Update PR bases on GitHub (best-effort).
    if pr_number.is_some() {
        let new_base = parent.clone();
        for child_name in &children {
            let child = state.get_branch(child_name)?;
            if let Some(child_pr) = child.pr_number
                && let Err(e) = github::update_pr_base(child_pr, &new_base)
            {
                ui::warn(&format!("Failed to update PR base for `{child_name}`: {e}"));
            }
        }
    }

    // If currently on the target branch, checkout parent first.
    if current == target {
        git::checkout(&parent)?;
    }

    // Delete local branch.
    if git::branch_exists(&target)
        && let Err(e) = git::delete_branch(&target, force)
    {
        if force {
            ui::warn(&format!("Failed to delete local branch `{target}`: {e}"));
        } else {
            ui::warn(&format!(
                "Branch `{target}` has unmerged changes — use --force to delete anyway"
            ));
            state.save()?;
            return Err(e);
        }
    }

    // Try to delete remote branch (ignore errors).
    let _ = git::delete_remote_branch(&state.remote, &target);

    state.remove_branch(&target);
    state.save()?;

    ui::success(&format!("Deleted branch `{target}`"));
    if !children.is_empty() {
        ui::hint(&format!(
            "Run `ez restack` to rebase reparented branches onto `{parent}`"
        ));
    }

    ui::receipt(&serde_json::json!({
        "cmd": "delete",
        "branch": target,
        "parent": parent,
        "reparented_children": children.len(),
    }));

    Ok(())
}

/// Delete a branch that has an associated worktree.
fn delete_with_worktree(
    state: &mut StackState,
    target: &str,
    current: &str,
    force: bool,
    yes: bool,
    worktree_map: &HashMap<String, String>,
) -> Result<()> {
    let wt_path = worktree_map[target].clone();
    let repo_root = git::main_worktree_root()?;
    let port = dev::dev_port(target);

    let current_dir = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_default();

    let inside_worktree = inside_worktree_path(&current_dir, &wt_path);

    if inside_worktree && !yes {
        ui::warn(&inside_worktree_delete_warning(target));
        if !ui::confirm("Delete this worktree and switch to the repo root?") {
            ui::info(&inside_worktree_delete_cancelled(target));
            return Ok(());
        }
    }

    // Pre-compute stack changes.
    let meta = state.get_branch(target)?;
    let parent = meta.parent.clone();
    let pr_number = meta.pr_number;
    let children = state.children_of(target);
    let child_prs: Vec<(String, Option<u64>)> = children
        .iter()
        .filter_map(|c| state.get_branch(c).ok().map(|m| (c.clone(), m.pr_number)))
        .collect();

    // --- Phase 2: Mutate filesystem ---

    if inside_worktree {
        std::env::set_current_dir(&repo_root)?;
    }

    let killed_pids = match dev::terminate_listener_processes(port) {
        Ok(pids) => {
            if !pids.is_empty() {
                ui::info(&format!(
                    "Stopped {} process(es) on dev port {}",
                    pids.len(),
                    port
                ));
            }
            pids
        }
        Err(e) => {
            ui::warn(&format!(
                "Failed to stop process(es) on dev port {}: {}",
                port, e
            ));
            Vec::new()
        }
    };

    let _ = git::worktree_prune();

    let wt_dir = std::path::Path::new(&wt_path);
    if wt_dir.exists() && wt_dir.join(".git").exists() {
        let result = if force {
            git::worktree_remove_force(&wt_path)
        } else {
            git::worktree_remove(&wt_path)
        };
        if let Err(e) = result {
            bail!(
                "Could not remove worktree at `{wt_path}`: {e}\n\
                 Use `ez delete {target} --force` to discard uncommitted changes"
            );
        }
        ui::info(&format!("Removed worktree at `{wt_path}`"));
    } else if wt_dir.exists() {
        let _ = std::fs::remove_dir_all(&wt_path);
        ui::info(&format!("Cleaned up stale directory at `{wt_path}`"));
    }

    // --- Phase 3: Mutate stack state ---

    // Reparent children.
    let children = state.reparent_children_preserving_parent_head(target, &parent)?;
    for child_name in &children {
        ui::info(&format!("Reparented `{child_name}` onto `{parent}`"));
    }

    // Update PR bases on GitHub (best-effort).
    if pr_number.is_some() {
        for (child_name, child_pr) in &child_prs {
            if let Some(pr) = child_pr {
                if let Err(e) = github::update_pr_base(*pr, &parent) {
                    ui::warn(&format!("Failed to update PR base for `{child_name}`: {e}"));
                }
            }
        }
    }

    // If current branch (in the main worktree) is the target, checkout parent.
    if current == target {
        let _ = git::checkout(&parent);
    }

    // Delete local branch.
    let _ = git::delete_branch(target, true);

    // Try to delete remote branch (ignore errors).
    let _ = git::delete_remote_branch(&state.remote, target);

    state.remove_branch(target);
    state.save()?;

    ui::success(&format!("Deleted branch `{target}`"));
    if !children.is_empty() {
        ui::hint(&format!(
            "Run `ez restack` to rebase reparented branches onto `{parent}`"
        ));
    }

    ui::receipt(&serde_json::json!({
        "cmd": "delete",
        "branch": target,
        "parent": parent,
        "dev_port": port,
        "killed_pids": killed_pids,
        "worktree": wt_path,
        "reparented_children": children.len(),
    }));

    // If we were inside the deleted worktree, print repo root for shell cd.
    if inside_worktree {
        println!("{repo_root}");
    }

    Ok(())
}

fn inside_worktree_path(current_dir: &str, worktree_path: &str) -> bool {
    current_dir == worktree_path || current_dir.starts_with(&format!("{worktree_path}/"))
}

fn inside_worktree_delete_warning(target: &str) -> String {
    format!(
        "You are inside the worktree for `{target}` that you are about to delete\n  → Re-run with `--yes` to skip this prompt"
    )
}

fn inside_worktree_delete_cancelled(target: &str) -> String {
    format!("Cancelled\n  → Re-run with `--yes`: `ez worktree delete {target} --yes`")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inside_worktree_path_matches_exact_and_nested() {
        assert!(inside_worktree_path(
            "/repo/.worktrees/feat",
            "/repo/.worktrees/feat"
        ));
        assert!(inside_worktree_path(
            "/repo/.worktrees/feat/src/app",
            "/repo/.worktrees/feat"
        ));
        assert!(!inside_worktree_path(
            "/repo/.worktrees/feat-two",
            "/repo/.worktrees/feat"
        ));
    }

    #[test]
    fn inside_worktree_delete_warning_mentions_yes_flag() {
        let warning = inside_worktree_delete_warning("feat/auth");
        assert!(warning.contains("feat/auth"));
        assert!(warning.contains("--yes"));
    }

    #[test]
    fn inside_worktree_delete_cancelled_mentions_worktree_delete_yes_command() {
        let warning = inside_worktree_delete_cancelled("feat/auth");
        assert!(warning.contains("ez worktree delete feat/auth --yes"));
    }
}
