use anyhow::Result;
use std::collections::BTreeSet;

use crate::cmd::track;
use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

#[derive(Debug, Clone)]
pub struct RebaseCandidate {
    pub branch: String,
    pub destination_tip: String,
    pub old_base: String,
    pub derived_base: bool,
}

struct ResolvedCandidate {
    parent: String,
    candidate: RebaseCandidate,
    initially_stale: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CherryStats {
    pub unique: u64,
    pub redundant: u64,
}

#[derive(Debug, Clone)]
pub struct BranchPreflight {
    pub branch: String,
    pub branch_tip: String,
    pub derived_base: bool,
    pub merge_commits: u64,
    pub cherry: CherryStats,
}

impl BranchPreflight {
    pub fn all_redundant(&self) -> bool {
        self.cherry.unique == 0 && self.cherry.redundant > 0 && self.merge_commits == 0
    }
}

#[derive(Debug, Clone, Default)]
pub struct RebasePreflight {
    pub branches: Vec<BranchPreflight>,
}

impl RebasePreflight {
    pub fn merge_commits(&self) -> u64 {
        self.branches
            .iter()
            .map(|branch| branch.merge_commits)
            .sum()
    }
}

pub fn cherry_stats(destination: &str, branch: &str, old_base: &str) -> Result<CherryStats> {
    let cherry = git::cherry_from(destination, branch, old_base)?;
    let mut stats = CherryStats::default();
    for line in cherry.lines() {
        if line.starts_with("+ ") {
            stats.unique += 1;
        } else if line.starts_with("- ") {
            stats.redundant += 1;
        }
    }
    Ok(stats)
}

pub fn inspect_candidate(candidate: &RebaseCandidate) -> Result<BranchPreflight> {
    let branch_tip = git::rev_parse(&candidate.branch)?;
    let merge_commits = git::rev_list_merge_count(&candidate.old_base, &candidate.branch)?;
    let cherry = cherry_stats(
        &candidate.destination_tip,
        &candidate.branch,
        &candidate.old_base,
    )?;
    Ok(BranchPreflight {
        branch: candidate.branch.clone(),
        branch_tip,
        derived_base: candidate.derived_base,
        merge_commits,
        cherry,
    })
}

pub fn inspect_candidates(candidates: &[RebaseCandidate]) -> Result<RebasePreflight> {
    let mut branches = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        branches.push(inspect_candidate(candidate)?);
    }
    Ok(RebasePreflight { branches })
}

pub fn restack_candidates(state: &StackState, order: &[String]) -> Vec<RebaseCandidate> {
    let mut candidates = Vec::new();
    let mut moving_branches = BTreeSet::new();
    for branch in order {
        let Some(meta) = state.branches.get(branch) else {
            continue;
        };
        let Some(resolved) = resolved_candidate(state, branch, &meta.parent, &meta.parent_head)
        else {
            continue;
        };
        let parent_will_move = moving_branches.contains(&resolved.parent);
        if !resolved.initially_stale && !parent_will_move {
            continue;
        }

        if parent_will_move || resolved.candidate.destination_tip != resolved.candidate.old_base {
            moving_branches.insert(branch.clone());
        }
        candidates.push(resolved.candidate);
    }
    candidates
}

fn resolved_candidate(
    state: &StackState,
    branch: &str,
    recorded_parent: &str,
    recorded_parent_head: &str,
) -> Option<ResolvedCandidate> {
    if !git::branch_exists(branch) {
        return None;
    }

    if let Ok(destination_tip) = git::rev_parse(recorded_parent) {
        let initially_stale = destination_tip != recorded_parent_head;
        let (old_base, derived_base) =
            crate::cmd::restack::effective_old_base(branch, recorded_parent, recorded_parent_head);
        return Some(ResolvedCandidate {
            parent: recorded_parent.to_string(),
            candidate: RebaseCandidate {
                branch: branch.to_string(),
                destination_tip,
                old_base,
                derived_base,
            },
            initially_stale,
        });
    }

    let inferred = track::infer_parent(branch, state).ok()?;
    let destination_tip = git::rev_parse(&inferred).ok()?;
    let old_base = git::merge_base(&inferred, branch)
        .unwrap_or_else(|_| destination_tip.clone())
        .trim()
        .to_string();
    Some(ResolvedCandidate {
        parent: inferred,
        candidate: RebaseCandidate {
            branch: branch.to_string(),
            destination_tip,
            old_base,
            derived_base: true,
        },
        initially_stale: true,
    })
}

