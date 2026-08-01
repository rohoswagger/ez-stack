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
