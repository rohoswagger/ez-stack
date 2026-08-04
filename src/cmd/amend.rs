use anyhow::{Result, bail};

use crate::cmd::restack;
use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

pub fn run(message: Option<&str>, all: bool) -> Result<()> {
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

    if all {
        git::add_all()?;
    }

    if !all && !git::has_staged_changes()? {
        bail!(EzError::UserMessage(
            "no staged changes to amend\n  → Stage files with `git add <files>`, or use `ez amend -a` to stage all".to_string()
        ));
    }

    let before = git::rev_parse("HEAD")?;

    git::commit_amend(message)?;

    let after = git::rev_parse("HEAD")?;
    let short_after = &after[..after.len().min(7)];
    ui::success(&format!("Amended commit {short_after}"));

    // Show diff stat so agents can verify what was amended.
    let (files, ins, del) = git::diff_stat_numbers();
    if let Ok(stat) = git::show_stat_head() {
        let stat = stat.trim();
        if !stat.is_empty() {
            eprintln!("{stat}");
        }
    }

    // Emit receipt.
    ui::receipt(&serde_json::json!({
        "cmd": "amend",
        "branch": current,
        "before": &before[..before.len().min(7)],
        "after": short_after,
        "files_changed": files,
        "insertions": ins,
        "deletions": del,
    }));

    // Auto-restack the whole subtree below the amended branch — not just direct
    // children, which would leave grandchildren detached from the stack.
    let current_root = git::repo_root()?;
    let restacked =
        restack::cascade_restack(&mut state, &current, &current_root, &current, "amend")?;

    // Return to the original branch after restacking (only if we may have moved).
    if restacked > 0 {
        git::checkout(&current)?;
    }

    state.save()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{CwdGuard, init_git_repo, run_cmd, take_env_lock, write_file};

    fn enter_managed_branch(prefix: &str) -> (std::path::PathBuf, CwdGuard) {
        let repo = init_git_repo(prefix);
        let parent_head =
            git::rev_parse_at(repo.to_str().expect("repo path"), "main").expect("main head");
        run_cmd(&repo, "git", &["checkout", "-b", "feat/topic"]);
        let cwd = CwdGuard::enter(&repo);
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/topic", "main", &parent_head, None, None);
        state.save().expect("save stack");
        (repo, cwd)
    }

    #[test]
    fn rejects_trunk_unmanaged_and_unstaged_amends() {
        let _lock = take_env_lock();

        let trunk = init_git_repo("amend-trunk");
        let trunk_cwd = CwdGuard::enter(&trunk);
        StackState::new("main".to_string())
            .save()
            .expect("save trunk stack");
        assert!(
            run(None, false)
                .expect_err("trunk must fail")
                .to_string()
                .contains("trunk")
        );
        drop(trunk_cwd);

        let unmanaged = init_git_repo("amend-unmanaged");
        run_cmd(&unmanaged, "git", &["checkout", "-b", "outside"]);
        let unmanaged_cwd = CwdGuard::enter(&unmanaged);
        StackState::new("main".to_string())
            .save()
            .expect("save unmanaged stack");
        assert!(
            run(None, false)
                .expect_err("unmanaged must fail")
                .to_string()
                .contains("not tracked by ez")
        );
        drop(unmanaged_cwd);

        let (_repo, _cwd) = enter_managed_branch("amend-unstaged");
        assert!(
            run(None, false)
                .expect_err("unstaged amend must fail")
                .to_string()
                .contains("no staged changes")
        );
    }

    #[test]
    fn stages_all_and_amends_managed_branch() {
        let _lock = take_env_lock();
        let (repo, _cwd) = enter_managed_branch("amend-all");
        write_file(&repo, "tracked.txt", "amended\n");

        run(Some("amended subject"), true).expect("amend succeeds");

        assert_eq!(
            git::log_oneline("-1", 1).expect("log")[0].1,
            "amended subject"
        );
        assert!(!git::has_uncommitted_changes().expect("status"));
    }
}
