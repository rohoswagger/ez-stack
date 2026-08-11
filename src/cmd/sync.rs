use anyhow::Result;

use crate::cmd::native_stack::{self, SkippedNativeStackComponent};
use crate::cmd::restack;
use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

fn cleanup_candidate_branches(trunk: &str, managed_branches: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut branches = Vec::new();

    for branch in managed_branches {
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

fn skipped_native_stack_receipt(component: &SkippedNativeStackComponent) -> serde_json::Value {
    serde_json::json!({
        "cmd": "sync",
        "branches": component.branches,
        "native_stack_action": "skipped",
        "native_stack_reason": component.reason,
        "native_stack_root": component.root,
    })
}

/// Worktrees ez created that no longer have a stack entry pointing at them.
///
/// A branch normally leaves the stack and its worktree in the same step. When something fails in
/// between — a merge that drops the entry and then hits a restack error before cleanup — the
/// worktree survives with no metadata left to find it by, so neither the cleanup loop above (which
/// only walks `state.branches`) nor `ez log` will ever mention it again.
///
/// Scope is deliberately narrow: only worktrees under *this repo's* `.worktrees/`, which ez
/// creates and owns. A branch checked out in an external worktree, or a plain local branch with
/// no worktree at all, is somebody else's and is left alone no matter how merged it looks.
fn orphaned_ez_worktrees(
    state: &StackState,
    main_root: &str,
    worktree_map: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    // Anchored to the repo root, not a bare `contains`: a checkout that itself lives under some
    // `.worktrees/` directory would otherwise make every external worktree look ez-owned.
    let owned_prefix = format!("{}/.worktrees/", main_root.trim_end_matches('/'));
    let mut orphans: Vec<(String, String)> = worktree_map
        .iter()
        .filter(|(branch, path)| {
            !state.branches.contains_key(*branch)
                && !state.is_trunk(branch)
                && path.starts_with(&owned_prefix)
        })
        .map(|(branch, path)| (branch.clone(), path.clone()))
        .collect();
    orphans.sort();
    orphans
}

/// Delete the worktree and local branch for orphans whose work already landed on trunk.
///
/// Returns the branches it cleaned. Anything that is not provably merged is left untouched and
/// reported, because an orphan with unmerged commits is lost work, not litter. The remote branch
/// is deliberately left alone: sync's tracked cleanup does not delete remote refs either, and the
/// evidence here is about the *local* tip, which says nothing about commits pushed from elsewhere.
fn prune_orphaned_ez_worktrees(
    state: &StackState,
    main_root: &str,
    current_dir: &str,
    original_root: &str,
    fetch_remote: &str,
    force: bool,
    shell_cd_path: &mut Option<String>,
) -> Vec<String> {
    // Read the worktree list from git rather than reusing the map captured before the cleanup
    // pass. Branches cleaned above are out of `state.branches` and have had their worktrees
    // removed, so a stale map entry would match every rule below and be re-reported as an orphan.
    let worktree_map: std::collections::HashMap<String, String> = git::worktree_list()
        .unwrap_or_default()
        .into_iter()
        .filter(|wt| wt.path != main_root)
        .filter_map(|wt| wt.branch.map(|branch| (branch, wt.path)))
        .collect();

    let orphans: Vec<(String, String)> = orphaned_ez_worktrees(state, main_root, &worktree_map)
        .into_iter()
        .filter(|(branch, _)| git::branch_exists(branch))
        .collect();
    if orphans.is_empty() {
        return Vec::new();
    }

    let branch_refs: Vec<&str> = orphans.iter().map(|(branch, _)| branch.as_str()).collect();
    let pr_statuses =
        github::get_pr_statuses_for(fetch_remote, state.repo.as_deref(), &branch_refs);

    let mut cleaned = Vec::new();
    for (branch, path) in &orphans {
        let pr_info = pr_statuses.get(branch.as_str());

        // Git has to agree before anything is deleted. An orphan has no recorded PR number, so
        // the only way to find its PR is `headRefName` — which returns the most recent PR that
        // ever used that branch *name*. After a name is recycled, or in a fork workflow where
        // several contributors use the same name, that is somebody else's merged PR, and trusting
        // its `merged` flag alone would delete live work. The tracked cleanup path sidesteps this
        // by looking PRs up by number; here git is the corroboration instead.
        //
        // Tip already in trunk covers a normal merge; an empty diff against trunk covers a squash
        // merge, where the commits are rewritten and ancestry no longer holds.
        let tip_in_trunk = git::is_ancestor(branch, &state.trunk);
        let content_in_trunk = || {
            git::diff(&format!("{}...{}", state.trunk, branch), true, false)
                .map(|stat| stat.trim().is_empty())
                .unwrap_or(false)
        };
        let merged = tip_in_trunk || (pr_info.is_some_and(|pr| pr.merged) && content_in_trunk());
        if !merged {
            ui::warn(&format!(
                "`{branch}` has a worktree at `{path}` but is not tracked by ez and is not merged"
            ));
            ui::hint(&format!(
                "Run `ez track {branch}` to manage it again, or `ez worktree delete {branch} --force` to discard it"
            ));
            ui::receipt(&serde_json::json!({
                "cmd": "sync",
                "branch": branch,
                "action": "cleanup_skipped",
                "reason": "orphaned_worktree_unmerged",
                "worktree": path,
            }));
            continue;
        }

        // Removing the worktree we are standing in would leave the process — and the rest of this
        // sync, which still has restacks and a `state.save()` to do — in a deleted directory.
        let is_current_worktree =
            inside_worktree_path(current_dir, path) || inside_worktree_path(original_root, path);
        if is_current_worktree && let Err(e) = std::env::set_current_dir(main_root) {
            ui::warn(&format!(
                "Could not move out of orphaned worktree `{path}` before cleanup: {e}"
            ));
            ui::receipt(&serde_json::json!({
                "cmd": "sync",
                "branch": branch,
                "action": "cleanup_skipped",
                "reason": "orphaned_worktree_cwd_move_failed",
                "worktree": path,
            }));
            continue;
        }

        let removed = if force {
            git::worktree_remove_force(path)
        } else {
            git::worktree_remove(path)
        };
        if let Err(e) = removed {
            ui::warn(&format!(
                "Could not remove orphaned worktree at `{path}`: {e}"
            ));
            ui::hint("Use `ez sync --force` to discard uncommitted changes");
            ui::receipt(&serde_json::json!({
                "cmd": "sync",
                "branch": branch,
                "action": "cleanup_skipped",
                "reason": "orphaned_worktree_remove_failed",
                "worktree": path,
            }));
            continue;
        }

        if is_current_worktree {
            *shell_cd_path = Some(main_root.to_string());
        }

        let _ = git::delete_branch(branch, true);
        ui::info(&format!(
            "Cleaned up orphaned worktree for `{branch}` (merged, no longer tracked)"
        ));
        ui::receipt(&serde_json::json!({
            "cmd": "sync",
            "branch": branch,
            "action": "cleaned",
            "reason": "orphaned_merged_worktree",
            "worktree": path,
        }));
        cleaned.push(branch.clone());
    }

    cleaned
}

fn reconcile_native_stacks(state: &StackState, repair_native_stack: bool) -> Result<()> {
    if state.is_fork_workflow() {
        let outcome = github::NativeStackOutcome::NotApplicable {
            reason: "GitHub native stacks require pull requests in one repository; fork/cross-repository workflows keep the ez stack local".to_string(),
        };
        ui::receipt(&crate::cmd::native_stack::receipt_value(
            "sync",
            &[],
            &[],
            &outcome,
        ));
        crate::cmd::native_stack::report_outcome(&outcome);
        return Ok(());
    }

    let plan = native_stack::native_stack_plan(state);

    for component in &plan.skipped {
        ui::warn(&format!(
            "GitHub native stack skipped for `{}`: ez's {} branch component cannot be represented as one linear GitHub stack",
            component.root,
            component.branches.len()
        ));
        ui::receipt(&skipped_native_stack_receipt(component));
    }

    let chains = native_stack::linkable_chains(&plan);
    if chains.is_empty() && plan.skipped.is_empty() {
        ui::receipt(&crate::cmd::native_stack::receipt_value(
            "sync",
            &[],
            &[],
            &github::NativeStackOutcome::NotNeeded,
        ));
        return Ok(());
    }

    let mut repair_error = None;
    for chain in chains {
        let outcome = if repair_native_stack {
            github::repair_native_stack_exact(
                &chain.pr_numbers,
                "ez sync --repair-native-stack",
                state.repo.as_deref(),
            )
        } else {
            github::reconcile_native_stack_exact(
                &chain.pr_numbers,
                "ez sync",
                state.repo.as_deref(),
            )
        };

        match outcome {
            Ok(outcome) => {
                crate::cmd::native_stack::report_outcome(&outcome);
                ui::receipt(&crate::cmd::native_stack::receipt_value(
                    "sync",
                    &chain.branches,
                    &chain.pr_numbers,
                    &outcome,
                ));
            }
            Err(err) => {
                ui::warn(&format!("GitHub native stack update skipped: {err}"));
                ui::receipt(&crate::cmd::native_stack::error_receipt_value(
                    "sync",
                    &chain.branches,
                    &chain.pr_numbers,
                    &err.to_string(),
                ));
                if repair_native_stack {
                    repair_error = Some(err);
                    break;
                }
            }
        }
    }

    if let Some(err) = repair_error {
        return Err(err);
    }

    Ok(())
}

fn update_reparented_pr_bases(
    state: &StackState,
    reparented_branches: &std::collections::BTreeSet<String>,
) -> bool {
    let mut all_updated = true;
    for branch in reparented_branches {
        let Some(meta) = state.branches.get(branch) else {
            continue;
        };
        let Some(pr_number) = meta.pr_number else {
            continue;
        };

        match github::update_pr_base(pr_number, &meta.parent, state.repo.as_deref()) {
            Ok(()) => {
                ui::info(&format!(
                    "Updated PR #{pr_number} base for `{branch}` to `{}`",
                    meta.parent
                ));
                ui::receipt(&serde_json::json!({
                    "cmd": "sync",
                    "branch": branch,
                    "action": "pr_base_updated",
                    "pr_number": pr_number,
                    "base": meta.parent,
                }));
            }
            Err(err) => {
                all_updated = false;
                ui::warn(&format!(
                    "Failed to update PR #{pr_number} base for `{branch}` to `{}`: {err}",
                    meta.parent
                ));
                ui::receipt(&serde_json::json!({
                    "cmd": "sync",
                    "branch": branch,
                    "action": "pr_base_update_error",
                    "pr_number": pr_number,
                    "base": meta.parent,
                    "error": err.to_string(),
                }));
            }
        }
    }
    all_updated
}

struct SyncFinalize<'a> {
    original_branch: &'a str,
    shell_cd_path: Option<String>,
    cleaned_current_worktree: bool,
    cleaned: &'a [String],
    restacked: usize,
    skipped: usize,
    reparented_branches: &'a std::collections::BTreeSet<String>,
    reconcile_native: bool,
    repair_native_stack: bool,
}

fn finalize_sync_after_mutations(state: &StackState, finalize: SyncFinalize<'_>) -> Result<()> {
    state.save()?;

    // Return to original branch if it still exists.
    // If it was cleaned up (merged), fall back to trunk — but trunk might be in another worktree.
    if finalize.cleaned_current_worktree {
        ui::info(&format!(
            "Current worktree `{}` was cleaned up — switched context to repo root",
            finalize.original_branch
        ));
    } else if git::branch_exists(finalize.original_branch) {
        let _ = git::checkout(finalize.original_branch);
    } else {
        match git::checkout(&state.trunk) {
            Ok(()) => ui::info(&format!(
                "Previous branch `{}` was cleaned up — switched to `{}`",
                finalize.original_branch, state.trunk
            )),
            Err(_) => ui::warn(&format!(
                "Previous branch `{}` was cleaned up. Switch to another branch manually \
                 (trunk may be checked out in another worktree).",
                finalize.original_branch
            )),
        }
    }

    if finalize.skipped > 0 {
        ui::info(&format!(
            "Synced ({} cleaned, {} restacked, {} skipped)",
            finalize.cleaned.len(),
            finalize.restacked,
            finalize.skipped
        ));
    } else if finalize.cleaned.is_empty() && finalize.restacked == 0 {
        ui::info("Everything is up to date");
    } else {
        ui::success(&format!(
            "Synced ({} cleaned, {} restacked)",
            finalize.cleaned.len(),
            finalize.restacked
        ));
    }

    // Prune stale worktree admin entries.
    let _ = git::worktree_prune();

    if let Some(path) = finalize.shell_cd_path {
        println!("{path}");
    }

    if update_reparented_pr_bases(state, finalize.reparented_branches) && finalize.reconcile_native
    {
        reconcile_native_stacks(state, finalize.repair_native_stack)?;
    } else if finalize.reconcile_native {
        ui::warn(
            "GitHub native stack update skipped because one or more PR bases could not be updated",
        );
        ui::receipt(&serde_json::json!({
            "cmd": "sync",
            "native_stack_action": "skipped",
            "native_stack_reason": "pr_base_update_failed",
        }));
    }

    Ok(())
}

pub fn run(dry_run: bool, autostash: bool, force: bool, repair_native_stack: bool) -> Result<()> {
    let state = StackState::load()?;
    let _lease_guard = if dry_run {
        None
    } else {
        Some(crate::worktree_lease::LeaseMutationGuard::acquire(
            "sync worktree stack",
        )?)
    };
    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }

    if dry_run {
        ui::header("Sync preview (--dry-run, no changes will be made)");
        ui::info(&format!("Would fetch from `{}`", state.fetch_remote()));
        ui::info(&format!(
            "Would update `{}` to latest remote (no checkout needed)",
            state.trunk
        ));

        let main_root = git::main_worktree_root().unwrap_or_else(|_| {
            git::repo_root().ok().unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_default()
                    .display()
                    .to_string()
            })
        });
        let dry_worktree_map: std::collections::HashMap<String, String> = git::worktree_list()
            .unwrap_or_default()
            .into_iter()
            .filter(|wt| wt.path != main_root)
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

        if state.is_fork_workflow() {
            ui::info("GitHub native stacks are not applicable to fork/cross-repository workflows");
        } else {
            let native_plan = native_stack::native_stack_plan(&state);
            for chain in native_stack::linkable_chains(&native_plan) {
                if repair_native_stack {
                    ui::info(&format!(
                        "Would repair GitHub native stack for PRs {:?} ({})",
                        chain.pr_numbers,
                        chain.branches.join(" -> ")
                    ));
                } else {
                    ui::info(&format!(
                        "Would reconcile GitHub native stack for PRs {:?} ({})",
                        chain.pr_numbers,
                        chain.branches.join(" -> ")
                    ));
                }
            }
            for component in &native_plan.skipped {
                ui::info(&format!(
                    "Would skip GitHub native stack for `{}` ({})",
                    component.root, component.reason
                ));
            }
        }

        if repair_native_stack {
            ui::hint(
                "Run `ez sync --repair-native-stack` (without --dry-run) to apply these changes",
            );
        } else {
            ui::hint("Run `ez sync` (without --dry-run) to apply these changes");
        }
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

    let result = run_sync_inner(force, repair_native_stack);

    if stashed {
        if let Err(e) = git::stash_pop() {
            ui::warn(&format!("Failed to pop autostash: {e}"));
        } else {
            ui::info("Restored stashed changes");
        }
    }

    result
}