pub fn move_candidates(
    state: &StackState,
    current: &str,
    onto: &str,
) -> Result<Vec<RebaseCandidate>> {
    let mut candidates = Vec::new();
    let current_meta = state.get_branch(current)?;
    let destination_tip = git::rev_parse(onto)?;
    let (old_base, derived_base) = crate::cmd::restack::effective_old_base(
        current,
        &current_meta.parent,
        &current_meta.parent_head,
    );
    candidates.push(RebaseCandidate {
        branch: current.to_string(),
        destination_tip,
        old_base,
        derived_base,
    });

    for branch in state.descendants_topo(current) {
        let meta = state.get_branch(&branch)?;
        if let Some(resolved) = resolved_candidate(state, &branch, &meta.parent, &meta.parent_head)
        {
            candidates.push(resolved.candidate);
        }
    }

    Ok(candidates)
}

pub fn enforce(cmd: &str, force: bool, preflight: &RebasePreflight) -> Result<()> {
    let merge_commits = preflight.merge_commits();
    let status = if merge_commits == 0 {
        "ok"
    } else if force {
        "forced"
    } else {
        "blocked"
    };
    let merge_branches: Vec<&str> = preflight
        .branches
        .iter()
        .filter(|branch| branch.merge_commits > 0)
        .map(|branch| branch.branch.as_str())
        .collect();
    let derived_bases = preflight
        .branches
        .iter()
        .filter(|branch| branch.derived_base)
        .count() as u64;
    let redundant_commits: u64 = preflight
        .branches
        .iter()
        .map(|branch| branch.cherry.redundant)
        .sum();
    let all_redundant: Vec<&str> = preflight
        .branches
        .iter()
        .filter(|branch| branch.all_redundant())
        .map(|branch| branch.branch.as_str())
        .collect();
    let hint = if merge_commits > 0 && !force {
        format!("Re-run `ez {cmd} --force` to linearize merge commits during rebase")
    } else {
        String::new()
    };

    ui::receipt(&serde_json::json!({
        "cmd": cmd,
        "action": "rebase_preflight",
        "status": status,
        "forced": force,
        "branches_checked": preflight.branches.len(),
        "merge_commits": merge_commits,
        "merge_branches": merge_branches,
        "derived_bases": derived_bases,
        "redundant_commits": redundant_commits,
        "all_redundant": all_redundant,
        "hint": hint,
    }));

    if merge_commits > 0 && !force {
        ui::warn(&format!(
            "Rebase preflight blocked `{cmd}`: {merge_commits} merge commit(s) would be linearized"
        ));
        ui::hint(&hint);
        return Err(EzError::UserMessage(format!(
            "`ez {cmd}` blocked by merge commits in {} branch(es)",
            merge_branches.len()
        ))
        .into());
    }

    Ok(())
}

