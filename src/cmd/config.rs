use anyhow::{Result, bail};

use crate::error::EzError;
use crate::stack::StackState;
use crate::ui;

/// Known config keys and their descriptions.
const KNOWN_KEYS: &[(&str, &str)] = &[
    ("trunk", "Trunk branch name (e.g. main, master, develop)"),
    ("remote", "Default git remote (e.g. origin, fork, upstream)"),
    (
        "upstream_remote",
        "Git remote to fetch trunk and PR refs from (defaults to remote)",
    ),
    (
        "default_from",
        "Default parent for `ez create` when on trunk",
    ),
    ("repo", "GitHub repo for PR operations (owner/name)"),
    (
        "fork_repo",
        "GitHub fork repo used as PR head repository (owner/name)",
    ),
    ("draft", "Default new PRs to draft (true/false)"),
    ("no_pr", "Default push to skip PR creation (true/false)"),
    (
        "rerere",
        "Enable git rerere for conflict recording (true/false)",
    ),
];

/// Keys that accept only boolean values.
const BOOL_KEYS: &[&str] = &["draft", "no_pr", "rerere"];
const OPTIONAL_KEYS: &[&str] = &[
    "upstream_remote",
    "default_from",
    "repo",
    "fork_repo",
    "draft",
    "no_pr",
    "rerere",
];
const REPO_KEYS: &[&str] = &["repo", "fork_repo"];

fn is_known_key(key: &str) -> bool {
    KNOWN_KEYS.iter().any(|(k, _)| *k == key)
}

fn is_bool_key(key: &str) -> bool {
    BOOL_KEYS.contains(&key)
}

fn is_optional_key(key: &str) -> bool {
    OPTIONAL_KEYS.contains(&key)
}

fn is_repo_key(key: &str) -> bool {
    REPO_KEYS.contains(&key)
}

fn known_keys_list() -> String {
    KNOWN_KEYS
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse a user-supplied string as a boolean.
/// Accepts: true, false, 1, 0, yes, no (case-insensitive).
fn parse_bool(value: &str) -> Result<bool> {
    match value.to_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => bail!(EzError::UserMessage(format!(
            "invalid boolean value `{value}`\n  → Accepted values: true, false, 1, 0, yes, no"
        ))),
    }
}

fn validate_repo_name(value: &str) -> Result<()> {
    let Some((owner, repo)) = value.split_once('/') else {
        bail!(EzError::UserMessage(format!(
            "invalid GitHub repo `{value}`\n  → Expected owner/name"
        )));
    };
    if value.trim() != value
        || owner.is_empty()
        || repo.is_empty()
        || repo.contains('/')
        || owner.chars().any(char::is_whitespace)
        || repo.chars().any(char::is_whitespace)
    {
        bail!(EzError::UserMessage(format!(
            "invalid GitHub repo `{value}`\n  → Expected owner/name"
        )));
    }
    Ok(())
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
            known_keys_list()
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
            known_keys_list()
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

pub fn unset(key: &str) -> Result<()> {
    if !is_known_key(key) {
        bail!(EzError::UserMessage(format!(
            "unknown config key `{key}`\n  → Known keys: {}",
            known_keys_list()
        )));
    }
    if !is_optional_key(key) {
        bail!(EzError::UserMessage(format!(
            "config key `{key}` cannot be unset\n  → Set it to a new value with: ez config set {key} <value>"
        )));
    }

    let mut state = StackState::load()?;
    let old_value = get_value(&state, key);
    unset_value(&mut state, key)?;
    state.save()?;

    match old_value {
        Some(old) => ui::success(&format!("{key}: {old} → (not set)")),
        None => ui::info(&format!("{key} is already unset")),
    }

    ui::receipt(&serde_json::json!({
        "cmd": "config unset",
        "key": key,
    }));

    Ok(())
}

fn get_value(state: &StackState, key: &str) -> Option<String> {
    match key {
        "trunk" => Some(state.trunk.clone()),
        "remote" => Some(state.remote.clone()),
        "upstream_remote" => state.upstream_remote.clone(),
        "default_from" => state.default_from.clone(),
        "repo" => state.repo.clone(),
        "fork_repo" => state.fork_repo.clone(),
        "draft" => state.draft.map(|v| v.to_string()),
        "no_pr" => state.no_pr.map(|v| v.to_string()),
        "rerere" => state.rerere.map(|v| v.to_string()),
        _ => None,
    }
}

fn set_value(state: &mut StackState, key: &str, value: &str) -> Result<()> {
    // Validate bool keys before setting.
    if is_bool_key(key) {
        let _ = parse_bool(value)?;
    }
    if is_repo_key(key) {
        validate_repo_name(value)?;
    }

    match key {
        "trunk" => {
            state.trunk = value.to_string();
        }
        "remote" => {
            state.remote = value.to_string();
        }
        "upstream_remote" => {
            state.upstream_remote = Some(value.to_string());
        }
        "default_from" => {
            state.default_from = Some(value.to_string());
        }
        "repo" => {
            state.repo = Some(value.to_string());
        }
        "fork_repo" => {
            state.fork_repo = Some(value.to_string());
        }
        "draft" => {
            state.draft = Some(parse_bool(value)?);
        }
        "no_pr" => {
            state.no_pr = Some(parse_bool(value)?);
        }
        "rerere" => {
            let enabled = parse_bool(value)?;
            state.rerere = Some(enabled);
            if enabled {
                enable_rerere();
            } else {
                disable_rerere();
            }
        }
        _ => {
            bail!(EzError::UserMessage(format!("unknown config key `{key}`")));
        }
    }
    Ok(())
}

fn unset_value(state: &mut StackState, key: &str) -> Result<()> {
    match key {
        "upstream_remote" => state.upstream_remote = None,
        "default_from" => state.default_from = None,
        "repo" => state.repo = None,
        "fork_repo" => state.fork_repo = None,
        "draft" => state.draft = None,
        "no_pr" => state.no_pr = None,
        "rerere" => {
            state.rerere = None;
            disable_rerere();
        }
        _ => bail!(EzError::UserMessage(format!(
            "config key `{key}` cannot be unset"
        ))),
    }
    Ok(())
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
        if let Ok(git_dir) = crate::git::git_common_dir() {
            let rr_cache = git_dir.join("rr-cache");
            if let Err(e) = std::fs::create_dir_all(&rr_cache) {
                crate::ui::warn(&format!("Could not create rr-cache directory: {e}"));
            } else {
                crate::ui::warn("Could not set git config — created rr-cache directory directly");
            }
        }
    }
}

