use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

/// Initialize this repo for ez: write stack metadata under `.git/ez/`.
///
/// When `with_rerere` is true, enables git rerere if not already on. The CLI sets this for `--yes`
/// (recommended defaults) and/or `--rerere`.
pub fn run(trunk: Option<String>, with_rerere: bool) -> Result<()> {
    if !git::is_repo() {
        bail!(EzError::NotARepo);
    }

    if StackState::is_initialized()? {
        bail!(EzError::AlreadyInitialized);
    }

    let trunk = match trunk {
        Some(t) => t,
        None => git::default_branch()?,
    };

    let mut state = StackState::new(trunk.clone());

    if !is_rerere_enabled() && with_rerere {
        enable_rerere();
        state.rerere = Some(true);
    }

    state.save()?;

    ui::success(&format!("Initialized ez with trunk branch `{trunk}`"));
    Ok(())
}

/// Check if git rerere is already enabled.
fn is_rerere_enabled() -> bool {
    std::process::Command::new("git")
        .args(["config", "rerere.enabled"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

/// Enable git rerere. Falls back to creating .git/rr-cache if git config fails.
fn enable_rerere() {
    let config_ok = std::process::Command::new("git")
        .args(["config", "rerere.enabled", "true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let autoupdate_ok = std::process::Command::new("git")
        .args(["config", "rerere.autoupdate", "true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !config_ok || !autoupdate_ok {
        // Fallback: create the rr-cache directory directly.
        if let Ok(git_dir) = git::git_common_dir() {
            let rr_cache = git_dir.join("rr-cache");
            if let Err(e) = std::fs::create_dir_all(&rr_cache) {
                ui::warn(&format!("Could not create rr-cache directory: {e}"));
            } else {
                ui::warn("Could not set git config — created rr-cache directory directly");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::StackState;
    use crate::test_support::{CwdGuard, init_git_repo, take_env_lock, temp_dir};

    #[test]
    fn init_fails_outside_git_repo() {
        let _guard = take_env_lock();
        let dir = temp_dir("init-not-repo");
        let _cwd = CwdGuard::enter(&dir);

        let err = run(None, false).expect_err("non-repo should fail");
        assert!(matches!(
            err.downcast_ref::<EzError>(),
            Some(EzError::NotARepo)
        ));
    }

    #[test]
    fn init_fails_when_state_already_exists() {
        let _guard = take_env_lock();
        let repo = init_git_repo("init-already");
        let _cwd = CwdGuard::enter(&repo);
        StackState::new("main".to_string())
            .save()
            .expect("save state");

        let err = run(None, false).expect_err("double init should fail");
        assert!(matches!(
            err.downcast_ref::<EzError>(),
            Some(EzError::AlreadyInitialized)
        ));
    }

    #[test]
    fn init_uses_default_branch_when_trunk_not_provided() {
        let _guard = take_env_lock();
        let repo = init_git_repo("init-default");
        let _cwd = CwdGuard::enter(&repo);

        run(None, false).expect("init should succeed");
        let state = StackState::load().expect("load state");
        assert_eq!(state.trunk, "main");
        assert_eq!(state.remote, "origin");
        assert_ne!(state.rerere, Some(true));
    }

    #[test]
    fn init_state_is_visible_from_nested_subdirectory() {
        let _guard = take_env_lock();
        let repo = init_git_repo("init-subdir");
        let _cwd = CwdGuard::enter(&repo);

        run(None, false).expect("init should succeed");
        std::fs::create_dir_all(repo.join("backend/api")).expect("create nested dirs");
        let _subdir = CwdGuard::enter(&repo.join("backend/api"));

        let state = StackState::load().expect("load state from subdir");
        assert_eq!(state.trunk, "main");
        assert_eq!(state.remote, "origin");
    }

    #[test]
    fn init_recommended_defaults_enable_git_rerere() {
        let _guard = take_env_lock();
        let repo = init_git_repo("init-rerere");
        let _cwd = CwdGuard::enter(&repo);

        // Same `with_rerere` the CLI passes for `ez init --yes` or `ez init --rerere`.
        run(None, true).expect("init should succeed");
        let state = StackState::load().expect("load state");
        assert_eq!(state.rerere, Some(true));
        assert_eq!(git_config("rerere.enabled"), "true");
        assert_eq!(git_config("rerere.autoupdate"), "true");
    }

    #[test]
    fn init_trunk_with_rerere_only_enables_rerere() {
        let _guard = take_env_lock();
        let repo = init_git_repo("init-trunk-rerere");
        let _cwd = CwdGuard::enter(&repo);

        run(Some("main".into()), true).expect("init should succeed");
        assert_eq!(git_config("rerere.enabled"), "true");
    }

    fn git_config(key: &str) -> String {
        let output = std::process::Command::new("git")
            .args(["config", "--get", key])
            .output()
            .expect("run git config");
        assert!(output.status.success(), "git config {key} should be set");
        String::from_utf8(output.stdout)
            .expect("utf8")
            .trim()
            .to_string()
    }
}
