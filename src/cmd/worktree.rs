use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

/// Resolve the `.worktrees/<name>` path relative to the repo root.
fn worktree_path(name: &str) -> Result<String> {
    let root = git::repo_root()?;
    Ok(format!("{root}/.worktrees/{name}"))
}

pub fn create(name: &str, from: Option<&str>) -> Result<()> {
    let mut state = StackState::load()?;
    let current = git::current_branch()?;

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
        ui::hint(&format!("Use `ez checkout {name}` to switch to it"));
        bail!(EzError::BranchAlreadyExists(name.to_string()));
    }

    let wt_path = worktree_path(name)?;

    // Create branch at parent tip (without switching).
    let parent_head = git::rev_parse(&parent)?;
    git::create_branch_at(name, &parent_head)?;
    state.add_branch(name, &parent, &parent_head);

    // Create worktree checking out the new branch.
    git::worktree_add(&wt_path, name)?;

    state.save()?;
    ui::success(&format!(
        "Created branch `{name}` on top of `{parent}` in worktree `{wt_path}`"
    ));

    Ok(())
}

pub fn delete(name: &str, force: bool) -> Result<()> {
    let mut state = StackState::load()?;

    let wt_path = worktree_path(name)?;

    // Determine which branch is checked out in that worktree.
    let branch = git::worktree_list()?
        .into_iter()
        .find(|wt| wt.path == wt_path)
        .and_then(|wt| wt.branch);

    // Remove the worktree.
    if std::path::Path::new(&wt_path).exists() {
        let result = if force {
            git::worktree_remove_force(&wt_path)
        } else {
            git::worktree_remove(&wt_path)
        };
        match result {
            Ok(()) => ui::success(&format!("Removed worktree at `{wt_path}`")),
            Err(e) => bail!(
                "Could not remove worktree at `{wt_path}`: {e}\n\
                 Use `ez worktree delete {name} --force` to discard uncommitted changes"
            ),
        }
    }

    // Clean up the branch from the stack if it was ez-managed.
    if let Some(branch_name) = &branch {
        if state.is_managed(branch_name) {
            let meta = state.get_branch(branch_name)?;
            let parent = meta.parent.clone();
            let pr_number = meta.pr_number;

            let parent_head_for_children =
                git::rev_parse(branch_name).unwrap_or_else(|_| meta.parent_head.clone());

            let children = state.children_of(branch_name);
            for child_name in &children {
                let child = state.get_branch_mut(child_name)?;
                child.parent = parent.clone();
                child.parent_head = parent_head_for_children.clone();
                ui::info(&format!("Reparented `{child_name}` onto `{parent}`"));
            }

            if pr_number.is_some() {
                for child_name in &children {
                    let child = state.get_branch(child_name)?;
                    if let Some(child_pr) = child.pr_number
                        && let Err(e) = github::update_pr_base(child_pr, &parent)
                    {
                        ui::warn(&format!("Failed to update PR base for `{child_name}`: {e}"));
                    }
                }
            }

            state.remove_branch(branch_name);

            // Delete the local branch (force, since the worktree is already gone).
            let _ = git::delete_branch(branch_name, true);

            state.save()?;
            ui::success(&format!("Deleted branch `{branch_name}`"));

            if !children.is_empty() {
                ui::hint(&format!(
                    "Run `ez restack` to rebase reparented branches onto `{parent}`"
                ));
            }
        } else {
            // Branch exists but isn't ez-managed — just delete it.
            let _ = git::delete_branch(branch_name, force);
            state.save()?;
        }
    } else {
        state.save()?;
    }

    let _ = git::worktree_prune();
    Ok(())
}

pub fn list() -> Result<()> {
    let worktrees = git::worktree_list()?;
    if worktrees.is_empty() {
        ui::info("No worktrees found");
        return Ok(());
    }
    for wt in worktrees {
        let name = std::path::Path::new(&wt.path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&wt.path);
        let branch = wt.branch.as_deref().unwrap_or("(detached HEAD)");
        eprintln!("{:<30} {}  {}", name, ui::dim(branch), ui::dim(&wt.path));
    }
    Ok(())
}
