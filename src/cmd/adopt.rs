use anyhow::{Result, bail};
use std::collections::{HashMap, HashSet};

use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

/// Information about a PR that can be adopted into the stack.
#[derive(Debug, Clone)]
struct AdoptCandidate {
    branch: String,
    base: String,
    pr_number: u64,
    title: String,
    is_draft: bool,
}

/// Build the adoption graph from open PRs.
/// Returns candidates keyed by branch name, only including branches whose
/// base chain leads back to trunk.
fn build_adopt_graph(trunk: &str, prs: &HashMap<String, github::PrInfo>) -> Vec<AdoptCandidate> {
    // Filter to open PRs only.
    let open_prs: HashMap<&str, &github::PrInfo> = prs
        .iter()
        .filter(|(_, pr)| pr.state == "OPEN" && !pr.merged)
        .map(|(branch, pr)| (branch.as_str(), pr))
        .collect();

    // Walk each PR's base chain to see if it leads to trunk.
    // A branch is adoptable if its base is either trunk or another open PR
    // whose own base chain leads to trunk.
    let mut valid: HashMap<String, AdoptCandidate> = HashMap::new();

    fn is_rooted_in_trunk(
        branch: &str,
        trunk: &str,
        open_prs: &HashMap<&str, &github::PrInfo>,
        cache: &mut HashMap<String, bool>,
    ) -> bool {
        if branch == trunk {
            return true;
        }
        if let Some(&cached) = cache.get(branch) {
            return cached;
        }
        // Prevent infinite recursion on cycles.
        cache.insert(branch.to_string(), false);

        let result = if let Some(pr) = open_prs.get(branch) {
            is_rooted_in_trunk(&pr.base, trunk, open_prs, cache)
        } else {
            false
        };
        cache.insert(branch.to_string(), result);
        result
    }

    let mut cache = HashMap::new();
    for (branch, pr) in &open_prs {
        if is_rooted_in_trunk(branch, trunk, &open_prs, &mut cache) {
            valid.insert(
                branch.to_string(),
                AdoptCandidate {
                    branch: branch.to_string(),
                    base: pr.base.clone(),
                    pr_number: pr.number,
                    title: pr.title.clone(),
                    is_draft: pr.is_draft,
                },
            );
        }
    }

    // Sort topologically: parents before children.
    let mut sorted = Vec::new();
    let mut visited = std::collections::HashSet::new();

    fn topo_visit(
        branch: &str,
        trunk: &str,
        valid: &HashMap<String, AdoptCandidate>,
        visited: &mut std::collections::HashSet<String>,
        sorted: &mut Vec<AdoptCandidate>,
    ) {
        if visited.contains(branch) || branch == trunk {
            return;
        }
        visited.insert(branch.to_string());
        if let Some(candidate) = valid.get(branch) {
            topo_visit(&candidate.base, trunk, valid, visited, sorted);
            sorted.push(candidate.clone());
        }
    }

    for branch in valid.keys() {
        topo_visit(branch, trunk, &valid, &mut visited, &mut sorted);
    }

    sorted
}

fn adoption_parent_head(branch: &str, parent: &str) -> Result<String> {
    git::merge_base(branch, parent)
}

fn expand_ancestor_chains(prs: &mut HashMap<String, github::PrInfo>, remote: &str, trunk: &str) {
    expand_ancestor_chains_with(prs, trunk, |refs| github::get_pr_statuses_for(remote, refs));
}

fn expand_ancestor_chains_with<F>(
    prs: &mut HashMap<String, github::PrInfo>,
    trunk: &str,
    mut fetch: F,
) where
    F: FnMut(&[&str]) -> HashMap<String, github::PrInfo>,
{
    // `tried` distinguishes "missing because we haven't fetched yet" from
    // "missing because no PR exists upstream"; without it we'd loop forever
    // re-fetching the same broken-chain base.
    let mut tried: HashSet<String> = HashSet::new();
    loop {
        let missing: Vec<String> = prs
            .values()
            .map(|pr| pr.base.clone())
            .filter(|base| base != trunk && !prs.contains_key(base) && !tried.contains(base))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if missing.is_empty() {
            break;
        }
        for branch in &missing {
            tried.insert(branch.clone());
        }
        let refs: Vec<&str> = missing.iter().map(String::as_str).collect();
        let new_prs = fetch(&refs);
        if new_prs.is_empty() {
            break;
        }
        prs.extend(new_prs);
    }
}

