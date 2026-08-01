use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::dev;
use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run(branch: Option<&str>, force: bool, yes: bool) -> Result<()> {
    let mut state = StackState::load()?;
    let _lease_guard =
        crate::worktree_lease::LeaseMutationGuard::acquire("delete worktree branch")?;
    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }
    let current = git::current_branch()?;

    let target = branch.unwrap_or(&current).to_string();

    if state.is_trunk(&target) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&target) {
        bail!(EzError::BranchNotInStack(target.clone()));
    }

    let worktrees = git::worktree_list()?;
    let main_worktree = worktrees
        .first()
        .map(|worktree| worktree.path.as_str())
        .ok_or_else(|| anyhow::anyhow!("could not determine main worktree root"))?;
    let linked_worktree = worktrees.iter().find(|worktree| {
        worktree.branch.as_deref() == Some(target.as_str()) && worktree.path != main_worktree
    });

    if let Some(worktree) = linked_worktree {
        crate::cmd::worktree::guard_worktree_lock(
            &target,
            &worktree.path,
            worktree.locked_reason.as_deref(),
            "delete",
        )?;
        return delete_with_worktree(&mut state, &target, force, yes, &worktree.path);
    }

    // No worktree — original branch-only delete path.

    // Worktree guard: if the target branch is checked out in another worktree, bail.
    let current_root = git::repo_root()?;
    if let Some(wt_path) = git::branch_checked_out_elsewhere(&target, &current_root)? {
        bail!(EzError::UserMessage(format!(
            "branch `{target}` is checked out in worktree `{wt_path}`\n  → Run `ez delete {target}` to remove it"
        )));
    }

    let meta = state.get_branch(&target)?;
    let parent = meta.parent.clone();
    let pr_number = meta.pr_number;

    // Reparent children.
    let children = state.reparent_children_preserving_parent_head(&target, &parent)?;
    for child_name in &children {
        ui::info(&format!("Reparented `{child_name}` onto `{parent}`"));
    }

    // If currently on the target branch, checkout parent first.
    let switched_from_target = current == target;
    if switched_from_target {
        git::checkout(&parent)?;
    }

    // Delete local branch.
    if git::branch_exists(&target) {
        if let Err(error) = git::delete_branch(&target, force) {
            if switched_from_target && let Err(rollback_error) = git::checkout(&target) {
                bail!(
                    "Could not delete local branch `{target}`: {error}\n\
                     Checkout rollback also failed: {rollback_error}\n  \
                     → The branch still exists; inspect the active worktree before retrying"
                );
            }
            return Err(error);
        }
    }

    // Update PR bases on GitHub only after the local deletion committed.
    if pr_number.is_some() {
        let new_base = parent.clone();
        for child_name in &children {
            let child = state.get_branch(child_name)?;
            if let Some(child_pr) = child.pr_number
                && let Err(e) = github::update_pr_base(child_pr, &new_base, state.repo.as_deref())
            {
                ui::warn(&format!("Failed to update PR base for `{child_name}`: {e}"));
            }
        }
    }

    // Try to delete remote branch (ignore errors).
    let _ = git::delete_remote_branch(&state.remote, &target);

    state.remove_branch(&target);
    state.save()?;

    ui::success(&format!("Deleted branch `{target}`"));
    if !children.is_empty() {
        ui::hint(&format!(
            "Run `ez restack` to rebase reparented branches onto `{parent}`"
        ));
    }

    ui::receipt(&serde_json::json!({
        "cmd": "delete",
        "branch": target,
        "parent": parent,
        "reparented_children": children.len(),
    }));

    Ok(())
}

