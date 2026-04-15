use anyhow::{Result, bail};

use crate::cmd::mutation_guard::tracked_only_untracked_hint;
use crate::error::EzError;
use crate::git;
use crate::hooks;
use crate::stack::{ScopeMode, StackState};
use crate::ui;

#[allow(clippy::too_many_arguments)]
pub fn run(
    name: &str,
    message: Option<&str>,
    all: bool,
    all_files: bool,
    from: Option<&str>,
    no_worktree: bool,
    scope: &[String],
    scope_mode: Option<ScopeMode>,
    hook: Option<&str>,
) -> Result<()> {
    // --hook with no value: list available hooks and exit.
    if hook == Some("") {
        let available = hooks::list_hooks("post-create");
        if available.is_empty() {
            ui::info("No post-create hooks found");
            ui::hint("Create .ez/hooks/post-create/<name>.md to add hooks");
        } else {
            ui::info("Available post-create hooks:");
            for name in &available {
                // Print to stdout (machine output, agent can parse).
                println!("  {name}");
            }
            ui::hint("Use: ez create <branch> --hook <name>");
        }
        return Ok(());
    }

    let mut state = StackState::load()?;
    let current = git::current_branch()?;

    // Determine the parent branch.
    let parent = if let Some(base) = from {
        if !state.is_trunk(base) && !state.is_managed(base) {
            bail!(EzError::UserMessage(format!(
                "branch `{base}` is not tracked by ez — use trunk or a managed branch with --from"
            )));
        }
        base.to_string()
    } else {
        if !state.is_trunk(&current) && !state.is_managed(&current) {
            bail!(EzError::UserMessage(format!(
                "current branch `{current}` is not tracked by ez — switch to a managed branch or trunk first"
            )));
        }
        current.clone()
    };

    if git::branch_exists(name) {
        ui::hint(&format!(
            "Use `ez switch {name}` to switch, or `ez delete {name}` to recreate"
        ));
        bail!(EzError::BranchAlreadyExists(name.to_string()));
    }

    // If a commit message was provided (only without --from due to clap conflicts_with),
    // stage and commit on the current branch first.
    if let Some(msg) = message {
        if all {
            let (_, _, untracked) = git::working_tree_status();
            if let Some(hint) = tracked_only_untracked_hint(untracked) {
                ui::hint(hint);
            }
            git::add_all()?;
        } else if all_files {
            git::add_all_including_untracked()?;
        }
        if !git::has_staged_changes()? {
            ui::hint(
                "Stage changes first: `git add <files>`, or drop -m to create without committing",
            );
            bail!(EzError::NothingToCommit);
        }
        git::commit(msg)?;
        ui::info(&format!("Committed on `{current}`: {msg}"));
    }

    let parent_head = git::rev_parse(&parent)?;
    let scope = normalize_scope_patterns(scope);
    let scope_mode = if scope.is_some() {
        Some(scope_mode.unwrap_or(ScopeMode::Warn))
    } else {
        None
    };

    // Decide whether to create a worktree.
    // Worktree mode is the default — only skip when --no-worktree is explicit.
    // --from controls which branch is the parent, not whether a worktree is created.
    let use_worktree = !no_worktree;

    if use_worktree {
        // Worktree creation path: create branch + worktree.
        let wt_path = git::worktree_path(name)?;

        git::create_branch_at(name, &parent_head)?;
        state.add_branch(name, &parent, &parent_head, scope.clone(), scope_mode);

        if let Err(e) = git::worktree_add(&wt_path, name) {
            // Rollback: remove the branch we just created.
            let _ = git::delete_branch(name, true);
            state.remove_branch(name);
            return Err(e);
        }

        if let Err(e) = state.save() {
            let _ = git::worktree_remove(&wt_path);
            let _ = git::delete_branch(name, true);
            return Err(e);
        }

        if from.is_some() {
            ui::success(&format!("Created `{name}` from `{parent}` → {wt_path}"));
        } else {
            ui::success(&format!("Created `{name}` → {wt_path}"));
        }
        ui::hint(&worktree_edit_hint(&wt_path));

        hooks::emit_hook("post-create", hook);

        ui::receipt(&serde_json::json!({
            "cmd": "create",
            "branch": name,
            "parent": parent,
            "head": &parent_head[..parent_head.len().min(7)],
            "worktree": wt_path,
            "scope_defined": scope.is_some(),
            "scope_mode": scope_mode.map(scope_mode_str),
        }));

        println!("{wt_path}");
    } else {
        // --no-worktree: create branch only, no worktree, no checkout.
        git::create_branch_at(name, &parent_head)?;
        state.add_branch(name, &parent, &parent_head, scope.clone(), scope_mode);
        if let Err(e) = state.save() {
            let _ = git::delete_branch(name, true);
            return Err(e);
        }
        if from.is_some() {
            ui::success(&format!("Created `{name}` from `{parent}`"));
        } else {
            ui::success(&format!("Created `{name}` on `{parent}`"));
        }

        hooks::emit_hook("post-create", hook);

        ui::receipt(&serde_json::json!({
            "cmd": "create",
            "branch": name,
            "parent": parent,
            "head": &parent_head[..parent_head.len().min(7)],
            "scope_defined": scope.is_some(),
            "scope_mode": scope_mode.map(scope_mode_str),
        }));
    }

    Ok(())
}

