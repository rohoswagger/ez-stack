use anyhow::{Result, bail};

use crate::cmd::preflight;
use crate::cmd::rebase_conflict;
use crate::cmd::track;
use crate::cmd::worktree;
use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

/// Why a single branch could not be restacked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestackFailureKind {
    /// The rebase stopped on a content conflict; the rebase itself was aborted.
    Conflict,
    /// git refused to rebase at all (dirty worktree, unresolvable parent, ...).
    Error(String),
}

impl RestackFailureKind {
    fn reason(&self) -> &'static str {
        match self {
            RestackFailureKind::Conflict => "conflict",
            RestackFailureKind::Error(_) => "error",
        }
    }

    fn detail(&self) -> String {
        match self {
            RestackFailureKind::Conflict => "rebase conflict".to_string(),
            RestackFailureKind::Error(msg) => msg.lines().next().unwrap_or(msg).to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RestackFailure {
    pub branch: String,
    pub parent: String,
    pub kind: RestackFailureKind,
}

/// Outcome of restacking a set of branches: how many moved, and which ones were left alone.
#[derive(Debug, Default)]
pub struct RestackReport {
    pub restacked: usize,
    pub failures: Vec<RestackFailure>,
}

impl RestackReport {
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }

    fn branch_list(&self) -> String {
        self.failures
            .iter()
            .map(|f| f.branch.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RestackOptions {
    pub force: bool,
}

/// Resolve the branch's parent and its current tip, healing metadata that git has outgrown.
///
/// If the recorded parent is gone from git — deleted by hand, or merged away in another worktree —
/// re-derive the closest tracked ancestor from the commit graph and record that instead of failing
/// the branch. The recorded parent is a cache of git, not the other way round.
fn resolve_parent(
    state: &mut StackState,
    branch: &str,
    recorded_parent: &str,
) -> std::result::Result<(String, String), String> {
    if let Ok(tip) = git::rev_parse(recorded_parent) {
        return Ok((recorded_parent.to_string(), tip));
    }

    let inferred = track::infer_parent(branch, state).map_err(|e| e.to_string())?;
    let tip = git::rev_parse(&inferred).map_err(|e| e.to_string())?;
    let base = git::merge_base(&inferred, branch).unwrap_or_else(|_| tip.clone());

    ui::warn(&format!(
        "Parent `{recorded_parent}` of `{branch}` no longer exists in git — reparenting onto `{inferred}` (derived from the commit graph)"
    ));
    if let Ok(meta) = state.get_branch_mut(branch) {
        meta.parent = inferred.clone();
        meta.parent_head = base.trim().to_string();
    }

    Ok((inferred, tip))
}

/// The base of the commit range to replay: trusted metadata when it still describes git reality,
/// otherwise a base derived from git itself.
///
/// `parent_head` is only a cache of where a branch forked from its parent. It goes stale whenever
/// history moves outside ez — an amend in another worktree, a parent that was squash-merged and
/// cleaned up, a hand-rolled `git rebase` — and replaying `stale..branch` then either resurrects
/// commits that are not the branch's own or conflicts for no reason at all. git's merge-base is
/// the real authority, so fall back to it whenever the recorded SHA is no longer part of the
/// branch. Returns the base and whether it had to be derived.
pub(crate) fn effective_old_base(
    branch: &str,
    parent: &str,
    stored_parent_head: &str,
) -> (String, bool) {
    if let Ok(parent_tip) = git::rev_parse(parent) {
        let parent_tip = parent_tip.trim();
        if parent_tip != stored_parent_head && git::is_ancestor(parent_tip, branch) {
            return (parent_tip.to_string(), true);
        }
    }

    let merge_base = git::merge_base(parent, branch)
        .ok()
        .map(|base| base.trim().to_string())
        .filter(|base| !base.is_empty());

    if !stored_parent_head.is_empty() && git::is_ancestor(stored_parent_head, branch) {
        if let Some(base) = merge_base.as_deref() {
            if stored_parent_head == base {
                return (stored_parent_head.to_string(), false);
            }
            if git::is_ancestor(stored_parent_head, base) {
                return (base.to_string(), true);
            }
        }
        return (stored_parent_head.to_string(), false);
    }

    if let Some(base) = merge_base {
        return (base, true);
    }

    (stored_parent_head.to_string(), false)
}

/// Leave the repo usable after a failed rebase so the next branch starts from a clean slate.
fn recover_after_failure(branch: &str, current_root: &str) {
    match git::abort_rebase_for_branch(branch, current_root) {
        Ok(true) => ui::info(&format!("Aborted the in-progress rebase on `{branch}`")),
        Ok(false) => {}
        Err(e) => ui::warn(&format!(
            "Could not clean up the interrupted rebase on `{branch}`: {e}\n  Hint: run `git rebase --abort` in that branch's worktree"
        )),
    }
}

/// Restack every branch in `order` whose parent tip has drifted from its recorded `parent_head`.
///
/// Each branch is attempted independently. A conflict or a git-level failure on one branch is
/// recorded and the loop moves on to the next: its `parent_head` is left stale so a later
/// `ez restack` retries exactly that branch, and any half-finished rebase is aborted so the
/// failure cannot cascade into the branches after it. `order` must be topological (parent before
/// child) so a tip moved this pass is seen by its children in the same pass.
///
/// Never returns `Err` — every problem lands in the report. Saving state is the caller's job.
#[cfg(test)]
fn restack_branches(
    state: &mut StackState,
    order: &[String],
    current_root: &str,
    cmd: &str,
) -> RestackReport {
    restack_branches_with_options(state, order, current_root, cmd, RestackOptions::default())
}

pub fn restack_branches_with_options(
    state: &mut StackState,
    order: &[String],
    current_root: &str,
    cmd: &str,
    options: RestackOptions,
) -> RestackReport {
    let mut report = RestackReport::default();

    for branch_name in order {
        let Ok(meta) = state.get_branch(branch_name) else {
            continue;
        };
        let parent = meta.parent.clone();
        let stored_parent_head = meta.parent_head.clone();

        if !git::branch_exists(branch_name) {
            continue;
        }

        let (parent, current_parent_tip) = match resolve_parent(state, branch_name, &parent) {
            Ok(resolved) => resolved,
            Err(detail) => {
                ui::warn(&format!(
                    "Skipped `{branch_name}` — its parent `{parent}` is gone and no replacement could be derived: {detail}"
                ));
                ui::hint(&format!(
                    "Run `ez track {branch_name} --parent <name>` to point it at a parent that exists"
                ));
                ui::receipt(&serde_json::json!({
                    "cmd": cmd,
                    "branch": branch_name,
                    "action": "restack_failed",
                    "reason": "unresolvable_parent",
                    "parent": parent,
                    "detail": detail,
                }));
                report.failures.push(RestackFailure {
                    branch: branch_name.clone(),
                    parent,
                    kind: RestackFailureKind::Error(detail),
                });
                continue;
            }
        };
        // resolve_parent may have rewritten the recorded base while healing the parent.
        let stored_parent_head = state
            .get_branch(branch_name)
            .map(|m| m.parent_head.clone())
            .unwrap_or(stored_parent_head);

        if current_parent_tip == stored_parent_head {
            continue;
        }

        if let Err(error) = worktree::guard_branch_worktree(branch_name, "restack") {
            let detail = error.to_string();
            ui::warn(&format!("Skipped `{branch_name}` — {detail}"));
            ui::receipt(&serde_json::json!({
                "cmd": cmd,
                "branch": branch_name,
                "action": "restack_failed",
                "reason": "worktree_guard",
                "parent": parent,
                "detail": detail,
            }));
            report.failures.push(RestackFailure {
                branch: branch_name.clone(),
                parent,
                kind: RestackFailureKind::Error(detail),
            });
            continue;
        }

        let before_sha = git::rev_parse(branch_name).unwrap_or_default();

        let (old_base, derived) = effective_old_base(branch_name, &parent, &stored_parent_head);
        if derived {
            ui::info(&format!(
                "Recorded base for `{branch_name}` is stale — using git's merge base with `{parent}` instead"
            ));
        }

        let candidate = preflight::RebaseCandidate {
            branch: branch_name.clone(),
            destination_tip: current_parent_tip.clone(),
            old_base: old_base.clone(),
            derived_base: derived,
        };
        let branch_preflight = match preflight::inspect_candidate(&candidate) {
            Ok(branch_preflight) => branch_preflight,
            Err(e) => {
                let detail = e.to_string();
                ui::warn(&format!(
                    "Could not preflight `{branch_name}` before restack: {detail}"
                ));
                report.failures.push(RestackFailure {
                    branch: branch_name.clone(),
                    parent,
                    kind: RestackFailureKind::Error(detail),
                });
                continue;
            }
        };
        if branch_preflight.merge_commits > 0 && !options.force {
            let force_command = match cmd {
                "restack" | "sync" | "move" => format!("ez {cmd} --force"),
                _ => "ez restack --force".to_string(),
            };
            let detail = format!(
                "{} merge commit(s) require `{force_command}` before rebase linearization",
                branch_preflight.merge_commits,
            );
            ui::warn(&format!("Skipped `{branch_name}` — {detail}"));
            ui::receipt(&serde_json::json!({
                "cmd": cmd,
                "branch": branch_name,
                "action": "restack_failed",
                "reason": "merge_commit_preflight",
                "parent": parent,
                "detail": detail,
            }));
            report.failures.push(RestackFailure {
                branch: branch_name.clone(),
                parent,
                kind: RestackFailureKind::Error(detail),
            });
            continue;
        }
        if branch_preflight.all_redundant() {
            let redundant_count = branch_preflight.cherry.redundant;
            ui::info(&format!(
                "Aligning `{branch_name}` to `{parent}` — {redundant_count} commit(s) already applied"
            ));
            match git::align_branch_to_target(
                branch_name,
                &current_parent_tip,
                &branch_preflight.branch_tip,
                current_root,
            ) {
                Ok(()) => {
                    if let Ok(meta) = state.get_branch_mut(branch_name) {
                        meta.parent_head = current_parent_tip;
                    }
                    report.restacked += 1;
                    let after_sha = git::rev_parse(branch_name).unwrap_or_default();
                    ui::receipt(&serde_json::json!({
                        "cmd": cmd,
                        "branch": branch_name,
                        "action": "restacked",
                        "method": "already_applied",
                        "parent": parent,
                        "before": &before_sha[..before_sha.len().min(7)],
                        "after": &after_sha[..after_sha.len().min(7)],
                        "redundant_commits": redundant_count,
                    }));
                }
                Err(e) => {
                    let detail = e.to_string();
                    ui::warn(&format!(
                        "Could not align `{branch_name}` to `{parent}`: {detail}"
                    ));
                    report.failures.push(RestackFailure {
                        branch: branch_name.clone(),
                        parent,
                        kind: RestackFailureKind::Error(detail),
                    });
                }
            }
            continue;
        }

        let sp = ui::spinner(&format!("Restacking `{branch_name}` onto `{parent}`..."));
        let outcome =
            git::rebase_onto_for_branch(&current_parent_tip, &old_base, branch_name, current_root);
        sp.finish_and_clear();

        match outcome {
            Ok(git::RebaseOutcome::RebasingComplete) => {
                if let Ok(meta) = state.get_branch_mut(branch_name) {
                    meta.parent_head = current_parent_tip;
                }
                report.restacked += 1;
                ui::info(&format!("Restacked `{branch_name}` onto `{parent}`"));

                let after_sha = git::rev_parse(branch_name).unwrap_or_default();
                ui::receipt(&serde_json::json!({
                    "cmd": cmd,
                    "branch": branch_name,
                    "action": "restacked",
                    "parent": parent,
                    "before": &before_sha[..before_sha.len().min(7)],
                    "after": &after_sha[..after_sha.len().min(7)],
                    "redundant_commits": branch_preflight.cherry.redundant,
                }));
            }
            Ok(git::RebaseOutcome::Conflict(conflict)) => {
                rebase_conflict::report(cmd, branch_name, &parent, &conflict, "ez restack");
                recover_after_failure(branch_name, current_root);
                ui::info(&format!(
                    "Left `{branch_name}` where it was and continued with the rest of the stack"
                ));
                report.failures.push(RestackFailure {
                    branch: branch_name.clone(),
                    parent,
                    kind: RestackFailureKind::Conflict,
                });
            }
            Err(e) => {
                let detail = e.to_string();
                ui::warn(&format!(
                    "Could not restack `{branch_name}` onto `{parent}`: {detail}"
                ));
                ui::hint(&format!(
                    "Fix `{branch_name}` (commit or stash its changes), then run `ez restack`"
                ));
                ui::receipt(&serde_json::json!({
                    "cmd": cmd,
                    "branch": branch_name,
                    "action": "restack_failed",
                    "reason": "git_error",
                    "parent": parent,
                    "detail": detail,
                }));
                report.failures.push(RestackFailure {
                    branch: branch_name.clone(),
                    parent,
                    kind: RestackFailureKind::Error(detail),
                });
            }
        }
    }

    report
}

/// Summarize the branches that were left un-restacked and turn them into the command's error.
///
/// Call this only after the caller has finished its own cleanup (saving state, returning to the
/// original branch) — everything that could succeed should already have succeeded by now.
pub fn incomplete_error(cmd: &str, report: &RestackReport) -> anyhow::Error {
    let count = report.failures.len();
    ui::warn(&format!(
        "{count} branch(es) could not be restacked — the rest of the stack is up to date"
    ));
    for failure in &report.failures {
        eprintln!(
            "    {} (onto {}): {}",
            failure.branch,
            failure.parent,
            failure.kind.detail()
        );
    }
    ui::hint(
        "Resolve the branches above, then run `ez restack` to finish — the branches that already restacked will be skipped",
    );
    ui::receipt(&serde_json::json!({
        "cmd": cmd,
        "action": "restack_incomplete",
        "restacked": report.restacked,
        "failed": report.failures.iter().map(|f| serde_json::json!({
            "branch": f.branch,
            "parent": f.parent,
            "reason": f.kind.reason(),
            "detail": f.kind.detail(),
        })).collect::<Vec<_>>(),
    }));

    EzError::RestackIncomplete {
        count,
        branches: report.branch_list(),
    }
    .into()
}

/// Restack the transitive descendants of `root` after `root`'s tip moved.
///
/// Walks `root`'s subtree in topological order (parent before child) and rebases
/// any branch whose parent tip no longer matches its recorded `parent_head`,
/// updating `parent_head` on success. Because a parent is always restacked before
/// its children, a tip moved this pass is observed by the next iteration — so the
/// whole subtree converges in one pass. This is the shared path for every
/// auto-restack-on-mutation command (`commit`, `amend`, `move`); restacking only
/// direct children left grandchildren detached from the stack.
///
/// A branch that cannot be restacked is skipped rather than aborting the cascade, so its
/// siblings still get updated; state is saved and `return_to` restored either way. Returns the
/// number of branches actually restacked, or bails with `RestackIncomplete` if any were skipped.
pub fn cascade_restack(
    state: &mut StackState,
    root: &str,
    current_root: &str,
    return_to: &str,
    cmd: &str,
) -> Result<usize> {
    cascade_restack_with_options(
        state,
        root,
        current_root,
        return_to,
        cmd,
        RestackOptions::default(),
    )
}

pub fn cascade_restack_with_options(
    state: &mut StackState,
    root: &str,
    current_root: &str,
    return_to: &str,
    cmd: &str,
    options: RestackOptions,
) -> Result<usize> {
    let order = state.descendants_topo(root);
    let report = restack_branches_with_options(state, &order, current_root, cmd, options);

    if !report.is_clean() {
        state.save()?;
        git::checkout(return_to)?;
        bail!(incomplete_error(cmd, &report));
    }

    Ok(report.restacked)
}

pub fn run(force: bool) -> Result<()> {
    let mut state = StackState::load()?;
    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }
    let original_branch = git::current_branch()?;
    let current_root = git::repo_root()?;

    let fetch_remote = state.fetch_remote().to_string();
    ui::info(&format!("Fetching from `{fetch_remote}`..."));
    git::fetch(&fetch_remote)?;
    match git::update_branch_to_latest_remote(
        &fetch_remote,
        &state.trunk,
        &original_branch,
        &current_root,
    ) {
        Ok(true) => ui::info(&format!("Updated `{}` to latest", state.trunk)),
        Ok(false) => {}
        Err(e) => ui::warn(&format!("Could not update `{}` — {e}", state.trunk)),
    }

    let order = state.topo_order();
    let candidates = preflight::restack_candidates(&state, &order);
    preflight::run("restack", force, &candidates)?;
    let report = restack_branches_with_options(
        &mut state,
        &order,
        &current_root,
        "restack",
        RestackOptions { force },
    );

    // Return to the original branch and persist whatever succeeded before surfacing failures.
    git::checkout(&original_branch)?;
    state.save()?;

    if report.restacked > 0 {
        ui::success(&format!("Restacked {} branch(es)", report.restacked));
    } else if report.is_clean() {
        ui::info("All branches are up to date — nothing to restack");
    }

    if !report.is_clean() {
        bail!(incomplete_error("restack", &report));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{CwdGuard, init_git_repo, run_cmd, take_env_lock, write_file};

    /// Commit a new file on `branch` and return the resulting tip SHA.
    fn commit_file(repo: &std::path::Path, branch: &str, file: &str) -> String {
        git::checkout(branch).expect("checkout");
        write_file(repo, file, "x\n");
        run_cmd(repo, "git", &["add", file]);
        run_cmd(repo, "git", &["commit", "-m", &format!("add {file}")]);
        git::rev_parse(branch).expect("rev-parse")
    }

    /// Commit `contents` to `file` on `branch` and return the resulting tip SHA.
    fn commit_contents(
        repo: &std::path::Path,
        branch: &str,
        file: &str,
        contents: &str,
        message: &str,
    ) -> String {
        git::checkout(branch).expect("checkout");
        write_file(repo, file, contents);
        run_cmd(repo, "git", &["add", file]);
        run_cmd(repo, "git", &["commit", "-m", message]);
        git::rev_parse(branch).expect("rev-parse")
    }

    #[test]
    fn restack_branches_skips_a_conflicting_branch_and_keeps_going() {
        let _guard = take_env_lock();
        let repo = init_git_repo("restack-isolate-conflict");
        let _cwd = CwdGuard::enter(&repo);

        let main_sha = git::rev_parse("main").expect("main sha");

        // Two independent branches off main. feat/bad edits the same line main will change.
        git::create_branch_at("feat/bad", "main").expect("branch bad");
        commit_contents(&repo, "feat/bad", "tracked.txt", "branch\n", "branch edit");
        git::create_branch_at("feat/good", "main").expect("branch good");
        commit_file(&repo, "feat/good", "good.txt");

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/bad", "main", &main_sha, None, None);
        state.add_branch("feat/good", "main", &main_sha, None, None);
        state.save().expect("save state");

        // Advance main so both branches are stale; feat/bad will conflict.
        let main_sha2 = commit_contents(&repo, "main", "tracked.txt", "trunk\n", "trunk edit");
        git::checkout("main").expect("checkout main");

        let root = git::repo_root().expect("repo root");
        let order = vec!["feat/bad".to_string(), "feat/good".to_string()];
        let report = restack_branches(&mut state, &order, &root, "test");

        // The conflicting branch is reported, not fatal.
        assert_eq!(report.failures.len(), 1, "only feat/bad should fail");
        assert_eq!(report.failures[0].branch, "feat/bad");
        assert_eq!(report.failures[0].kind, RestackFailureKind::Conflict);

        // The branch after it in the order still got restacked.
        assert_eq!(report.restacked, 1, "feat/good should still be restacked");
        assert!(git::is_ancestor("main", "feat/good"));
        assert_eq!(
            state.get_branch("feat/good").expect("good").parent_head,
            main_sha2
        );

        // The failed branch keeps its stale base so a later `ez restack` retries exactly it.
        assert_eq!(
            state.get_branch("feat/bad").expect("bad").parent_head,
            main_sha,
        );
        assert!(!git::is_ancestor("main", "feat/bad"));

        // And the repo is not left mid-rebase.
        assert!(
            !git::abort_rebase_for_branch("feat/bad", &root).expect("abort check"),
            "no rebase should still be in progress after the failure"
        );
    }

    #[test]
    fn restack_branches_derives_base_when_metadata_went_stale() {
        let _guard = take_env_lock();
        let repo = init_git_repo("restack-stale-base");
        let _cwd = CwdGuard::enter(&repo);

        let main_sha = git::rev_parse("main").expect("main sha");

        // feat/a forks from main and carries one commit.
        git::create_branch_at("feat/a", "main").expect("branch a");
        commit_file(&repo, "feat/a", "a.txt");

        // Record a parent_head that no longer describes git: a SHA on a branch that is not part
        // of feat/a's history at all (the shape left behind by a merged-and-deleted parent).
        git::create_branch_at("gone", "main").expect("branch gone");
        let bogus_sha = commit_file(&repo, "gone", "gone.txt");
        run_cmd(&repo, "git", &["checkout", "main"]);
        run_cmd(&repo, "git", &["branch", "-D", "gone"]);

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", &bogus_sha, None, None);
        state.save().expect("save state");

        // Advance main so a restack is attempted.
        let main_sha2 = commit_file(&repo, "main", "trunk.txt");
        assert_ne!(main_sha2, main_sha);
        git::checkout("main").expect("checkout main");

        let root = git::repo_root().expect("repo root");
        let report = restack_branches(&mut state, &["feat/a".to_string()], &root, "test");

        assert!(
            report.is_clean(),
            "stale metadata must not fail the restack: {:?}",
            report.failures
        );
        assert_eq!(report.restacked, 1);
        assert!(git::is_ancestor("main", "feat/a"));

        // Deriving the base from git means feat/a keeps its own commit and picks up nothing
        // from the deleted branch.
        git::checkout("feat/a").expect("checkout a");
        assert!(repo.join("a.txt").exists(), "feat/a keeps its own commit");
        assert!(
            !repo.join("gone.txt").exists(),
            "the unrelated branch's commit must not be replayed onto feat/a"
        );
        assert_eq!(
            state.get_branch("feat/a").expect("a").parent_head,
            main_sha2
        );
    }

    #[test]
    fn restack_branches_reparents_when_the_recorded_parent_is_gone_from_git() {
        let _guard = take_env_lock();
        let repo = init_git_repo("restack-parent-gone");
        let _cwd = CwdGuard::enter(&repo);

        let main_sha = git::rev_parse("main").expect("main sha");

        // main -> feat/a -> feat/b, then feat/a is deleted outside ez.
        git::create_branch_at("feat/a", "main").expect("branch a");
        let a_sha = commit_file(&repo, "feat/a", "a.txt");
        git::create_branch_at("feat/b", "feat/a").expect("branch b");
        commit_file(&repo, "feat/b", "b.txt");

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", &main_sha, None, None);
        state.add_branch("feat/b", "feat/a", &a_sha, None, None);
        state.save().expect("save state");

        run_cmd(&repo, "git", &["checkout", "main"]);
        run_cmd(&repo, "git", &["branch", "-D", "feat/a"]);
        let main_sha2 = commit_file(&repo, "main", "trunk.txt");
        git::checkout("main").expect("checkout main");

        let root = git::repo_root().expect("repo root");
        let order = vec!["feat/a".to_string(), "feat/b".to_string()];
        let report = restack_branches(&mut state, &order, &root, "test");

        // feat/a is simply absent from git and skipped; feat/b heals onto trunk instead of failing.
        assert!(
            report.is_clean(),
            "a deleted parent must not fail the run: {:?}",
            report.failures
        );
        assert_eq!(report.restacked, 1);
        assert_eq!(state.get_branch("feat/b").expect("b").parent, "main");
        assert_eq!(
            state.get_branch("feat/b").expect("b").parent_head,
            main_sha2
        );
        assert!(git::is_ancestor("main", "feat/b"));
    }

    #[test]
    fn effective_old_base_trusts_metadata_that_still_matches_git() {
        let _guard = take_env_lock();
        let repo = init_git_repo("restack-base-trusted");
        let _cwd = CwdGuard::enter(&repo);

        let main_sha = git::rev_parse("main").expect("main sha");
        git::create_branch_at("feat/a", "main").expect("branch a");
        commit_file(&repo, "feat/a", "a.txt");

        let (base, derived) = effective_old_base("feat/a", "main", &main_sha);
        assert_eq!(base, main_sha);
        assert!(!derived, "a valid recorded base should be used as-is");
    }

    #[test]
    fn cascade_restack_rebases_grandchildren_not_just_direct_children() {
        let _guard = take_env_lock();
        let repo = init_git_repo("cascade-grandchildren");
        let _cwd = CwdGuard::enter(&repo);

        let main_sha = git::rev_parse("main").expect("main sha");

        // Build a 3-deep linear stack: main -> feat/a -> feat/b -> feat/c.
        git::create_branch_at("feat/a", "main").expect("branch a");
        let a_sha1 = commit_file(&repo, "feat/a", "a.txt");
        git::create_branch_at("feat/b", "feat/a").expect("branch b");
        let b_sha1 = commit_file(&repo, "feat/b", "b.txt");
        git::create_branch_at("feat/c", "feat/b").expect("branch c");
        commit_file(&repo, "feat/c", "c.txt");

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", &main_sha, None, None);
        state.add_branch("feat/b", "feat/a", &a_sha1, None, None);
        state.add_branch("feat/c", "feat/b", &b_sha1, None, None);
        state.save().expect("save state");

        // Advance feat/a — this is the mutation that auto-restack must cascade.
        let a_sha2 = commit_file(&repo, "feat/a", "a2.txt");
        assert_ne!(a_sha2, a_sha1);

        let root = git::repo_root().expect("repo root");
        let restacked =
            cascade_restack(&mut state, "feat/a", &root, "feat/a", "test").expect("cascade");

        // Both the direct child AND the grandchild must be restacked.
        assert_eq!(restacked, 2, "both feat/b and feat/c should be restacked");

        // feat/a's new commit must have propagated all the way to the grandchild.
        assert!(
            git::is_ancestor("feat/a", "feat/c"),
            "feat/a tip must be an ancestor of the grandchild feat/c"
        );
        git::checkout("feat/c").expect("checkout c");
        assert!(
            repo.join("a2.txt").exists(),
            "feat/a's new file must reach the grandchild's working tree"
        );

        // Metadata must track the new tips so the equality guard stays accurate.
        assert_eq!(state.get_branch("feat/b").expect("b").parent_head, a_sha2);
        let b_sha2 = git::rev_parse("feat/b").expect("b sha2");
        assert_eq!(state.get_branch("feat/c").expect("c").parent_head, b_sha2);
    }

    #[test]
    fn cascade_restack_saves_failure_state_and_returns_to_root() {
        let _guard = take_env_lock();
        let repo = init_git_repo("cascade-conflict");
        let _cwd = CwdGuard::enter(&repo);

        let main_sha = git::rev_parse("main").expect("main sha");

        git::create_branch_at("feat/a", "main").expect("branch a");
        let a_sha1 = commit_file(&repo, "feat/a", "a.txt");
        git::create_branch_at("feat/b", "feat/a").expect("branch b");
        commit_contents(&repo, "feat/b", "tracked.txt", "child\n", "child edit");

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", &main_sha, None, None);
        state.add_branch("feat/b", "feat/a", &a_sha1, None, None);
        state.save().expect("save state");

        let a_sha2 = commit_contents(&repo, "feat/a", "tracked.txt", "parent\n", "parent edit");
        assert_ne!(a_sha1, a_sha2);

        let root = git::repo_root().expect("repo root");
        let error = cascade_restack(&mut state, "feat/a", &root, "feat/a", "test")
            .expect_err("conflicting child should make cascade incomplete");

        assert!(error.to_string().contains("could not be restacked"));
        assert_eq!(git::current_branch().expect("current branch"), "feat/a");
        assert_eq!(
            state.get_branch("feat/b").expect("b").parent_head,
            a_sha1,
            "failed child keeps its stale parent_head for retry"
        );
        assert_eq!(
            StackState::load()
                .expect("load saved state")
                .get_branch("feat/b")
                .expect("saved b")
                .parent_head,
            a_sha1,
            "cascade failure should persist the retryable state"
        );
        assert!(
            !git::abort_rebase_for_branch("feat/b", &root).expect("abort check"),
            "failed cascade should not leave a rebase in progress"
        );
    }
}