/// Delete a branch that has an associated worktree.
fn delete_with_worktree(
    state: &mut StackState,
    target: &str,
    force: bool,
    yes: bool,
    wt_path: &str,
) -> Result<()> {
    let repo_root = git::main_worktree_root()?;
    let port = dev::dev_port(target);

    let current_dir = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_default();

    let inside_worktree = inside_worktree_path(&current_dir, wt_path);

    if inside_worktree && !yes {
        ui::warn(&inside_worktree_delete_warning(target));
        if !ui::confirm("Delete this worktree and switch to the repo root?") {
            ui::info(&inside_worktree_delete_cancelled(target));
            return Ok(());
        }
    }

    // Pre-compute stack changes.
    let meta = state.get_branch(target)?;
    let parent = meta.parent.clone();
    let pr_number = meta.pr_number;
    let children = state.children_of(target);
    let child_prs: Vec<(String, Option<u64>)> = children
        .iter()
        .filter_map(|c| state.get_branch(c).ok().map(|m| (c.clone(), m.pr_number)))
        .collect();

    let wt_dir = Path::new(wt_path);
    if !wt_dir.exists() {
        bail!(
            "refuse to remove `{wt_path}` because the registered worktree path is missing\n  \
             → Run `git worktree prune`, then retry"
        );
    }
    if !wt_dir.join(".git").is_file() {
        bail!(
            "refuse to remove `{wt_path}` because it is no longer a valid Git worktree\n  \
             → Move or remove the directory manually after preserving any user files"
        );
    }

    let listener_pids = match dev::listener_processes_in_worktree(port, wt_path) {
        Ok(pids) => pids,
        Err(error) => {
            ui::warn(&format!(
                "Could not verify process ownership on dev port {port}: {error}"
            ));
            Vec::new()
        }
    };

    // --- Phase 2: Mutate filesystem ---

    if inside_worktree {
        std::env::set_current_dir(&repo_root)?;
    }

    let claim = format!("ez-delete:{}:{target}", std::process::id());
    git::worktree_lock(wt_path, &claim).map_err(|error| {
        anyhow::anyhow!("Could not claim `{target}` worktree at `{wt_path}` for deletion: {error}")
    })?;
    if let Err(error) = verify_delete_claim(wt_path, target, &claim) {
        release_delete_claim(wt_path, target, &claim);
        return Err(error);
    }
    if let Err(error) = git::worktree_prune() {
        release_delete_claim(wt_path, target, &claim);
        return Err(error);
    }
    if let Err(error) = verify_delete_claim(wt_path, target, &claim) {
        release_delete_claim(wt_path, target, &claim);
        return Err(error);
    }
    let quarantine = quarantine_path(wt_dir)?;
    let quarantine_str = quarantine.to_string_lossy().to_string();
    if let Err(error) = quarantine_claimed_worktree(wt_path, &quarantine_str, target, &claim) {
        release_delete_claim(wt_path, target, &claim);
        return Err(error);
    }
    if let Err(error) = git::worktree_unlock(&quarantine_str) {
        let rollback = restore_quarantined_worktree(&quarantine_str, wt_path, target, &claim).err();
        if let Some(rollback_error) = rollback {
            bail!(
                "Could not unlock quarantined `{target}` worktree at `{quarantine_str}`: {error}\n\
                 Worktree-path rollback also failed: {rollback_error}\n  \
                 → Preserve `{quarantine_str}` and inspect `git worktree list` before retrying"
            );
        }
        bail!(
            "Could not unlock quarantined `{target}` worktree at `{quarantine_str}`: {error}\n  \
             → Restored worktree at `{wt_path}`"
        );
    }

    let removal = if force {
        git::worktree_remove_force(&quarantine_str)
    } else {
        git::worktree_remove(&quarantine_str)
    };
    if let Err(error) = removal {
        let rollback = restore_quarantined_worktree(&quarantine_str, wt_path, target, &claim).err();
        if let Some(rollback_error) = rollback {
            bail!(
                "Could not remove worktree at `{wt_path}`: {error}\n\
                 Worktree-path rollback also failed: {rollback_error}\n  \
                 → Preserve `{quarantine_str}` and inspect `git worktree list` before retrying"
            );
        }
        bail!(
            "Could not remove worktree at `{wt_path}`: {error}\n\
             Use `ez delete {target} --force` to discard uncommitted changes"
        );
    }
    ui::info(&format!("Removed worktree at `{wt_path}`"));

    if let Err(error) = git::delete_branch(target, true) {
        let rollback = if git::branch_exists(target) && !wt_dir.exists() {
            git::worktree_add(wt_path, target).err()
        } else {
            None
        };
        if let Some(rollback_error) = rollback {
            bail!(
                "Could not delete local branch `{target}` after removing its worktree: {error}\n\
                 Worktree rollback also failed: {rollback_error}\n  \
                 → The branch still exists; recreate its worktree before retrying"
            );
        }
        bail!(
            "Could not delete local branch `{target}` after removing its worktree: {error}\n  \
             → Restored the worktree at `{wt_path}`; resolve the ref error and retry"
        );
    }

    // Stop only processes whose cwd belonged to this exact worktree, and only
    // after both the worktree and local branch were removed successfully.
    let killed_pids = match dev::terminate_processes(&listener_pids) {
        Ok(pids) => {
            if !pids.is_empty() {
                ui::info(&format!(
                    "Stopped {} process(es) on dev port {}",
                    pids.len(),
                    port
                ));
            }
            pids
        }
        Err(e) => {
            ui::warn(&format!(
                "Failed to stop process(es) on dev port {}: {}",
                port, e
            ));
            Vec::new()
        }
    };

    // --- Phase 3: Mutate stack state ---

    // Reparent children.
    let children = state.reparent_children_preserving_parent_head(target, &parent)?;
    for child_name in &children {
        ui::info(&format!("Reparented `{child_name}` onto `{parent}`"));
    }

    // Update PR bases on GitHub (best-effort).
    if pr_number.is_some() {
        for (child_name, child_pr) in &child_prs {
            if let Some(pr) = child_pr {
                if let Err(e) = github::update_pr_base(*pr, &parent, state.repo.as_deref()) {
                    ui::warn(&format!("Failed to update PR base for `{child_name}`: {e}"));
                }
            }
        }
    }

    // Try to delete remote branch (ignore errors).
    let _ = git::delete_remote_branch(&state.remote, target);

    state.remove_branch(target);
    state.save()?;

    ui::success(&format!("Deleted branch `{target}`"));
    if !children.is_empty() {
        ui::hint(&format!(
            "Run `ez restack` to rebase reparented branches onto `{parent}`"
        ));
    }

    ui::receipt(&serde_json::json!({
        "cmd": "delete",
        "branch": target,
        "parent": parent,
        "dev_port": port,
        "killed_pids": killed_pids,
        "worktree": wt_path,
        "reparented_children": children.len(),
    }));

    // If we were inside the deleted worktree, print repo root for shell cd.
    if inside_worktree {
        println!("{repo_root}");
    }

    Ok(())
}

