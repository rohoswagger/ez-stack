use anyhow::Result;
use dialoguer::Select;
use std::collections::HashMap;

use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

fn branch_worktree_map(
    worktrees: impl IntoIterator<Item = git::WorktreeInfo>,
) -> HashMap<String, String> {
    worktrees
        .into_iter()
        .filter_map(|wt| wt.branch.map(|b| (b, wt.path)))
        .collect()
}

/// Build a map of branch name → worktree path for branches in worktrees.
pub(crate) fn worktree_map() -> HashMap<String, String> {
    branch_worktree_map(git::worktree_list().unwrap_or_default())
}

fn worktree_edit_hint(wt_path: &str) -> String {
    if wt_path.contains("/.worktrees/") {
        format!(
            "Edit files under `{wt_path}`. This branch lives in a linked worktree, not the main repo checkout."
        )
    } else {
        format!("Edit files under `{wt_path}`.")
    }
}

pub(crate) fn stale_switch_target_warning(
    state: &StackState,
    target: &str,
) -> Result<Option<String>> {
    if state.is_trunk(target) || !state.is_managed(target) {
        return Ok(None);
    }

    let meta = state.get_branch(target)?;
    let parent = meta.parent.clone();

    if state.is_trunk(&parent) {
        // Best-effort refresh so the warning compares against latest trunk, not a stale local ref.
        if let (Ok(current_branch), Ok(current_root)) = (git::current_branch(), git::repo_root()) {
            let _ = git::fetch_branch(&state.remote, &state.trunk);
            let _ = git::update_branch_to_latest_remote(
                &state.remote,
                &state.trunk,
                &current_branch,
                &current_root,
            );
        }
    }

    if git::is_ancestor(&parent, target) {
        return Ok(None);
    }

    Ok(Some(format!(
        "branch `{target}` is not restacked on `{parent}`"
    )))
}

/// Switch to a branch. If it's in a worktree, print the path to stdout for cd.
pub(crate) fn switch_to(
    state: &StackState,
    target: &str,
    wt_map: &HashMap<String, String>,
) -> Result<()> {
    let stale_warning = stale_switch_target_warning(state, target)?;

    if let Some(wt_path) = wt_map.get(target) {
        // Branch is in a worktree — print path to stdout for shell wrapper to cd.
        ui::success(&format!("Switching to `{target}` in worktree `{wt_path}`"));
        ui::hint(&worktree_edit_hint(wt_path));
        println!("{wt_path}");
    } else if state.is_managed(target) {
        // Managed branch without a worktree — create one and cd into it.
        let wt_path = git::worktree_path(target)?;
        git::worktree_add(&wt_path, target)?;
        ui::success(&format!("Created worktree for `{target}` → {wt_path}"));
        ui::hint(&worktree_edit_hint(&wt_path));
        println!("{wt_path}");
    } else {
        // Trunk or unmanaged — plain checkout.
        git::checkout(target)?;
        ui::success(&format!("Switched to `{target}`"));
    }

    if let Some(warning) = stale_warning {
        ui::warn(&warning);
        ui::hint("Run `ez restack`");
    }

    Ok(())
}

