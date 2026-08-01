use anyhow::{Result, bail};

use crate::cmd::push::{push_or_update_pr, resolve_draft};
use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

fn branches_to_submit(path_to_trunk: &[String], trunk: &str) -> Vec<String> {
    path_to_trunk
        .iter()
        .rev()
        .filter(|b| b.as_str() != trunk)
        .cloned()
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    draft: bool,
    no_draft: bool,
    title: Option<&str>,
    body: Option<&str>,
    body_file: Option<&str>,
    remote_override: Option<&str>,
    repo_override: Option<&str>,
    fork_repo_override: Option<&str>,
) -> Result<()> {
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

    let effective_draft = resolve_draft(draft, no_draft, state.draft);

    let resolved_body: Option<String> = match body_file {
        Some(path) => Some(github::body_from_file(path)?),
        None => body.map(|s| s.to_string()),
    };

    // path_to_trunk returns [current, ..., trunk].
    // We want to iterate bottom-to-top (trunk-side first), skipping trunk itself.
    let path = state.path_to_trunk(&current);
    let branches_to_submit = branches_to_submit(&path, &state.trunk);

    if branches_to_submit.is_empty() {
        ui::info("No branches to submit.");
        return Ok(());
    }

    let effective_remote = remote_override
        .map(str::to_string)
        .unwrap_or_else(|| state.remote.clone());
    let effective_repo = repo_override
        .map(str::to_string)
        .or_else(|| state.repo.clone());
    let effective_fork_repo = fork_repo_override
        .map(str::to_string)
        .or_else(|| state.fork_repo.clone());
    let is_fork_workflow = state.is_fork_workflow_with(
        Some(&effective_remote),
        effective_repo.as_deref(),
        effective_fork_repo.as_deref(),
    );
    let body_explicitly_set = body.is_some() || body_file.is_some();
    let mut pr_urls: Vec<(String, String)> = Vec::new();
    let mut pr_numbers: Vec<u64> = Vec::new();

    for branch in &branches_to_submit {
        git::fetch_branch(&effective_remote, branch)?;
    }

    let branch_refs: Vec<&str> = branches_to_submit.iter().map(String::as_str).collect();
    let sp = ui::spinner(&format!(
        "Pushing {} branch(es) atomically...",
        branch_refs.len()
    ));
    git::push_atomic(&effective_remote, &branch_refs)?;
    sp.finish_and_clear();
    ui::info(&format!("Pushed {} branch(es)", branch_refs.len()));

    for branch in &branches_to_submit {
        let parent = state.get_branch(branch)?.parent.clone();

        // Create or update the PR.
        let pr_url = push_or_update_pr(
            &mut state,
            branch,
            &parent,
            effective_draft,
            title,
            resolved_body.as_deref(),
            body_explicitly_set,
            &effective_remote,
            effective_repo.as_deref(),
            effective_fork_repo.as_deref(),
            is_fork_workflow,
        )?;

        let pr_number = state.get_branch(branch).ok().and_then(|m| m.pr_number);
        if let Some(number) = pr_number {
            pr_numbers.push(number);
        }
        ui::receipt(&serde_json::json!({
            "cmd": "submit",
            "branch": branch,
            "pr_number": pr_number,
            "pr_url": pr_url,
            "remote": effective_remote.clone(),
            "repo": effective_repo.clone(),
            "fork_repo": effective_fork_repo.clone(),
        }));

        pr_urls.push((branch.clone(), pr_url));
    }

    state.save()?;

    let native_stack_outcome = if is_fork_workflow {
        Ok(github::NativeStackOutcome::NotApplicable {
            reason:
                "fork and cross-repository pull requests are not supported by GitHub native stacks"
                    .to_string(),
        })
    } else {
        github::ensure_native_stack(&pr_numbers, effective_repo.as_deref())
    };

    match native_stack_outcome {
        Ok(outcome) => {
            crate::cmd::native_stack::report_outcome(&outcome);
            ui::receipt(&crate::cmd::native_stack::receipt_value(
                "submit",
                &branches_to_submit,
                &pr_numbers,
                &outcome,
            ));
        }
        Err(err) => {
            ui::warn(&format!("GitHub native stack update skipped: {err}"));
            ui::receipt(&crate::cmd::native_stack::error_receipt_value(
                "submit",
                &branches_to_submit,
                &pr_numbers,
                &err.to_string(),
            ));
        }
    }

    // Print summary.
    ui::success(&format!("Submitted {} PR(s):", pr_urls.len()));
    for (branch, url) in &pr_urls {
        ui::info(&format!("  {branch} -> {url}"));
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

    #[test]
    fn branches_to_submit_orders_bottom_to_top_and_skips_trunk() {
        let path = vec![
            "feat/c".to_string(),
            "feat/b".to_string(),
            "feat/a".to_string(),
            "main".to_string(),
        ];
        assert_eq!(
            branches_to_submit(&path, "main"),
            vec![
                "feat/a".to_string(),
                "feat/b".to_string(),
                "feat/c".to_string()
            ]
        );
    }

    #[test]
    fn branches_to_submit_handles_trunk_only_path() {
        let path = vec!["main".to_string()];
        assert!(branches_to_submit(&path, "main").is_empty());
    }

    #[test]
    fn submit_atomic_push_failure_aborts_before_github_mutation() {
        let _guard = take_env_lock();
        let repo = init_git_repo("submit-atomic-abort");
        let _cwd = CwdGuard::enter(&repo);

        let main_head = cmd_output(&repo, "git", &["rev-parse", "HEAD"]);
        run_cmd(&repo, "git", &["checkout", "-b", "feat/a"]);
        write_file(&repo, "a.txt", "a\n");
        run_cmd(&repo, "git", &["add", "a.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "add a"]);
        let a_head = cmd_output(&repo, "git", &["rev-parse", "HEAD"]);

        run_cmd(&repo, "git", &["checkout", "-b", "feat/b"]);
        write_file(&repo, "b.txt", "b\n");
        run_cmd(&repo, "git", &["add", "b.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "add b"]);

        let mut state = StackState::new("main".to_string());
        state.remote = "missing-origin".to_string();
        state.add_branch("feat/a", "main", &main_head, None, None);
        state.add_branch("feat/b", "feat/a", &a_head, None, None);
        state.save().expect("save state");
        let before = std::fs::read_to_string(StackState::state_path().expect("state path"))
            .expect("state before");

        let fake_dir = install_fake_bin(
            "submit-atomic-abort-gh",
            "gh",
            r#"#!/bin/sh
case "$1 $2" in
  "pr create"|"pr edit"|"api -X")
    echo "$@" >> "$EZ_FAKE_GH_LOG"
    ;;
esac
exit 0
"#,
        );
        let gh_log = fake_dir.join("gh-mutating.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &gh_log);
        }
        let _path = PathGuard::install(&fake_dir);

        let err = run(false, false, None, None, None, None, None, None)
            .expect_err("atomic push should fail");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }
        assert!(
            err.to_string().contains("missing-origin")
                || err
                    .to_string()
                    .contains("does not appear to be a git repository")
                || err
                    .to_string()
                    .contains("Could not read from remote repository"),
            "unexpected error: {err:#}"
        );
        assert!(
            !gh_log.exists(),
            "GitHub mutation should not occur before a successful atomic push"
        );
        let after = std::fs::read_to_string(StackState::state_path().expect("state path"))
            .expect("state after");
        assert_eq!(after, before, "submit state should not change");

        let state = StackState::load().expect("load state");
        assert_eq!(state.get_branch("feat/a").expect("feat/a").pr_number, None);
        assert_eq!(state.get_branch("feat/b").expect("feat/b").pr_number, None);
    }
}