fn quarantine_path(worktree: &Path) -> Result<PathBuf> {
    let parent = worktree
        .parent()
        .ok_or_else(|| anyhow::anyhow!("worktree path has no parent: {}", worktree.display()))?;
    let name = worktree
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("worktree");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = parent.join(format!(".ez-delete-{}-{nonce}-{name}", std::process::id()));
    if path.exists() {
        bail!(
            "refuse to quarantine `{}` because `{}` already exists",
            worktree.display(),
            path.display()
        );
    }
    Ok(path)
}

fn quarantine_claimed_worktree(
    original: &str,
    quarantine: &str,
    expected_branch: &str,
    claim: &str,
) -> Result<()> {
    std::fs::rename(original, quarantine).map_err(|error| {
        anyhow::anyhow!(
            "Could not quarantine `{expected_branch}` worktree from `{original}` to `{quarantine}`: {error}"
        )
    })?;
    if let Err(error) = git::worktree_repair(quarantine) {
        let _ = std::fs::rename(quarantine, original);
        let _ = git::worktree_repair(original);
        bail!(
            "Could not repair quarantined `{expected_branch}` worktree at `{quarantine}`: {error}"
        );
    }
    if let Err(error) = verify_delete_claim(quarantine, expected_branch, claim) {
        let rollback =
            restore_quarantined_worktree(quarantine, original, expected_branch, claim).err();
        if let Some(rollback_error) = rollback {
            bail!("{error}\nWorktree-path rollback also failed: {rollback_error}");
        }
        return Err(error);
    }
    Ok(())
}

fn restore_quarantined_worktree(
    quarantine: &str,
    original: &str,
    expected_branch: &str,
    claim: &str,
) -> Result<()> {
    if !Path::new(quarantine).exists() {
        bail!("quarantined worktree path `{quarantine}` no longer exists");
    }
    if Path::new(original).exists() {
        bail!("original worktree path `{original}` is now occupied");
    }

    let registered = git::worktree_list()?
        .into_iter()
        .find(|worktree| worktree.path == quarantine);
    let Some(registered) = registered else {
        bail!("quarantined worktree `{quarantine}` is no longer registered");
    };
    if registered.branch.as_deref() != Some(expected_branch) {
        bail!(
            "quarantined worktree ownership changed: expected `{expected_branch}`, found `{}`",
            registered.branch.as_deref().unwrap_or("<detached HEAD>")
        );
    }
    if registered.locked_reason.as_deref() != Some(claim) {
        git::worktree_lock(quarantine, claim)?;
    }
    verify_delete_claim(quarantine, expected_branch, claim)?;

    std::fs::rename(quarantine, original).map_err(|error| {
        anyhow::anyhow!("Could not restore worktree path `{original}`: {error}")
    })?;
    git::worktree_repair(original)?;
    verify_delete_claim(original, expected_branch, claim)?;
    git::worktree_unlock(original)?;
    Ok(())
}