fn run_sync_inner(force: bool, repair_native_stack: bool) -> Result<()> {
    let mut state = StackState::load()?;
    let original_branch = git::current_branch()?;
    let original_root = git::repo_root()?;
    let mut shell_cd_path: Option<String> = None;
    let mut cleaned_current_worktree = false;
    let mut reparented_branches = std::collections::BTreeSet::new();

    // Fetch from remote.
    let fetch_remote = state.fetch_remote().to_string();
    ui::info(&format!("Fetching from `{fetch_remote}`..."));
    git::fetch(&fetch_remote)?;

    match git::reset_branch_to_latest_remote(
        &fetch_remote,
        &state.trunk,
        &original_branch,
        &original_root,
    ) {
        Ok(true) => ui::info(&format!(
            "Reset `{}` to latest `{}/{}`",
            state.trunk, fetch_remote, state.trunk
        )),
        Ok(false) => {}
        Err(e) => {
            return Err(EzError::UserMessage(format!(
                "Could not update trunk `{}` from `{}/{}` without overwriting local changes.\n  → commit or stash changes in the `{}` worktree, or run `ez sync --autostash` from that worktree.\n\nUnderlying error: {e}",
                state.trunk, fetch_remote, state.trunk, state.trunk
            ))
            .into());
        }
    }

    let main_root = git::main_worktree_root().unwrap_or_else(|_| original_root.clone());

    // Build branch→worktree map for pruning merged branches.
    // Trust git's worktree registry and exclude only the actual main worktree root.
    let worktree_map: std::collections::HashMap<String, String> = git::worktree_list()
        .unwrap_or_default()
        .into_iter()
        .filter(|wt| wt.path != main_root)
        .filter_map(|wt| wt.branch.map(|b| (b, wt.path)))
        .collect();
    let current_dir = std::env::current_dir()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Detect merged PRs and clean up.
    let managed_branches = {
        let mut order: Vec<String> = state
            .topo_order()
            .into_iter()
            .filter(|branch| state.branches.contains_key(branch))
            .collect();
        // Also include branches not in topo order (orphaned branches).
        for key in state.branches.keys() {
            if !order.contains(key) {
                order.push(key.clone());
            }
        }
        order
    };
    let cleanup_candidates = cleanup_candidate_branches(&state.trunk, &managed_branches);
    let mut cleaned = Vec::new();
    let has_any_prs = !cleanup_candidates.is_empty();
    let pr_statuses = if has_any_prs {
        let sp = ui::spinner("Checking PR states...");
        let statuses = if state.is_fork_workflow() {
            let numbered_branches: Vec<(&str, u64)> = cleanup_candidates
                .iter()
                .filter_map(|branch| {
                    state
                        .branches
                        .get(branch)
                        .and_then(|meta| meta.pr_number.map(|number| (branch.as_str(), number)))
                })
                .collect();
            github::get_pr_statuses_by_number(
                &fetch_remote,
                state.repo.as_deref(),
                &numbered_branches,
            )
        } else {
            let branch_refs: Vec<&str> = cleanup_candidates.iter().map(String::as_str).collect();
            github::get_pr_statuses_for(&fetch_remote, state.repo.as_deref(), &branch_refs)
        };
        sp.finish_and_clear();
        statuses
    } else {
        std::collections::HashMap::new()
    };

    for branch_name in &cleanup_candidates {
        let meta = state.get_branch(branch_name)?.clone();
        let pr_info = pr_statuses.get(branch_name.as_str());
        let pr_number = meta.pr_number.or(pr_info.map(|pr| pr.number));
        let parent = meta.parent;

        // Auto-clean branches that no longer exist locally (deleted outside of ez).
        if !git::branch_exists(branch_name) {
            if git::branch_exists(&parent) {
                reparented_branches
                    .extend(state.reparent_children_preserving_parent_head(branch_name, &parent)?);
            } else {
                // Parent also deleted — reparent children to trunk, but keep their old base SHA
                // so a later restack still knows what to rebase from.
                let trunk_name = state.trunk.clone();
                reparented_branches.extend(
                    state.reparent_children_preserving_parent_head(branch_name, &trunk_name)?,
                );
            }
            state.remove_branch(branch_name);
            ui::info(&format!("Cleaned up `{branch_name}` (deleted outside ez)"));
            ui::receipt(&serde_json::json!({
                "cmd": "sync",
                "branch": branch_name,
                "action": "cleaned",
                "reason": "deleted_outside_ez",
                "parent": parent,
            }));
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
        let merged_via_diff = if pr_info.is_none() && !merged_via_git && pr_number.is_none() {
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
            if let Err(error) =
                crate::cmd::worktree::guard_registered_worktree(branch_name, wt_path, "clean up")
            {
                ui::warn(&error.to_string());
                ui::info(&format!(
                    "Kept `{branch_name}` tracked because its worktree is protected"
                ));
                ui::receipt(&serde_json::json!({
                    "cmd": "sync",
                    "branch": branch_name,
                    "action": "cleanup_skipped",
                    "reason": "worktree_locked",
                    "parent": parent,
                    "worktree": wt_path,
                }));
                continue;
            }
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

        reparented_branches
            .extend(state.reparent_children_preserving_parent_head(branch_name, &parent)?);
        state.remove_branch(branch_name);

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

    // Safety net for worktrees whose stack entry disappeared without them — see
    // `prune_orphaned_ez_worktrees`. It re-reads the worktree list itself, because the map above
    // predates the cleanup that just ran.
    cleaned.extend(prune_orphaned_ez_worktrees(
        &state,
        &main_root,
        &current_dir,
        &original_root,
        &fetch_remote,
        force,
        &mut shell_cd_path,
    ));

    let order = state.topo_order();
    let candidates = crate::cmd::preflight::restack_candidates(&state, &order);
    let preflight_error = crate::cmd::preflight::run("sync", force, &candidates).err();
    if let Some(err) = preflight_error {
        finalize_sync_after_mutations(
            &state,
            SyncFinalize {
                original_branch: &original_branch,
                shell_cd_path,
                cleaned_current_worktree,
                cleaned: &cleaned,
                restacked: 0,
                skipped: candidates.len(),
                reparented_branches: &reparented_branches,
                reconcile_native: false,
                repair_native_stack,
            },
        )?;
        return Err(err);
    }

    // Restack remaining branches. Each branch is attempted independently: one branch that
    // conflicts or refuses to rebase is reported and skipped, and the rest of the stack still
    // gets synced. Failures are surfaced at the very end, after state is saved and the original
    // branch restored, so a stuck branch never leaves the repo mid-rebase.
    let report = restack::restack_branches_with_options(
        &mut state,
        &order,
        &original_root,
        "sync",
        restack::RestackOptions { force },
    );
    let restacked = report.restacked;

    finalize_sync_after_mutations(
        &state,
        SyncFinalize {
            original_branch: &original_branch,
            shell_cd_path,
            cleaned_current_worktree,
            cleaned: &cleaned,
            restacked,
            skipped: report.failures.len(),
            reparented_branches: &reparented_branches,
            reconcile_native: report.is_clean(),
            repair_native_stack,
        },
    )?;

    if !report.is_clean() {
        anyhow::bail!(restack::incomplete_error("sync", &report));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_signatures_compile() {
        // Verifies the public API is correct at compile time.
        let f: fn(bool, bool, bool, bool) -> anyhow::Result<()> = super::run;
        let _ = std::mem::size_of_val(&f);
    }

    #[test]
    fn orphaned_ez_worktrees_only_claims_untracked_branches_in_ez_owned_worktrees() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/tracked", "main", "aaa", None, None);

        let worktrees = std::collections::HashMap::from([
            // Orphan: ez created the worktree, but nothing in the stack points at it anymore.
            (
                "feat/orphan".to_string(),
                "/repo/.worktrees/feat-orphan".to_string(),
            ),
            // Still tracked — the normal cleanup loop owns this one.
            (
                "feat/tracked".to_string(),
                "/repo/.worktrees/feat-tracked".to_string(),
            ),
            // Someone else's worktree (Superconductor, a manual `git worktree add`). Not ours.
            (
                "feat/external".to_string(),
                "/elsewhere/feat-external".to_string(),
            ),
            // Trunk checked out in a second worktree is never litter.
            ("main".to_string(), "/repo/.worktrees/main".to_string()),
        ]);

        let orphans = orphaned_ez_worktrees(&state, "/repo", &worktrees);

        assert_eq!(
            orphans,
            vec![(
                "feat/orphan".to_string(),
                "/repo/.worktrees/feat-orphan".to_string()
            )]
        );
    }

    #[test]
    fn orphaned_ez_worktrees_anchors_ownership_to_this_repo_root() {
        let state = StackState::new("main".to_string());

        // A checkout that itself lives under a `.worktrees/` directory. A bare substring test
        // would claim every sibling worktree in that directory as ez's to delete.
        let main_root = "/home/dev/.worktrees/project";
        let worktrees = std::collections::HashMap::from([
            (
                "feat/ours".to_string(),
                "/home/dev/.worktrees/project/.worktrees/feat-ours".to_string(),
            ),
            (
                "feat/theirs".to_string(),
                "/home/dev/.worktrees/some-other-tool".to_string(),
            ),
        ]);

        let orphans = orphaned_ez_worktrees(&state, main_root, &worktrees);

        assert_eq!(
            orphans,
            vec![(
                "feat/ours".to_string(),
                "/home/dev/.worktrees/project/.worktrees/feat-ours".to_string()
            )],
            "only worktrees under this repo's own .worktrees/ are ez-owned"
        );
    }

    #[test]
    fn prune_orphaned_ez_worktrees_ignores_branches_the_tracked_pass_already_cleaned() {
        let _guard = crate::test_support::take_env_lock();
        let repo = crate::test_support::init_git_repo("sync-orphan-after-tracked-cleanup");
        let _cwd = crate::test_support::CwdGuard::enter(&repo);

        // Model the state right after the tracked cleanup loop: the branch is gone from git and
        // from `state.branches`, and its worktree has already been removed. Reusing the worktree
        // map captured before that loop would re-report it here.
        let state = StackState::new("main".to_string());
        let main_root = git::repo_root().expect("repo root");
        let mut shell_cd_path = None;

        let cleaned = prune_orphaned_ez_worktrees(
            &state,
            &main_root,
            &main_root,
            &main_root,
            "origin",
            false,
            &mut shell_cd_path,
        );

        assert!(
            cleaned.is_empty(),
            "a branch already cleaned by the tracked pass must not be revisited as an orphan"
        );
    }

    #[test]
    fn prune_orphaned_ez_worktrees_keeps_an_orphan_git_cannot_confirm_is_merged() {
        let _guard = crate::test_support::take_env_lock();
        let repo = crate::test_support::init_git_repo("sync-orphan-unmerged-kept");
        let _cwd = crate::test_support::CwdGuard::enter(&repo);

        // An untracked branch in an ez-owned worktree carrying work that is NOT on trunk. This is
        // the shape a recycled branch name takes: a `headRefName` PR lookup can hand back an old
        // merged PR for the same name, so git ancestry is what has to decide.
        git::create_branch_at("feat/recycled", "main").expect("branch");
        git::checkout("feat/recycled").expect("checkout");
        crate::test_support::write_file(&repo, "unmerged.txt", "work\n");
        crate::test_support::run_cmd(&repo, "git", &["add", "unmerged.txt"]);
        crate::test_support::run_cmd(&repo, "git", &["commit", "-m", "unmerged work"]);
        git::checkout("main").expect("back to main");

        let main_root = git::repo_root().expect("repo root");
        let worktree = format!("{main_root}/.worktrees/feat-recycled");
        git::worktree_add(&worktree, "feat/recycled").expect("worktree add");

        let state = StackState::new("main".to_string());
        let mut shell_cd_path = None;

        let cleaned = prune_orphaned_ez_worktrees(
            &state,
            &main_root,
            &main_root,
            &main_root,
            "origin",
            false,
            &mut shell_cd_path,
        );

        assert!(
            cleaned.is_empty(),
            "an orphan whose commits are not in trunk must be kept, not deleted"
        );
        assert!(
            git::branch_exists("feat/recycled"),
            "the branch must survive"
        );
        assert!(
            std::path::Path::new(&worktree).exists(),
            "the worktree must survive"
        );
    }

    #[test]
    fn cleanup_candidate_branches_excludes_local_unmanaged_branches() {
        let managed = vec![
            "main".to_string(),
            "feat/a".to_string(),
            "feat/a".to_string(),
        ];

        assert_eq!(
            cleanup_candidate_branches("main", &managed),
            vec!["feat/a".to_string()]
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

    #[test]
    fn skipped_native_stack_receipt_explains_branching_component() {
        let value = skipped_native_stack_receipt(&SkippedNativeStackComponent {
            root: "feat/a".to_string(),
            branches: vec![
                "feat/a".to_string(),
                "feat/b".to_string(),
                "feat/c".to_string(),
            ],
            pr_numbers: vec![101, 102, 103],
            reason: "branching_component",
        });

        assert_eq!(value["cmd"], "sync");
        assert_eq!(value["native_stack_action"], "skipped");
        assert_eq!(value["native_stack_reason"], "branching_component");
        assert_eq!(value["native_stack_root"], "feat/a");
    }
}
