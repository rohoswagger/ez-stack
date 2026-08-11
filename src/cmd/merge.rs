use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};

use crate::cmd::restack;
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
    status: github::MergePrOutcome,
    restacked: usize,
    pushed: Vec<String>,
    worktree: Option<String>,
}

struct NativeMergeOutcome {
    status: github::MergePrOutcome,
    branches: Vec<String>,
    pr_numbers: Vec<u64>,
    merged_pr_number: u64,
}

fn linked_worktree_map() -> Result<HashMap<String, String>> {
    let main_root = git::main_worktree_root()?;
    Ok(git::worktree_list()?
        .into_iter()
        .filter(|wt| wt.path != main_root)
        .filter_map(|wt| wt.branch.map(|branch| (branch, wt.path)))
        .collect())
}

fn preflight_clean_linked_worktrees(
    targets: &[MergeTarget],
    worktree_map: &HashMap<String, String>,
) -> Result<()> {
    for target in targets {
        let Some(path) = worktree_map.get(&target.branch) else {
            continue;
        };
        crate::cmd::worktree::guard_registered_worktree(&target.branch, path, "merge")?;
        let (staged, modified, untracked) = git::working_tree_status_at(path);
        if staged > 0 || modified > 0 || untracked > 0 {
            bail!(EzError::UserMessage(format!(
                "Cannot merge `{}` while linked worktree `{}` has uncommitted changes (staged: {}, modified: {}, untracked: {})\n  → Commit/stash changes or clean the worktree, then retry",
                target.branch, path, staged, modified, untracked
            )));
        }
    }
    Ok(())
}

fn inside_worktree_path(current_dir: &str, worktree_path: &str) -> bool {
    current_dir == worktree_path || current_dir.starts_with(&format!("{worktree_path}/"))
}

fn merge_receipt_value(outcome: &MergeOutcome, method: &str, stack: bool) -> serde_json::Value {
    let status = match outcome.status {
        github::MergePrOutcome::Merged => "merged",
        github::MergePrOutcome::Enqueued => "enqueued",
    };
    serde_json::json!({
        "cmd": "merge",
        "branch": outcome.branch,
        "pr_number": outcome.pr_number,
        "status": status,
        "method": method,
        "stack": stack,
        "restacked": outcome.restacked,
        "pushed_branches": outcome.pushed,
        "worktree": outcome.worktree,
    })
}

fn native_merge_receipt_value(outcome: &NativeMergeOutcome, method: &str) -> serde_json::Value {
    let status = match outcome.status {
        github::MergePrOutcome::Merged => "merged",
        github::MergePrOutcome::Enqueued => "enqueued",
    };
    serde_json::json!({
        "cmd": "merge",
        "mode": "native_stack",
        "status": status,
        "method": method,
        "stack": true,
        "branches": outcome.branches,
        "pr_numbers": outcome.pr_numbers,
        "merged_pr_number": outcome.merged_pr_number,
    })
}