fn orphan_local_prs<'a>(prs: &'a HashMap<String, github::PrInfo>, trunk: &str) -> Vec<&'a String> {
    let mut orphans: Vec<&String> = prs
        .iter()
        .filter(|(_, pr)| pr.base != trunk && !prs.contains_key(&pr.base))
        .map(|(branch, _)| branch)
        .collect();
    orphans.sort();
    orphans
}

fn fetch_local_prs(remote: &str) -> Result<HashMap<String, github::PrInfo>> {
    let local = git::branch_list().unwrap_or_default();
    if local.is_empty() {
        return Ok(HashMap::new());
    }
    let refs: Vec<&str> = local.iter().map(String::as_str).collect();
    let mut prs = github::get_pr_statuses_for(remote, &refs);
    prs.retain(|_, pr| pr.state == "OPEN" && !pr.merged);
    Ok(prs)
}

fn fetch_prs_by_branches(
    remote: &str,
    trunk: &str,
    branches: &[String],
) -> Result<HashMap<String, github::PrInfo>> {
    let refs: Vec<&str> = branches.iter().map(String::as_str).collect();
    let mut prs = github::get_pr_statuses_for(remote, &refs);
    expand_ancestor_chains(&mut prs, remote, trunk);
    prs.retain(|_, pr| pr.state == "OPEN" && !pr.merged);
    Ok(prs)
}

fn fetch_prs_by_number(
    remote: &str,
    trunk: &str,
    number: u64,
) -> Result<(String, HashMap<String, github::PrInfo>)> {
    let (head, pr) = github::get_pr_by_number(remote, number).ok_or_else(|| {
        anyhow::anyhow!("PR #{number} not found — make sure it exists and is accessible")
    })?;
    let title = pr.title.clone();
    let mut prs = HashMap::new();
    prs.insert(head, pr);
    expand_ancestor_chains(&mut prs, remote, trunk);
    prs.retain(|_, p| p.state == "OPEN" && !p.merged);
    Ok((title, prs))
}

