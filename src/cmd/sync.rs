use anyhow::Result;

use crate::cmd::rebase_conflict;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

fn cleanup_candidate_branches(
    trunk: &str,
    managed_branches: &[String],
    local_branches: &[String],
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut branches = Vec::new();

    for branch in managed_branches {
        if branch != trunk && seen.insert(branch.clone()) {
            branches.push(branch.clone());
        }
    }

    for branch in local_branches {
        if branch != trunk && seen.insert(branch.clone()) {
            branches.push(branch.clone());
        }
    }

    branches
}

fn cleanup_reason(
    pr_info: Option<&github::PrInfo>,
    merged_via_git: bool,
    merged_via_diff: bool,
) -> Option<&'static str> {
    if let Some(pr) = pr_info {
        if pr.merged {
            Some("merged")
        } else if pr.state == "CLOSED" {
            Some("pr_closed")
        } else {
            None
        }
    } else if merged_via_git || merged_via_diff {
        Some("merged")
    } else {
        None
    }
}

fn inside_worktree_path(current_dir: &str, worktree_path: &str) -> bool {
    fn normalize(path: &str) -> std::path::PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| std::path::PathBuf::from(path))
    }

    let current = normalize(current_dir);
    let worktree = normalize(worktree_path);
    current == worktree || current.starts_with(&worktree)
}