fn merge_targets(
    state: &StackState,
    current: &str,
    stack: bool,
    repo: Option<&str>,
) -> Result<Vec<MergeTarget>> {
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
            let title = github::get_pr_status(&pr_number.to_string(), repo)?
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

fn exact_native_stack_match(targets: &[MergeTarget], repo: Option<&str>) -> Result<bool> {
    if targets.len() < 2 {
        return Ok(false);
    }
    let pr_numbers = targets
        .iter()
        .map(|target| target.pr_number)
        .collect::<Vec<_>>();
    let top_pr = *pr_numbers.last().expect("non-empty pr numbers");
    Ok(github::native_stack_for_pr(top_pr, repo)?
        .is_some_and(|native| native.pull_requests == pr_numbers))
}

fn move_to_main_root_for_targets(
    targets: &[MergeTarget],
    worktree_map: &HashMap<String, String>,
    current_dir: &str,
    main_root: &str,
) -> Result<Option<String>> {
    let should_move = targets.iter().any(|target| {
        worktree_map
            .get(&target.branch)
            .is_some_and(|path| inside_worktree_path(current_dir, path))
    });
    if should_move {
        std::env::set_current_dir(main_root)?;
        println!("{main_root}");
        return Ok(Some(main_root.to_string()));
    }
    Ok(None)
}

fn cleanup_merged_branch(
    state: &StackState,
    branch: &str,
    linked_worktree: Option<&str>,
    trunk: &str,
) -> Result<()> {
    if let Some(path) = linked_worktree {
        git::worktree_remove(path)?;
        ui::info(&format!("Removed worktree at `{path}`"));
    }

    if git::branch_exists(branch) {
        if git::current_branch()? == branch {
            git::checkout(trunk)?;
        }
        let _ = git::delete_branch(branch, true);
    }

    let _ = git::delete_remote_branch(&state.remote, branch);
    Ok(())
}

fn fetch_restack_and_push_remaining(
    state: &mut StackState,
    fetch_remote: &str,
    push_remote: &str,
) -> Result<(usize, Vec<String>)> {
    let sp = ui::spinner("Fetching latest changes...");
    git::fetch(fetch_remote)?;
    sp.finish_and_clear();

    let current_root = git::repo_root()?;
    let current_branch = git::current_branch()?;

    // The shared restack engine reads local refs, so bring local trunk up to the tip that was
    // just merged before asking it what has drifted.
    match git::update_branch_to_latest_remote(
        fetch_remote,
        &state.trunk,
        &current_branch,
        &current_root,
    ) {
        Ok(true) => ui::info(&format!("Updated `{}` to latest", state.trunk)),
        Ok(false) => {}
        Err(e) => ui::warn(&format!("Could not update `{}` — {e}", state.trunk)),
    }

    let order = state.topo_order();
    let report = restack::restack_branches_with_options(
        &mut *state,
        &order,
        &current_root,
        "merge",
        restack::RestackOptions::default(),
    );

    for branch_name in &report.restacked_branches {
        let sp = ui::spinner(&format!("Pushing restacked `{branch_name}` after merge..."));
        git::fetch_branch(push_remote, branch_name)?;
        git::push(push_remote, branch_name, true)?;
        sp.finish_and_clear();
        ui::info(&format!("Pushed `{branch_name}`"));
    }

    state.save()?;

    // The merge itself is done and recorded. Branches that could not be replayed are reported
    // individually and surface as exit 3, exactly as they do for `ez sync` and `ez restack`.
    if !report.is_clean() {
        bail!(restack::incomplete_error("merge", &report));
    }

    Ok((report.restacked, report.restacked_branches))
}

fn reparent_external_children_after_native_merge(
    state: &mut StackState,
    targets: &[MergeTarget],
) -> Result<Vec<String>> {
    let target_branches = targets
        .iter()
        .map(|target| target.branch.clone())
        .collect::<HashSet<_>>();
    let trunk = state.trunk.clone();
    let mut reparented = Vec::new();

    for target in targets {
        for child_name in state.children_of(&target.branch) {
            if target_branches.contains(&child_name) {
                continue;
            }
            let child = state.get_branch_mut(&child_name)?;
            child.parent = trunk.clone();
            reparented.push(child_name);
        }
    }

    reparented.sort();
    reparented.dedup();
    Ok(reparented)
}

fn merge_native_stack(
    state: &mut StackState,
    targets: &[MergeTarget],
    method: &str,
    worktree_map: &HashMap<String, String>,
    current_dir: &str,
    main_root: &str,
    repo: Option<&str>,
) -> Result<NativeMergeOutcome> {
    let trunk = state.trunk.clone();
    let top = targets
        .last()
        .ok_or_else(|| anyhow::anyhow!("native merge requires at least one target"))?;
    let branches = targets
        .iter()
        .map(|target| target.branch.clone())
        .collect::<Vec<_>>();
    let pr_numbers = targets
        .iter()
        .map(|target| target.pr_number)
        .collect::<Vec<_>>();

    let sp = ui::spinner(&format!(
        "Merging native stack via PR #{}...",
        top.pr_number
    ));
    let status = github::merge_native_stack_pr(top.pr_number, method, repo)?;
    sp.finish_and_clear();

    if status == github::MergePrOutcome::Enqueued {
        ui::info(&format!(
            "Native stack merge enqueued via PR #{}; local branches and worktrees preserved",
            top.pr_number
        ));
        return Ok(NativeMergeOutcome {
            status,
            branches,
            pr_numbers,
            merged_pr_number: top.pr_number,
        });
    }

    ui::info(&format!(
        "Merged native stack via PR #{} ({})",
        top.pr_number,
        branches.join(", ")
    ));

    let reparented = reparent_external_children_after_native_merge(state, targets)?;
    for child_name in &reparented {
        ui::info(&format!("Reparented `{child_name}` onto `{trunk}`"));
        if let Some(child_pr) = state.get_branch(child_name)?.pr_number
            && let Err(e) = github::update_pr_base(child_pr, &trunk, repo)
        {
            ui::warn(&format!("Failed to update PR base for `{child_name}`: {e}"));
        }
    }

    for target in targets {
        state.remove_branch(&target.branch);
    }
    state.save()?;

    move_to_main_root_for_targets(targets, worktree_map, current_dir, main_root)?;

    // Same ordering rule as the sequential path: the merged branches are cleaned up before the
    // restack, so a restack failure cannot strand them without metadata.
    for target in targets {
        cleanup_merged_branch(
            state,
            &target.branch,
            worktree_map.get(&target.branch).map(String::as_str),
            &trunk,
        )?;
    }

    let fetch_remote = state.fetch_remote().to_string();
    let push_remote = state.remote.clone();
    let (restacked, pushed) = fetch_restack_and_push_remaining(state, &fetch_remote, &push_remote)?;
    if restacked > 0 {
        ui::info(&format!("Restacked {restacked} branch(es)"));
    }
    if !pushed.is_empty() {
        ui::info(&format!(
            "Updated {} remote branch(es) after restack",
            pushed.len()
        ));
    }

    Ok(NativeMergeOutcome {
        status,
        branches,
        pr_numbers,
        merged_pr_number: top.pr_number,
    })
}

#[allow(clippy::too_many_arguments)]
fn merge_branch(
    state: &mut StackState,
    branch: &str,
    pr_number: u64,
    method: &str,
    linked_worktree: Option<&str>,
    current_dir: &str,
    main_root: &str,
    repo: Option<&str>,
) -> Result<MergeOutcome> {
    let trunk = state.trunk.clone();
    let fetch_remote = state.fetch_remote().to_string();
    let push_remote = state.remote.clone();

    let sp = ui::spinner(&format!("Merging PR #{pr_number}..."));
    let status = github::merge_pr(pr_number, method, repo)?;
    sp.finish_and_clear();

    if status == github::MergePrOutcome::Enqueued {
        ui::info(&format!(
            "Merge enqueued for PR #{pr_number} (`{branch}`); local branch and worktree preserved"
        ));
        return Ok(MergeOutcome {
            branch: branch.to_string(),
            pr_number,
            status,
            restacked: 0,
            pushed: Vec::new(),
            worktree: linked_worktree.map(str::to_string),
        });
    }

    ui::info(&format!("Merged PR #{pr_number} for `{branch}`"));

    let children = state.reparent_children_preserving_parent_head(branch, &trunk)?;
    for child_name in &children {
        ui::info(&format!("Reparented `{child_name}` onto `{trunk}`"));

        if let Some(child_pr) = state.get_branch(child_name)?.pr_number
            && let Err(e) = github::update_pr_base(child_pr, &trunk, repo)
        {
            ui::warn(&format!("Failed to update PR base for `{child_name}`: {e}"));
        }
    }

    state.remove_branch(branch);
    state.save()?;

    move_to_main_root_for_targets(
        &[MergeTarget {
            branch: branch.to_string(),
            pr_number,
            title: String::new(),
        }],
        &linked_worktree
            .map(|path| HashMap::from([(branch.to_string(), path.to_string())]))
            .unwrap_or_default(),
        current_dir,
        main_root,
    )?;

    // Clean up before restacking the rest of the stack. The branch is already merged and its
    // stack entry is already gone, so nothing about deleting it depends on whether some sibling
    // rebases cleanly — and if the restack fails, a `?` here would strand the branch and its
    // worktree with no metadata left for `ez sync` to find them by.
    cleanup_merged_branch(state, branch, linked_worktree, &trunk)?;

    let (restacked, restacked_for_push) =
        fetch_restack_and_push_remaining(state, &fetch_remote, &push_remote)?;

    Ok(MergeOutcome {
        branch: branch.to_string(),
        pr_number,
        status,
        restacked,
        pushed: restacked_for_push,
        worktree: linked_worktree.map(str::to_string),
    })
}

pub fn run(method: &str, yes: bool, stack: bool) -> Result<()> {
    let _lease_guard = crate::worktree_lease::LeaseMutationGuard::acquire("merge worktree stack")?;
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

    let repo = state.repo.clone();
    let repo = repo.as_deref();
    let targets = merge_targets(&state, &current, stack, repo)?;
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

    let worktree_map = linked_worktree_map()?;
    preflight_clean_linked_worktrees(&targets, &worktree_map)?;

    let main_root = git::main_worktree_root()?;
    let current_dir = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_default();

    if stack && !state.is_fork_workflow() && exact_native_stack_match(&targets, repo)? {
        let outcome = merge_native_stack(
            &mut state,
            &targets,
            method,
            &worktree_map,
            &current_dir,
            &main_root,
            repo,
        )?;
        let enqueued = outcome.status == github::MergePrOutcome::Enqueued;
        let merged_count = outcome.branches.len();
        ui::receipt(&native_merge_receipt_value(&outcome, method));
        if enqueued {
            ui::success(&format!(
                "Native stack merge enqueued via PR #{}",
                outcome.merged_pr_number
            ));
        } else {
            ui::success(&format!(
                "Merged {merged_count} PR(s) from the native stack"
            ));
        }
        return Ok(());
    }

    let mut total_restacked = 0;
    let mut total_pushed = 0usize;
    let mut enqueued = None::<String>;

    for target in &targets {
        let outcome = merge_branch(
            &mut state,
            &target.branch,
            target.pr_number,
            method,
            worktree_map.get(&target.branch).map(String::as_str),
            &current_dir,
            &main_root,
            repo,
        )?;
        total_restacked += outcome.restacked;
        total_pushed += outcome.pushed.len();
        ui::receipt(&merge_receipt_value(&outcome, method, stack));
        if outcome.status == github::MergePrOutcome::Enqueued {
            enqueued = Some(outcome.branch);
            break;
        }
    }

    if total_restacked > 0 {
        ui::info(&format!("Restacked {total_restacked} branch(es)"));
    }
    if total_pushed > 0 {
        ui::info(&format!(
            "Updated {total_pushed} remote branch(es) after restack"
        ));
    }

    if let Some(branch) = enqueued {
        ui::success(&format!("Merge enqueued for `{branch}`"));
    } else if stack {
        ui::success(&format!(
            "Merged {} PR(s) from the current stack",
            targets.len()
        ));
    } else {
        ui::success("Merge complete");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        CwdGuard, PathGuard, cmd_output, init_git_repo, install_fake_bin, run_cmd, take_env_lock,
        write_file,
    };
    use std::path::{Path, PathBuf};

    struct MergeRepo {
        repo: PathBuf,
        worktree: PathBuf,
    }

    struct StackMergeRepo {
        repo: PathBuf,
        first_worktree: PathBuf,
        second_worktree: PathBuf,
        first_branch: String,
        second_branch: String,
        first_pr: u64,
        second_pr: u64,
    }

    struct NativeSideBranchRepo {
        repo: PathBuf,
        first_worktree: PathBuf,
        second_worktree: PathBuf,
        side_worktree: PathBuf,
        first_branch: String,
        second_branch: String,
        side_branch: String,
        first_pr: u64,
        second_pr: u64,
        landed_head: String,
    }

    fn install_logging_fake_gh(prefix: &str, pr_number: u64) -> (PathBuf, PathBuf) {
        install_logging_fake_gh_with_status(prefix, pr_number, "merged")
    }

    fn install_logging_fake_gh_with_status(
        prefix: &str,
        pr_number: u64,
        status: &str,
    ) -> (PathBuf, PathBuf) {
        let log = crate::test_support::temp_dir(prefix).join("gh.log");
        let script = format!(
            r#"#!/bin/sh
echo "$@" >> "$GH_LOG"
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  printf '{{"number":{},"url":"https://github.com/org/repo/pull/{}","state":"OPEN","title":"Feature","isDraft":false,"mergedAt":null,"baseRefName":"main"}}\n'
  exit 0
fi
if [ "$1" = "api" ]; then
  if [ "$2" = "-X" ] && [ "$3" = "PUT" ]; then
    cat >/dev/null
    printf '{{"status":"{}"}}\n'
    exit 0
  fi
  printf '[]\n'
  exit 0
fi
exit 0
"#,
            pr_number, pr_number, status
        );
        (install_fake_bin(prefix, "gh", &script), log)
    }

    fn install_native_stack_fake_gh(
        prefix: &str,
        lookup_pr: u64,
        native_prs: &[u64],
        status: &str,
    ) -> (PathBuf, PathBuf) {
        let log = crate::test_support::temp_dir(prefix).join("gh.log");
        let stack_json = if native_prs.is_empty() {
            "[]".to_string()
        } else {
            let pull_requests = native_prs
                .iter()
                .map(|pr| format!(r#"{{"number":{pr}}}"#))
                .collect::<Vec<_>>()
                .join(",");
            format!(r#"[{{"number":88,"pull_requests":[{pull_requests}]}}]"#)
        };
        let script = format!(
            r#"#!/bin/sh
echo "$@" >> "$GH_LOG"
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  printf '{{"number":0,"url":"https://github.com/org/repo/pull/0","state":"OPEN","title":"Feature","isDraft":false,"mergedAt":null,"baseRefName":"main"}}\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request={}" ]; then
  printf '{}\n'
  exit 0
fi
if [ "$1" = "api" ]; then
  if [ "$2" = "-X" ] && [ "$3" = "PUT" ]; then
    cat >/dev/null
    printf '{{"status":"{}"}}\n'
    exit 0
  fi
  printf '[]\n'
  exit 0
fi
exit 0
"#,
            lookup_pr, stack_json, status
        );
        (install_fake_bin(prefix, "gh", &script), log)
    }

    fn init_merge_repo(name: &str, branch: &str, pr_number: u64) -> MergeRepo {
        let repo = init_git_repo(name);
        let bare = crate::test_support::temp_dir(&format!("{name}-remote")).join("origin.git");
        std::fs::create_dir_all(&bare).expect("create bare remote dir");
        run_cmd(&bare, "git", &["init", "--bare"]);
        run_cmd(
            &repo,
            "git",
            &["remote", "add", "origin", bare.to_str().expect("bare path")],
        );
        run_cmd(&repo, "git", &["push", "-u", "origin", "main"]);

        run_cmd(&repo, "git", &["checkout", "-b", branch]);
        write_file(&repo, "feature.txt", "feature\n");
        run_cmd(&repo, "git", &["add", "feature.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "feature"]);
        run_cmd(&repo, "git", &["push", "-u", "origin", branch]);
        run_cmd(&repo, "git", &["checkout", "main"]);

        let _cwd = CwdGuard::enter(&repo);
        let parent_head = git::rev_parse("main").expect("main head");
        let mut state = StackState::new("main".to_string());
        state.add_branch(branch, "main", &parent_head, None, None);
        state.get_branch_mut(branch).expect("branch").pr_number = Some(pr_number);
        state.save().expect("save state");

        let worktree = repo.join(".worktrees").join(branch.replace('/', "-"));
        run_cmd(
            &repo,
            "git",
            &["worktree", "add", worktree.to_str().expect("wt"), branch],
        );

        MergeRepo { repo, worktree }
    }

    fn init_stack_merge_repo(name: &str) -> StackMergeRepo {
        let repo = init_git_repo(name);
        let bare = crate::test_support::temp_dir(&format!("{name}-remote")).join("origin.git");
        std::fs::create_dir_all(&bare).expect("create bare remote dir");
        run_cmd(&bare, "git", &["init", "--bare"]);
        run_cmd(
            &repo,
            "git",
            &["remote", "add", "origin", bare.to_str().expect("bare path")],
        );
        run_cmd(&repo, "git", &["push", "-u", "origin", "main"]);

        let first_branch = "feat/native-a".to_string();
        let second_branch = "feat/native-b".to_string();
        let first_pr = 101;
        let second_pr = 102;

        run_cmd(&repo, "git", &["checkout", "-b", &first_branch]);
        write_file(&repo, "a.txt", "a\n");
        run_cmd(&repo, "git", &["add", "a.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "a"]);
        run_cmd(&repo, "git", &["push", "-u", "origin", &first_branch]);

        run_cmd(&repo, "git", &["checkout", "-b", &second_branch]);
        write_file(&repo, "b.txt", "b\n");
        run_cmd(&repo, "git", &["add", "b.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "b"]);
        run_cmd(&repo, "git", &["push", "-u", "origin", &second_branch]);
        run_cmd(&repo, "git", &["checkout", "main"]);

        let _cwd = CwdGuard::enter(&repo);
        let main_head = git::rev_parse("main").expect("main head");
        let first_head = git::rev_parse(&first_branch).expect("first head");
        let mut state = StackState::new("main".to_string());
        state.add_branch(&first_branch, "main", &main_head, None, None);
        state
            .get_branch_mut(&first_branch)
            .expect("first")
            .pr_number = Some(first_pr);
        state.add_branch(&second_branch, &first_branch, &first_head, None, None);
        state
            .get_branch_mut(&second_branch)
            .expect("second")
            .pr_number = Some(second_pr);
        state.save().expect("save state");

        let first_worktree = repo.join(".worktrees").join(first_branch.replace('/', "-"));
        let second_worktree = repo
            .join(".worktrees")
            .join(second_branch.replace('/', "-"));
        run_cmd(
            &repo,
            "git",
            &[
                "worktree",
                "add",
                first_worktree.to_str().expect("first wt"),
                &first_branch,
            ],
        );
        run_cmd(
            &repo,
            "git",
            &[
                "worktree",
                "add",
                second_worktree.to_str().expect("second wt"),
                &second_branch,
            ],
        );

        StackMergeRepo {
            repo,
            first_worktree,
            second_worktree,
            first_branch,
            second_branch,
            first_pr,
            second_pr,
        }
    }

    fn init_native_side_branch_repo(name: &str) -> NativeSideBranchRepo {
        let repo = init_git_repo(name);
        let bare = crate::test_support::temp_dir(&format!("{name}-remote")).join("origin.git");
        std::fs::create_dir_all(&bare).expect("create bare remote dir");
        run_cmd(&bare, "git", &["init", "--bare"]);
        run_cmd(
            &repo,
            "git",
            &["remote", "add", "origin", bare.to_str().expect("bare path")],
        );
        run_cmd(&repo, "git", &["push", "-u", "origin", "main"]);

        let first_branch = "feat/native-a".to_string();
        let second_branch = "feat/native-b".to_string();
        let side_branch = "feat/native-c".to_string();
        let first_pr = 201;
        let second_pr = 202;

        run_cmd(&repo, "git", &["checkout", "-b", &first_branch]);
        write_file(&repo, "a.txt", "a\n");
        run_cmd(&repo, "git", &["add", "a.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "a"]);
        run_cmd(&repo, "git", &["push", "-u", "origin", &first_branch]);
        let first_head = cmd_output(&repo, "git", &["rev-parse", &first_branch]);

        run_cmd(&repo, "git", &["checkout", "-b", &second_branch]);
        write_file(&repo, "b.txt", "b\n");
        run_cmd(&repo, "git", &["add", "b.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "b"]);
        run_cmd(&repo, "git", &["push", "-u", "origin", &second_branch]);
        let second_head = cmd_output(&repo, "git", &["rev-parse", &second_branch]);

        run_cmd(&repo, "git", &["checkout", &first_branch]);
        run_cmd(&repo, "git", &["checkout", "-b", &side_branch]);
        write_file(&repo, "c.txt", "c\n");
        run_cmd(&repo, "git", &["add", "c.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "c"]);
        run_cmd(&repo, "git", &["push", "-u", "origin", &side_branch]);

        run_cmd(&repo, "git", &["checkout", "main"]);
        run_cmd(&repo, "git", &["merge", "--ff-only", &second_head]);
        let landed_head = cmd_output(&repo, "git", &["rev-parse", "main"]);
        run_cmd(&repo, "git", &["push", "origin", "main"]);
        run_cmd(&repo, "git", &["checkout", "main"]);
        run_cmd(&repo, "git", &["reset", "--hard", "origin/main~2"]);

        let _cwd = CwdGuard::enter(&repo);
        let main_head = git::rev_parse("main").expect("main head");
        let mut state = StackState::new("main".to_string());
        state.add_branch(&first_branch, "main", &main_head, None, None);
        state
            .get_branch_mut(&first_branch)
            .expect("first")
            .pr_number = Some(first_pr);
        state.add_branch(&second_branch, &first_branch, &first_head, None, None);
        state
            .get_branch_mut(&second_branch)
            .expect("second")
            .pr_number = Some(second_pr);
        state.add_branch(&side_branch, &first_branch, &first_head, None, None);
        state.save().expect("save state");

        let first_worktree = repo.join(".worktrees").join(first_branch.replace('/', "-"));
        let second_worktree = repo
            .join(".worktrees")
            .join(second_branch.replace('/', "-"));
        let side_worktree = repo.join(".worktrees").join(side_branch.replace('/', "-"));
        run_cmd(
            &repo,
            "git",
            &[
                "worktree",
                "add",
                first_worktree.to_str().expect("first wt"),
                &first_branch,
            ],
        );
        run_cmd(
            &repo,
            "git",
            &[
                "worktree",
                "add",
                second_worktree.to_str().expect("second wt"),
                &second_branch,
            ],
        );
        run_cmd(
            &repo,
            "git",
            &[
                "worktree",
                "add",
                side_worktree.to_str().expect("side wt"),
                &side_branch,
            ],
        );

        NativeSideBranchRepo {
            repo,
            first_worktree,
            second_worktree,
            side_worktree,
            first_branch,
            second_branch,
            side_branch,
            first_pr,
            second_pr,
            landed_head,
        }
    }

    fn gh_log_contains_put(log: &Path) -> bool {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .any(|line| line.contains("api -X PUT"))
    }

    fn gh_log_put_count(log: &Path) -> usize {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("api -X PUT"))
            .count()
    }

    fn remote_branch_exists(repo: &Path, branch: &str) -> bool {
        !cmd_output(repo, "git", &["ls-remote", "--heads", "origin", branch]).is_empty()
    }

    fn local_branch_exists(repo: &Path, branch: &str) -> bool {
        !cmd_output(repo, "git", &["branch", "--list", branch]).is_empty()
    }

    fn install_merge_fold_extra_pr_edit_failing_gh(prefix: &str) -> (PathBuf, PathBuf) {
        let log = crate::test_support::temp_dir(prefix).join("gh.log");
        let script = r#"#!/bin/sh
echo "$@" >> "$GH_LOG"
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  printf '{"number":%s,"url":"https://github.com/org/repo/pull/%s","state":"OPEN","title":"Feature %s","isDraft":false,"mergedAt":null,"baseRefName":"main"}\n' "$3" "$3" "$3"
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ]; then
  echo "simulated pr edit failure" >&2
  exit 1
fi
if [ "$1" = "api" ]; then
  if [ "$2" = "-X" ] && [ "$3" = "PUT" ]; then
    cat >/dev/null
    printf '{"status":"merged"}\n'
    exit 0
  fi
  printf '[]\n'
  exit 0
fi
exit 0
"#;
        (install_fake_bin(prefix, "gh", script), log)
    }

    #[test]
    fn merge_fold_extra_preflight_skips_targets_without_linked_worktrees() {
        let targets = vec![MergeTarget {
            branch: "feat/no-worktree".to_string(),
            pr_number: 7,
            title: "No worktree".to_string(),
        }];
        let worktrees = HashMap::new();

        preflight_clean_linked_worktrees(&targets, &worktrees)
            .expect("missing linked worktree should be skipped");
    }

    #[test]
    fn merge_fold_extra_merge_targets_require_pr_metadata() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/no-pr", "main", "main-sha", None, None);

        let err = match merge_targets(&state, "feat/no-pr", false, None) {
            Ok(_) => panic!("missing PR should abort"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("has no associated PR"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn merge_fold_extra_single_target_is_not_native_stack_match() {
        let targets = vec![MergeTarget {
            branch: "feat/one".to_string(),
            pr_number: 9,
            title: "One".to_string(),
        }];

        let matched = exact_native_stack_match(&targets, None).expect("single target check");

        assert!(!matched);
    }

    #[test]
    fn merge_fold_extra_cleanup_switches_off_current_branch_before_delete() {
        let _guard = take_env_lock();
        let repo = init_git_repo("merge-fold-extra-current-cleanup");
        run_cmd(&repo, "git", &["checkout", "-b", "feat/current"]);
        write_file(&repo, "current.txt", "current\n");
        run_cmd(&repo, "git", &["add", "current.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "current"]);
        let _cwd = CwdGuard::enter(&repo);
        let state = StackState::new("main".to_string());

        cleanup_merged_branch(&state, "feat/current", None, "main").expect("cleanup current");

        assert_eq!(
            cmd_output(&repo, "git", &["branch", "--show-current"]),
            "main"
        );
        assert!(!local_branch_exists(&repo, "feat/current"));
    }

    #[test]
    fn merge_fold_extra_reparented_child_pr_base_failure_warns_but_merges_parent() {
        let _guard = take_env_lock();
        let parent_branch = "feat/reparent-parent";
        let child_branch = "feat/reparent-child";
        let parent_pr = 61;
        let child_pr = 62;
        let merge_repo = init_merge_repo(
            "merge-fold-extra-child-pr-edit-failure",
            parent_branch,
            parent_pr,
        );
        run_cmd(
            &merge_repo.repo,
            "git",
            &["checkout", "-b", child_branch, parent_branch],
        );
        write_file(&merge_repo.repo, "child.txt", "child\n");
        run_cmd(&merge_repo.repo, "git", &["add", "child.txt"]);
        run_cmd(&merge_repo.repo, "git", &["commit", "-m", "child"]);
        run_cmd(
            &merge_repo.repo,
            "git",
            &["push", "-u", "origin", child_branch],
        );
        run_cmd(&merge_repo.repo, "git", &["checkout", "main"]);
        let (fake_dir, gh_log) =
            install_merge_fold_extra_pr_edit_failing_gh("merge-fold-extra-child-pr-edit-gh");
        let _path = PathGuard::install(&fake_dir);
        unsafe {
            std::env::set_var("GH_LOG", &gh_log);
        }
        let _cwd = CwdGuard::enter(&merge_repo.worktree);
        let parent_head = git::rev_parse(parent_branch).expect("parent head");
        let mut state = StackState::load().expect("load state");
        state.add_branch(child_branch, parent_branch, &parent_head, None, None);
        state.get_branch_mut(child_branch).expect("child").pr_number = Some(child_pr);
        state.save().expect("save child state");

        run("squash", true, false).expect("parent merge should complete");

        let log = std::fs::read_to_string(&gh_log).expect("gh log");
        assert!(
            log.contains("pr edit 62 --base main"),
            "child PR base update should be attempted: {log}"
        );
        assert!(!local_branch_exists(&merge_repo.repo, parent_branch));
        assert!(local_branch_exists(&merge_repo.repo, child_branch));
        let state = {
            let _main_cwd = CwdGuard::enter(&merge_repo.repo);
            StackState::load().expect("load state")
        };
        let child = state.get_branch(child_branch).expect("child state");
        assert_eq!(child.parent, "main");
    }

    #[test]
    fn merge_fold_extra_fetch_restack_skips_up_to_date_managed_parent() {
        let _guard = take_env_lock();
        let repo = init_git_repo("merge-fold-extra-fetch-skip");
        let bare =
            crate::test_support::temp_dir("merge-fold-extra-fetch-skip-remote").join("origin.git");
        std::fs::create_dir_all(&bare).expect("create bare remote dir");
        run_cmd(&bare, "git", &["init", "--bare"]);
        run_cmd(
            &repo,
            "git",
            &["remote", "add", "origin", bare.to_str().expect("bare path")],
        );
        run_cmd(&repo, "git", &["push", "-u", "origin", "main"]);
        run_cmd(&repo, "git", &["checkout", "-b", "feat/base"]);
        write_file(&repo, "base.txt", "base\n");
        run_cmd(&repo, "git", &["add", "base.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "base"]);
        run_cmd(&repo, "git", &["push", "-u", "origin", "feat/base"]);
        run_cmd(&repo, "git", &["checkout", "-b", "feat/child"]);
        write_file(&repo, "child.txt", "child\n");
        run_cmd(&repo, "git", &["add", "child.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "child"]);
        run_cmd(&repo, "git", &["push", "-u", "origin", "feat/child"]);
        run_cmd(&repo, "git", &["checkout", "main"]);
        let _cwd = CwdGuard::enter(&repo);
        let main_head = git::rev_parse("main").expect("main head");
        let base_head = git::rev_parse("feat/base").expect("base head");
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/base", "main", &main_head, None, None);
        state.add_branch("feat/child", "feat/base", &base_head, None, None);

        let (restacked, pushed) =
            fetch_restack_and_push_remaining(&mut state, "origin", "origin").expect("fetch skip");

        assert_eq!(restacked, 0);
        assert!(pushed.is_empty());
        assert_eq!(
            state.get_branch("feat/child").expect("child").parent_head,
            base_head
        );
    }

    #[test]
    fn fetch_restack_survives_a_stack_entry_whose_branch_is_gone_from_git() {
        let _guard = take_env_lock();
        let repo = init_git_repo("merge-fetch-restack-missing-branch");
        let bare = crate::test_support::temp_dir("merge-fetch-restack-missing-branch-remote")
            .join("origin.git");
        std::fs::create_dir_all(&bare).expect("create bare remote dir");
        run_cmd(&bare, "git", &["init", "--bare"]);
        run_cmd(
            &repo,
            "git",
            &["remote", "add", "origin", bare.to_str().expect("bare path")],
        );
        run_cmd(&repo, "git", &["push", "-u", "origin", "main"]);
        run_cmd(&repo, "git", &["checkout", "-b", "feat/live"]);
        write_file(&repo, "live.txt", "live\n");
        run_cmd(&repo, "git", &["add", "live.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "live"]);
        run_cmd(&repo, "git", &["push", "-u", "origin", "feat/live"]);
        run_cmd(&repo, "git", &["checkout", "main"]);
        let _cwd = CwdGuard::enter(&repo);

        let main_head = git::rev_parse("main").expect("main head");
        let mut state = StackState::new("main".to_string());
        // A stack entry left behind for a branch that no longer exists in git — deleted by hand,
        // or cleaned up in another worktree. Metadata is a cache, so this must not be fatal.
        state.add_branch("feat/ghost", "main", &main_head, None, None);
        state.add_branch("feat/live", "main", &main_head, None, None);

        // Advance trunk so there is real restack work to do behind the ghost entry.
        write_file(&repo, "trunk.txt", "trunk\n");
        run_cmd(&repo, "git", &["add", "trunk.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "trunk moves"]);
        run_cmd(&repo, "git", &["push", "origin", "main"]);
        let main_head2 = git::rev_parse("main").expect("main head 2");

        let (restacked, pushed) = fetch_restack_and_push_remaining(&mut state, "origin", "origin")
            .expect("a missing branch must not fail the post-merge restack");

        assert_eq!(restacked, 1, "the live branch should still be restacked");
        assert_eq!(pushed, vec!["feat/live".to_string()]);
        assert!(git::is_ancestor("main", "feat/live"));
        assert_eq!(
            state.get_branch("feat/live").expect("live").parent_head,
            main_head2
        );
    }

    #[test]
    fn merge_receipt_includes_removed_worktree_when_present() {
        let outcome = MergeOutcome {
            branch: "feat/linked".to_string(),
            pr_number: 42,
            status: github::MergePrOutcome::Merged,
            restacked: 0,
            pushed: Vec::new(),
            worktree: Some("/repo/.worktrees/feat-linked".to_string()),
        };

        let receipt = merge_receipt_value(&outcome, "squash", false);

        assert_eq!(receipt["cmd"], "merge");
        assert_eq!(receipt["branch"], "feat/linked");
        assert_eq!(receipt["status"], "merged");
        assert_eq!(receipt["worktree"], "/repo/.worktrees/feat-linked");
    }

    #[test]
    fn merge_from_current_linked_worktree_cleans_up_worktree_branch_and_state() {
        let _guard = take_env_lock();
        let branch = "feat/linked";
        let pr_number = 42;
        let merge_repo = init_merge_repo("merge-linked-clean", branch, pr_number);
        let (fake_dir, gh_log) = install_logging_fake_gh("merge-linked-clean-gh", pr_number);
        let _path = PathGuard::install(&fake_dir);
        unsafe {
            std::env::set_var("GH_LOG", &gh_log);
        }
        let _cwd = CwdGuard::enter(&merge_repo.worktree);

        run("squash", true, false).expect("merge should succeed");

        assert!(
            gh_log_contains_put(&gh_log),
            "GitHub merge PUT should occur"
        );
        assert_eq!(
            std::fs::canonicalize(std::env::current_dir().expect("cwd")).expect("canonical cwd"),
            std::fs::canonicalize(&merge_repo.repo).expect("canonical repo")
        );
        assert!(
            !merge_repo.worktree.exists(),
            "linked worktree should be removed"
        );
        assert_eq!(
            cmd_output(&merge_repo.repo, "git", &["branch", "--list", branch]),
            "",
            "local branch should be deleted"
        );
        assert!(
            !remote_branch_exists(&merge_repo.repo, branch),
            "remote branch should be deleted after merged cleanup"
        );
        let state = {
            let _main_cwd = CwdGuard::enter(&merge_repo.repo);
            StackState::load().expect("load state")
        };
        assert!(
            !state.branches.contains_key(branch),
            "branch should be removed from stack state"
        );
    }

    #[test]
    fn merged_branch_is_cleaned_up_even_when_the_post_merge_restack_fails() {
        let _guard = take_env_lock();
        let branch = "feat/linked";
        let pr_number = 42;
        let merge_repo = init_merge_repo("merge-cleanup-before-restack", branch, pr_number);

        // A sibling that cannot be replayed onto the new trunk: both touch the same line.
        run_cmd(&merge_repo.repo, "git", &["checkout", "main"]);
        let main_head = {
            let _cwd = CwdGuard::enter(&merge_repo.repo);
            git::rev_parse("main").expect("main head")
        };
        run_cmd(&merge_repo.repo, "git", &["checkout", "-b", "feat/sibling"]);
        write_file(&merge_repo.repo, "clash.txt", "sibling\n");
        run_cmd(&merge_repo.repo, "git", &["add", "clash.txt"]);
        run_cmd(&merge_repo.repo, "git", &["commit", "-m", "sibling"]);
        run_cmd(
            &merge_repo.repo,
            "git",
            &["push", "-u", "origin", "feat/sibling"],
        );

        run_cmd(&merge_repo.repo, "git", &["checkout", "main"]);
        write_file(&merge_repo.repo, "clash.txt", "trunk\n");
        run_cmd(&merge_repo.repo, "git", &["add", "clash.txt"]);
        run_cmd(&merge_repo.repo, "git", &["commit", "-m", "trunk clash"]);
        run_cmd(&merge_repo.repo, "git", &["push", "origin", "main"]);

        {
            let _cwd = CwdGuard::enter(&merge_repo.repo);
            let mut state = StackState::load().expect("load state");
            state.add_branch("feat/sibling", "main", &main_head, None, None);
            state.save().expect("save state");
        }

        let (fake_dir, gh_log) =
            install_logging_fake_gh("merge-cleanup-before-restack-gh", pr_number);
        let _path = PathGuard::install(&fake_dir);
        unsafe {
            std::env::set_var("GH_LOG", &gh_log);
        }
        let _cwd = CwdGuard::enter(&merge_repo.worktree);

        let result = run("squash", true, false);

        assert!(
            result.is_err(),
            "the conflicting sibling should still surface as a restack failure"
        );

        // ...but the merge itself is done, so its branch must not be left behind with no stack
        // entry to find it by. That combination is unrecoverable: `ez sync` only walks tracked
        // branches, so an orphan here would never be cleaned up again.
        assert!(
            !merge_repo.worktree.exists(),
            "merged branch's worktree should be removed despite the restack failure"
        );
        assert_eq!(
            cmd_output(&merge_repo.repo, "git", &["branch", "--list", branch]),
            "",
            "merged branch should be deleted despite the restack failure"
        );
        let state = {
            let _main_cwd = CwdGuard::enter(&merge_repo.repo);
            StackState::load().expect("load state")
        };
        assert!(
            !state.branches.contains_key(branch),
            "merged branch should be out of stack state"
        );
    }

    #[test]
    fn merge_rejects_dirty_target_worktree_before_github_merge() {
        let _guard = take_env_lock();
        let branch = "feat/dirty";
        let pr_number = 43;
        let merge_repo = init_merge_repo("merge-linked-dirty", branch, pr_number);
        write_file(&merge_repo.worktree, "dirty.txt", "dirty\n");
        let (fake_dir, gh_log) = install_logging_fake_gh("merge-linked-dirty-gh", pr_number);
        let _path = PathGuard::install(&fake_dir);
        unsafe {
            std::env::set_var("GH_LOG", &gh_log);
        }
        let _cwd = CwdGuard::enter(&merge_repo.worktree);

        let err = run("squash", true, false).expect_err("dirty worktree should abort");
        let message = err.to_string();
        assert!(
            message.contains(branch),
            "error should name branch: {message}"
        );
        assert!(
            message.contains(merge_repo.worktree.to_str().expect("wt path")),
            "error should name worktree path: {message}"
        );
        assert!(
            message.contains("commit/stash") || message.contains("clean"),
            "error should tell user how to proceed: {message}"
        );
        assert!(
            !gh_log_contains_put(&gh_log),
            "dirty preflight must abort before GitHub merge PUT"
        );
        assert!(merge_repo.worktree.exists(), "worktree should remain");
        assert_ne!(
            cmd_output(&merge_repo.repo, "git", &["branch", "--list", branch]),
            "",
            "local branch should remain"
        );
        let state = {
            let _main_cwd = CwdGuard::enter(&merge_repo.repo);
            StackState::load().expect("load state")
        };
        assert!(
            state.branches.contains_key(branch),
            "stack state should preserve dirty branch"
        );
    }

    #[test]
    fn merge_enqueued_preserves_current_linked_worktree_branch_state_and_remote() {
        let _guard = take_env_lock();
        let branch = "feat/enqueued";
        let pr_number = 44;
        let merge_repo = init_merge_repo("merge-linked-enqueued", branch, pr_number);
        let (fake_dir, gh_log) =
            install_logging_fake_gh_with_status("merge-linked-enqueued-gh", pr_number, "enqueued");
        let _path = PathGuard::install(&fake_dir);
        unsafe {
            std::env::set_var("GH_LOG", &gh_log);
        }
        let _cwd = CwdGuard::enter(&merge_repo.worktree);

        run("squash", true, false).expect("enqueued merge should succeed");

        assert_eq!(gh_log_put_count(&gh_log), 1, "one merge PUT should occur");
        assert_eq!(
            std::fs::canonicalize(std::env::current_dir().expect("cwd")).expect("canonical cwd"),
            std::fs::canonicalize(&merge_repo.worktree).expect("canonical worktree"),
            "enqueued merge should leave cwd inside the linked worktree"
        );
        assert!(
            merge_repo.worktree.exists(),
            "linked worktree should be preserved"
        );
        assert_ne!(
            cmd_output(&merge_repo.repo, "git", &["branch", "--list", branch]),
            "",
            "local branch should be preserved"
        );
        assert!(
            remote_branch_exists(&merge_repo.repo, branch),
            "remote branch should be preserved"
        );
        let state = {
            let _main_cwd = CwdGuard::enter(&merge_repo.repo);
            StackState::load().expect("load state")
        };
        assert!(
            state.branches.contains_key(branch),
            "stack state should preserve enqueued branch"
        );
    }

    #[test]
    fn native_stack_exact_match_merges_top_once_and_cleans_all_targets() {
        let _guard = take_env_lock();
        let stack_repo = init_stack_merge_repo("merge-native-exact-merged");
        let (fake_dir, gh_log) = install_native_stack_fake_gh(
            "merge-native-exact-merged-gh",
            stack_repo.second_pr,
            &[stack_repo.first_pr, stack_repo.second_pr],
            "merged",
        );
        let _path = PathGuard::install(&fake_dir);
        unsafe {
            std::env::set_var("GH_LOG", &gh_log);
        }
        let _cwd = CwdGuard::enter(&stack_repo.first_worktree);

        run("squash", true, true).expect("native stack merge should succeed");

        let log = std::fs::read_to_string(&gh_log).expect("gh log");
        assert_eq!(gh_log_put_count(&gh_log), 1, "only top PR should merge");
        assert!(
            log.contains("pulls/102/merge-async"),
            "top PR should be merged: {log}"
        );
        assert!(
            !log.contains("pulls/101/merge-async"),
            "bottom PR should not be merged separately: {log}"
        );
        assert!(
            !stack_repo.first_worktree.exists(),
            "first worktree removed"
        );
        assert!(
            !stack_repo.second_worktree.exists(),
            "second worktree removed"
        );
        assert!(!local_branch_exists(
            &stack_repo.repo,
            &stack_repo.first_branch
        ));
        assert!(!local_branch_exists(
            &stack_repo.repo,
            &stack_repo.second_branch
        ));
        assert!(!remote_branch_exists(
            &stack_repo.repo,
            &stack_repo.first_branch
        ));
        assert!(!remote_branch_exists(
            &stack_repo.repo,
            &stack_repo.second_branch
        ));
        let state = {
            let _main_cwd = CwdGuard::enter(&stack_repo.repo);
            StackState::load().expect("load state")
        };
        assert!(!state.branches.contains_key(&stack_repo.first_branch));
        assert!(!state.branches.contains_key(&stack_repo.second_branch));
    }

    #[test]
    fn native_stack_merged_restacks_side_branch_and_pushes_it() {
        let _guard = take_env_lock();
        let repo = init_native_side_branch_repo("merge-native-side-branch");
        let (fake_dir, gh_log) = install_native_stack_fake_gh(
            "merge-native-side-branch-gh",
            repo.second_pr,
            &[repo.first_pr, repo.second_pr],
            "merged",
        );
        let _path = PathGuard::install(&fake_dir);
        unsafe {
            std::env::set_var("GH_LOG", &gh_log);
        }
        let _cwd = CwdGuard::enter(&repo.second_worktree);

        run("squash", true, true).expect("native stack merge should restack side branch");

        assert_eq!(gh_log_put_count(&gh_log), 1, "only top PR should merge");
        assert!(!repo.first_worktree.exists(), "first worktree removed");
        assert!(!repo.second_worktree.exists(), "second worktree removed");
        assert!(repo.side_worktree.exists(), "side worktree remains");

        let state = {
            let _main_cwd = CwdGuard::enter(&repo.repo);
            StackState::load().expect("load state")
        };
        let side = state.get_branch(&repo.side_branch).expect("side meta");
        assert_eq!(side.parent, "main");
        assert_eq!(side.parent_head, repo.landed_head);
        assert!(!state.branches.contains_key(&repo.first_branch));
        assert!(!state.branches.contains_key(&repo.second_branch));

        run_cmd(
            &repo.repo,
            "git",
            &[
                "merge-base",
                "--is-ancestor",
                "origin/main",
                &repo.side_branch,
            ],
        );
        let local_side = cmd_output(&repo.repo, "git", &["rev-parse", &repo.side_branch]);
        let remote_side = cmd_output(
            &repo.repo,
            "git",
            &["rev-parse", &format!("origin/{}", repo.side_branch)],
        );
        assert_eq!(
            local_side, remote_side,
            "restacked side branch should be pushed"
        );
    }

    #[test]
    fn native_stack_enqueued_merges_top_once_and_preserves_all_targets() {
        let _guard = take_env_lock();
        let stack_repo = init_stack_merge_repo("merge-native-exact-enqueued");
        let (fake_dir, gh_log) = install_native_stack_fake_gh(
            "merge-native-exact-enqueued-gh",
            stack_repo.second_pr,
            &[stack_repo.first_pr, stack_repo.second_pr],
            "enqueued",
        );
        let _path = PathGuard::install(&fake_dir);
        unsafe {
            std::env::set_var("GH_LOG", &gh_log);
        }
        let _cwd = CwdGuard::enter(&stack_repo.first_worktree);

        run("squash", true, true).expect("native stack enqueue should succeed");

        let log = std::fs::read_to_string(&gh_log).expect("gh log");
        assert_eq!(gh_log_put_count(&gh_log), 1, "only top PR should enqueue");
        assert!(
            log.contains("pulls/102/merge-async"),
            "top PR should be enqueued: {log}"
        );
        assert!(stack_repo.first_worktree.exists(), "first worktree remains");
        assert!(
            stack_repo.second_worktree.exists(),
            "second worktree remains"
        );
        assert!(local_branch_exists(
            &stack_repo.repo,
            &stack_repo.first_branch
        ));
        assert!(local_branch_exists(
            &stack_repo.repo,
            &stack_repo.second_branch
        ));
        assert!(remote_branch_exists(
            &stack_repo.repo,
            &stack_repo.first_branch
        ));
        assert!(remote_branch_exists(
            &stack_repo.repo,
            &stack_repo.second_branch
        ));
        let state = {
            let _main_cwd = CwdGuard::enter(&stack_repo.repo);
            StackState::load().expect("load state")
        };
        assert!(state.branches.contains_key(&stack_repo.first_branch));
        assert!(state.branches.contains_key(&stack_repo.second_branch));
    }

    #[test]
    fn native_stack_mismatch_uses_sequential_merge_path() {
        let _guard = take_env_lock();
        let stack_repo = init_stack_merge_repo("merge-native-mismatch");
        let (fake_dir, gh_log) = install_native_stack_fake_gh(
            "merge-native-mismatch-gh",
            stack_repo.second_pr,
            &[999],
            "merged",
        );
        let _path = PathGuard::install(&fake_dir);
        unsafe {
            std::env::set_var("GH_LOG", &gh_log);
        }
        let _cwd = CwdGuard::enter(&stack_repo.first_worktree);

        run("squash", true, true).expect("sequential stack merge should succeed");

        assert_eq!(
            gh_log_put_count(&gh_log),
            2,
            "mismatched native stack should merge each target sequentially"
        );
    }

    #[test]
    fn native_stack_empty_lookup_uses_sequential_merge_path() {
        let _guard = take_env_lock();
        let stack_repo = init_stack_merge_repo("merge-native-empty");
        let (fake_dir, gh_log) = install_native_stack_fake_gh(
            "merge-native-empty-gh",
            stack_repo.second_pr,
            &[],
            "merged",
        );
        let _path = PathGuard::install(&fake_dir);
        unsafe {
            std::env::set_var("GH_LOG", &gh_log);
        }
        let _cwd = CwdGuard::enter(&stack_repo.first_worktree);

        run("squash", true, true).expect("sequential stack merge should succeed");

        assert_eq!(
            gh_log_put_count(&gh_log),
            2,
            "empty native stack lookup should merge each target sequentially"
        );
    }

    #[test]
    fn native_stack_merge_404_preserves_local_state_without_legacy_fallback() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "merge-native-async-404",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "PUT" ] && [ "$4" = "repos/org/repo/pulls/2/merge-async" ]; then
  echo "HTTP 404: Not Found" >&2
  exit 1
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "PUT" ] && [ "$4" = "repos/org/repo/pulls/2/merge" ]; then
  echo legacy >> "$EZ_FAKE_GH_LOG"
  echo '{"merged":true}'
  exit 0
fi
exit 2
"#,
        );
        let legacy_log = fake_dir.join("legacy.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &legacy_log);
        }
        let _path = PathGuard::install(&fake_dir);

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.add_branch("feat/b", "feat/a", "bbb", None, None);
        let targets = vec![
            MergeTarget {
                branch: "feat/a".to_string(),
                pr_number: 1,
                title: "A".to_string(),
            },
            MergeTarget {
                branch: "feat/b".to_string(),
                pr_number: 2,
                title: "B".to_string(),
            },
        ];

        let err = merge_native_stack(
            &mut state,
            &targets,
            "squash",
            &HashMap::new(),
            "/tmp",
            "/tmp",
            None,
        )
        .err()
        .expect("native stack merge must reject unavailable async API");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }
        assert!(err.to_string().contains("will not fall back"));
        assert!(state.branches.contains_key("feat/a"));
        assert!(state.branches.contains_key("feat/b"));
        assert!(
            !legacy_log.exists(),
            "legacy merge endpoint must not be called"
        );
    }

    #[test]
    fn merged_current_worktree_moves_to_main_before_cleanup_failure_and_persists_state() {
        let _guard = take_env_lock();
        let branch = "feat/remove-fails";
        let pr_number = 45;
        let merge_repo = init_merge_repo("merge-cleanup-failure", branch, pr_number);
        let (fake_gh_dir, gh_log) = install_logging_fake_gh("merge-cleanup-failure-gh", pr_number);
        let _gh_path = PathGuard::install(&fake_gh_dir);
        unsafe {
            std::env::set_var("GH_LOG", &gh_log);
        }

        let real_git = cmd_output(Path::new("/"), "which", &["git"]);
        let fake_git_script = format!(
            r#"#!/bin/sh
if [ "$1" = "worktree" ] && [ "$2" = "remove" ]; then
  echo "simulated worktree remove failure" >&2
  exit 1
fi
exec "{}" "$@"
"#,
            real_git
        );
        let fake_git_dir = install_fake_bin("merge-cleanup-failure-git", "git", &fake_git_script);
        let _git_path = PathGuard::install(&fake_git_dir);
        let _cwd = CwdGuard::enter(&merge_repo.worktree);

        let err = run("squash", true, false).expect_err("worktree removal should fail");

        assert!(
            err.to_string()
                .contains("simulated worktree remove failure"),
            "unexpected error: {err:#}"
        );
        assert_eq!(
            std::fs::canonicalize(std::env::current_dir().expect("cwd")).expect("canonical cwd"),
            std::fs::canonicalize(&merge_repo.repo).expect("canonical repo"),
            "process cwd should move to main before cleanup failure"
        );
        assert!(merge_repo.worktree.exists(), "failed worktree remains");
        assert!(
            local_branch_exists(&merge_repo.repo, branch),
            "local branch should remain when cleanup fails"
        );
        let state = {
            let _main_cwd = CwdGuard::enter(&merge_repo.repo);
            StackState::load().expect("load state")
        };
        assert!(
            !state.branches.contains_key(branch),
            "state removal should be persisted before destructive cleanup"
        );
    }
}
