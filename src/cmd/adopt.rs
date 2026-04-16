use anyhow::{Result, bail};
use std::collections::HashMap;

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

    fn is_rooted_in_trunk<'a>(
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

pub fn run(pr: Option<u64>, specific_branches: &[String]) -> Result<()> {
    let mut state = StackState::load().or_else(|_| {
        // If ez isn't initialized, try to auto-detect trunk and init.
        let trunk = git::default_branch().unwrap_or_else(|_| "main".to_string());
        let state = StackState::new(trunk.clone());
        state.save()?;
        ui::success(&format!("Initialized ez with trunk branch `{trunk}`"));
        Ok::<StackState, anyhow::Error>(state)
    })?;

    // Check gh authentication.
    if !github::is_gh_authenticated() {
        bail!(EzError::GhError(
            "not authenticated — run `gh auth login` first".to_string()
        ));
    }

    // Fetch all open PRs.
    let sp = ui::spinner("Fetching PR graph from GitHub...");
    let all_prs = github::get_all_pr_statuses();
    sp.finish_and_clear();

    if all_prs.is_empty() {
        ui::info("No PRs found in this repository");
        return Ok(());
    }

    // If --pr is specified, find the specific PR and its chain.
    let candidates = if let Some(pr_number) = pr {
        // Find the PR by number and clone the title for error messages.
        let target_title = all_prs
            .values()
            .find(|p| p.number == pr_number)
            .map(|p| p.title.clone())
            .ok_or_else(|| {
                anyhow::anyhow!("PR #{pr_number} not found — make sure it exists and is open")
            })?;

        let target_branch = all_prs
            .iter()
            .find(|(_, p)| p.number == pr_number)
            .map(|(b, _)| b.clone())
            .unwrap();

        // Build the full chain from this PR down to trunk.
        let mut chain_branches: Vec<String> = Vec::new();
        let mut current = target_branch.clone();
        let mut seen = std::collections::HashSet::new();
        loop {
            if current == state.trunk || !seen.insert(current.clone()) {
                break;
            }
            chain_branches.push(current.clone());
            if let Some(pr) = all_prs.get(&current) {
                current = pr.base.clone();
            } else {
                break;
            }
        }

        // Filter graph to only include branches in this chain.
        let chain_set: std::collections::HashSet<&str> =
            chain_branches.iter().map(|s| s.as_str()).collect();
        let filtered: HashMap<String, github::PrInfo> = all_prs
            .into_iter()
            .filter(|(b, _)| chain_set.contains(b.as_str()))
            .collect();

        let graph = build_adopt_graph(&state.trunk, &filtered);
        if graph.is_empty() {
            bail!(
                "PR #{pr_number} (`{}`) does not lead back to trunk `{}`",
                target_title,
                state.trunk
            );
        }
        graph
    } else if !specific_branches.is_empty() {
        // Filter to specific branches.
        let branch_set: std::collections::HashSet<&str> =
            specific_branches.iter().map(|s| s.as_str()).collect();
        let filtered: HashMap<String, github::PrInfo> = all_prs
            .into_iter()
            .filter(|(b, _)| branch_set.contains(b.as_str()))
            .collect();

        // Check for branches that have no PRs.
        for branch in specific_branches {
            if !filtered.contains_key(branch.as_str()) {
                ui::warn(&format!("Branch `{branch}` has no open PR — skipping"));
            }
        }

        let graph = build_adopt_graph(&state.trunk, &filtered);
        if graph.is_empty() {
            bail!(
                "None of the specified branches have open PRs rooted on `{}`",
                state.trunk
            );
        }
        graph
    } else {
        // Adopt all open PRs rooted on trunk.
        let graph = build_adopt_graph(&state.trunk, &all_prs);
        if graph.is_empty() {
            ui::info("No open PRs found that are rooted on trunk");
            return Ok(());
        }
        graph
    };

    // Report what we found.
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

    // Adopt each candidate.
    let mut adopted = 0;
    let mut skipped = 0;

    for candidate in &candidates {
        if state.is_managed(&candidate.branch) {
            // Already tracked — just update PR number if missing.
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

        // Ensure the local branch exists. Fetch from remote if needed.
        if !git::branch_exists(&candidate.branch) {
            ui::info(&format!("Fetching `{}` from remote...", candidate.branch));
            git::fetch_branch(&state.remote, &candidate.branch)?;

            // Create local tracking branch.
            let remote_ref = format!("{}/{}", state.remote, candidate.branch);
            if git::branch_exists(&remote_ref) {
                git::create_branch_at(&candidate.branch, &remote_ref)?;
            } else {
                ui::warn(&format!(
                    "Could not fetch `{}` — skipping",
                    candidate.branch
                ));
                skipped += 1;
                continue;
            }
        }

        // Resolve parent head.
        let parent = &candidate.base;
        let parent_head = git::rev_parse(parent).unwrap_or_else(|_| {
            // Parent might be a remote branch.
            git::rev_parse(&format!("{}/{}", state.remote, parent)).unwrap_or_default()
        });

        if parent_head.is_empty() {
            ui::warn(&format!(
                "Could not resolve parent `{parent}` for `{}` — skipping",
                candidate.branch
            ));
            skipped += 1;
            continue;
        }

        // Add to stack state.
        state.add_branch(&candidate.branch, parent, &parent_head, None, None);

        // Set the PR number.
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

    // Summary.
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
}
