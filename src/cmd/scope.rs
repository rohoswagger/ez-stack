use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::stack::{BranchMeta, ScopeMode, StackState};
use crate::ui;

pub fn show() -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;
    let meta = current_branch_meta(&state, &current)?;

    if let Some(patterns) = &meta.scope {
        let mode = meta.effective_scope_mode();
        ui::header(&format!("Scope: `{current}`"));
        ui::info(&format!("Mode: {}", scope_mode_str(mode)));
        for pattern in patterns {
            eprintln!("  {pattern}");
        }
    } else {
        ui::info(&format!("No scope configured for `{current}`"));
        ui::hint("Use `ez scope set <pattern>` to configure one");
    }

    Ok(())
}

pub fn add(patterns: &[String], mode: Option<ScopeMode>) -> Result<()> {
    let mut state = StackState::load()?;
    let current = git::current_branch()?;
    current_branch_meta(&state, &current)?;

    let patterns = normalize_scope_patterns(patterns);
    if patterns.is_empty() {
        bail!(EzError::UserMessage(
            "scope add requires at least one non-empty pattern".to_string(),
        ));
    }

    let meta = state.get_branch_mut(&current)?;
    let mut next = meta.scope.clone().unwrap_or_default();
    for pattern in patterns {
        if !next.iter().any(|existing| existing == &pattern) {
            next.push(pattern);
        }
    }
    let mode_to_store = mode.unwrap_or_else(|| meta.effective_scope_mode());
    meta.scope = Some(next.clone());
    meta.scope_mode = Some(mode_to_store);
    state.save()?;

    ui::success(&format!(
        "Updated scope for `{current}` ({})",
        scope_mode_str(mode_to_store)
    ));
    ui::receipt(&serde_json::json!({
        "cmd": "scope",
        "action": "add",
        "branch": current,
        "scope_defined": true,
        "scope_mode": scope_mode_str(mode_to_store),
        "scope": next,
    }));

    Ok(())
}

pub fn set(patterns: &[String], mode: Option<ScopeMode>) -> Result<()> {
    let mut state = StackState::load()?;
    let current = git::current_branch()?;
    current_branch_meta(&state, &current)?;

    let patterns = normalize_scope_patterns(patterns);
    if patterns.is_empty() {
        bail!(EzError::UserMessage(
            "scope set requires at least one non-empty pattern".to_string(),
        ));
    }

    let meta = state.get_branch_mut(&current)?;
    let mode_to_store = mode.unwrap_or_else(|| meta.effective_scope_mode());
    meta.scope = Some(patterns.clone());
    meta.scope_mode = Some(mode_to_store);
    state.save()?;

    ui::success(&format!(
        "Set scope for `{current}` ({})",
        scope_mode_str(mode_to_store)
    ));
    ui::receipt(&serde_json::json!({
        "cmd": "scope",
        "action": "set",
        "branch": current,
        "scope_defined": true,
        "scope_mode": scope_mode_str(mode_to_store),
        "scope": patterns,
    }));

    Ok(())
}

pub fn clear() -> Result<()> {
    let mut state = StackState::load()?;
    let current = git::current_branch()?;
    current_branch_meta(&state, &current)?;

    let meta = state.get_branch_mut(&current)?;
    meta.scope = None;
    meta.scope_mode = None;
    state.save()?;

    ui::success(&format!("Cleared scope for `{current}`"));
    ui::receipt(&serde_json::json!({
        "cmd": "scope",
        "action": "clear",
        "branch": current,
        "scope_defined": false,
    }));

    Ok(())
}

fn current_branch_meta<'a>(state: &'a StackState, current: &str) -> Result<&'a BranchMeta> {
    if current == "HEAD" {
        bail!(EzError::DetachedHead);
    }
    if state.is_trunk(current) {
        bail!(EzError::OnTrunk);
    }
    if !state.is_managed(current) {
        bail!(EzError::BranchNotInStack(current.to_string()));
    }
    state.get_branch(current)
}

fn normalize_scope_patterns(patterns: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for pattern in patterns {
        let trimmed = pattern.trim();
        if trimmed.is_empty() || normalized.iter().any(|p| p == trimmed) {
            continue;
        }
        normalized.push(trimmed.to_string());
    }
    normalized
}

fn scope_mode_str(mode: ScopeMode) -> &'static str {
    match mode {
        ScopeMode::Warn => "warn",
        ScopeMode::Strict => "strict",
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_scope_patterns;

    #[test]
    fn normalize_scope_patterns_dedupes_and_trims() {
        let input = vec![
            " src/auth/** ".to_string(),
            "src/auth/**".to_string(),
            "".to_string(),
            "tests/auth/**".to_string(),
        ];
        assert_eq!(
            normalize_scope_patterns(&input),
            vec!["src/auth/**".to_string(), "tests/auth/**".to_string()]
        );
    }
}