fn normalize_scope_patterns(patterns: &[String]) -> Option<Vec<String>> {
    let mut normalized = Vec::new();
    for pattern in patterns {
        let trimmed = pattern.trim();
        if trimmed.is_empty() || normalized.iter().any(|p| p == trimmed) {
            continue;
        }
        normalized.push(trimmed.to_string());
    }
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn scope_mode_str(mode: ScopeMode) -> &'static str {
    match mode {
        ScopeMode::Warn => "warn",
        ScopeMode::Strict => "strict",
    }
}

fn worktree_edit_hint(wt_path: &str) -> String {
    format!(
        "Edit files under `{wt_path}`. This branch lives in a linked worktree, not the main repo checkout."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git;
    use crate::stack::{BranchMeta, StackState};
    use crate::test_support::{CwdGuard, init_git_repo, take_env_lock};
    use std::collections::HashMap;

    fn make_state() -> StackState {
        let mut branches = HashMap::new();
        branches.insert(
            "feat/base".to_string(),
            BranchMeta {
                name: "feat/base".to_string(),
                parent: "main".to_string(),
                parent_head: "abc".to_string(),
                pr_number: None,
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
    fn test_from_valid_targets() {
        let state = make_state();
        // Both trunk and managed branches are valid --from targets
        assert!(state.is_trunk("main"));
        assert!(state.is_managed("feat/base"));
        // Untracked branches are not valid
        assert!(!state.is_managed("random-branch"));
        assert!(!state.is_trunk("random-branch"));
    }

    #[test]
    fn normalize_scope_patterns_trims_dedupes_and_drops_empty_values() {
        assert_eq!(
            normalize_scope_patterns(&[
                " src/auth/** ".to_string(),
                "".to_string(),
                "src/auth/**".to_string(),
                "  ".to_string(),
                "tests/auth/**".to_string(),
            ]),
            Some(vec!["src/auth/**".to_string(), "tests/auth/**".to_string()])
        );
        assert_eq!(normalize_scope_patterns(&[" ".to_string()]), None);
    }

    #[test]
    fn worktree_edit_hint_mentions_worktree_path_and_main_checkout() {
        let hint = worktree_edit_hint("/repo/.worktrees/feat-x");
        assert!(hint.contains("/repo/.worktrees/feat-x"));
        assert!(hint.contains("main repo checkout"));
    }

    #[test]
    fn create_rejects_unmanaged_current_branch_without_from() {
        let _guard = take_env_lock();
        let repo = init_git_repo("create-unmanaged-current");
        let _cwd = CwdGuard::enter(&repo);

        let state = StackState::new("main".to_string());
        state.save().expect("save state");
        git::create_branch("scratch").expect("create scratch");

        let err = run("feat/new", None, false, false, None, true, &[], None, None)
            .expect_err("unmanaged current branch should fail");
        assert!(
            err.to_string()
                .contains("current branch `scratch` is not tracked by ez"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn create_from_creates_worktree_by_default() {
        let _guard = take_env_lock();
        let repo = init_git_repo("create-from-worktree");
        let _cwd = CwdGuard::enter(&repo);

        let state = StackState::new("main".to_string());
        state.save().expect("save state");

        // ez create feat/test --from main should create a worktree.
        run(
            "feat/test",
            None,
            false,
            false,
            Some("main"),
            false,
            &[],
            None,
            None,
        )
        .expect("create with --from should succeed");

        // Verify the branch exists.
        assert!(git::branch_exists("feat/test"));

        // Verify a worktree was created at .worktrees/feat-test.
        let wt_path = git::worktree_path("feat/test").expect("worktree path");
        assert!(
            std::path::Path::new(&wt_path).exists(),
            "worktree directory should exist at {wt_path}"
        );

        // Verify the worktree shows up in git worktree list.
        let worktrees = git::worktree_list().expect("worktree list");
        let has_wt = worktrees
            .iter()
            .any(|wt| wt.branch.as_deref() == Some("feat/test"));
        assert!(has_wt, "feat/test should appear in git worktree list");

        // Verify the branch is stacked correctly on main.
        let reloaded = StackState::load().expect("reload state");
        let meta = reloaded.get_branch("feat/test").expect("branch meta");
        assert_eq!(meta.parent, "main");
    }

    #[test]
    fn create_from_no_worktree_skips_worktree() {
        let _guard = take_env_lock();
        let repo = init_git_repo("create-from-no-wt");
        let _cwd = CwdGuard::enter(&repo);

        let state = StackState::new("main".to_string());
        state.save().expect("save state");

        // ez create feat/test --from main --no-worktree should NOT create a worktree.
        run(
            "feat/test",
            None,
            false,
            false,
            Some("main"),
            true,
            &[],
            None,
            None,
        )
        .expect("create with --from --no-worktree should succeed");

        assert!(git::branch_exists("feat/test"));

        let wt_path = git::worktree_path("feat/test").expect("worktree path");
        assert!(
            !std::path::Path::new(&wt_path).exists(),
            "worktree directory should NOT exist when --no-worktree is used"
        );
    }

    #[test]
    fn create_no_worktree_without_from_does_not_switch_branch() {
        let _guard = take_env_lock();
        let repo = init_git_repo("create-no-wt-no-switch");
        let _cwd = CwdGuard::enter(&repo);

        let state = StackState::new("main".to_string());
        state.save().expect("save state");

        // Should be on main before create.
        assert_eq!(git::current_branch().expect("branch"), "main");

        // --no-worktree without --from should NOT switch to the new branch.
        run("feat/test", None, false, false, None, true, &[], None, None)
            .expect("create --no-worktree should succeed");

        // Branch exists but we're still on main.
        assert!(git::branch_exists("feat/test"));
        assert_eq!(
            git::current_branch().expect("branch"),
            "main",
            "should still be on main after --no-worktree create"
        );
    }

    #[test]
    fn create_from_managed_parent_stacks_correctly() {
        let _guard = take_env_lock();
        let repo = init_git_repo("create-from-managed");
        let _cwd = CwdGuard::enter(&repo);

        let parent_head = git::rev_parse("main").expect("main head");
        git::create_branch_at("feat/base", "main").expect("create base");

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/base", "main", &parent_head, None, None);
        state.save().expect("save state");

        // Create child from managed (non-trunk) parent.
        run(
            "feat/child",
            None,
            false,
            false,
            Some("feat/base"),
            true,
            &[],
            None,
            None,
        )
        .expect("create from managed parent should succeed");

        let reloaded = StackState::load().expect("reload state");
        let meta = reloaded.get_branch("feat/child").expect("child meta");
        assert_eq!(meta.parent, "feat/base");
        assert_eq!(meta.parent_head, parent_head);
    }

    #[test]
    fn create_rejects_duplicate_branch_with_from() {
        let _guard = take_env_lock();
        let repo = init_git_repo("create-dup-from");
        let _cwd = CwdGuard::enter(&repo);

        let state = StackState::new("main".to_string());
        state.save().expect("save state");

        // Create the branch first.
        run(
            "feat/test",
            None,
            false,
            false,
            Some("main"),
            true,
            &[],
            None,
            None,
        )
        .expect("first create should succeed");

        // Second create with same name should fail.
        let err = run(
            "feat/test",
            None,
            false,
            false,
            Some("main"),
            true,
            &[],
            None,
            None,
        )
        .expect_err("duplicate should fail");
        assert!(
            err.to_string().contains("already exists"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn create_rejects_unmanaged_from_branch() {
        let _guard = take_env_lock();
        let repo = init_git_repo("create-unmanaged-from");
        let _cwd = CwdGuard::enter(&repo);

        let state = StackState::new("main".to_string());
        state.save().expect("save state");
        git::create_branch_at("scratch", "main").expect("create scratch");

        let err = run(
            "feat/new",
            None,
            false,
            false,
            Some("scratch"),
            true,
            &[],
            None,
            None,
        )
        .expect_err("unmanaged --from branch should fail");
        assert!(
            err.to_string()
                .contains("branch `scratch` is not tracked by ez"),
            "unexpected error: {err:#}"
        );
    }
}