/// Disable git rerere when the repo config is set back to false.
fn disable_rerere() {
    for key in ["rerere.enabled", "rerere.autoupdate"] {
        let ok = std::process::Command::new("git")
            .args(["config", key, "false"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !ok {
            crate::ui::warn(&format!("Could not set git config `{key}` to false"));
        }
    }
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
        assert_eq!(get_value(&state, "upstream_remote"), None);
        assert_eq!(get_value(&state, "fork_repo"), None);
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
    fn set_updates_upstream_remote_and_fork_repo() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let mut state = StackState::load().unwrap();
        set_value(&mut state, "upstream_remote", "upstream").unwrap();
        set_value(&mut state, "fork_repo", "fork-owner/repo").unwrap();
        state.save().unwrap();

        let reloaded = StackState::load().unwrap();
        assert_eq!(reloaded.upstream_remote, Some("upstream".to_string()));
        assert_eq!(reloaded.fork_repo, Some("fork-owner/repo".to_string()));
    }

    #[test]
    fn repo_keys_require_owner_name() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let mut state = StackState::load().unwrap();
        let err = set_value(&mut state, "repo", "owner-only").expect_err("should reject");
        assert!(err.to_string().contains("Expected owner/name"));
        let err =
            set_value(&mut state, "fork_repo", "owner/repo/extra").expect_err("should reject");
        assert!(err.to_string().contains("Expected owner/name"));
    }

    #[test]
    fn unset_optional_key_clears_value() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let mut state = StackState::load().unwrap();
        set_value(&mut state, "fork_repo", "fork-owner/repo").unwrap();
        unset_value(&mut state, "fork_repo").unwrap();
        assert_eq!(state.fork_repo, None);
    }

    #[test]
    fn unset_required_key_fails() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let mut state = StackState::load().unwrap();
        let err = unset_value(&mut state, "remote").expect_err("should fail");
        assert!(err.to_string().contains("cannot be unset"));
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
        assert!(is_known_key("upstream_remote"));
        assert!(is_known_key("default_from"));
        assert!(is_known_key("repo"));
        assert!(is_known_key("fork_repo"));
        assert!(is_known_key("draft"));
        assert!(is_known_key("no_pr"));
        assert!(is_known_key("rerere"));
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
    fn parse_bool_accepts_valid_values() {
        assert!(parse_bool("true").unwrap());
        assert!(parse_bool("True").unwrap());
        assert!(parse_bool("TRUE").unwrap());
        assert!(parse_bool("1").unwrap());
        assert!(parse_bool("yes").unwrap());
        assert!(parse_bool("Yes").unwrap());
        assert!(!parse_bool("false").unwrap());
        assert!(!parse_bool("False").unwrap());
        assert!(!parse_bool("0").unwrap());
        assert!(!parse_bool("no").unwrap());
        assert!(!parse_bool("No").unwrap());
    }

    #[test]
    fn parse_bool_rejects_invalid_values() {
        let err = parse_bool("maybe").expect_err("should fail");
        assert!(err.to_string().contains("invalid boolean value"));
    }

    #[test]
    fn set_draft_validates_bool() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        set("draft", "true").expect("set draft true");
        let state = StackState::load().unwrap();
        assert_eq!(state.draft, Some(true));

        set("draft", "false").expect("set draft false");
        let state = StackState::load().unwrap();
        assert_eq!(state.draft, Some(false));

        let err = set("draft", "maybe").expect_err("should fail");
        assert!(err.to_string().contains("invalid boolean value"));
    }

    #[test]
    fn set_no_pr_validates_bool() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        set("no_pr", "yes").expect("set no_pr yes");
        let state = StackState::load().unwrap();
        assert_eq!(state.no_pr, Some(true));
    }

    #[test]
    fn set_rerere_false_disables_git_config() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        set("rerere", "true").expect("set rerere true");
        assert_eq!(git_config("rerere.enabled"), "true");
        assert_eq!(git_config("rerere.autoupdate"), "true");

        set("rerere", "false").expect("set rerere false");
        let state = StackState::load().unwrap();
        assert_eq!(state.rerere, Some(false));
        assert_eq!(git_config("rerere.enabled"), "false");
        assert_eq!(git_config("rerere.autoupdate"), "false");
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

    #[test]
    fn get_returns_bool_keys_as_string() {
        let _guard = take_env_lock();
        let (_repo, _cwd) = setup_state();

        let state = StackState::load().unwrap();
        assert_eq!(get_value(&state, "draft"), None);
        assert_eq!(get_value(&state, "no_pr"), None);
        assert_eq!(get_value(&state, "rerere"), None);
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
        assert_eq!(state.draft, None);
        assert_eq!(state.no_pr, None);
        assert_eq!(state.rerere, None);
    }
}
