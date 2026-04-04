use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::scope::{ScopeDecision, evaluate_scope};
use crate::stack::{BranchMeta, ScopeMode, StackState};
use crate::ui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageMode {
    Tracked,
    All,
}

pub(crate) fn tracked_only_untracked_hint(untracked_count: usize) -> Option<&'static str> {
    if untracked_count > 0 {
        Some(
            "Untracked files detected. `-a` stages tracked files only, use `-A`/`-Am` to include them.",
        )
    } else {
        None
    }
}

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
    stage_mode: Option<StageMode>,
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

    if stage_mode.is_some() && !paths.is_empty() {
        bail!(EzError::UserMessage(
            "cannot combine --all (-a) or --all-files (-A) with path arguments\n  → Use `ez commit -am \"msg\"` for tracked files, `ez commit -Am \"msg\"` to include untracked files, or `ez commit -m \"msg\" -- <paths>` to stage specific files".to_string()
        ));
    }

    if matches!(stage_mode, Some(StageMode::Tracked)) {
        let (_, _, untracked) = git::working_tree_status();
        if let Some(hint) = tracked_only_untracked_hint(untracked) {
            ui::hint(hint);
        }
    }

    if !paths.is_empty() {
        git::add_paths(paths)?;
    } else if let Some(stage_mode) = stage_mode {
        match stage_mode {
            StageMode::Tracked => git::add_all()?,
            StageMode::All => git::add_all_including_untracked()?,
        }
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
        "Re-stage and retry with `ez commit -m \"...\" -- <paths>`, `ez commit -am \"...\"`, `ez commit -Am \"...\"`, or `git add -p && ez commit -m \"...\"`",
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
    use crate::stack::StackState;
    use crate::test_support::{CwdGuard, cmd_output, init_git_repo, take_env_lock, write_file};

    fn init_managed_feature_repo(
        name: &str,
        scope: Option<Vec<String>>,
        scope_mode: Option<ScopeMode>,
    ) -> std::path::PathBuf {
        let repo = init_git_repo(name);
        let _cwd = CwdGuard::enter(&repo);
        StackState::new("main".to_string())
            .save()
            .expect("save initial state");
        git::create_branch("feat/test").expect("create feature branch");
        let parent_head = git::rev_parse("main").expect("parent head");
        let mut state = StackState::load().expect("load state");
        state.add_branch("feat/test", "main", &parent_head, scope, scope_mode);
        state.save().expect("save managed state");
        repo
    }

    #[test]
    fn receipt_data_marks_no_scope() {
        let receipt = receipt_data_from_scope_decision(&ScopeDecision::NoConfig);
        assert!(!receipt.scope_defined);
        assert!(receipt.scope_mode.is_none());
        assert!(receipt.out_of_scope_files.is_empty());
    }

    #[test]
    fn tracked_only_untracked_hint_only_shows_when_needed() {
        assert!(tracked_only_untracked_hint(0).is_none());
        assert!(
            tracked_only_untracked_hint(1)
                .expect("hint")
                .contains("`-A`/`-Am`")
        );
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

    #[test]
    fn commit_with_guard_rejects_trunk_commits() {
        let _guard = take_env_lock();
        let repo = init_git_repo("mutation-trunk");
        let _cwd = CwdGuard::enter(&repo);
        StackState::new("main".to_string())
            .save()
            .expect("save state");

        let err = commit_with_guard("msg", None, false, &[]).expect_err("trunk commit should fail");
        assert!(matches!(
            err.downcast_ref::<EzError>(),
            Some(EzError::OnTrunk)
        ));
    }

    #[test]
    fn commit_with_guard_rejects_unmanaged_branch() {
        let _guard = take_env_lock();
        let repo = init_git_repo("mutation-unmanaged");
        let _cwd = CwdGuard::enter(&repo);
        StackState::new("main".to_string())
            .save()
            .expect("save state");
        git::create_branch("scratch").expect("create scratch");

        let err =
            commit_with_guard("msg", None, false, &[]).expect_err("unmanaged branch should fail");
        assert!(matches!(
            err.downcast_ref::<EzError>(),
            Some(EzError::BranchNotInStack(name)) if name == "scratch"
        ));
    }

    #[test]
    fn commit_with_guard_if_changed_returns_none_when_nothing_is_staged() {
        let _guard = take_env_lock();
        let repo = init_managed_feature_repo("mutation-if-changed", None, None);
        let _cwd = CwdGuard::enter(&repo);

        let outcome = commit_with_guard("msg", None, true, &[]).expect("guard should succeed");
        assert!(outcome.is_none());
    }

    #[test]
    fn commit_with_guard_rejects_all_plus_paths() {
        let _guard = take_env_lock();
        let repo = init_managed_feature_repo("mutation-all-paths", None, None);
        let _cwd = CwdGuard::enter(&repo);

        let err = commit_with_guard(
            "msg",
            Some(StageMode::Tracked),
            false,
            &["tracked.txt".to_string()],
        )
        .expect_err("all plus paths should fail");
        assert!(
            err.to_string()
                .contains("cannot combine --all (-a) or --all-files (-A) with path arguments"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn commit_with_guard_blocks_out_of_scope_files_in_strict_mode() {
        let _guard = take_env_lock();
        let repo = init_managed_feature_repo(
            "mutation-scope-strict",
            Some(vec!["src/auth/**".to_string()]),
            Some(ScopeMode::Strict),
        );
        let _cwd = CwdGuard::enter(&repo);
        write_file(&repo, "src/billing/invoice.rs", "pub fn invoice() {}\n");

        let err = commit_with_guard(
            "feat: wrong scope",
            None,
            false,
            &["src/billing/invoice.rs".to_string()],
        )
        .expect_err("strict scope should block out-of-scope commit");
        assert!(
            err.to_string()
                .contains("staged files are outside the scope for `feat/test`"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn commit_with_guard_stages_only_selected_paths() {
        let _guard = take_env_lock();
        let repo = init_managed_feature_repo("mutation-selected-paths", None, None);
        let _cwd = CwdGuard::enter(&repo);
        write_file(&repo, "selected.txt", "selected\n");
        write_file(&repo, "ignored.txt", "ignored\n");

        let outcome = commit_with_guard(
            "feat: add selected file",
            None,
            false,
            &["selected.txt".to_string()],
        )
        .expect("commit should succeed")
        .expect("commit outcome");

        assert_eq!(outcome.files_changed, 1);
        assert_eq!(
            cmd_output(
                &repo,
                "git",
                &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]
            ),
            "selected.txt"
        );
        assert_eq!(git::working_tree_status(), (0, 0, 1));
    }

    #[test]
    fn commit_with_guard_all_files_stages_untracked_files() {
        let _guard = take_env_lock();
        let repo = init_managed_feature_repo("mutation-all-files", None, None);
        let _cwd = CwdGuard::enter(&repo);
        write_file(&repo, "new.txt", "new\n");

        let outcome = commit_with_guard("feat: add new file", Some(StageMode::All), false, &[])
            .expect("commit should succeed")
            .expect("commit outcome");

        assert_eq!(outcome.files_changed, 1);
        assert_eq!(
            cmd_output(
                &repo,
                "git",
                &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"]
            ),
            "new.txt"
        );
        assert_eq!(git::working_tree_status(), (0, 0, 0));
    }
}