pub fn run(pr: Option<u64>, specific_branches: &[String]) -> Result<()> {
    let mut state = StackState::load().or_else(|_| {
        let trunk = git::default_branch().unwrap_or_else(|_| "main".to_string());
        let state = StackState::new(trunk.clone());
        state.save()?;
        ui::success(&format!("Initialized ez with trunk branch `{trunk}`"));
        Ok::<StackState, anyhow::Error>(state)
    })?;

    if !github::is_gh_authenticated() {
        bail!(EzError::GhError(
            "not authenticated — run `gh auth login` first".to_string()
        ));
    }

    let candidates = if let Some(pr_number) = pr {
        let sp = ui::spinner(&format!("Fetching PR #{pr_number} and its chain..."));
        let (title, prs) = fetch_prs_by_number(&state.remote, &state.trunk, pr_number)?;
        sp.finish_and_clear();

        let graph = build_adopt_graph(&state.trunk, &prs);
        if graph.is_empty() {
            bail!(
                "PR #{pr_number} (`{}`) does not lead back to trunk `{}`",
                title,
                state.trunk
            );
        }
        graph
    } else if !specific_branches.is_empty() {
        let sp = ui::spinner("Fetching PRs for named branches...");
        let prs = fetch_prs_by_branches(&state.remote, &state.trunk, specific_branches)?;
        sp.finish_and_clear();

        for branch in specific_branches {
            if !prs.contains_key(branch.as_str()) {
                ui::warn(&format!("Branch `{branch}` has no open PR — skipping"));
            }
        }

        let graph = build_adopt_graph(&state.trunk, &prs);
        if graph.is_empty() {
            bail!(
                "None of the specified branches have open PRs rooted on `{}`",
                state.trunk
            );
        }
        graph
    } else {
        // Default scopes strictly to local branches. Local PRs whose base
        // isn't another local PR (or trunk) are warned and dropped — we
        // deliberately don't auto-expand to the remote chain, since that
        // would silently re-introduce per-PR network cost in large repos.
        let sp = ui::spinner("Fetching PRs for local branches...");
        let prs = fetch_local_prs(&state.remote)?;
        sp.finish_and_clear();

        if prs.is_empty() {
            ui::info("No open PRs found for local branches");
            ui::hint(
                "Run `ez adopt --pr <number>` to adopt a specific PR, or `ez track` to track a branch without a PR",
            );
            return Ok(());
        }

        for orphan in orphan_local_prs(&prs, &state.trunk) {
            let pr_info = &prs[orphan];
            ui::warn(&format!(
                "`{orphan}` (#{}) bases on `{}` which has no local PR — skipping",
                pr_info.number, pr_info.base
            ));
            ui::hint(&format!(
                "Run `ez adopt --pr {}` to walk the remote chain for this branch",
                pr_info.number
            ));
        }

        let graph = build_adopt_graph(&state.trunk, &prs);
        if graph.is_empty() {
            ui::info("No open PRs found for local branches that root on trunk");
            return Ok(());
        }
        graph
    };

    ui::header(&format!("Found {} branch(es) to adopt", candidates.len()));
    for c in &candidates {
        let draft = if c.is_draft { " [draft]" } else { "" };
        let already = if state.is_managed(&c.branch) {
            " (already tracked)"
        } else {
            ""
        };
        ui::info(&format!(
            "  #{} {} → {} (base: `{}`){draft}{already}",
            c.pr_number, c.branch, c.title, c.base
        ));
    }

    let mut adopted = 0;
    let mut skipped = 0;

    for candidate in &candidates {
        if state.is_managed(&candidate.branch) {
            if let Ok(meta) = state.get_branch_mut(&candidate.branch) {
                if meta.pr_number.is_none() {
                    meta.pr_number = Some(candidate.pr_number);
                    ui::info(&format!(
                        "Updated PR number for `{}` → #{}",
                        candidate.branch, candidate.pr_number
                    ));
                }
            }
            skipped += 1;
            continue;
        }

        if !git::branch_exists(&candidate.branch) {
            ui::info(&format!("Fetching `{}` from remote...", candidate.branch));
            let pr_ref = git::fetch_pr_head(&state.remote, candidate.pr_number)?;
            git::create_branch_at(&candidate.branch, &pr_ref)?;
        }

        let parent = &candidate.base;
        let parent_head = match adoption_parent_head(&candidate.branch, parent) {
            Ok(parent_head) => parent_head,
            Err(_) => {
                ui::warn(&format!(
                    "Could not resolve parent `{parent}` for `{}` — skipping",
                    candidate.branch
                ));
                skipped += 1;
                continue;
            }
        };

        if parent_head.is_empty() {
            ui::warn(&format!(
                "Could not resolve parent `{parent}` for `{}` — skipping",
                candidate.branch
            ));
            skipped += 1;
            continue;
        }

        state.add_branch(&candidate.branch, parent, &parent_head, None, None);
        if let Ok(meta) = state.get_branch_mut(&candidate.branch) {
            meta.pr_number = Some(candidate.pr_number);
        }

        let draft = if candidate.is_draft { " [draft]" } else { "" };
        ui::success(&format!(
            "Adopted `{}` (#{}, base: `{}`){draft}",
            candidate.branch, candidate.pr_number, candidate.base
        ));

        adopted += 1;
    }

    state.save()?;

    if adopted == 0 && skipped > 0 {
        ui::info(&format!("All {skipped} branch(es) were already tracked"));
    } else {
        ui::success(&format!(
            "Adopted {adopted} branch(es), {skipped} already tracked"
        ));
    }

    ui::hint("Run `ez log` to see the adopted stack, then `ez switch <branch>` to start working");

    ui::receipt(&serde_json::json!({
        "cmd": "adopt",
        "adopted": adopted,
        "skipped": skipped,
        "branches": candidates.iter().map(|c| c.branch.clone()).collect::<Vec<_>>(),
    }));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::PrInfo;

    fn make_pr(branch: &str, base: &str, number: u64) -> (String, PrInfo) {
        (
            branch.to_string(),
            PrInfo {
                number,
                url: format!("https://github.com/org/repo/pull/{number}"),
                state: "OPEN".to_string(),
                title: format!("PR for {branch}"),
                base: base.to_string(),
                is_draft: false,
                merged: false,
            },
        )
    }

    #[test]
    fn build_adopt_graph_finds_linear_stack() {
        let mut prs = HashMap::new();
        let (k, v) = make_pr("feat/a", "main", 1);
        prs.insert(k, v);
        let (k, v) = make_pr("feat/b", "feat/a", 2);
        prs.insert(k, v);
        let (k, v) = make_pr("feat/c", "feat/b", 3);
        prs.insert(k, v);

        let graph = build_adopt_graph("main", &prs);

        assert_eq!(graph.len(), 3);
        // Topological order: parents before children.
        let names: Vec<&str> = graph.iter().map(|c| c.branch.as_str()).collect();
        assert!(
            names.iter().position(|&n| n == "feat/a").unwrap()
                < names.iter().position(|&n| n == "feat/b").unwrap()
        );
        assert!(
            names.iter().position(|&n| n == "feat/b").unwrap()
                < names.iter().position(|&n| n == "feat/c").unwrap()
        );
    }

    #[test]
    fn build_adopt_graph_excludes_branches_not_rooted_on_trunk() {
        let mut prs = HashMap::new();
        let (k, v) = make_pr("feat/a", "main", 1);
        prs.insert(k, v);
        // feat/orphan bases on "develop" which is not trunk.
        let (k, v) = make_pr("feat/orphan", "develop", 2);
        prs.insert(k, v);

        let graph = build_adopt_graph("main", &prs);

        assert_eq!(graph.len(), 1);
        assert_eq!(graph[0].branch, "feat/a");
    }

    #[test]
    fn build_adopt_graph_excludes_merged_and_closed_prs() {
        let mut prs = HashMap::new();
        let (k, v) = make_pr("feat/a", "main", 1);
        prs.insert(k, v);
        let (k, mut v) = make_pr("feat/merged", "main", 2);
        v.merged = true;
        prs.insert(k, v);
        let (k, mut v) = make_pr("feat/closed", "main", 3);
        v.state = "CLOSED".to_string();
        prs.insert(k, v);

        let graph = build_adopt_graph("main", &prs);

        assert_eq!(graph.len(), 1);
        assert_eq!(graph[0].branch, "feat/a");
    }

    #[test]
    fn build_adopt_graph_handles_diamond_stacks() {
        let mut prs = HashMap::new();
        let (k, v) = make_pr("feat/base", "main", 1);
        prs.insert(k, v);
        let (k, v) = make_pr("feat/left", "feat/base", 2);
        prs.insert(k, v);
        let (k, v) = make_pr("feat/right", "feat/base", 3);
        prs.insert(k, v);

        let graph = build_adopt_graph("main", &prs);

        assert_eq!(graph.len(), 3);
        // feat/base must come before both children.
        let names: Vec<&str> = graph.iter().map(|c| c.branch.as_str()).collect();
        let base_pos = names.iter().position(|&n| n == "feat/base").unwrap();
        let left_pos = names.iter().position(|&n| n == "feat/left").unwrap();
        let right_pos = names.iter().position(|&n| n == "feat/right").unwrap();
        assert!(base_pos < left_pos);
        assert!(base_pos < right_pos);
    }

    #[test]
    fn build_adopt_graph_handles_cycle_gracefully() {
        let mut prs = HashMap::new();
        // Cycle: a→b, b→a — neither roots on trunk.
        let (k, v) = make_pr("feat/a", "feat/b", 1);
        prs.insert(k, v);
        let (k, v) = make_pr("feat/b", "feat/a", 2);
        prs.insert(k, v);

        let graph = build_adopt_graph("main", &prs);

        // Cycles can't reach trunk, so nothing is adoptable.
        assert!(graph.is_empty());
    }

    #[test]
    fn build_adopt_graph_empty_prs_returns_empty() {
        let prs = HashMap::new();
        let graph = build_adopt_graph("main", &prs);
        assert!(graph.is_empty());
    }

    #[test]
    fn build_adopt_graph_single_pr_on_trunk() {
        let mut prs = HashMap::new();
        let (k, v) = make_pr("feat/solo", "main", 42);
        prs.insert(k, v);

        let graph = build_adopt_graph("main", &prs);

        assert_eq!(graph.len(), 1);
        assert_eq!(graph[0].branch, "feat/solo");
        assert_eq!(graph[0].pr_number, 42);
        assert_eq!(graph[0].base, "main");
    }

    #[test]
    fn build_adopt_graph_deep_chain() {
        let mut prs = HashMap::new();
        // Chain of 5 deep: a→b→c→d→e
        let (k, v) = make_pr("feat/a", "main", 1);
        prs.insert(k, v);
        let (k, v) = make_pr("feat/b", "feat/a", 2);
        prs.insert(k, v);
        let (k, v) = make_pr("feat/c", "feat/b", 3);
        prs.insert(k, v);
        let (k, v) = make_pr("feat/d", "feat/c", 4);
        prs.insert(k, v);
        let (k, v) = make_pr("feat/e", "feat/d", 5);
        prs.insert(k, v);

        let graph = build_adopt_graph("main", &prs);
        assert_eq!(graph.len(), 5);

        // Verify topological order.
        let names: Vec<&str> = graph.iter().map(|c| c.branch.as_str()).collect();
        for i in 0..names.len() - 1 {
            assert!(
                names.iter().position(|&n| n == names[i]).unwrap()
                    < names.iter().position(|&n| n == names[i + 1]).unwrap(),
                "{} should come before {}",
                names[i],
                names[i + 1]
            );
        }
    }

    #[test]
    fn build_adopt_graph_preserves_draft_flag() {
        let mut prs = HashMap::new();
        let (k, mut v) = make_pr("feat/draft-branch", "main", 10);
        v.is_draft = true;
        prs.insert(k, v);

        let graph = build_adopt_graph("main", &prs);
        assert_eq!(graph.len(), 1);
        assert!(graph[0].is_draft);
    }

    #[test]
    fn build_adopt_graph_partial_chain_missing_middle() {
        let mut prs = HashMap::new();
        // feat/a → main (exists)
        // feat/c → feat/b (feat/b has NO PR — missing link)
        let (k, v) = make_pr("feat/a", "main", 1);
        prs.insert(k, v);
        let (k, v) = make_pr("feat/c", "feat/b", 3);
        prs.insert(k, v);

        let graph = build_adopt_graph("main", &prs);

        // Only feat/a should be adoptable; feat/c can't reach trunk through feat/b.
        assert_eq!(graph.len(), 1);
        assert_eq!(graph[0].branch, "feat/a");
    }

    #[test]
    fn expand_ancestor_chains_fetches_missing_parents_until_trunk() {
        let mut prs = HashMap::new();
        let (k, v) = make_pr("feat/c", "feat/b", 3);
        prs.insert(k, v);

        let mut remote: HashMap<String, github::PrInfo> = HashMap::new();
        let (k, v) = make_pr("feat/a", "main", 1);
        remote.insert(k, v);
        let (k, v) = make_pr("feat/b", "feat/a", 2);
        remote.insert(k, v);

        let mut calls: Vec<Vec<String>> = Vec::new();
        expand_ancestor_chains_with(&mut prs, "main", |refs| {
            calls.push(refs.iter().map(|s| (*s).to_string()).collect());
            let mut out = HashMap::new();
            for r in refs {
                if let Some(pr) = remote.get(*r) {
                    out.insert((*r).to_string(), pr.clone());
                }
            }
            out
        });

        assert!(prs.contains_key("feat/a"));
        assert!(prs.contains_key("feat/b"));
        assert!(prs.contains_key("feat/c"));
        assert_eq!(calls.len(), 2, "expected one batch per stack level");
    }

    #[test]
    fn expand_ancestor_chains_terminates_when_base_has_no_pr() {
        let mut prs = HashMap::new();
        let (k, v) = make_pr("feat/c", "feat/b", 3);
        prs.insert(k, v);

        let mut call_count = 0usize;
        expand_ancestor_chains_with(&mut prs, "main", |_refs| {
            call_count += 1;
            HashMap::new()
        });

        assert_eq!(call_count, 1);
        assert_eq!(prs.len(), 1);
        assert!(prs.contains_key("feat/c"));
    }

    #[test]
    fn expand_ancestor_chains_does_nothing_when_all_bases_are_trunk() {
        let mut prs = HashMap::new();
        let (k, v) = make_pr("feat/a", "main", 1);
        prs.insert(k, v);

        let mut call_count = 0usize;
        expand_ancestor_chains_with(&mut prs, "main", |_refs| {
            call_count += 1;
            HashMap::new()
        });

        assert_eq!(call_count, 0);
    }

    #[test]
    fn orphan_local_prs_flags_branches_whose_base_is_neither_trunk_nor_local() {
        let mut prs = HashMap::new();
        let (k, v) = make_pr("feat/a", "main", 1);
        prs.insert(k, v);
        let (k, v) = make_pr("feat/b", "feat/a", 2);
        prs.insert(k, v);
        let (k, v) = make_pr("feat/c", "feat/missing", 3);
        prs.insert(k, v);

        let orphans = orphan_local_prs(&prs, "main");
        assert_eq!(orphans, vec![&"feat/c".to_string()]);
    }

    #[test]
    fn orphan_local_prs_returns_sorted_for_stable_warning_order() {
        let mut prs = HashMap::new();
        let (k, v) = make_pr("feat/z", "missing-x", 3);
        prs.insert(k, v);
        let (k, v) = make_pr("feat/a", "missing-y", 1);
        prs.insert(k, v);
        let (k, v) = make_pr("feat/m", "missing-z", 2);
        prs.insert(k, v);

        let orphans = orphan_local_prs(&prs, "main");
        let names: Vec<&str> = orphans.iter().map(|s| s.as_str()).collect();
        assert_eq!(names, vec!["feat/a", "feat/m", "feat/z"]);
    }

    #[test]
    fn adoption_parent_head_uses_merge_base_not_parent_tip() {
        use crate::test_support::{CwdGuard, init_git_repo, take_env_lock, write_file};

        let _guard = take_env_lock();
        let repo = init_git_repo("adopt-parent-head");
        let _cwd = CwdGuard::enter(&repo);

        git::create_branch("feat/base").expect("create base");
        write_file(&repo, "base.txt", "base\n");
        git::add_all_including_untracked().expect("stage base");
        git::commit("base commit").expect("commit base");
        let original_base = git::rev_parse("feat/base").expect("base sha");

        git::create_branch("feat/child").expect("create child");
        write_file(&repo, "child.txt", "child\n");
        git::add_all_including_untracked().expect("stage child");
        git::commit("child commit").expect("commit child");

        git::checkout("feat/base").expect("checkout base");
        write_file(&repo, "base-2.txt", "base 2\n");
        git::add_all_including_untracked().expect("stage base advance");
        git::commit("advance base").expect("commit base advance");
        let advanced_base = git::rev_parse("feat/base").expect("advanced base sha");

        assert_ne!(original_base, advanced_base);
        assert_eq!(
            adoption_parent_head("feat/child", "feat/base").expect("parent head"),
            original_base
        );
    }
}