pub fn run(dry_run: bool, autostash: bool, force: bool) -> Result<()> {
    let state = StackState::load()?;
    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }

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
                ui::info(&format!(
                    "Would check if PR for `{branch_name}` is merged or closed"
                ));
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
    let mut shell_cd_path: Option<String> = None;
    let mut cleaned_current_worktree = false;

    // Fetch from remote.
    ui::info(&format!("Fetching from `{}`...", state.remote));
    git::fetch(&state.remote)?;

    match git::reset_branch_to_latest_remote(
        &state.remote,
        &state.trunk,
        &original_branch,
        &original_root,
    ) {
        Ok(true) => ui::info(&format!(
            "Reset `{}` to latest `{}/{}`",
            state.trunk, state.remote, state.trunk
        )),
        Ok(false) => {}
        Err(e) => ui::warn(&format!("Could not update `{}` — {e}", state.trunk)),
    }

    // Build branch→worktree map for pruning merged branches.
    // Only include worktrees under .worktrees/ — the main worktree must never be removed.
    let worktree_map: std::collections::HashMap<String, String> = git::worktree_list()
        .unwrap_or_default()
        .into_iter()
        .filter(|wt| wt.path.contains("/.worktrees/"))
        .filter_map(|wt| wt.branch.map(|b| (b, wt.path)))
        .collect();
    let main_root = git::main_worktree_root().unwrap_or_else(|_| original_root.clone());
    let current_dir = std::env::current_dir()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Detect merged PRs and clean up.
    let managed_branches = {
        let mut order = state.topo_order();
        // Also include branches not in topo order (orphaned branches).
        for key in state.branches.keys() {
            if !order.contains(key) {
                order.push(key.clone());
            }
        }
        order
    };
    let local_branches = git::branch_list().unwrap_or_default();
    let cleanup_candidates =
        cleanup_candidate_branches(&state.trunk, &managed_branches, &local_branches);
    let mut cleaned = Vec::new();
    let has_any_prs = !cleanup_candidates.is_empty();
    let pr_statuses = if has_any_prs {
        let sp = ui::spinner("Checking PR states...");
        let statuses = github::get_all_pr_statuses();
        sp.finish_and_clear();
        statuses
    } else {
        std::collections::HashMap::new()
    };

    for branch_name in &cleanup_candidates {
        let meta = state.get_branch(branch_name).ok().cloned();
        let is_managed = meta.is_some();
        let pr_info = pr_statuses.get(branch_name.as_str());
        let pr_number = meta
            .as_ref()
            .and_then(|m| m.pr_number)
            .or(pr_info.map(|pr| pr.number));
        let parent = meta.as_ref().map(|m| m.parent.clone());

        // Auto-clean branches that no longer exist locally (deleted outside of ez).
        if !git::branch_exists(branch_name) {
            if !is_managed {
                continue;
            }
            let parent = parent.clone().expect("managed branch should have a parent");
            if git::branch_exists(&parent) {
                let _ = state.reparent_children_preserving_parent_head(branch_name, &parent)?;
            } else {
                // Parent also deleted — reparent children to trunk, but keep their old base SHA
                // so a later restack still knows what to rebase from.
                let trunk_name = state.trunk.clone();
                let _ = state.reparent_children_preserving_parent_head(branch_name, &trunk_name)?;
            }
            state.remove_branch(branch_name);
            ui::info(&format!("Cleaned up `{branch_name}` (deleted outside ez)"));
            cleaned.push(branch_name.clone());
            continue;
        }

        let merged_via_git = if pr_info.is_none() {
            git::is_ancestor(branch_name, &state.trunk)
        } else {
            false
        };

        // Diff-level check: only for branches WITHOUT a PR.
        // If a PR exists, the PR status is authoritative. An empty diff might just
        // mean someone cherry-picked the changes, not that the PR was merged.
        let merged_via_diff =
            if is_managed && pr_info.is_none() && !merged_via_git && pr_number.is_none() {
                let range = format!("{}...{}", state.trunk, branch_name);
                git::diff(&range, true, false)
                    .map(|stat| stat.trim().is_empty())
                    .unwrap_or(false)
            } else {
                false
            };

        let cleanup_reason = cleanup_reason(pr_info, merged_via_git, merged_via_diff);

        if cleanup_reason.is_none() {
            continue;
        }

        let cleanup_reason = cleanup_reason.unwrap_or("merged");

        // Remove worktree for this branch (if any) before mutating stack state.
        // If cleanup fails, keep the branch tracked so `ez sync --force` or `ez delete`
        // can recover it later.
        if let Some(wt_path) = worktree_map.get(branch_name.as_str()) {
            let is_current_worktree = inside_worktree_path(&current_dir, wt_path)
                || inside_worktree_path(&original_root, wt_path);
            if is_current_worktree && let Err(e) = std::env::set_current_dir(&main_root) {
                ui::warn(&format!(
                    "Could not move out of worktree `{wt_path}` before cleanup: {e}"
                ));
                ui::info(&format!(
                    "Kept `{branch_name}` tracked because cleanup did not complete"
                ));
                ui::receipt(&serde_json::json!({
                    "cmd": "sync",
                    "branch": branch_name,
                    "action": "cleanup_skipped",
                    "reason": "cwd_move_failed",
                    "parent": parent,
                    "worktree": wt_path,
                }));
                continue;
            }
            let result = if force {
                git::worktree_remove_force(wt_path)
            } else {
                git::worktree_remove(wt_path)
            };
            match result {
                Ok(()) => {
                    ui::info(&format!("Removed worktree at `{wt_path}`"));
                    if is_current_worktree {
                        shell_cd_path = Some(main_root.clone());
                        cleaned_current_worktree = true;
                    }
                }
                Err(e) => {
                    ui::warn(&format!(
                        "Could not remove worktree at `{wt_path}`: {e}\n  Hint: use `ez sync --force` to discard uncommitted changes"
                    ));
                    ui::info(&format!(
                        "Kept `{branch_name}` tracked because cleanup did not complete"
                    ));
                    ui::receipt(&serde_json::json!({
                        "cmd": "sync",
                        "branch": branch_name,
                        "action": "cleanup_skipped",
                        "reason": "worktree_remove_failed",
                        "parent": parent,
                        "worktree": wt_path,
                    }));
                    continue;
                }
            }
        }

        // If we're on the branch being deleted, switch to trunk first.
        if *branch_name == original_branch && !cleaned_current_worktree {
            if let Err(e) = git::checkout(&state.trunk) {
                ui::warn(&format!("Could not switch to trunk: {e}"));
                ui::info(&format!(
                    "Kept `{branch_name}` tracked because cleanup did not complete"
                ));
                ui::receipt(&serde_json::json!({
                    "cmd": "sync",
                    "branch": branch_name,
                    "action": "cleanup_skipped",
                    "reason": "checkout_failed",
                    "parent": parent,
                }));
                continue;
            }
        }

        // Delete local branch. If this fails, keep the branch tracked so cleanup can be retried.
        if git::branch_exists(branch_name)
            && let Err(e) = git::delete_branch(branch_name, true)
        {
            ui::warn(&format!(
                "Could not delete local branch `{branch_name}`: {e}"
            ));
            ui::info(&format!(
                "Kept `{branch_name}` tracked because cleanup did not complete"
            ));
            ui::receipt(&serde_json::json!({
                "cmd": "sync",
                "branch": branch_name,
                "action": "cleanup_skipped",
                "reason": "branch_delete_failed",
                "parent": parent,
            }));
            continue;
        }

        if is_managed {
            let parent_name = parent.clone().expect("managed branch should have a parent");
            let _ = state.reparent_children_preserving_parent_head(branch_name, &parent_name)?;

            state.remove_branch(branch_name);
        }

        let cleanup_label = if cleanup_reason == "pr_closed" {
            "PR closed"
        } else {
            "merged"
        };
        ui::info(&format!("Cleaned up `{branch_name}` ({cleanup_label})"));
        ui::receipt(&serde_json::json!({
            "cmd": "sync",
            "branch": branch_name,
            "action": "cleaned",
            "reason": cleanup_reason,
        }));
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
        if let Ok(Some(_wt_path)) = git::branch_checked_out_elsewhere(branch_name, &original_root) {
            ui::warn(&format!("Skipped `{branch_name}` (in worktree)"));
            continue;
        }

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

                // Post-restack: detect and auto-drop redundant commits.
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
                    "cmd": "sync",
                    "branch": branch_name,
                    "action": "restacked",
                    "parent": parent,
                    "before": &before_sha[..before_sha.len().min(7)],
                    "after": &after_sha[..after_sha.len().min(7)],
                    "redundant_commits": redundant_count,
                }));
            }
            git::RebaseOutcome::Conflict(conflict) => {
                // Save progress so the user can fix and continue.
                state.save()?;
                rebase_conflict::report("sync", branch_name, &parent, &conflict, "ez restack");
                anyhow::bail!(crate::error::EzError::RebaseConflict(branch_name.clone()));
            }
        }
    }

    state.save()?;

    // Return to original branch if it still exists.
    // If it was cleaned up (merged), fall back to trunk — but trunk might be in another worktree.
    if cleaned_current_worktree {
        ui::info(&format!(
            "Current worktree `{original_branch}` was cleaned up — switched context to repo root"
        ));
    } else if git::branch_exists(&original_branch) {
        let _ = git::checkout(&original_branch);
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

    if cleaned.is_empty() && restacked == 0 {
        ui::info("Everything is up to date");
    } else {
        ui::success(&format!(
            "Synced ({} cleaned, {} restacked)",
            cleaned.len(),
            restacked
        ));
    }

    // Prune stale worktree admin entries.
    let _ = git::worktree_prune();

    if let Some(path) = shell_cd_path {
        println!("{path}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_signatures_compile() {
        // Verifies the public API is correct at compile time.
        let f: fn(bool, bool, bool) -> anyhow::Result<()> = super::run;
        let _ = std::mem::size_of_val(&f);
    }

    #[test]
    fn cleanup_candidate_branches_includes_local_unmanaged_branches() {
        let managed = vec!["feat/a".to_string()];
        let local = vec![
            "main".to_string(),
            "feat/a".to_string(),
            "feat/b".to_string(),
            "feat/c".to_string(),
        ];

        assert_eq!(
            cleanup_candidate_branches("main", &managed, &local),
            vec![
                "feat/a".to_string(),
                "feat/b".to_string(),
                "feat/c".to_string()
            ]
        );
    }

    #[test]
    fn cleanup_reason_prefers_pr_state() {
        let closed = github::PrInfo {
            number: 42,
            url: String::new(),
            state: "CLOSED".to_string(),
            title: String::new(),
            base: "main".to_string(),
            is_draft: false,
            merged: false,
        };
        let merged = github::PrInfo {
            merged: true,
            ..closed.clone()
        };

        assert_eq!(cleanup_reason(Some(&merged), false, false), Some("merged"));
        assert_eq!(
            cleanup_reason(Some(&closed), false, false),
            Some("pr_closed")
        );
        assert_eq!(
            cleanup_reason(
                Some(&github::PrInfo {
                    state: "OPEN".to_string(),
                    ..closed
                }),
                true,
                true
            ),
            None
        );
        assert_eq!(cleanup_reason(None, true, false), Some("merged"));
        assert_eq!(cleanup_reason(None, false, true), Some("merged"));
        assert_eq!(cleanup_reason(None, false, false), None);
    }

    #[test]
    fn inside_worktree_path_matches_nested_paths_only() {
        assert!(inside_worktree_path(
            "/repo/.worktrees/feat-a",
            "/repo/.worktrees/feat-a"
        ));
        assert!(inside_worktree_path(
            "/repo/.worktrees/feat-a/src/components",
            "/repo/.worktrees/feat-a"
        ));
        assert!(!inside_worktree_path(
            "/repo/.worktrees/feat-ab",
            "/repo/.worktrees/feat-a"
        ));
    }
}
