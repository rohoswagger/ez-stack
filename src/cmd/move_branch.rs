use anyhow::{Result, bail};

use crate::cmd::native_stack;
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

    // Look GitHub up before touching anything local. A PR that belongs to a native stack cannot
    // have its base changed directly — GitHub rejects it with "Cannot change the base branch
    // because the pull request is part of a stack" — so knowing this up front is what lets the
    // move restructure the stack instead of reporting a success it did not achieve.
    let native_stack_before = detect_native_stack(pr_number, state.repo.as_deref(), &state);

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

    // Update the PR base directly only when the PR stands alone. For a PR inside a native stack
    // GitHub refuses the edit, and the stack restructure below is what moves the base instead —
    // attempting it here would only emit a warning for an operation that was never going to work.
    let pr_base_outcome = match (pr_number, native_stack_before.is_some()) {
        (None, _) => PrBaseOutcome::NoPr,
        (Some(_), true) => PrBaseOutcome::DeferredToNativeStack,
        (Some(pr), false) => {
            let base = if state.is_trunk(onto) {
                state.trunk.clone()
            } else {
                onto.to_string()
            };
            match github::update_pr_base(pr, &base, state.repo.as_deref()) {
                Ok(()) => PrBaseOutcome::Updated,
                Err(e) => {
                    ui::warn(&format!("Failed to update PR base: {e}"));
                    PrBaseOutcome::Failed(e.to_string())
                }
            }
        }
    };

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

    // Report the local half on its own. Whatever happens to GitHub next, the rebase and the stack
    // metadata are already done, and an agent reading the receipts needs to see that separately
    // from the remote outcome rather than inferring both from a single "Moved".
    ui::success(&format!("Moved `{current}` onto `{onto}` locally"));
    if restacked > 0 {
        ui::info(&format!("Restacked {restacked} branch(es)"));
    }
    ui::receipt(&serde_json::json!({
        "cmd": "move",
        "branch": current,
        "from": old_parent,
        "onto": onto,
        "local": "moved",
        "restacked": restacked,
        "pr_base": pr_base_outcome.receipt_value(),
        "native_stack_before": match &native_stack_before {
            Some(info) => serde_json::json!({
                "number": info.number,
                "size": info.pull_requests.len(),
            }),
            None => serde_json::Value::Null,
        },
    }));

    // Restructure the GitHub stack to match the new topology. Inserting a PR into the middle of an
    // existing native stack is only supported by dissolving and recreating it, which is what
    // `repair` does — a plain base change is what GitHub rejected in the first place.
    //
    // Only when the branch was actually in a stack. A move that involves no native stack has no
    // business reconciling every other chain in the repo, and doing so would make `ez move` start
    // failing on repos and GitHub installations that have nothing to do with this move.
    if let Some(info) = &native_stack_before {
        ui::info(&format!(
            "PR #{} belongs to GitHub native stack #{} ({} PRs) — restructuring it to match",
            pr_number.unwrap_or_default(),
            info.number,
            info.pull_requests.len()
        ));
        native_stack::reconcile_stacks(&state, "move", &format!("ez move --onto {onto}"), true)?;
    }

    Ok(())
}

/// What happened to the PR's base branch, kept distinct from the local move in the receipt.
enum PrBaseOutcome {
    NoPr,
    Updated,
    Failed(String),
    /// The PR is in a native stack, so its base moves as part of restructuring that stack.
    DeferredToNativeStack,
}

impl PrBaseOutcome {
    fn receipt_value(&self) -> serde_json::Value {
        match self {
            PrBaseOutcome::NoPr => serde_json::Value::Null,
            PrBaseOutcome::Updated => serde_json::Value::String("updated".to_string()),
            PrBaseOutcome::Failed(detail) => serde_json::json!({
                "status": "failed",
                "detail": detail,
            }),
            PrBaseOutcome::DeferredToNativeStack => {
                serde_json::Value::String("deferred_to_native_stack".to_string())
            }
        }
    }
}

/// The GitHub native stack a PR belongs to, or `None` when it has none, has no PR, or GitHub
/// cannot answer.
///
/// A lookup failure deliberately reads as "no native stack" rather than aborting: `ez move` is
/// primarily a local operation, and it should not start refusing to work because GitHub is
/// unreachable. The restructure afterwards is what surfaces a genuine remote problem.
fn detect_native_stack(
    pr_number: Option<u64>,
    repo: Option<&str>,
    state: &StackState,
) -> Option<github::NativeStackInfo> {
    let pr = pr_number?;
    if state.is_fork_workflow() {
        return None;
    }
    match github::lookup_native_stack_for_pr(pr, repo) {
        Ok(github::NativeStackLookup::Found(info)) => Some(info),
        Ok(_) => None,
        Err(e) => {
            ui::warn(&format!(
                "Could not check whether PR #{pr} is in a GitHub native stack: {e}"
            ));
            None
        }
    }
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
    fn pr_base_receipt_distinguishes_every_remote_outcome() {
        // The point of these values: "Moved" on its own told an agent nothing about whether the
        // PR's base actually followed the branch. Each state has to be separately readable.
        assert_eq!(PrBaseOutcome::NoPr.receipt_value(), serde_json::Value::Null);
        assert_eq!(PrBaseOutcome::Updated.receipt_value(), "updated");
        assert_eq!(
            PrBaseOutcome::DeferredToNativeStack.receipt_value(),
            "deferred_to_native_stack"
        );

        let failed = PrBaseOutcome::Failed(
            "Cannot change the base branch because the pull request is part of a stack".to_string(),
        )
        .receipt_value();
        assert_eq!(failed["status"], "failed");
        assert!(
            failed["detail"]
                .as_str()
                .expect("detail")
                .contains("part of a stack"),
            "the reason has to survive into the receipt: {failed}"
        );
    }

    #[test]
    fn detect_native_stack_is_none_without_a_pr_or_in_a_fork_workflow() {
        let state = sample_state();
        // No PR: nothing to look up, and no GitHub call should be attempted.
        assert!(detect_native_stack(None, None, &state).is_none());

        // Fork workflows keep the ez stack local — GitHub native stacks need one repository.
        let mut fork_state = sample_state();
        fork_state.upstream_remote = Some("upstream".to_string());
        assert!(fork_state.is_fork_workflow());
        assert!(detect_native_stack(Some(42), None, &fork_state).is_none());
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
