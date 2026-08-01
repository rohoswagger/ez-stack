use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::stack::StackState;

pub fn run() -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current));
    }

    let meta = state.get_branch(&current)?;
    // Machine output to stdout — pipeable.
    println!("{}", meta.parent);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{CwdGuard, init_git_repo, run_cmd, take_env_lock};

    #[test]
    fn reports_parent_and_rejects_trunk_or_unmanaged_branches() {
        let _lock = take_env_lock();
        let repo = init_git_repo("parent-command");
        let main_head =
            git::rev_parse_at(repo.to_str().expect("repo path"), "main").expect("main head");
        let cwd = CwdGuard::enter(&repo);
        StackState::new("main".to_string())
            .save()
            .expect("save trunk state");
        assert!(
            run()
                .expect_err("trunk must fail")
                .to_string()
                .contains("trunk")
        );
        drop(cwd);

        run_cmd(&repo, "git", &["checkout", "-b", "outside"]);
        let cwd = CwdGuard::enter(&repo);
        assert!(
            run()
                .expect_err("unmanaged must fail")
                .to_string()
                .contains("not tracked by ez")
        );
        drop(cwd);

        run_cmd(&repo, "git", &["checkout", "-b", "feat/topic"]);
        let _cwd = CwdGuard::enter(&repo);
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/topic", "main", &main_head, None, None);
        state.save().expect("save managed state");
        run().expect("managed parent succeeds");
    }
}