pub fn run(name: Option<&str>) -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;
    let wt_map = worktree_map();

    // Direct checkout by name or PR number.
    if let Some(arg) = name {
        let target = if let Ok(pr_num) = arg.parse::<u64>() {
            state
                .branches
                .values()
                .find(|m| m.pr_number == Some(pr_num))
                .map(|m| m.name.clone())
                .ok_or_else(|| {
                    EzError::UserMessage(format!(
                        "No branch found with PR #{pr_num}\n  → Run `ez branch` to see all branches"
                    ))
                })?
        } else {
            if !state.is_trunk(arg) && !state.is_managed(arg) {
                anyhow::bail!(EzError::BranchNotInStack(arg.to_string()));
            }
            arg.to_string()
        };

        if target == current {
            ui::info(&format!("Already on `{target}`"));
            return Ok(());
        }

        switch_to(&state, &target, &wt_map)?;
        return Ok(());
    }

    // Interactive selector (existing code below, unchanged).

    // Collect all managed branches, sorted
    let mut branches: Vec<String> = state.branches.keys().cloned().collect();
    branches.sort();

    // Add trunk at the beginning
    branches.insert(0, state.trunk.clone());

    // Build display items with PR badges
    let display_items: Vec<String> = branches
        .iter()
        .map(|name| {
            let is_current = name == &current;
            let branch_text = ui::branch_display(name, is_current);

            if let Some(meta) = state.branches.get(name)
                && let Some(pr_number) = meta.pr_number
            {
                if let Ok(Some(pr)) = github::get_pr_status(name) {
                    return format!(
                        "{} {}",
                        branch_text,
                        ui::pr_badge(pr.number, &pr.state, pr.is_draft)
                    );
                }
                return format!("{} {}", branch_text, ui::pr_badge(pr_number, "OPEN", false));
            }

            branch_text
        })
        .collect();

    // Find the index of the current branch for default selection
    let default_idx = branches.iter().position(|b| b == &current).unwrap_or(0);

    let selection = Select::new()
        .with_prompt("Select branch")
        .items(&display_items)
        .default(default_idx)
        .interact()?;

    let selected = &branches[selection];

    if selected == &current {
        ui::info(&format!("Already on `{selected}`"));
        return Ok(());
    }

    switch_to(&state, selected, &wt_map)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::{BranchMeta, StackState};
    use crate::test_support::{CwdGuard, init_git_repo, take_env_lock, write_file};
    use std::collections::HashMap;

    fn state_with_pr() -> StackState {
        let mut branches = HashMap::new();
        branches.insert(
            "feat/x".to_string(),
            BranchMeta {
                name: "feat/x".to_string(),
                parent: "main".to_string(),
                parent_head: "abc".to_string(),
                pr_number: Some(99),
                scope: None,
                scope_mode: None,
            },
        );
        StackState {
            trunk: "main".to_string(),
            remote: "origin".to_string(),
            branches,
        }
    }

    #[test]
    fn test_find_branch_by_pr_number() {
        let state = state_with_pr();
        let found = state
            .branches
            .values()
            .find(|m| m.pr_number == Some(99))
            .map(|m| m.name.clone());
        assert_eq!(found, Some("feat/x".to_string()));
    }

    #[test]
    fn test_arg_parses_as_pr_number() {
        assert!("99".parse::<u64>().is_ok());
        assert!("feat/x".parse::<u64>().is_err());
        assert!("0".parse::<u64>().is_ok());
    }

    #[test]
    fn stale_switch_target_warning_reports_stale_branch() {
        let _guard = take_env_lock();
        let repo = init_git_repo("checkout-restack-guard");
        let _cwd = CwdGuard::enter(&repo);

        let parent_head = git::rev_parse("main").expect("main head");
        git::create_branch_at("feat/test", "main").expect("create feature");

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/test", "main", &parent_head, None, None);
        state.save().expect("save state");

        write_file(&repo, "tracked.txt", "updated on main\n");
        git::add_paths(&["tracked.txt".to_string()]).expect("stage main");
        git::commit("advance main").expect("commit main");

        let warning = stale_switch_target_warning(&state, "feat/test")
            .expect("warning resolution should succeed")
            .expect("stale branch should warn");
        assert!(warning.contains("not restacked on `main`"));
    }

    #[test]
    fn stale_switch_target_warning_skips_fresh_branch() {
        let _guard = take_env_lock();
        let repo = init_git_repo("checkout-restack-guard-clean");
        let _cwd = CwdGuard::enter(&repo);

        let parent_head = git::rev_parse("main").expect("main head");
        git::create_branch_at("feat/test", "main").expect("create feature");

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/test", "main", &parent_head, None, None);

        assert!(
            stale_switch_target_warning(&state, "feat/test")
                .expect("warning resolution should succeed")
                .is_none()
        );
    }

    #[test]
    fn worktree_edit_hint_mentions_worktree_path_and_main_checkout() {
        let hint = worktree_edit_hint("/repo/.worktrees/feat-x");
        assert!(hint.contains("/repo/.worktrees/feat-x"));
        assert!(hint.contains("main repo checkout"));
    }

    #[test]
    fn branch_worktree_map_includes_main_and_linked_worktrees() {
        let wt_map = branch_worktree_map(vec![
            git::WorktreeInfo {
                path: "/repo".to_string(),
                branch: Some("main".to_string()),
            },
            git::WorktreeInfo {
                path: "/repo/.worktrees/feat-x".to_string(),
                branch: Some("feat/x".to_string()),
            },
            git::WorktreeInfo {
                path: "/repo/detached".to_string(),
                branch: None,
            },
        ]);

        assert_eq!(wt_map.get("main"), Some(&"/repo".to_string()));
        assert_eq!(
            wt_map.get("feat/x"),
            Some(&"/repo/.worktrees/feat-x".to_string())
        );
        assert!(!wt_map.contains_key("detached"));
    }

    #[test]
    fn switch_to_creates_worktree_for_managed_branch_without_one() {
        let _guard = take_env_lock();
        let repo = init_git_repo("checkout-auto-worktree");
        let _cwd = CwdGuard::enter(&repo);

        // Set up a managed branch without a worktree.
        let parent_head = git::rev_parse("main").expect("main head");
        git::create_branch_at("feat/test", "main").expect("create branch");

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/test", "main", &parent_head, None, None);
        state.save().expect("save state");

        // Build worktree map — feat/test should NOT be in it yet.
        let wt_map = worktree_map();
        assert!(
            !wt_map.contains_key("feat/test"),
            "feat/test should not be in worktree map before switch"
        );

        // switch_to should create the worktree.
        switch_to(&state, "feat/test", &wt_map).expect("switch should succeed");

        // Verify the worktree was created.
        let wt_path = git::worktree_path("feat/test").expect("worktree path");
        assert!(
            std::path::Path::new(&wt_path).exists(),
            "worktree directory should exist at {wt_path}"
        );

        // Verify it shows in git worktree list.
        let worktrees = git::worktree_list().expect("worktree list");
        let has_wt = worktrees
            .iter()
            .any(|wt| wt.branch.as_deref() == Some("feat/test"));
        assert!(has_wt, "feat/test should appear in git worktree list");
    }

    #[test]
    fn switch_to_trunk_does_plain_checkout() {
        let _guard = take_env_lock();
        let repo = init_git_repo("checkout-trunk-plain");
        let _cwd = CwdGuard::enter(&repo);

        // Create a temporary branch to switch away from main.
        git::create_branch("temp-branch").expect("create temp");

        let state = StackState::new("main".to_string());
        state.save().expect("save state");

        let wt_map = worktree_map();

        // Switching to trunk should do a plain checkout, not create a worktree.
        switch_to(&state, "main", &wt_map).expect("switch to trunk should succeed");
        assert_eq!(
            git::current_branch().expect("branch"),
            "main",
            "should be on main after switch"
        );
    }

    #[test]
    fn switch_to_existing_worktree_does_not_create_another() {
        let _guard = take_env_lock();
        let repo = init_git_repo("checkout-existing-wt");
        let _cwd = CwdGuard::enter(&repo);

        let parent_head = git::rev_parse("main").expect("main head");
        git::create_branch_at("feat/test", "main").expect("create branch");

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/test", "main", &parent_head, None, None);
        state.save().expect("save state");

        // Create the worktree manually first.
        let wt_path = git::worktree_path("feat/test").expect("worktree path");
        git::worktree_add(&wt_path, "feat/test").expect("manual worktree add");

        // Build worktree map — feat/test SHOULD be in it now.
        let wt_map = worktree_map();
        assert!(
            wt_map.contains_key("feat/test"),
            "feat/test should be in worktree map"
        );

        // switch_to should succeed and NOT create a second worktree.
        switch_to(&state, "feat/test", &wt_map).expect("switch should succeed");

        // Only one worktree for feat/test should exist.
        let worktrees = git::worktree_list().expect("worktree list");
        let wt_count = worktrees
            .iter()
            .filter(|wt| wt.branch.as_deref() == Some("feat/test"))
            .count();
        assert_eq!(
            wt_count, 1,
            "should have exactly one worktree for feat/test"
        );
    }

    #[test]
    fn switch_to_unmanaged_branch_falls_through_to_checkout() {
        let _guard = take_env_lock();
        let repo = init_git_repo("checkout-unmanaged");
        let _cwd = CwdGuard::enter(&repo);

        git::create_branch_at("scratch", "main").expect("create scratch");

        let state = StackState::new("main".to_string());
        state.save().expect("save state");

        let wt_map = worktree_map();

        // Unmanaged branch not in worktree map should do plain checkout.
        switch_to(&state, "scratch", &wt_map).expect("switch should succeed");
        assert_eq!(
            git::current_branch().expect("branch"),
            "scratch",
            "should be on scratch after switch"
        );

        // No worktree should have been created.
        let wt_path = git::worktree_path("scratch").expect("worktree path");
        assert!(
            !std::path::Path::new(&wt_path).exists(),
            "no worktree should be created for unmanaged branch"
        );
    }
}