pub fn run(cmd: &str, force: bool, candidates: &[RebaseCandidate]) -> Result<RebasePreflight> {
    let preflight = inspect_candidates(candidates)?;
    enforce(cmd, force, &preflight)?;
    Ok(preflight)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{CwdGuard, init_git_repo, run_cmd, take_env_lock, write_file};

    fn commit_file(repo: &std::path::Path, name: &str, contents: &str, message: &str) -> String {
        write_file(repo, name, contents);
        run_cmd(repo, "git", &["add", name]);
        run_cmd(repo, "git", &["commit", "-m", message]);
        crate::test_support::cmd_output(repo, "git", &["rev-parse", "HEAD"])
    }

    fn branch_preflight(
        branch: &str,
        derived_base: bool,
        merge_commits: u64,
        unique: u64,
        redundant: u64,
    ) -> BranchPreflight {
        BranchPreflight {
            branch: branch.to_string(),
            branch_tip: format!("{branch}-tip"),
            derived_base,
            merge_commits,
            cherry: CherryStats { unique, redundant },
        }
    }

    #[test]
    fn all_redundant_requires_only_redundant_non_merge_commits() {
        assert!(branch_preflight("feat/redundant", false, 0, 0, 2).all_redundant());
        assert!(!branch_preflight("feat/unique", false, 0, 1, 2).all_redundant());
        assert!(!branch_preflight("feat/merge", false, 1, 0, 2).all_redundant());
        assert!(!branch_preflight("feat/empty", false, 0, 0, 0).all_redundant());
    }

    #[test]
    fn merge_commits_sums_all_branch_reports() {
        let preflight = RebasePreflight {
            branches: vec![
                branch_preflight("feat/a", false, 2, 0, 0),
                branch_preflight("feat/b", true, 3, 0, 1),
            ],
        };

        assert_eq!(preflight.merge_commits(), 5);
    }

    #[test]
    fn enforce_allows_clean_and_forced_merge_preflights() {
        let clean = RebasePreflight {
            branches: vec![branch_preflight("feat/clean", true, 0, 0, 1)],
        };
        enforce("sync", false, &clean).expect("clean preflight should pass");

        let forced = RebasePreflight {
            branches: vec![branch_preflight("feat/merge", true, 1, 0, 0)],
        };
        enforce("sync", true, &forced).expect("forced merge preflight should pass");
    }

    #[test]
    fn enforce_blocks_unforced_merge_preflights() {
        let preflight = RebasePreflight {
            branches: vec![
                branch_preflight("feat/a", true, 0, 0, 1),
                branch_preflight("feat/merge", false, 2, 1, 0),
            ],
        };

        let err = enforce("sync", false, &preflight)
            .expect_err("unforced merge preflight should be blocked");

        assert!(
            err.to_string()
                .contains("`ez sync` blocked by merge commits in 1 branch(es)"),
            "unexpected preflight error: {err:#}"
        );
    }

    #[test]
    fn inspect_candidates_counts_unique_and_redundant_cherry_commits() {
        let _guard = take_env_lock();
        let repo = init_git_repo("preflight-cherry-stats");
        let _cwd = CwdGuard::enter(&repo);
        let base = crate::test_support::cmd_output(&repo, "git", &["rev-parse", "HEAD"]);
        run_cmd(&repo, "git", &["checkout", "-b", "feat/unique"]);
        commit_file(&repo, "unique.txt", "unique\n", "unique");
        run_cmd(&repo, "git", &["checkout", "main"]);
        run_cmd(&repo, "git", &["checkout", "-b", "feat/redundant"]);
        let redundant = commit_file(&repo, "redundant.txt", "redundant\n", "redundant");
        run_cmd(&repo, "git", &["checkout", "main"]);
        commit_file(&repo, "main-only.txt", "main only\n", "main-only");
        run_cmd(&repo, "git", &["cherry-pick", &redundant]);
        let destination_tip = crate::test_support::cmd_output(&repo, "git", &["rev-parse", "HEAD"]);

        let preflight = inspect_candidates(&[
            RebaseCandidate {
                branch: "feat/unique".to_string(),
                destination_tip: destination_tip.clone(),
                old_base: base.clone(),
                derived_base: false,
            },
            RebaseCandidate {
                branch: "feat/redundant".to_string(),
                destination_tip,
                old_base: base,
                derived_base: true,
            },
        ])
        .expect("inspect candidates");

        assert_eq!(preflight.branches.len(), 2);
        assert_eq!(preflight.branches[0].cherry.unique, 1);
        assert_eq!(preflight.branches[0].cherry.redundant, 0);
        assert_eq!(preflight.branches[1].cherry.unique, 0);
        assert_eq!(preflight.branches[1].cherry.redundant, 1);
        assert!(preflight.branches[1].derived_base);
    }

    #[test]
    fn restack_candidates_skip_missing_branches_and_infer_missing_parents() {
        let _guard = take_env_lock();
        let repo = init_git_repo("preflight-restack-candidates");
        let _cwd = CwdGuard::enter(&repo);
        let main_tip = crate::test_support::cmd_output(&repo, "git", &["rev-parse", "HEAD"]);
        let mut state = StackState::new("main".to_string());
        run_cmd(&repo, "git", &["checkout", "-b", "feat/orphan"]);
        commit_file(&repo, "orphan.txt", "orphan\n", "orphan");
        state.add_branch("feat/missing", "main", &main_tip, None, None);
        state.add_branch("feat/orphan", "gone-parent", "deadbeef", None, None);

        let candidates = restack_candidates(
            &state,
            &[
                "unmanaged".to_string(),
                "feat/missing".to_string(),
                "feat/orphan".to_string(),
            ],
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].branch, "feat/orphan");
        assert_eq!(candidates[0].destination_tip, main_tip);
        assert!(candidates[0].derived_base);
    }

    #[test]
    fn move_candidates_include_descendants_with_resolved_bases() {
        let _guard = take_env_lock();
        let repo = init_git_repo("preflight-move-candidates");
        let _cwd = CwdGuard::enter(&repo);
        let main_tip = crate::test_support::cmd_output(&repo, "git", &["rev-parse", "HEAD"]);
        let mut state = StackState::new("main".to_string());
        run_cmd(&repo, "git", &["checkout", "-b", "feat/base"]);
        let base_tip = commit_file(&repo, "base.txt", "base\n", "base");
        run_cmd(&repo, "git", &["checkout", "-b", "feat/child"]);
        commit_file(&repo, "child.txt", "child\n", "child");
        state.add_branch("feat/base", "main", &main_tip, None, None);
        state.add_branch("feat/child", "feat/base", &base_tip, None, None);

        let candidates =
            move_candidates(&state, "feat/base", "main").expect("move candidates should resolve");

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.branch.as_str())
                .collect::<Vec<_>>(),
            vec!["feat/base", "feat/child"]
        );
        assert_eq!(candidates[0].destination_tip, main_tip);
        assert_eq!(candidates[1].destination_tip, base_tip);
    }
}
