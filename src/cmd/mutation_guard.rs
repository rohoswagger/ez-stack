use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::scope::{ScopeDecision, evaluate_scope};
use crate::stack::{BranchMeta, ScopeMode, StackState};
use crate::ui;

#[derive(Debug, Clone)]
pub struct CommitOutcome {
    pub current: String,
    pub before: String,
    pub after: String,
    pub files_changed: u64,
    pub insertions: u64,
    pub deletions: u64,
    pub scope: ScopeReceiptData,
}

#[derive(Debug, Clone)]
pub struct ScopeReceiptData {
    pub scope_defined: bool,
    pub scope_mode: Option<String>,
    pub out_of_scope_files: Vec<String>,
}

pub fn commit_with_guard(
    message: &str,
    all: bool,
    if_changed: bool,
    paths: &[String],
) -> Result<Option<CommitOutcome>> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current));
    }

    if all && !paths.is_empty() {
        bail!(EzError::UserMessage(
            "cannot combine --all (-a) with path arguments\n  → Use `ez commit -am \"msg\"` to stage everything, or `ez commit -m \"msg\" -- <paths>` to stage specific files".to_string()
        ));
    }

    if !paths.is_empty() {
        git::add_paths(paths)?;
    } else if all {
        git::add_all()?;
    }

    if if_changed && !git::has_staged_changes()? {
        return Ok(None);
    }

    if !git::has_staged_changes()? {
        bail!(EzError::NothingToCommit);
    }

    let meta = state.get_branch(&current)?;
    let staged_files = git::staged_files()?;
    let scope = scope_preflight(&current, meta, &staged_files)?;

    let before = git::rev_parse("HEAD")?;
    let pre_modified = git::modified_files();

    if let Err(e) = git::commit(message) {
        report_hook_changes(&pre_modified);
        return Err(e);
    }

    let after = git::rev_parse("HEAD")?;
    let (files_changed, insertions, deletions) = git::diff_stat_numbers();

    Ok(Some(CommitOutcome {
        current,
        before,
        after,
        files_changed,
        insertions,
        deletions,
        scope,
    }))
}

fn scope_preflight(
    branch: &str,
    meta: &BranchMeta,
    staged_files: &[String],
) -> Result<ScopeReceiptData> {
    let Some(patterns) = meta.scope.as_ref() else {
        return Ok(ScopeReceiptData {
            scope_defined: false,
            scope_mode: None,
            out_of_scope_files: Vec::new(),
        });
    };

    let mode = meta.effective_scope_mode();
    let matched_files = git::staged_files_matching_scope(patterns)?;
    let decision = evaluate_scope(patterns, mode, staged_files, &matched_files);
    let receipt = receipt_data_from_scope_decision(&decision);

    if let ScopeDecision::OutOfBounds(report) = decision {
        ui::warn(&format!("Branch scope mismatch for `{branch}`"));
        eprintln!("  Out of scope:");
        for file in &report.out_of_scope_files {
            eprintln!("    {file}");
        }
        ui::hint("Commit only intended files with: `ez commit -m \"...\" -- <paths>`");
        ui::hint("Or update the branch scope with: `ez scope add ...` or `ez scope set ...`");

        if report.mode == ScopeMode::Strict {
            bail!(EzError::UserMessage(format!(
                "staged files are outside the scope for `{branch}`"
            )));
        }
    }

    Ok(receipt)
}

fn receipt_data_from_scope_decision(decision: &ScopeDecision) -> ScopeReceiptData {
    match decision {
        ScopeDecision::NoConfig => ScopeReceiptData {
            scope_defined: false,
            scope_mode: None,
            out_of_scope_files: Vec::new(),
        },
        ScopeDecision::InBounds(report) | ScopeDecision::OutOfBounds(report) => ScopeReceiptData {
            scope_defined: true,
            scope_mode: Some(scope_mode_str(report.mode).to_string()),
            out_of_scope_files: report.out_of_scope_files.clone(),
        },
    }
}

fn report_hook_changes(pre_modified: &[String]) {
    let post_modified = git::modified_files();
    let hook_changed: Vec<&String> = post_modified
        .iter()
        .filter(|file| !pre_modified.contains(file))
        .collect();

    if hook_changed.is_empty() {
        return;
    }

    ui::warn(&format!(
        "Pre-commit hook modified {} file(s):",
        hook_changed.len()
    ));
    for file in &hook_changed {
        eprintln!("  {file}");
    }
    ui::hint(
        "Re-stage and retry with `ez commit -m \"...\" -- <paths>`, `ez commit -am \"...\"`, or `git add -p && ez commit -m \"...\"`",
    );
}

fn scope_mode_str(mode: ScopeMode) -> &'static str {
    match mode {
        ScopeMode::Warn => "warn",
        ScopeMode::Strict => "strict",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::{ScopeDecision, ScopeReport};

    #[test]
    fn receipt_data_marks_no_scope() {
        let receipt = receipt_data_from_scope_decision(&ScopeDecision::NoConfig);
        assert!(!receipt.scope_defined);
        assert!(receipt.scope_mode.is_none());
        assert!(receipt.out_of_scope_files.is_empty());
    }

    #[test]
    fn receipt_data_copies_scope_violation() {
        let decision = ScopeDecision::OutOfBounds(ScopeReport {
            mode: ScopeMode::Strict,
            patterns: vec!["src/auth/**".to_string()],
            in_scope_files: vec!["src/auth/a.rs".to_string()],
            out_of_scope_files: vec!["src/billing/b.rs".to_string()],
        });
        let receipt = receipt_data_from_scope_decision(&decision);
        assert!(receipt.scope_defined);
        assert_eq!(receipt.scope_mode.as_deref(), Some("strict"));
        assert_eq!(
            receipt.out_of_scope_files,
            vec!["src/billing/b.rs".to_string()]
        );
    }
}