fn verify_delete_claim(path: &str, expected_branch: &str, claim: &str) -> Result<()> {
    let worktrees = git::worktree_list()?;
    let Some(worktree) = worktrees.iter().find(|worktree| worktree.path == path) else {
        bail!(
            "worktree ownership changed at `{path}`: expected `{expected_branch}`, but the path is no longer registered"
        );
    };
    if worktree.branch.as_deref() != Some(expected_branch)
        || worktree.locked_reason.as_deref() != Some(claim)
    {
        let actual_branch = worktree.branch.as_deref().unwrap_or("<detached HEAD>");
        let actual_lock = worktree.locked_reason.as_deref().unwrap_or("<unlocked>");
        bail!(
            "worktree ownership changed at `{path}`: expected `{expected_branch}` with deletion claim `{claim}`, found `{actual_branch}` with lock `{actual_lock}`\n  \
             → No replacement worktree was removed; inspect `git worktree list` and retry"
        );
    }
    Ok(())
}

fn release_delete_claim(path: &str, expected_branch: &str, claim: &str) {
    if verify_delete_claim(path, expected_branch, claim).is_ok() {
        let _ = git::worktree_unlock(path);
    }
}

fn inside_worktree_path(current_dir: &str, worktree_path: &str) -> bool {
    current_dir == worktree_path || current_dir.starts_with(&format!("{worktree_path}/"))
}

fn inside_worktree_delete_warning(target: &str) -> String {
    format!(
        "You are inside the worktree for `{target}` that you are about to delete\n  → Re-run with `--yes` to skip this prompt"
    )
}

