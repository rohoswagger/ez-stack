use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

/// Start tracking an existing local branch in the ez stack.
///
/// Pure metadata write — no rebase, no network, no branch creation. Use this
/// when you (or a tool) created a branch with raw `git checkout -b` and want
/// ez to take over future commits, restacks, and pushes.
///
/// - `branch` defaults to the current branch.
/// - `parent` is inferred via merge-base if not given: among trunk and the
///   already-tracked branches, the one whose merge-base with the target is
///   the strictly deepest descendant wins. Defaults to trunk if no other
///   branch is a closer ancestor.
/// - `parent_head` is set to the merge-base of (parent, branch), so the next
///   `ez sync`/`ez restack` correctly rebases branch onto current parent tip.
pub fn run(branch: Option<String>, parent: Option<String>) -> Result<()> {
    let mut state = StackState::load()?;

    let target = match branch {
        Some(b) => b,
        None => git::current_branch()?,
    };

    if state.is_trunk(&target) {
        bail!(EzError::UserMessage(format!(
            "`{target}` is the trunk branch and cannot be tracked as a stacked branch\n  → Run `ez create <name>` to start a new stack on top of trunk"
        )));
    }

    if !git::branch_exists(&target) {
        bail!(EzError::UserMessage(format!(
            "branch `{target}` does not exist locally\n  → Run `git branch` to see local branches, or `ez create {target}` to create it"
        )));
    }

    if state.is_managed(&target) {
        let existing = state.get_branch(&target)?;
        bail!(EzError::UserMessage(format!(
            "branch `{target}` is already tracked (parent: `{}`)\n  → Run `ez move --parent <name>` to reparent, or `ez log` to inspect the stack",
            existing.parent
        )));
    }

    let parent_name = match parent {
        Some(p) => {
            if p == target {
                bail!(EzError::UserMessage(format!(
                    "cannot set `{target}` as its own parent"
                )));
            }
            if p != state.trunk && !state.is_managed(&p) {
                bail!(EzError::UserMessage(format!(
                    "parent `{p}` is not the trunk or a tracked branch\n  → Run `ez list` to see tracked branches, or `ez track {p}` first to track it"
                )));
            }
            if !git::branch_exists(&p) {
                bail!(EzError::UserMessage(format!(
                    "parent `{p}` does not exist locally"
                )));
            }
            p
        }
        None => infer_parent(&target, &state)?,
    };

    let parent_head = git::merge_base(&parent_name, &target).map_err(|_| {
        EzError::UserMessage(format!(
            "`{target}` and `{parent_name}` have no common history\n  → Pass `--parent <name>` explicitly"
        ))
    })?;

    let ahead = git::rev_list_count(&parent_head, &target).unwrap_or(0);

    state.add_branch(&target, &parent_name, &parent_head, None, None);
    state.save()?;

    let short = &parent_head[..parent_head.len().min(7)];
    ui::success(&format!(
        "Tracking `{target}` on `{parent_name}` (parent_head: {short})"
    ));
    if ahead > 0 {
        ui::info(&format!(
            "`{target}` is {ahead} commit(s) ahead of `{parent_name}`"
        ));
    } else {
        ui::info(&format!(
            "`{target}` has no commits beyond `{parent_name}` yet"
        ));
    }
    ui::hint(&format!(
        "Run `ez log` to see the stack, or `ez restack` if `{parent_name}` has new commits"
    ));

    ui::receipt(&serde_json::json!({
        "cmd": "track",
        "branch": target,
        "parent": parent_name,
        "parent_head": short,
        "commits_ahead": ahead,
    }));

    Ok(())
}

