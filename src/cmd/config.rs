use anyhow::{Result, bail};

use crate::error::EzError;
use crate::stack::StackState;
use crate::ui;

/// Known config keys and their descriptions.
const KNOWN_KEYS: &[(&str, &str)] = &[
    ("trunk", "Trunk branch name (e.g. main, master, develop)"),
    ("remote", "Default git remote (e.g. origin, fork, upstream)"),
    (
        "default_from",
        "Default parent for `ez create` when on trunk",
    ),
    ("repo", "GitHub repo for PR operations (owner/name)"),
];

fn is_known_key(key: &str) -> bool {
    KNOWN_KEYS.iter().any(|(k, _)| *k == key)
}

pub fn list() -> Result<()> {
    let state = StackState::load()?;

    ui::header("ez config");
    for (key, description) in KNOWN_KEYS {
        let value = get_value(&state, key);
        let display = match &value {
            Some(v) => v.clone(),
            None => "(not set)".to_string(),
        };
        eprintln!("  {key:15} = {display}");
        eprintln!("  {}", ui::dim(&format!("  {description}")));
    }
    Ok(())
}

pub fn get(key: &str) -> Result<()> {
    let state = StackState::load()?;

    if !is_known_key(key) {
        bail!(EzError::UserMessage(format!(
            "unknown config key `{key}`\n  → Known keys: {}",
            KNOWN_KEYS
                .iter()
                .map(|(k, _)| *k)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    match get_value(&state, key) {
        Some(v) => {
            // Print to stdout (not stderr) so it's scriptable
            println!("{v}");
        }
        None => {
            bail!(EzError::UserMessage(format!(
                "config key `{key}` is not set\n  → Set it with: ez config set {key} <value>"
            )));
        }
    }
    Ok(())
}

pub fn set(key: &str, value: &str) -> Result<()> {
    if !is_known_key(key) {
        bail!(EzError::UserMessage(format!(
            "unknown config key `{key}`\n  → Known keys: {}",
            KNOWN_KEYS
                .iter()
                .map(|(k, _)| *k)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    let mut state = StackState::load()?;
    let old_value = get_value(&state, key);

    set_value(&mut state, key, value)?;
    state.save()?;

    match old_value {
        Some(old) if old != value => {
            ui::success(&format!("{key}: {old} → {value}"));
        }
        Some(_) => {
            ui::info(&format!("{key} is already set to `{value}`"));
        }
        None => {
            ui::success(&format!("{key} = {value}"));
        }
    }

    ui::receipt(&serde_json::json!({
        "cmd": "config set",
        "key": key,
        "value": value,
    }));

    Ok(())
}

fn get_value(state: &StackState, key: &str) -> Option<String> {
    match key {
        "trunk" => Some(state.trunk.clone()),
        "remote" => Some(state.remote.clone()),
        "default_from" => state.default_from.clone(),
        "repo" => state.repo.clone(),
        _ => None,
    }
}

fn set_value(state: &mut StackState, key: &str, value: &str) -> Result<()> {
    match key {
        "trunk" => {
            state.trunk = value.to_string();
        }
        "remote" => {
            state.remote = value.to_string();
        }
        "default_from" => {
            state.default_from = Some(value.to_string());
        }
        "repo" => {
            state.repo = Some(value.to_string());
        }
        _ => {
            bail!(EzError::UserMessage(format!("unknown config key `{key}`")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::StackState;
    use crate::test_support::{CwdGuard, init_git_repo, take_env_lock};

    fn setup_state() -> (std::path::PathBuf, CwdGuard) {
        let repo = init_git_repo("config-test");
        let cwd = CwdGuard::enter(&repo);
        StackState::new("main".to_string())
            .save()
            .expect("save state");
        (repo, cwd)
    }

    #[test]
    fn get_returns_trunk() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let state = StackState::load().unwrap();
        assert_eq!(get_value(&state, "trunk"), Some("main".to_string()));
    }

    #[test]
    fn get_returns_remote() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let state = StackState::load().unwrap();
        assert_eq!(get_value(&state, "remote"), Some("origin".to_string()));
    }

    #[test]
    fn get_returns_none_for_unset_optional_keys() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let state = StackState::load().unwrap();
        assert_eq!(get_value(&state, "default_from"), None);
        assert_eq!(get_value(&state, "repo"), None);
    }

    #[test]
    fn set_updates_trunk() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let mut state = StackState::load().unwrap();
        set_value(&mut state, "trunk", "develop").unwrap();
        state.save().unwrap();

        let reloaded = StackState::load().unwrap();
        assert_eq!(reloaded.trunk, "develop");
    }

    #[test]
    fn set_updates_remote() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let mut state = StackState::load().unwrap();
        set_value(&mut state, "remote", "fork").unwrap();
        state.save().unwrap();

        let reloaded = StackState::load().unwrap();
        assert_eq!(reloaded.remote, "fork");
    }

    #[test]
    fn set_updates_default_from() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let mut state = StackState::load().unwrap();
        set_value(&mut state, "default_from", "dev").unwrap();
        state.save().unwrap();

        let reloaded = StackState::load().unwrap();
        assert_eq!(reloaded.default_from, Some("dev".to_string()));
    }

    #[test]
    fn set_updates_repo() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let mut state = StackState::load().unwrap();
        set_value(&mut state, "repo", "owner/repo").unwrap();
        state.save().unwrap();

        let reloaded = StackState::load().unwrap();
        assert_eq!(reloaded.repo, Some("owner/repo".to_string()));
    }

    #[test]
    fn unknown_key_returns_none() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let state = StackState::load().unwrap();
        assert_eq!(get_value(&state, "nonexistent"), None);
    }

    #[test]
    fn unknown_key_fails_on_set() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let mut state = StackState::load().unwrap();
        let err = set_value(&mut state, "nonexistent", "val").expect_err("should fail");
        assert!(err.to_string().contains("unknown config key"));
    }

    #[test]
    fn is_known_key_works() {
        assert!(is_known_key("trunk"));
        assert!(is_known_key("remote"));
        assert!(is_known_key("default_from"));
        assert!(is_known_key("repo"));
        assert!(!is_known_key("garbage"));
    }

    #[test]
    fn list_does_not_panic() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        // list prints to stderr, just make sure it doesn't error
        list().expect("list should succeed");
    }

    #[test]
    fn get_unknown_key_fails() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let err = get("bogus").expect_err("should fail");
        assert!(err.to_string().contains("unknown config key"));
    }

    #[test]
    fn get_unset_key_fails() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let err = get("default_from").expect_err("should fail");
        assert!(err.to_string().contains("not set"));
    }

    #[test]
    fn roundtrip_set_get() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        set("remote", "myfork").expect("set should succeed");

        let state = StackState::load().unwrap();
        assert_eq!(state.remote, "myfork");
    }

    #[test]
    fn backward_compat_loads_old_state_without_new_fields() {
        let _guard = take_env_lock();
        let repo = init_git_repo("config-compat");
        let _cwd = CwdGuard::enter(&repo);

        // Write state JSON in the old format (no default_from, no repo)
        let dir = StackState::meta_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let old_json = r#"{
            "trunk": "main",
            "remote": "origin",
            "branches": {}
        }"#;
        std::fs::write(StackState::state_path().unwrap(), old_json).unwrap();

        let state = StackState::load().expect("should load old format");
        assert_eq!(state.trunk, "main");
        assert_eq!(state.remote, "origin");
        assert_eq!(state.default_from, None);
        assert_eq!(state.repo, None);
    }
}