fn inside_worktree_delete_cancelled(target: &str) -> String {
    format!("Cancelled\n  → Re-run with `--yes`: `ez worktree delete {target} --yes`")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        CwdGuard, PathGuard, init_git_repo, install_fake_bin, run_cmd, take_env_lock, temp_dir,
    };

    #[test]
    fn inside_worktree_path_matches_exact_and_nested() {
        assert!(inside_worktree_path(
            "/repo/.worktrees/feat",
            "/repo/.worktrees/feat"
        ));
        assert!(inside_worktree_path(
            "/repo/.worktrees/feat/src/app",
            "/repo/.worktrees/feat"
        ));
        assert!(!inside_worktree_path(
            "/repo/.worktrees/feat-two",
            "/repo/.worktrees/feat"
        ));
    }

    #[test]
    fn inside_worktree_delete_warning_mentions_yes_flag() {
        let warning = inside_worktree_delete_warning("feat/auth");
        assert!(warning.contains("feat/auth"));
        assert!(warning.contains("--yes"));
    }

    #[test]
    fn inside_worktree_delete_cancelled_mentions_worktree_delete_yes_command() {
        let warning = inside_worktree_delete_cancelled("feat/auth");
        assert!(warning.contains("ez worktree delete feat/auth --yes"));
    }

    #[test]
    fn restore_quarantined_worktree_rejects_missing_quarantine_path() {
        let missing = temp_dir("delete-missing-quarantine")
            .join("missing")
            .to_string_lossy()
            .to_string();
        let original = temp_dir("delete-missing-quarantine-original")
            .join("original")
            .to_string_lossy()
            .to_string();

        let error = restore_quarantined_worktree(&missing, &original, "feat/missing", "claim")
            .expect_err("missing quarantine path should fail");

        assert!(error.to_string().contains("no longer exists"));
    }

    #[test]
    fn restore_quarantined_worktree_rejects_unregistered_quarantine_path() {
        let _lock = take_env_lock();
        let repo = init_git_repo("delete-unregistered-quarantine");
        let _cwd = CwdGuard::enter(&repo);
        let quarantine = temp_dir("delete-unregistered-quarantine-path")
            .join("quarantine")
            .to_string_lossy()
            .to_string();
        std::fs::create_dir_all(&quarantine).expect("create unregistered quarantine");
        let original = temp_dir("delete-unregistered-quarantine-original")
            .join("original")
            .to_string_lossy()
            .to_string();

        let error = restore_quarantined_worktree(&quarantine, &original, "feat/missing", "claim")
            .expect_err("unregistered quarantine should fail");

        assert!(error.to_string().contains("is no longer registered"));
    }

    #[test]
    fn restore_quarantined_worktree_rejects_changed_branch_ownership() {
        let _lock = take_env_lock();
        let repo = init_git_repo("delete-changed-branch-quarantine");
        let _cwd = CwdGuard::enter(&repo);
        run_cmd(&repo, "git", &["branch", "feat/actual"]);
        let quarantine = temp_dir("delete-changed-branch-quarantine-path").join("quarantine");
        run_cmd(
            &repo,
            "git",
            &[
                "worktree",
                "add",
                quarantine.to_str().expect("quarantine path"),
                "feat/actual",
            ],
        );
        let quarantine =
            std::fs::canonicalize(&quarantine).expect("canonicalize quarantine worktree path");
        run_cmd(
            &repo,
            "git",
            &[
                "worktree",
                "lock",
                "--reason",
                "claim",
                quarantine.to_str().expect("quarantine path"),
            ],
        );
        let original = temp_dir("delete-changed-branch-original")
            .join("original")
            .to_string_lossy()
            .to_string();

        let error = restore_quarantined_worktree(
            quarantine.to_str().expect("quarantine path"),
            &original,
            "feat/expected",
            "claim",
        )
        .expect_err("changed branch ownership should fail");

        assert!(error.to_string().contains("ownership changed"));
        assert!(error.to_string().contains("feat/actual"));
    }

    #[test]
    fn verify_delete_claim_rejects_unregistered_path() {
        let _lock = take_env_lock();
        let repo = init_git_repo("delete-claim-unregistered");
        let _cwd = CwdGuard::enter(&repo);
        let path = temp_dir("delete-claim-unregistered-path")
            .join("missing-worktree")
            .to_string_lossy()
            .to_string();

        let error = verify_delete_claim(&path, "feat/missing", "claim")
            .expect_err("unregistered path should fail claim verification");

        assert!(error.to_string().contains("path is no longer registered"));
    }

    #[test]
    fn quarantine_claimed_worktree_reports_rename_failure() {
        let original = temp_dir("delete-quarantine-rename-missing")
            .join("missing")
            .to_string_lossy()
            .to_string();
        let quarantine = temp_dir("delete-quarantine-rename-destination")
            .join("quarantine")
            .to_string_lossy()
            .to_string();

        let error = quarantine_claimed_worktree(&original, &quarantine, "feat/missing", "claim")
            .expect_err("missing original path should fail quarantine");

        assert!(error.to_string().contains("Could not quarantine"));
        assert!(error.to_string().contains("feat/missing"));
    }

    #[test]
    fn quarantine_claimed_worktree_restores_original_when_repair_fails() {
        let _lock = take_env_lock();
        let repo = init_git_repo("delete-quarantine-repair-source");
        let _cwd = CwdGuard::enter(&repo);
        let original = temp_dir("delete-quarantine-repair-original").join("original");
        std::fs::create_dir_all(&original).expect("create original worktree path");
        std::fs::write(original.join("user.txt"), "preserve\n").expect("write original file");
        let quarantine = original
            .parent()
            .expect("original parent")
            .join("quarantine")
            .to_string_lossy()
            .to_string();
        let fake_bin = install_fake_bin(
            "delete-quarantine-repair-fake-git",
            "git",
            "#!/bin/sh\n\
             if [ \"$1 $2\" = \"worktree repair\" ]; then\n\
             \techo 'simulated repair failure' >&2\n\
             \texit 1\n\
             fi\n\
             exec \"$EZ_TEST_REAL_GIT\" \"$@\"\n",
        );
        let real_git = std::process::Command::new("which")
            .arg("git")
            .output()
            .expect("which git");
        assert!(real_git.status.success());
        let real_git = String::from_utf8_lossy(&real_git.stdout).trim().to_string();
        // SAFETY: this test holds the shared environment mutex.
        unsafe {
            std::env::set_var("EZ_TEST_REAL_GIT", real_git);
        }
        let _path = PathGuard::install(&fake_bin);

        let error = quarantine_claimed_worktree(
            original.to_str().expect("original path"),
            &quarantine,
            "feat/repair",
            "claim",
        )
        .expect_err("repair failure should abort quarantine");

        assert!(error.to_string().contains("Could not repair quarantined"));
        assert!(original.exists());
        assert_eq!(
            std::fs::read_to_string(original.join("user.txt")).expect("read restored file"),
            "preserve\n"
        );
        assert!(!Path::new(&quarantine).exists());
    }
}
