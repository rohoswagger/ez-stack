use anyhow::{Result, bail};

use crate::cmd::rebase_conflict;
use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

struct MergeTarget {
    branch: String,
    pr_number: u64,
    title: String,
}

struct MergeOutcome {
    branch: String,
    pr_number: u64,
    restacked: usize,
    pushed: Vec<String>,
}

fn merge_targets(state: &StackState, current: &str, stack: bool) -> Result<Vec<MergeTarget>> {
    let branches = if stack {
        state.linear_stack(current)?
    } else {
        vec![state.stack_bottom(current)]
    };

    branches
        .into_iter()
        .map(|branch| {
            let meta = state.get_branch(&branch)?;
            let pr_number = match meta.pr_number {
                Some(number) => number,
                None => bail!(EzError::UserMessage(format!(
                    "Branch `{branch}` has no associated PR — run `ez submit` first"
                ))),
            };
            let title = github::get_pr_status(&branch)?
                .map(|pr| pr.title)
                .unwrap_or_else(|| "(unknown)".to_string());
            Ok(MergeTarget {
                branch,
                pr_number,
                title,
            })
        })
        .collect()
}

fn merge_branch(
    state: &mut StackState,
    branch: &str,
    pr_number: u64,
    method: &str,
) -> Result<MergeOutcome> {
    let trunk = state.trunk.clone();
    let remote = state.remote.clone();

    let sp = ui::spinner(&format!("Merging PR #{pr_number}..."));
    github::merge_pr(pr_number, method)?;
    sp.finish_and_clear();
    ui::info(&format!("Merged PR #{pr_number} for `{branch}`"));

    let children = state.reparent_children_preserving_parent_head(branch, &trunk)?;
    for child_name in &children {
        ui::info(&format!("Reparented `{child_name}` onto `{trunk}`"));

        if let Some(child_pr) = state.get_branch(child_name)?.pr_number
            && let Err(e) = github::update_pr_base(child_pr, &trunk)
        {
            ui::warn(&format!("Failed to update PR base for `{child_name}`: {e}"));
        }
    }

    state.remove_branch(branch);

    if git::branch_exists(branch) {
        if git::current_branch()? == branch {
            git::checkout(&trunk)?;
        }
        let _ = git::delete_branch(branch, true);
    }

    // Best-effort remote cleanup to preserve prior `gh pr merge --delete-branch`
    // behavior while using the non-interactive REST merge path.
    let _ = git::delete_remote_branch(&remote, branch);

    let sp = ui::spinner("Fetching latest changes...");
    git::fetch(&remote)?;
    sp.finish_and_clear();

    let order = state.topo_order();
    let current_root = git::repo_root()?;
    let mut restacked = 0;
    let mut restacked_for_push = Vec::<String>::new();

    for branch_name in &order {
        let meta = state.get_branch(branch_name)?;
        let parent = meta.parent.clone();
        let stored_parent_head = meta.parent_head.clone();

        let current_parent_tip = if state.is_trunk(&parent) {
            git::rev_parse(&format!("{remote}/{parent}"))?
        } else {
            git::rev_parse(&parent)?
        };

        if current_parent_tip == stored_parent_head {
            continue;
        }

        let sp = ui::spinner(&format!("Restacking `{branch_name}` onto `{parent}`..."));
        let outcome = git::rebase_onto_for_branch(
            &current_parent_tip,
            &stored_parent_head,
            branch_name,
            &current_root,
        )?;
        sp.finish_and_clear();

        match outcome {
            git::RebaseOutcome::RebasingComplete => {
                let meta = state.get_branch_mut(branch_name)?;
                meta.parent_head = current_parent_tip;
                restacked += 1;
                restacked_for_push.push(branch_name.clone());
                ui::info(&format!("Restacked `{branch_name}` onto `{parent}`"));
            }
            git::RebaseOutcome::Conflict(conflict) => {
                state.save()?;
                rebase_conflict::report("merge", branch_name, &parent, &conflict, "ez restack");
                bail!(EzError::RebaseConflict(branch_name.clone()));
            }
        }
    }

    if !restacked_for_push.is_empty() {
        for branch_name in &restacked_for_push {
            let sp = ui::spinner(&format!("Pushing restacked `{branch_name}` after merge..."));
            git::fetch_branch(&remote, branch_name)?;
            git::push(&remote, branch_name, true)?;
            sp.finish_and_clear();
            ui::info(&format!("Pushed `{branch_name}`"));
        }
    }

    state.save()?;

    Ok(MergeOutcome {
        branch: branch.to_string(),
        pr_number,
        restacked,
        pushed: restacked_for_push,
    })
}

pub fn run(method: &str, yes: bool, stack: bool) -> Result<()> {
    let mut state = StackState::load()?;
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

    let targets = merge_targets(&state, &current, stack)?;
    if targets.is_empty() {
        ui::info("No PRs to merge.");
        return Ok(());
    }

    if !yes {
        let confirmed = if stack {
            let summary = targets
                .iter()
                .map(|target| format!("#{} `{}`", target.pr_number, target.branch))
                .collect::<Vec<_>>()
                .join(", ");
            ui::confirm(&format!(
                "Merge {} PRs from the current stack ({summary})?",
                targets.len()
            ))
        } else {
            let target = &targets[0];
            ui::confirm(&format!(
                "Merge PR #{} for `{}` ({})?",
                target.pr_number, target.branch, target.title
            ))
        };

        if !confirmed {
            ui::info("Aborted");
            return Ok(());
        }
    }

    let mut total_restacked = 0;
    let mut total_pushed = 0usize;

    for target in &targets {
        let outcome = merge_branch(&mut state, &target.branch, target.pr_number, method)?;
        total_restacked += outcome.restacked;
        total_pushed += outcome.pushed.len();
        ui::receipt(&serde_json::json!({
            "cmd": "merge",
            "branch": outcome.branch,
            "pr_number": outcome.pr_number,
            "method": method,
            "stack": stack,
            "restacked": outcome.restacked,
            "pushed_branches": outcome.pushed,
        }));
    }

    if total_restacked > 0 {
        ui::info(&format!("Restacked {total_restacked} branch(es)"));
    }
    if total_pushed > 0 {
        ui::info(&format!(
            "Updated {total_pushed} remote branch(es) after restack"
        ));
    }

    if stack {
        ui::success(&format!(
            "Merged {} PR(s) from the current stack",
            targets.len()
        ));
    } else {
        ui::success("Merge complete");
    }

    Ok(())
}
