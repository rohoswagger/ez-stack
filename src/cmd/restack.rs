use anyhow::{Result, bail};

use crate::cmd::rebase_conflict;
use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

/// Restack the transitive descendants of `root` after `root`'s tip moved.
///
/// Walks `root`'s subtree in topological order (parent before child) and rebases
/// any branch whose parent tip no longer matches its recorded `parent_head`,
/// updating `parent_head` on success. Because a parent is always restacked before
/// its children, a tip moved this pass is observed by the next iteration — so the
/// whole subtree converges in one pass. This is the shared path for every
/// auto-restack-on-mutation command (`commit`, `amend`, `move`); restacking only
/// direct children left grandchildren detached from the stack.
///
/// On conflict: saves state, returns to `return_to`, reports, and bails with
/// `RebaseConflict`. Returns the number of branches actually restacked.
pub fn cascade_restack(
    state: &mut StackState,
    root: &str,
    current_root: &str,
    return_to: &str,
    cmd: &str,
) -> Result<usize> {
    let mut restacked = 0;

    for branch_name in state.descendants_topo(root) {
        let meta = state.get_branch(&branch_name)?;
        let parent = meta.parent.clone();
        let stored_parent_head = meta.parent_head.clone();

        let current_parent_tip = git::rev_parse(&parent)?;
        if current_parent_tip == stored_parent_head {
            continue;
        }

        ui::info(&format!("Restacking `{branch_name}`..."));
        let outcome = git::rebase_onto_for_branch(
            &current_parent_tip,
            &stored_parent_head,
            &branch_name,
            current_root,
        )?;

        match outcome {
            git::RebaseOutcome::RebasingComplete => {
                let meta = state.get_branch_mut(&branch_name)?;
                meta.parent_head = current_parent_tip;
                restacked += 1;
            }
            git::RebaseOutcome::Conflict(conflict) => {
                state.save()?;
                git::checkout(return_to)?;
                rebase_conflict::report(cmd, &branch_name, &parent, &conflict, "ez restack");
                bail!(EzError::RebaseConflict(branch_name.clone()));
            }
        }
    }

    Ok(restacked)
}

pub fn run() -> Result<()> {
    let mut state = StackState::load()?;
    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }
    let original_branch = git::current_branch()?;
    let current_root = git::repo_root()?;

    ui::info(&format!("Fetching from `{}`...", state.remote));
    git::fetch(&state.remote)?;
    match git::update_branch_to_latest_remote(
        &state.remote,
        &state.trunk,
        &original_branch,
        &current_root,
    ) {
        Ok(true) => ui::info(&format!("Updated `{}` to latest", state.trunk)),
        Ok(false) => {}
        Err(e) => ui::warn(&format!("Could not update `{}` — {e}", state.trunk)),
    }

    let order = state.topo_order();
    let mut restacked = 0;

    for branch_name in &order {
        let meta = state.get_branch(branch_name)?;
        let parent = meta.parent.clone();
        let stored_parent_head = meta.parent_head.clone();

        let current_parent_tip = git::rev_parse(&parent)?;

        if current_parent_tip == stored_parent_head {
            continue;
        }

        // Branch is stale — rebase onto the new parent tip (in its worktree if needed).
        let before_sha = git::rev_parse(branch_name).unwrap_or_default();

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
                ui::info(&format!("Restacked `{branch_name}` onto `{parent}`"));

                // Auto-drop commits whose patches are already upstream.
                let mut redundant_count: u64 = 0;
                if let Ok(cherry) = git::cherry(&parent, branch_name) {
                    let redundant: Vec<&str> =
                        cherry.lines().filter(|l| l.starts_with("- ")).collect();
                    if !redundant.is_empty() {
                        redundant_count = redundant.len() as u64;
                        ui::info(&format!(
                            "Dropping {redundant_count} redundant commit(s) from `{branch_name}` (already in `{parent}`)",
                        ));
                        match git::rebase_for_branch(&parent, branch_name, &current_root) {
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
                    "cmd": "restack",
                    "branch": branch_name,
                    "action": "restacked",
                    "parent": parent,
                    "before": &before_sha[..before_sha.len().min(7)],
                    "after": &after_sha[..after_sha.len().min(7)],
                    "redundant_commits": redundant_count,
                }));
            }
            git::RebaseOutcome::Conflict(conflict) => {
                git::checkout(&original_branch)?;
                state.save()?;
                rebase_conflict::report("restack", branch_name, &parent, &conflict, "ez restack");
                bail!(EzError::RebaseConflict(branch_name.clone()));
            }
        }
    }

    // Return to the original branch.
    git::checkout(&original_branch)?;

    state.save()?;

    if restacked == 0 {
        ui::info("All branches are up to date — nothing to restack");
    }

    if restacked > 0 {
        ui::success(&format!("Restacked {restacked} branch(es)"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{CwdGuard, init_git_repo, run_cmd, take_env_lock, write_file};

    /// Commit a new file on `branch` and return the resulting tip SHA.
    fn commit_file(repo: &std::path::Path, branch: &str, file: &str) -> String {
        git::checkout(branch).expect("checkout");
        write_file(repo, file, "x\n");
        run_cmd(repo, "git", &["add", file]);
        run_cmd(repo, "git", &["commit", "-m", &format!("add {file}")]);
        git::rev_parse(branch).expect("rev-parse")
    }

    #[test]
    fn cascade_restack_rebases_grandchildren_not_just_direct_children() {
        let _guard = take_env_lock();
        let repo = init_git_repo("cascade-grandchildren");
        let _cwd = CwdGuard::enter(&repo);

        let main_sha = git::rev_parse("main").expect("main sha");

        // Build a 3-deep linear stack: main -> feat/a -> feat/b -> feat/c.
        git::create_branch_at("feat/a", "main").expect("branch a");
        let a_sha1 = commit_file(&repo, "feat/a", "a.txt");
        git::create_branch_at("feat/b", "feat/a").expect("branch b");
        let b_sha1 = commit_file(&repo, "feat/b", "b.txt");
        git::create_branch_at("feat/c", "feat/b").expect("branch c");
        commit_file(&repo, "feat/c", "c.txt");

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", &main_sha, None, None);
        state.add_branch("feat/b", "feat/a", &a_sha1, None, None);
        state.add_branch("feat/c", "feat/b", &b_sha1, None, None);
        state.save().expect("save state");

        // Advance feat/a — this is the mutation that auto-restack must cascade.
        let a_sha2 = commit_file(&repo, "feat/a", "a2.txt");
        assert_ne!(a_sha2, a_sha1);

        let root = git::repo_root().expect("repo root");
        let restacked =
            cascade_restack(&mut state, "feat/a", &root, "feat/a", "test").expect("cascade");

        // Both the direct child AND the grandchild must be restacked.
        assert_eq!(restacked, 2, "both feat/b and feat/c should be restacked");

        // feat/a's new commit must have propagated all the way to the grandchild.
        assert!(
            git::is_ancestor("feat/a", "feat/c"),
            "feat/a tip must be an ancestor of the grandchild feat/c"
        );
        git::checkout("feat/c").expect("checkout c");
        assert!(
            repo.join("a2.txt").exists(),
            "feat/a's new file must reach the grandchild's working tree"
        );

        // Metadata must track the new tips so the equality guard stays accurate.
        assert_eq!(state.get_branch("feat/b").expect("b").parent_head, a_sha2);
        let b_sha2 = git::rev_parse("feat/b").expect("b sha2");
        assert_eq!(state.get_branch("feat/c").expect("c").parent_head, b_sha2);
    }
}
