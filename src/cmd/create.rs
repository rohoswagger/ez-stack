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
    // Worktree mode: default when no --from and no --no-worktree.
    let use_worktree = !no_worktree && from.is_none();

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

        ui::success(&format!("Created `{name}` → {wt_path}"));
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
    } else if from.is_some() {
        // Create at the tip of --from without switching branches.
        git::create_branch_at(name, &parent_head)?;
        state.add_branch(name, &parent, &parent_head, scope.clone(), scope_mode);
        if let Err(e) = state.save() {
            let _ = git::delete_branch(name, true);
            return Err(e);
        }
        ui::success(&format!("Created `{name}` from `{parent}`"));

        hooks::emit_hook("post-create", hook);

        ui::receipt(&serde_json::json!({
            "cmd": "create",
            "branch": name,
            "parent": parent,
            "head": &parent_head[..parent_head.len().min(7)],
            "scope_defined": scope.is_some(),
            "scope_mode": scope_mode.map(scope_mode_str),
        }));
    } else {
        // --no-worktree: original behavior — create and switch.
        git::create_branch(name)?;
        state.add_branch(name, &parent, &parent_head, scope.clone(), scope_mode);
        if let Err(e) = state.save() {
            let _ = git::delete_branch(name, true);
            return Err(e);
        }
        ui::success(&format!("Created `{name}` on `{parent}`"));

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
            default_from: None,
            repo: None,
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