/// Pick the closest tracked ancestor of `branch` as its parent.
///
/// Trunk is always a fallback. Among trunk + tracked branches, the candidate
/// whose merge-base with `branch` is the strictly deepest descendant wins:
/// that's the candidate closest to `branch` in commit graph terms. Ties go to
/// the existing best (trunk-first iteration), so trunk wins only when no
/// tracked branch is a strict descendant.
pub fn infer_parent(branch: &str, state: &StackState) -> Result<String> {
    let trunk_mb = git::merge_base(&state.trunk, branch).map_err(|_| {
        EzError::UserMessage(format!(
            "could not find a merge-base between `{branch}` and trunk `{}`\n  → Pass `--parent <name>` explicitly",
            state.trunk
        ))
    })?;
    let mut best = (state.trunk.clone(), trunk_mb);

    for candidate in state.branches.keys() {
        if candidate == branch {
            continue;
        }
        let Ok(mb) = git::merge_base(candidate, branch) else {
            continue;
        };
        if mb != best.1 && git::is_ancestor(&best.1, &mb) {
            best = (candidate.clone(), mb);
        }
    }

    Ok(best.0)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    /// Pure version of the parent-selection rule, decoupled from git I/O so we
    /// can test the tie-breaking and descendant-preference logic directly.
    ///
    /// `merge_bases` maps candidate branch → merge-base SHA with the target.
    /// `descendants` maps (ancestor SHA, descendant SHA) → true to model
    /// `git::is_ancestor`. The function reproduces `infer_parent`'s rule:
    /// start with trunk, replace it iff another candidate's merge-base is a
    /// strict descendant of the current best's merge-base.
    fn select_best_parent(
        trunk: &str,
        candidates: &[&str],
        merge_bases: &HashMap<&str, &str>,
        descendants: &HashMap<(&str, &str), bool>,
    ) -> String {
        let mut best_branch = trunk.to_string();
        let mut best_mb = *merge_bases
            .get(trunk)
            .expect("trunk must have a merge-base");

        for c in candidates {
            if *c == trunk {
                continue;
            }
            let Some(mb) = merge_bases.get(*c) else {
                continue;
            };
            let is_strict_descendant =
                *mb != best_mb && *descendants.get(&(best_mb, *mb)).unwrap_or(&false);
            if is_strict_descendant {
                best_branch = c.to_string();
                best_mb = *mb;
            }
        }
        best_branch
    }

    #[test]
    fn defaults_to_trunk_when_no_other_branch_is_a_closer_ancestor() {
        // Scenario: branch B was branched off trunk. Tracked branch A is a
        // sibling — its merge-base with B is also at trunk's tip. Trunk wins.
        let mut mb = HashMap::new();
        mb.insert("main", "sha_root");
        mb.insert("feat/a", "sha_root");
        // sha_root is not a strict descendant of itself.
        let descendants = HashMap::new();

        let got = select_best_parent("main", &["main", "feat/a"], &mb, &descendants);
        assert_eq!(got, "main");
    }

    #[test]
    fn picks_tracked_branch_when_its_merge_base_is_strictly_deeper() {
        // Scenario: branch B was branched off feat/a, which itself was
        // branched off main. merge-base(main, B) = sha_root; merge-base(feat/a, B) = sha_a.
        // sha_a is a descendant of sha_root — feat/a wins.
        let mut mb = HashMap::new();
        mb.insert("main", "sha_root");
        mb.insert("feat/a", "sha_a");

        let mut descendants = HashMap::new();
        descendants.insert(("sha_root", "sha_a"), true);

        let got = select_best_parent("main", &["main", "feat/a"], &mb, &descendants);
        assert_eq!(got, "feat/a");
    }

    #[test]
    fn picks_deepest_when_multiple_tracked_branches_overlap() {
        // Chain: main → feat/a → feat/b → B. Both feat/a and feat/b are
        // ancestors of B; feat/b is deeper. feat/b wins.
        let mut mb = HashMap::new();
        mb.insert("main", "sha_root");
        mb.insert("feat/a", "sha_a");
        mb.insert("feat/b", "sha_b");

        let mut descendants = HashMap::new();
        descendants.insert(("sha_root", "sha_a"), true);
        descendants.insert(("sha_root", "sha_b"), true);
        descendants.insert(("sha_a", "sha_b"), true);

        // Iteration order over HashMap keys is non-deterministic; both orders
        // must produce the same winner.
        let got1 = select_best_parent("main", &["main", "feat/a", "feat/b"], &mb, &descendants);
        let got2 = select_best_parent("main", &["main", "feat/b", "feat/a"], &mb, &descendants);
        assert_eq!(got1, "feat/b");
        assert_eq!(got2, "feat/b");
    }

    #[test]
    fn ignores_sibling_branches_whose_merge_base_is_unrelated_to_target() {
        // feat/a is a sibling of B, branched off main at the same SHA.
        // feat/c is in a different lineage — its merge-base is at trunk too.
        // Neither is a strict descendant; trunk wins.
        let mut mb = HashMap::new();
        mb.insert("main", "sha_root");
        mb.insert("feat/a", "sha_root");
        mb.insert("feat/c", "sha_root");
        let descendants = HashMap::new();

        let got = select_best_parent("main", &["main", "feat/a", "feat/c"], &mb, &descendants);
        assert_eq!(got, "main");
    }
}
