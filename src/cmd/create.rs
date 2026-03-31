use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::hooks;
use crate::stack::StackState;
use crate::ui;

pub fn run(
    name: &str,
    message: Option<&str>,
    all: bool,
    from: Option<&str>,
    no_worktree: bool,
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
            git::add_all()?;
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

    // Decide whether to create a worktree.
    // Worktree mode: default when no --from and no --no-worktree.
    let use_worktree = !no_worktree && from.is_none();

    if use_worktree {
        // Worktree creation path: create branch + worktree.
        let wt_path = git::worktree_path(name)?;

        git::create_branch_at(name, &parent_head)?;
        state.add_branch(name, &parent, &parent_head);

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

        hooks::emit_hook("post-create", hook);

        ui::receipt(&serde_json::json!({
            "cmd": "create",
            "branch": name,
            "parent": parent,
            "head": &parent_head[..parent_head.len().min(7)],
            "worktree": wt_path,
        }));

        println!("{wt_path}");
    } else if from.is_some() {
        // Create at the tip of --from without switching branches.
        git::create_branch_at(name, &parent_head)?;
        state.add_branch(name, &parent, &parent_head);
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
        }));
    } else {
        // --no-worktree: original behavior — create and switch.
        git::create_branch(name)?;
        state.add_branch(name, &parent, &parent_head);
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
        }));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::stack::{BranchMeta, StackState};
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
}
