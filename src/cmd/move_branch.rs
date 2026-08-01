use anyhow::{Result, bail};

use crate::cmd::rebase_conflict;
use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run(onto: Option<&str>, force: bool) -> Result<()> {
    let mut state = StackState::load()?;
    let Some(onto) = onto.filter(|value| !value.trim().is_empty()) else {
        bail!(EzError::UserMessage(missing_onto_message(&state)));
    };

    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current.clone()));
    }

    // The --onto target must be trunk or a managed branch.
    if !state.is_trunk(onto) && !state.is_managed(onto) {
        bail!(EzError::UserMessage(format!(
            "Target branch `{onto}` is not trunk or a managed branch"
        )));
    }

    // Prevent moving onto self.
    if onto == current {
        bail!(EzError::UserMessage(
            "Cannot move a branch onto itself".to_string()
        ));
    }

    // Prevent moving onto a descendant (would create a cycle).
    let path = state.path_to_trunk(onto);
    if path.contains(&current) {
        bail!(EzError::UserMessage(format!(
            "Cannot move `{current}` onto `{onto}` — `{onto}` is a descendant of `{current}`"
        )));
    }

    let meta = state.get_branch(&current)?;
    let old_parent = meta.parent.clone();
    let old_parent_head = meta.parent_head.clone();
    let pr_number = meta.pr_number;

    let new_parent_head = git::rev_parse(onto)?;
    let candidates = crate::cmd::preflight::move_candidates(&state, &current, onto)?;
    let preflight = crate::cmd::preflight::run("move", force, &candidates)?;
    let (old_base, derived) =
        crate::cmd::restack::effective_old_base(&current, &old_parent, &old_parent_head);
    if derived {
        ui::info(&format!(
            "Recorded base for `{current}` is stale — deriving its replay range from git instead"
        ));
    }

    let current_preflight = preflight
        .branches
        .iter()
        .find(|branch| branch.branch == current);
    if current_preflight.is_some_and(|branch| branch.all_redundant()) {
        let branch_preflight = current_preflight.expect("checked is_some");
        git::align_branch_to_target(
            &current,
            &new_parent_head,
            &branch_preflight.branch_tip,
            &git::repo_root()?,
        )?;
        ui::receipt(&serde_json::json!({
            "cmd": "move",
            "branch": current,
            "action": "restacked",
            "method": "already_applied",
            "parent": onto,
            "redundant_commits": branch_preflight.cherry.redundant,
        }));
    } else {
        // Rebase current branch onto the new parent.
        let sp = ui::spinner(&format!("Rebasing `{current}` onto `{onto}`..."));
        let outcome = git::rebase_onto(&new_parent_head, &old_base, &current)?;
        sp.finish_and_clear();

        if let git::RebaseOutcome::Conflict(conflict) = outcome {
            rebase_conflict::report(
                "move",
                &current,
                onto,
                &conflict,
                &format!("ez move --onto {onto}"),
            );
            bail!(EzError::RebaseConflict(current.clone()));
        }
    }

    // Update branch metadata.
    let meta = state.get_branch_mut(&current)?;
    meta.parent = onto.to_string();
    meta.parent_head = new_parent_head;

    // Update PR base if a PR exists.
    if let Some(pr) = pr_number {
        let base = if state.is_trunk(onto) {
            state.trunk.clone()
        } else {
            onto.to_string()
        };
        if let Err(e) = github::update_pr_base(pr, &base, state.repo.as_deref()) {
            ui::warn(&format!("Failed to update PR base: {e}"));
        }
    }

    // Restack the whole subtree onto the moved branch — descendants beyond direct
    // children also need to follow, or they're left detached from the stack.
    let current_root = git::repo_root()?;
    let restacked = crate::cmd::restack::cascade_restack_with_options(
        &mut state,
        &current,
        &current_root,
        &current,
        "move",
        crate::cmd::restack::RestackOptions { force },
    )?;

    // Checkout the current branch again (rebase may have left us on a descendant).
    git::checkout(&current)?;

    state.save()?;

    ui::success(&format!("Moved `{current}` onto `{onto}`"));
    if restacked > 0 {
        ui::info(&format!("Restacked {restacked} branch(es)"));
    }

    ui::receipt(&serde_json::json!({
        "cmd": "move",
        "branch": current,
        "from": old_parent,
        "onto": onto,
    }));

    Ok(())
}

fn missing_onto_message(state: &StackState) -> String {
    let mut branches = vec![format!("{} (trunk)", state.trunk)];
    branches.extend(state.topo_order());

    let mut message = String::from("Missing target branch for `ez move --onto`\n");
    message.push_str("Available branches:\n");
    for branch in branches {
        message.push_str(&format!("  - {branch}\n"));
    }
    message.push_str("  -> Run: `ez move --onto <branch>`");
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> StackState {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/base", "main", "aaa", None, None);
        state.add_branch("feat/top", "feat/base", "bbb", None, None);
        state
    }

    #[test]
    fn missing_onto_message_lists_trunk_and_managed_branches() {
        let message = missing_onto_message(&sample_state());

        assert!(message.contains("Missing target branch for `ez move --onto`"));
        assert!(message.contains("Available branches:"));
        assert!(message.contains("  - main (trunk)"));
        assert!(message.contains("  - feat/base"));
        assert!(message.contains("  - feat/top"));
    }
}
