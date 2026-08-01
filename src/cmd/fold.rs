use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::Path;

use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

#[derive(Debug)]
struct FoldValidationInput<'a> {
    target: &'a str,
    parent: &'a str,
    trunk: &'a str,
    pr_number: Option<u64>,
    parent_is_ancestor: bool,
}

fn validate_fold_input(input: &FoldValidationInput<'_>) -> Result<()> {
    if input.target == input.trunk {
        bail!(EzError::OnTrunk);
    }
    if input.parent == input.trunk {
        bail!(EzError::UserMessage(format!(
            "`{}` is the bottom stack layer; folding it would move local trunk\n  → Use `ez merge --local` when local trunk landing is available",
            input.target
        )));
    }
    if input.pr_number.is_some() {
        bail!(EzError::UserMessage(format!(
            "`{}` already has a pull request; local fold currently supports PR-less layers only\n  → Merge the PR, or remove its PR association before folding",
            input.target
        )));
    }
    if !input.parent_is_ancestor {
        bail!(EzError::UserMessage(format!(
            "`{}` no longer contains the current tip of parent `{}`\n  → Run `ez restack`, then retry the fold",
            input.target, input.parent
        )));
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct FoldSnapshot {
    original_state_path: std::path::PathBuf,
    original_state_bytes: Vec<u8>,
    target: String,
    target_sha: String,
    target_worktree: Option<String>,
    parent: String,
    parent_sha: String,
    parent_worktree: Option<String>,
}

pub fn run(branch: Option<&str>, yes: bool) -> Result<()> {
    let _lease_guard = crate::worktree_lease::LeaseMutationGuard::acquire("fold worktree branch")?;
    run_with_saver(branch, yes, StackState::save)
}

fn run_with_saver<F>(branch: Option<&str>, yes: bool, save_state: F) -> Result<()>
where
    F: FnOnce(&StackState) -> Result<()>,
{
    let mut state = StackState::load()?;
    let current_branch = git::current_branch()?;
    let main_root = git::main_worktree_root()?;
    let target = branch.unwrap_or(&current_branch).to_string();

    if state.is_trunk(&target) {
        bail!(EzError::OnTrunk);
    }
    if !state.is_managed(&target) {
        bail!(EzError::BranchNotInStack(target));
    }
    if !git::branch_exists(&target) {
        bail!(EzError::UserMessage(format!(
            "local branch `{target}` is missing\n  → Run `ez adopt {target}` to rehydrate it before folding"
        )));
    }

    let target_meta = state.get_branch(&target)?.clone();
    let parent = target_meta.parent.clone();
    if !git::branch_exists(&parent) {
        bail!(EzError::UserMessage(format!(
            "parent branch `{parent}` is missing\n  → Restore or re-adopt the parent before folding `{target}`"
        )));
    }

    validate_fold_input(&FoldValidationInput {
        target: &target,
        parent: &parent,
        trunk: &state.trunk,
        pr_number: target_meta.pr_number,
        parent_is_ancestor: git::is_ancestor(&parent, &target),
    })?;

    let worktree_map = linked_branch_worktrees()?;
    if worktree_map
        .get(&target)
        .is_some_and(|path| same_path(path, &main_root))
    {
        bail!(EzError::UserMessage(format!(
            "`{target}` is checked out in the main worktree and cannot be removed safely\n  → Switch the main worktree to `{}` or materialize `{target}` as a linked worktree first",
            state.trunk
        )));
    }

    let descendants = state.descendants_topo(&target);
    ensure_clean_worktree_fleet(&target, &parent, &descendants, &worktree_map)?;
    ensure_fully_restacked(&state, &descendants)?;

    if !yes {
        ui::warn(&format!(
            "Fold `{target}` into `{parent}` and remove the local `{target}` branch/worktree?"
        ));
        if !ui::confirm("Continue?") {
            ui::info("Fold cancelled");
            return Ok(());
        }
    }

    let target_sha = git::rev_parse(&target)?;
    let parent_sha = git::rev_parse(&parent)?;
    let folded_commits = git::rev_list_count(&parent, &target)?;
    let original_state_path = StackState::state_path()?;
    let original_state_bytes = std::fs::read(&original_state_path)
        .with_context(|| format!("snapshot `{}`", original_state_path.display()))?;
    let target_worktree = worktree_map.get(&target).cloned();
    let parent_worktree = worktree_map.get(&parent).cloned();
    let current_dir = std::env::current_dir().context("read current directory")?;
    let inside_target = target_worktree
        .as_deref()
        .is_some_and(|path| path_is_within(&current_dir, Path::new(path)));
    let destination = parent_worktree.clone().unwrap_or_else(|| main_root.clone());

    if let Some(path) = target_worktree.as_deref() {
        crate::cmd::worktree::guard_registered_worktree(&target, path, "fold")?;
    }
    if let Some(path) = parent_worktree.as_deref() {
        crate::cmd::worktree::guard_registered_worktree(&parent, path, "advance during fold")?;
    }

    let snapshot = FoldSnapshot {
        original_state_path,
        original_state_bytes,
        target: target.clone(),
        target_sha: target_sha.clone(),
        target_worktree: target_worktree.clone(),
        parent: parent.clone(),
        parent_sha: parent_sha.clone(),
        parent_worktree: parent_worktree.clone(),
    };

    if inside_target {
        std::env::set_current_dir(&destination)
            .with_context(|| format!("move out of folded worktree before removing `{target}`"))?;
    }

    if let Err(error) = advance_parent(
        &parent,
        &target_sha,
        &parent_sha,
        parent_worktree.as_deref(),
    ) {
        return Err(error).with_context(|| format!("advance `{parent}` to `{target}`"));
    }

    let mutation_result = (|| -> Result<Vec<String>> {
        if let Some(path) = target_worktree.as_deref() {
            git::worktree_remove(path)
                .with_context(|| format!("remove folded worktree `{path}`"))?;
        }

        git::delete_branch(&target, true)
            .with_context(|| format!("remove folded local branch `{target}`"))?;

        let children = state.reparent_children_preserving_parent_head(&target, &parent)?;
        state.remove_branch(&target);
        save_state(&state).context("save folded stack state")?;
        Ok(children)
    })();

    let children = match mutation_result {
        Ok(children) => children,
        Err(error) => {
            let rollback_errors = rollback_fold(&snapshot);
            if rollback_errors.is_empty() {
                return Err(error).context("fold failed; all local changes were rolled back");
            }
            bail!(
                "{error:#}\nFold rollback was incomplete:\n  - {}\n  → Inspect `ez status --json` and `git worktree list` before retrying",
                rollback_errors.join("\n  - ")
            );
        }
    };

    ui::success(&format!("Folded `{target}` into `{parent}`"));
    ui::info(&format!(
        "Preserved {folded_commits} commit(s) without rewriting history"
    ));
    if !children.is_empty() {
        ui::info(&format!(
            "Reparented {} direct child layer(s) onto `{parent}`",
            children.len()
        ));
    }
    ui::receipt(&serde_json::json!({
        "cmd": "fold",
        "branch": target,
        "into": parent,
        "before_parent": parent_sha,
        "after_parent": target_sha,
        "folded_commits": folded_commits,
        "mode": "fast_forward_boundary",
        "removed_branch": true,
        "removed_worktree": target_worktree,
        "reparented_children": children,
        "remote_preserved": true,
    }));

    if inside_target {
        println!("{destination}");
    }

    Ok(())
}

fn linked_branch_worktrees() -> Result<HashMap<String, String>> {
    Ok(git::worktree_list()?
        .into_iter()
        .filter_map(|worktree| worktree.branch.map(|branch| (branch, worktree.path)))
        .collect())
}

fn ensure_clean_worktree_fleet(
    target: &str,
    parent: &str,
    descendants: &[String],
    worktrees: &HashMap<String, String>,
) -> Result<()> {
    let mut branches = vec![parent.to_string(), target.to_string()];
    branches.extend(descendants.iter().cloned());
    branches.sort();
    branches.dedup();

    let dirty: Vec<String> = branches
        .into_iter()
        .filter_map(|branch| {
            let path = worktrees.get(&branch)?;
            let (staged, modified, untracked) = git::working_tree_status_at(path);
            (staged + modified + untracked > 0)
                .then(|| format!("`{branch}` at `{path}` ({staged} staged, {modified} modified, {untracked} untracked)"))
        })
        .collect();

    if !dirty.is_empty() {
        bail!(EzError::UserMessage(format!(
            "cannot fold while the affected worktree fleet is dirty:\n  - {}\n  → Commit or stash those worktrees, then retry",
            dirty.join("\n  - ")
        )));
    }
    Ok(())
}

fn ensure_fully_restacked(state: &StackState, descendants: &[String]) -> Result<()> {
    for branch in descendants {
        let meta = state.get_branch(branch)?;
        if !git::branch_exists(branch) || !git::branch_exists(&meta.parent) {
            bail!(EzError::UserMessage(format!(
                "descendant `{branch}` or its parent `{}` is missing locally\n  → Re-adopt the stack before folding",
                meta.parent
            )));
        }

        let parent_tip = git::rev_parse(&meta.parent)?;
        if meta.parent_head != parent_tip || !git::is_ancestor(&parent_tip, branch) {
            bail!(EzError::UserMessage(format!(
                "descendant `{branch}` is not fully restacked on `{}`\n  → Run `ez restack` from the stack, then retry the fold",
                meta.parent
            )));
        }
    }
    Ok(())
}

fn advance_parent(
    branch: &str,
    target: &str,
    expected_old: &str,
    worktree: Option<&str>,
) -> Result<()> {
    match worktree {
        Some(path) => git::fast_forward_merge_at(path, target),
        None => git::compare_and_swap_local_branch_ref(branch, target, expected_old),
    }
}

fn restore_parent(snapshot: &FoldSnapshot) -> Result<()> {
    match snapshot.parent_worktree.as_deref() {
        Some(path) => {
            let current = git::rev_parse_at(path, "HEAD")?;
            if current != snapshot.target_sha {
                bail!(
                    "parent worktree `{path}` moved to {current}; expected folded tip {}",
                    snapshot.target_sha
                );
            }
            git::reset_keep_at(path, &snapshot.parent_sha)
        }
        None => git::compare_and_swap_local_branch_ref(
            &snapshot.parent,
            &snapshot.parent_sha,
            &snapshot.target_sha,
        ),
    }
}

fn rollback_fold(snapshot: &FoldSnapshot) -> Vec<String> {
    let mut errors = Vec::new();

    if !git::branch_exists(&snapshot.target)
        && let Err(error) = git::create_branch_at(&snapshot.target, &snapshot.target_sha)
    {
        errors.push(format!(
            "restore branch `{}` at {}: {error:#}",
            snapshot.target, snapshot.target_sha
        ));
    }

    if let Some(path) = snapshot.target_worktree.as_deref()
        && git::branch_exists(&snapshot.target)
        && !Path::new(path).exists()
        && let Err(error) = git::worktree_add(path, &snapshot.target)
    {
        errors.push(format!("restore worktree `{path}`: {error:#}"));
    }

    if let Err(error) = restore_parent(snapshot) {
        errors.push(format!(
            "restore parent `{}` at {}: {error:#}",
            snapshot.parent, snapshot.parent_sha
        ));
    }

    if let Err(error) = std::fs::write(
        &snapshot.original_state_path,
        &snapshot.original_state_bytes,
    ) {
        errors.push(format!("restore stack metadata: {error:#}"));
    }

    errors
}

fn same_path(left: &str, right: &str) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| Path::new(left).to_path_buf());
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| Path::new(right).to_path_buf());
    left == right
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::BranchMeta;
    use crate::test_support::{
        CwdGuard, init_git_repo, run_cmd, take_env_lock, temp_dir, write_file,
    };

    struct FoldRepo {
        repo: std::path::PathBuf,
        base_worktree: std::path::PathBuf,
        target_worktree: std::path::PathBuf,
        child_worktree: std::path::PathBuf,
        base_sha: String,
        target_sha: String,
        child_sha: String,
    }

    impl FoldRepo {
        fn new(name: &str, materialize_base: bool, with_child: bool) -> Self {
            let repo = init_git_repo(name);
            let root = temp_dir(&format!("{name}-worktrees"));
            let base_worktree = root.join("base");
            let target_worktree = root.join("target");
            let child_worktree = root.join("child");

            run_cmd(&repo, "git", &["checkout", "-b", "feat/base"]);
            write_file(&repo, "base.txt", "base\n");
            run_cmd(&repo, "git", &["add", "base.txt"]);
            run_cmd(&repo, "git", &["commit", "-m", "base"]);

            run_cmd(&repo, "git", &["checkout", "-b", "feat/target"]);
            write_file(&repo, "target.txt", "target\n");
            run_cmd(&repo, "git", &["add", "target.txt"]);
            run_cmd(&repo, "git", &["commit", "-m", "target"]);

            run_cmd(&repo, "git", &["checkout", "-b", "feat/child"]);
            write_file(&repo, "child.txt", "child\n");
            run_cmd(&repo, "git", &["add", "child.txt"]);
            run_cmd(&repo, "git", &["commit", "-m", "child"]);

            let main_sha = command_output(&repo, &["rev-parse", "main"]);
            let base_sha = command_output(&repo, &["rev-parse", "feat/base"]);
            let target_sha = command_output(&repo, &["rev-parse", "feat/target"]);
            let child_sha = command_output(&repo, &["rev-parse", "feat/child"]);

            run_cmd(&repo, "git", &["checkout", "main"]);
            if materialize_base {
                run_cmd(
                    &repo,
                    "git",
                    &[
                        "worktree",
                        "add",
                        base_worktree.to_str().expect("base path"),
                        "feat/base",
                    ],
                );
            }
            run_cmd(
                &repo,
                "git",
                &[
                    "worktree",
                    "add",
                    target_worktree.to_str().expect("target path"),
                    "feat/target",
                ],
            );
            if with_child {
                run_cmd(
                    &repo,
                    "git",
                    &[
                        "worktree",
                        "add",
                        child_worktree.to_str().expect("child path"),
                        "feat/child",
                    ],
                );
            }

            let _cwd = CwdGuard::enter(&repo);
            let mut state = StackState::new("main".to_string());
            state.add_branch("feat/base", "main", &main_sha, None, None);
            state.add_branch("feat/target", "feat/base", &base_sha, None, None);
            if with_child {
                state.add_branch("feat/child", "feat/target", &target_sha, None, None);
            }
            state.save().expect("save stack");

            Self {
                repo,
                base_worktree,
                target_worktree,
                child_worktree,
                base_sha,
                target_sha,
                child_sha,
            }
        }
    }

    fn command_output(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {:?} failed", args);
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn fold_validation_rejects_pr_backed_and_bottom_layers() {
        let pr_backed = FoldValidationInput {
            target: "feat/child",
            parent: "feat/base",
            trunk: "main",
            pr_number: Some(42),
            parent_is_ancestor: true,
        };
        assert!(validate_fold_input(&pr_backed).is_err());

        let bottom = FoldValidationInput {
            target: "feat/base",
            parent: "main",
            trunk: "main",
            pr_number: None,
            parent_is_ancestor: true,
        };
        assert!(validate_fold_input(&bottom).is_err());
    }

    #[test]
    fn fold_validation_requires_linear_history() {
        let diverged = FoldValidationInput {
            target: "feat/child",
            parent: "feat/base",
            trunk: "main",
            pr_number: None,
            parent_is_ancestor: false,
        };
        assert!(validate_fold_input(&diverged).is_err());
    }

    #[test]
    fn folds_middle_worktree_without_rewriting_child() {
        let _lock = take_env_lock();
        let fixture = FoldRepo::new("fold-middle", true, true);
        let _cwd = CwdGuard::enter(&fixture.repo);

        run(Some("feat/target"), true).expect("fold target");

        assert_eq!(
            command_output(&fixture.repo, &["rev-parse", "feat/base"]),
            fixture.target_sha
        );
        assert_eq!(
            command_output(&fixture.repo, &["rev-parse", "feat/child"]),
            fixture.child_sha
        );
        assert!(!git::branch_exists("feat/target"));
        assert!(!fixture.target_worktree.exists());
        assert!(fixture.base_worktree.exists());
        assert!(fixture.child_worktree.exists());

        let state = StackState::load().expect("load stack");
        assert!(!state.is_managed("feat/target"));
        let child = state.get_branch("feat/child").expect("child metadata");
        assert_eq!(child.parent, "feat/base");
        assert_eq!(child.parent_head, fixture.target_sha);
    }

    #[test]
    fn folds_when_parent_has_no_worktree() {
        let _lock = take_env_lock();
        let fixture = FoldRepo::new("fold-no-parent-worktree", false, false);
        let _cwd = CwdGuard::enter(&fixture.repo);

        run(Some("feat/target"), true).expect("fold target");

        assert_eq!(
            command_output(&fixture.repo, &["rev-parse", "feat/base"]),
            fixture.target_sha
        );
        assert!(!git::branch_exists("feat/target"));
        assert!(!fixture.target_worktree.exists());
    }

    #[test]
    fn folds_target_without_a_linked_worktree() {
        let _lock = take_env_lock();
        let fixture = FoldRepo::new("fold-no-target-worktree", true, false);
        run_cmd(
            &fixture.repo,
            "git",
            &[
                "worktree",
                "remove",
                fixture.target_worktree.to_str().expect("target path"),
            ],
        );
        let _cwd = CwdGuard::enter(&fixture.repo);

        run(Some("feat/target"), true).expect("fold target");

        assert_eq!(
            command_output(&fixture.repo, &["rev-parse", "feat/base"]),
            fixture.target_sha
        );
        assert!(!git::branch_exists("feat/target"));
        assert!(
            !StackState::load()
                .expect("load stack")
                .is_managed("feat/target")
        );
    }

    #[test]
    fn folds_into_parent_checked_out_in_main_worktree() {
        let _lock = take_env_lock();
        let fixture = FoldRepo::new("fold-parent-main-worktree", true, false);
        run_cmd(
            &fixture.repo,
            "git",
            &[
                "worktree",
                "remove",
                fixture.base_worktree.to_str().expect("base path"),
            ],
        );
        run_cmd(&fixture.repo, "git", &["checkout", "feat/base"]);
        let _cwd = CwdGuard::enter(&fixture.target_worktree);

        run(None, true).expect("fold into main-worktree parent");

        assert_eq!(
            std::fs::canonicalize(std::env::current_dir().expect("current dir"))
                .expect("canonical current dir"),
            std::fs::canonicalize(&fixture.repo).expect("canonical main worktree")
        );
        assert_eq!(
            command_output(&fixture.repo, &["rev-parse", "HEAD"]),
            fixture.target_sha
        );
        assert_eq!(
            command_output(&fixture.repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "feat/base"
        );
        assert!(!fixture.target_worktree.exists());
        assert!(!git::branch_exists("feat/target"));
    }

    #[test]
    fn folding_current_linked_worktree_moves_process_to_parent_worktree() {
        let _lock = take_env_lock();
        let fixture = FoldRepo::new("fold-current-worktree", true, false);
        let _cwd = CwdGuard::enter(&fixture.target_worktree);

        run(None, true).expect("fold current target");

        assert_eq!(
            std::fs::canonicalize(std::env::current_dir().expect("current dir"))
                .expect("canonical current dir"),
            std::fs::canonicalize(&fixture.base_worktree).expect("canonical base worktree")
        );
        assert!(!fixture.target_worktree.exists());
        assert_eq!(
            command_output(&fixture.repo, &["rev-parse", "feat/base"]),
            fixture.target_sha
        );
    }

    #[test]
    fn dirty_worktree_aborts_without_mutation() {
        let _lock = take_env_lock();
        let fixture = FoldRepo::new("fold-dirty", true, true);
        write_file(&fixture.target_worktree, "dirty.txt", "dirty\n");
        let _cwd = CwdGuard::enter(&fixture.repo);
        let state_before = std::fs::read(StackState::state_path().expect("state path"))
            .expect("read state before");

        let error = run(Some("feat/target"), true).expect_err("dirty fold must fail");
        assert!(error.to_string().contains("worktree fleet is dirty"));
        assert_eq!(
            command_output(&fixture.repo, &["rev-parse", "feat/base"]),
            fixture.base_sha
        );
        assert_eq!(
            std::fs::read(StackState::state_path().expect("state path")).expect("read state after"),
            state_before
        );
        assert!(git::branch_exists("feat/target"));
        assert!(fixture.target_worktree.exists());
    }

    #[test]
    fn stale_descendant_aborts_without_mutation() {
        let _lock = take_env_lock();
        let fixture = FoldRepo::new("fold-stale-child", true, true);
        let _cwd = CwdGuard::enter(&fixture.repo);
        let mut state = StackState::load().expect("load stack");
        state
            .get_branch_mut("feat/child")
            .expect("child")
            .parent_head = fixture.base_sha.clone();
        state.save().expect("save stale state");

        let error = run(Some("feat/target"), true).expect_err("stale child must fail");
        assert!(error.to_string().contains("not fully restacked"));
        assert_eq!(
            command_output(&fixture.repo, &["rev-parse", "feat/base"]),
            fixture.base_sha
        );
        assert!(git::branch_exists("feat/target"));
        assert!(fixture.target_worktree.exists());
    }

    #[test]
    fn pr_backed_target_aborts_without_mutation() {
        let _lock = take_env_lock();
        let fixture = FoldRepo::new("fold-pr-backed", true, false);
        let _cwd = CwdGuard::enter(&fixture.repo);
        let mut state = StackState::load().expect("load stack");
        state
            .get_branch_mut("feat/target")
            .expect("target")
            .pr_number = Some(42);
        state.save().expect("save pr metadata");

        let error = run(Some("feat/target"), true).expect_err("PR-backed fold must fail");
        assert!(error.to_string().contains("PR-less"));
        assert_eq!(
            command_output(&fixture.repo, &["rev-parse", "feat/base"]),
            fixture.base_sha
        );
        assert!(git::branch_exists("feat/target"));
    }

    #[test]
    fn save_failure_rolls_back_refs_worktree_and_state() {
        let _lock = take_env_lock();
        let fixture = FoldRepo::new("fold-rollback", true, true);
        let _cwd = CwdGuard::enter(&fixture.repo);
        let state_before = std::fs::read(StackState::state_path().expect("state path"))
            .expect("read state before");

        let error = run_with_saver(Some("feat/target"), true, |_| {
            bail!("injected state save failure")
        })
        .expect_err("save failure must abort");
        assert!(error.to_string().contains("rolled back"));

        assert_eq!(
            command_output(&fixture.repo, &["rev-parse", "feat/base"]),
            fixture.base_sha
        );
        assert_eq!(
            command_output(&fixture.repo, &["rev-parse", "feat/target"]),
            fixture.target_sha
        );
        assert!(fixture.target_worktree.exists());
        assert_eq!(
            command_output(
                &fixture.target_worktree,
                &["rev-parse", "--abbrev-ref", "HEAD"]
            ),
            "feat/target"
        );
        assert_eq!(
            command_output(&fixture.target_worktree, &["rev-parse", "HEAD"]),
            fixture.target_sha
        );
        assert_eq!(
            std::fs::read(StackState::state_path().expect("state path")).expect("read state after"),
            state_before
        );
    }

    #[test]
    fn rollback_preserves_edit_that_arrives_in_parent_worktree() {
        let _lock = take_env_lock();
        let fixture = FoldRepo::new("fold-rollback-concurrent-edit", true, false);
        let _cwd = CwdGuard::enter(&fixture.repo);
        let parent_worktree = fixture.base_worktree.clone();

        let error = run_with_saver(Some("feat/target"), true, |_| {
            write_file(&parent_worktree, "base.txt", "concurrent edit\n");
            bail!("injected state save failure")
        })
        .expect_err("save failure must abort");
        assert!(error.to_string().contains("rolled back"));

        assert_eq!(
            command_output(&fixture.repo, &["rev-parse", "feat/base"]),
            fixture.base_sha
        );
        assert_eq!(
            std::fs::read_to_string(fixture.base_worktree.join("base.txt"))
                .expect("read concurrent edit"),
            "concurrent edit\n"
        );
        assert!(
            command_output(&fixture.base_worktree, &["status", "--porcelain"]).contains("base.txt"),
            "the concurrent edit should remain visible as a worktree change"
        );
        assert!(git::branch_exists("feat/target"));
        assert!(fixture.target_worktree.exists());
    }

    #[test]
    fn target_in_main_worktree_is_rejected() {
        let _lock = take_env_lock();
        let fixture = FoldRepo::new("fold-main-worktree", true, false);
        run_cmd(
            &fixture.repo,
            "git",
            &[
                "worktree",
                "remove",
                fixture.target_worktree.to_str().unwrap(),
            ],
        );
        run_cmd(&fixture.repo, "git", &["checkout", "feat/target"]);
        let _cwd = CwdGuard::enter(&fixture.repo);

        let error = run(Some("feat/target"), true).expect_err("main worktree target must fail");
        assert!(error.to_string().contains("main worktree"));
        assert_eq!(
            command_output(&fixture.repo, &["rev-parse", "feat/base"]),
            fixture.base_sha
        );
    }

    #[test]
    fn multiple_children_reparent_without_changing_bases() {
        let mut state = StackState::new("main".to_string());
        state.branches.insert(
            "feat/a".to_string(),
            BranchMeta {
                name: "feat/a".to_string(),
                parent: "feat/target".to_string(),
                parent_head: "target-sha".to_string(),
                pr_number: None,
                scope: None,
                scope_mode: None,
            },
        );
        state.branches.insert(
            "feat/b".to_string(),
            BranchMeta {
                name: "feat/b".to_string(),
                parent: "feat/target".to_string(),
                parent_head: "target-sha".to_string(),
                pr_number: None,
                scope: None,
                scope_mode: None,
            },
        );

        let children = state
            .reparent_children_preserving_parent_head("feat/target", "feat/base")
            .expect("reparent");
        assert_eq!(children, vec!["feat/a", "feat/b"]);
        for child in children {
            let meta = state.get_branch(&child).expect("child metadata");
            assert_eq!(meta.parent, "feat/base");
            assert_eq!(meta.parent_head, "target-sha");
        }
    }
}
