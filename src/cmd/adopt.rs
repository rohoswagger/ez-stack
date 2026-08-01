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
    pr_number: Option<u64>,
    title: String,
    is_draft: bool,
}

#[derive(Debug, Clone)]
struct WorktreeProvisioning {
    created: usize,
    paths: Vec<String>,
    created_paths: Vec<String>,
}

fn is_active_pr(pr: &github::PrInfo) -> bool {
    pr.state == "OPEN" && !pr.merged
}

/// Build the adoption graph from open PRs.
/// Returns candidates keyed by branch name, only including branches whose
/// base chain leads back to trunk.
fn build_adopt_graph(trunk: &str, prs: &HashMap<String, github::PrInfo>) -> Vec<AdoptCandidate> {
    // Filter to open PRs only.
    let open_prs: HashMap<&str, &github::PrInfo> = prs
        .iter()
        .filter(|(_, pr)| is_active_pr(pr))
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
                    pr_number: Some(pr.number),
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

fn build_native_adopt_candidates(
    trunk: &str,
    ordered_prs: &[(String, github::PrInfo)],
) -> Vec<AdoptCandidate> {
    let mut candidates = Vec::new();
    let mut parent = trunk.to_string();

    for (branch, pr) in ordered_prs {
        if !is_active_pr(pr) {
            continue;
        }

        candidates.push(AdoptCandidate {
            branch: branch.clone(),
            base: parent.clone(),
            pr_number: Some(pr.number),
            title: pr.title.clone(),
            is_draft: pr.is_draft,
        });
        parent = branch.clone();
    }

    candidates
}

fn build_explicit_chain_candidates(
    trunk: &str,
    branches: &[String],
    prs: &HashMap<String, github::PrInfo>,
) -> Result<Vec<AdoptCandidate>> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut parent = trunk.to_string();

    for branch in branches {
        if !seen.insert(branch.as_str()) {
            bail!(EzError::UserMessage(format!(
                "branch `{branch}` was specified more than once"
            )));
        }
        if branch == trunk {
            bail!(EzError::UserMessage(format!(
                "cannot adopt trunk branch `{trunk}` as a stacked branch"
            )));
        }

        if let Some(pr) = prs.get(branch).filter(|pr| is_active_pr(pr)) {
            if pr.base != parent {
                bail!(EzError::UserMessage(format!(
                    "`{branch}` has open PR #{} with reported base `{}`, but its explicit parent is `{parent}`. Reorder the branch arguments or use `ez adopt --pr {}` to adopt the PR stack from GitHub.",
                    pr.number, pr.base, pr.number
                )));
            }
            candidates.push(AdoptCandidate {
                branch: branch.clone(),
                base: parent.clone(),
                pr_number: Some(pr.number),
                title: pr.title.clone(),
                is_draft: pr.is_draft,
            });
        } else {
            candidates.push(AdoptCandidate {
                branch: branch.clone(),
                base: parent.clone(),
                pr_number: None,
                title: branch.clone(),
                is_draft: false,
            });
        }

        parent = branch.clone();
    }

    Ok(candidates)
}

fn adoption_parent_head(branch: &str, parent: &str) -> Result<String> {
    git::merge_base(branch, parent)
}

fn expand_ancestor_chains(
    prs: &mut HashMap<String, github::PrInfo>,
    remote: &str,
    repo: Option<&str>,
    trunk: &str,
) {
    expand_ancestor_chains_with(prs, trunk, |refs| {
        github::get_pr_statuses_for(remote, repo, refs)
    });
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

fn fetch_local_prs(remote: &str, repo: Option<&str>) -> Result<HashMap<String, github::PrInfo>> {
    let local = git::branch_list().unwrap_or_default();
    if local.is_empty() {
        return Ok(HashMap::new());
    }
    let refs: Vec<&str> = local.iter().map(String::as_str).collect();
    let mut prs = github::get_pr_statuses_for(remote, repo, &refs);
    prs.retain(|_, pr| is_active_pr(pr));
    Ok(prs)
}

fn fetch_prs_by_number(
    remote: &str,
    repo: Option<&str>,
    trunk: &str,
    number: u64,
) -> Result<(String, HashMap<String, github::PrInfo>)> {
    let (head, pr) = github::get_pr_by_number(remote, repo, number).ok_or_else(|| {
        anyhow::anyhow!("PR #{number} not found — make sure it exists and is accessible")
    })?;
    let title = pr.title.clone();
    let mut prs = HashMap::new();
    prs.insert(head, pr);
    expand_ancestor_chains(&mut prs, remote, repo, trunk);
    prs.retain(|_, p| is_active_pr(p));
    Ok((title, prs))
}

fn fetch_candidates_by_pr_number(
    remote: &str,
    repo: Option<&str>,
    trunk: &str,
    number: u64,
    use_native_stack: bool,
) -> Result<(String, Option<u64>, Vec<AdoptCandidate>)> {
    if use_native_stack && let Some(native_stack) = github::native_stack_for_pr(number, repo)? {
        let mut ordered_prs = Vec::new();
        let mut requested_title = None;

        for pr_number in &native_stack.pull_requests {
            let (head, pr) = github::get_pr_by_number(remote, repo, *pr_number).ok_or_else(|| {
                anyhow::anyhow!(
                    "PR #{pr_number} from native stack #{} not found — make sure it exists and is accessible",
                    native_stack.number
                )
            })?;
            if *pr_number == number {
                requested_title = Some(pr.title.clone());
            }
            ordered_prs.push((head, pr));
        }

        let title =
            requested_title.unwrap_or_else(|| format!("native stack #{}", native_stack.number));
        return Ok((
            title,
            Some(native_stack.number),
            build_native_adopt_candidates(trunk, &ordered_prs),
        ));
    }

    let (title, prs) = fetch_prs_by_number(remote, repo, trunk, number)?;
    Ok((title, None, build_adopt_graph(trunk, &prs)))
}

fn validate_managed_adopt_candidates(
    state: &StackState,
    candidates: &[AdoptCandidate],
    native_stack_number: Option<u64>,
) -> Result<()> {
    for candidate in candidates {
        let Some(meta) = state.branches.get(&candidate.branch) else {
            continue;
        };
        let pr_conflicts = match (meta.pr_number, candidate.pr_number) {
            (None, Some(_)) => false,
            (None, None) => false,
            (Some(local), Some(remote)) => local != remote,
            (Some(_), None) => true,
        };
        if meta.parent == candidate.base && !pr_conflicts {
            continue;
        }

        let local_pr = format_pr_number(meta.pr_number);
        let remote_pr = format_pr_number(candidate.pr_number);
        let context = native_stack_number
            .map(|number| format!("native stack #{number} "))
            .unwrap_or_default();
        bail!(EzError::UserMessage(format!(
            "{context}conflicts with local ez metadata for `{}`: local parent=`{}`, remote parent=`{}`, local PR={}, remote PR={}. Resolve the local stack metadata before adopting this branch.",
            candidate.branch, meta.parent, candidate.base, local_pr, remote_pr
        )));
    }

    Ok(())
}

fn format_pr_number(number: Option<u64>) -> String {
    number
        .map(|number| format!("#{number}"))
        .unwrap_or_else(|| "none".to_string())
}

fn fetch_and_validate_candidate_refs(
    remote: &str,
    candidates: &[AdoptCandidate],
) -> Result<HashMap<String, String>> {
    let mut candidate_refs = HashMap::new();
    for candidate in candidates {
        if let Some(pr_number) = candidate.pr_number {
            let pr_ref = git::fetch_pr_head(remote, pr_number)?;
            candidate_refs.insert(candidate.branch.clone(), pr_ref);
            continue;
        }

        let local_exists = git::branch_exists(&candidate.branch);
        match git::fetch_remote_branch_ref(remote, &candidate.branch) {
            Ok(remote_ref) => {
                candidate_refs.insert(candidate.branch.clone(), remote_ref);
            }
            Err(remote_err) => {
                if !local_exists {
                    bail!(EzError::UserMessage(format!(
                        "branch `{}` was not found locally or on remote `{remote}`: {remote_err}",
                        candidate.branch
                    )));
                }
            }
        }
    }
    validate_existing_branch_pr_refs(candidates, &candidate_refs)?;
    Ok(candidate_refs)
}

fn validate_existing_branch_pr_refs(
    candidates: &[AdoptCandidate],
    candidate_refs: &HashMap<String, String>,
) -> Result<()> {
    for candidate in candidates {
        if !git::branch_exists(&candidate.branch) {
            continue;
        }
        let Some(ref_name) = candidate_refs.get(&candidate.branch) else {
            continue;
        };
        if git::is_ancestor(ref_name, &candidate.branch) {
            continue;
        }
        let detail = candidate
            .pr_number
            .map(|number| format!("PR #{number} head"))
            .unwrap_or_else(|| "remote branch".to_string());
        bail!(EzError::UserMessage(format!(
            "local branch `{}` does not contain {detail} `{ref_name}`",
            candidate.branch
        )));
    }
    Ok(())
}

fn rollback_worktrees(paths: &[String]) {
    for path in paths.iter().rev() {
        let _ = git::worktree_remove(path);
    }
}

fn rollback_created_branches(branches: &[String]) {
    for branch in branches.iter().rev() {
        let _ = git::delete_branch(branch, true);
    }
}

fn provision_worktrees(
    candidates: &[AdoptCandidate],
    no_worktrees: bool,
) -> Result<WorktreeProvisioning> {
    if no_worktrees {
        return Ok(WorktreeProvisioning {
            created: 0,
            paths: Vec::new(),
            created_paths: Vec::new(),
        });
    }

    let mut worktree_map: HashMap<String, String> = git::worktree_list()?
        .into_iter()
        .filter_map(|wt| wt.branch.map(|branch| (branch, wt.path)))
        .collect();
    let mut paths = Vec::new();
    let mut created_paths = Vec::new();

    for candidate in candidates {
        if let Some(path) = worktree_map.get(&candidate.branch) {
            paths.push(path.clone());
            continue;
        }

        let path = match git::worktree_path(&candidate.branch) {
            Ok(path) => path,
            Err(err) => {
                rollback_worktrees(&created_paths);
                return Err(err);
            }
        };
        if let Err(err) = git::worktree_add(&path, &candidate.branch) {
            rollback_worktrees(&created_paths);
            return Err(err);
        }
        worktree_map.insert(candidate.branch.clone(), path.clone());
        paths.push(path);
        created_paths.push(paths.last().expect("path just pushed").clone());
    }

    Ok(WorktreeProvisioning {
        created: created_paths.len(),
        paths,
        created_paths,
    })
}

fn adopt_candidates_transactionally(
    state: &mut StackState,
    candidates: &[AdoptCandidate],
    no_worktrees: bool,
    candidate_refs: &HashMap<String, String>,
    abort_on_parent_resolution_failure: bool,
) -> Result<(usize, usize, WorktreeProvisioning)> {
    let original_state = state.clone();
    let mut created_branches = Vec::new();
    let mut adopted = 0;
    let mut skipped = 0;

    for candidate in candidates {
        let branch_existed = git::branch_exists(&candidate.branch);
        if !branch_existed {
            ui::info(&format!("Fetching `{}` from remote...", candidate.branch));
            let Some(source_ref) = candidate_refs.get(&candidate.branch) else {
                rollback_created_branches(&created_branches);
                *state = original_state;
                return Err(anyhow::anyhow!(
                    "internal error: missing fetched source ref for `{}`",
                    candidate.branch
                ));
            };
            if let Err(err) = git::create_branch_at(&candidate.branch, source_ref) {
                rollback_created_branches(&created_branches);
                *state = original_state;
                return Err(err);
            }
            created_branches.push(candidate.branch.clone());
        }

        if state.is_managed(&candidate.branch) {
            if let Ok(meta) = state.get_branch_mut(&candidate.branch) {
                if meta.pr_number.is_none()
                    && let Some(pr_number) = candidate.pr_number
                {
                    meta.pr_number = Some(pr_number);
                    ui::info(&format!(
                        "Updated PR number for `{}` → #{}",
                        candidate.branch, pr_number
                    ));
                }
            }
            skipped += 1;
            continue;
        }

        let parent = &candidate.base;
        let parent_head = match adoption_parent_head(&candidate.branch, parent) {
            Ok(parent_head) => parent_head,
            Err(err) => {
                if abort_on_parent_resolution_failure {
                    rollback_created_branches(&created_branches);
                    *state = original_state;
                    return Err(EzError::UserMessage(format!(
                        "could not resolve parent `{parent}` for `{}`: {err}",
                        candidate.branch
                    ))
                    .into());
                }
                ui::warn(&format!(
                    "Could not resolve parent `{parent}` for `{}` — skipping",
                    candidate.branch
                ));
                if !branch_existed {
                    let _ = git::delete_branch(&candidate.branch, true);
                    created_branches.retain(|branch| branch != &candidate.branch);
                }
                skipped += 1;
                continue;
            }
        };

        if parent_head.is_empty() {
            if abort_on_parent_resolution_failure {
                rollback_created_branches(&created_branches);
                *state = original_state;
                return Err(EzError::UserMessage(format!(
                    "could not resolve parent `{parent}` for `{}`",
                    candidate.branch
                ))
                .into());
            }
            ui::warn(&format!(
                "Could not resolve parent `{parent}` for `{}` — skipping",
                candidate.branch
            ));
            if !branch_existed {
                let _ = git::delete_branch(&candidate.branch, true);
                created_branches.retain(|branch| branch != &candidate.branch);
            }
            skipped += 1;
            continue;
        }

        state.add_branch(&candidate.branch, parent, &parent_head, None, None);
        if let Ok(meta) = state.get_branch_mut(&candidate.branch) {
            meta.pr_number = candidate.pr_number;
        }

        let draft = if candidate.is_draft { " [draft]" } else { "" };
        let pr_label = candidate
            .pr_number
            .map(|number| format!("#{number}, "))
            .unwrap_or_default();
        ui::success(&format!(
            "Adopted `{}` ({pr_label}base: `{}`){draft}",
            candidate.branch, candidate.base
        ));

        adopted += 1;
    }

    let provisioning = match provision_worktrees(candidates, no_worktrees) {
        Ok(provisioning) => provisioning,
        Err(err) => {
            rollback_created_branches(&created_branches);
            *state = original_state;
            return Err(err);
        }
    };

    if let Err(err) = state.save() {
        rollback_worktrees(&provisioning.created_paths);
        rollback_created_branches(&created_branches);
        *state = original_state;
        return Err(err);
    }

    Ok((adopted, skipped, provisioning))
}

fn adopt_receipt_value(
    adopted: usize,
    skipped: usize,
    candidates: &[AdoptCandidate],
    native_stack_number: Option<u64>,
    provisioning: &WorktreeProvisioning,
) -> serde_json::Value {
    serde_json::json!({
        "cmd": "adopt",
        "adopted": adopted,
        "skipped": skipped,
        "branches": candidates.iter().map(|c| c.branch.clone()).collect::<Vec<_>>(),
        "native_stack_number": native_stack_number,
        "pr_numbers": candidates.iter().map(|c| c.pr_number).collect::<Vec<_>>(),
        "worktrees_created": provisioning.created,
        "worktree_paths": &provisioning.paths,
    })
}

pub fn run(pr: Option<u64>, specific_branches: &[String], no_worktrees: bool) -> Result<()> {
    let mut state = StackState::load().or_else(|_| {
        let trunk = git::default_branch().unwrap_or_else(|_| "main".to_string());
        let state = StackState::new(trunk.clone());
        state.save()?;
        ui::success(&format!("Initialized ez with trunk branch `{trunk}`"));
        Ok::<StackState, anyhow::Error>(state)
    })?;

    let gh_authenticated = github::is_gh_authenticated();
    if (pr.is_some() || specific_branches.is_empty()) && !gh_authenticated {
        bail!(EzError::GhError(
            "not authenticated — run `gh auth login` first".to_string()
        ));
    }

    let (candidates, native_stack_number, explicit_chain) = if let Some(pr_number) = pr {
        let sp = ui::spinner(&format!("Fetching PR #{pr_number} and its chain..."));
        let (title, native_stack_number, graph) = fetch_candidates_by_pr_number(
            state.fetch_remote(),
            state.repo.as_deref(),
            &state.trunk,
            pr_number,
            !state.is_fork_workflow(),
        )?;
        sp.finish_and_clear();

        if graph.is_empty() {
            bail!(
                "PR #{pr_number} (`{}`) does not lead back to trunk `{}`",
                title,
                state.trunk
            );
        }
        (graph, native_stack_number, false)
    } else if !specific_branches.is_empty() {
        let prs = if gh_authenticated {
            let refs: Vec<&str> = specific_branches.iter().map(String::as_str).collect();
            let mut prs =
                github::get_pr_statuses_for(state.fetch_remote(), state.repo.as_deref(), &refs);
            prs.retain(|_, pr| is_active_pr(pr));
            prs
        } else {
            HashMap::new()
        };

        let all_have_open_pr = specific_branches
            .iter()
            .all(|branch| prs.contains_key(branch.as_str()));

        if all_have_open_pr {
            let sp = ui::spinner("Fetching PRs for named branches...");
            let mut prs = prs;
            expand_ancestor_chains(
                &mut prs,
                state.fetch_remote(),
                state.repo.as_deref(),
                &state.trunk,
            );
            prs.retain(|_, pr| is_active_pr(pr));
            sp.finish_and_clear();

            let graph = build_adopt_graph(&state.trunk, &prs);
            if graph.is_empty() {
                bail!(
                    "None of the specified branches have open PRs rooted on `{}`",
                    state.trunk
                );
            }
            (graph, None, false)
        } else {
            if gh_authenticated {
                for branch in specific_branches {
                    if !prs.contains_key(branch.as_str()) {
                        ui::warn(&format!(
                            "Branch `{branch}` has no open PR — treating arguments as an explicit stack"
                        ));
                    }
                }
            }
            (
                build_explicit_chain_candidates(&state.trunk, specific_branches, &prs)?,
                None,
                true,
            )
        }
    } else {
        // Default scopes strictly to local branches. Local PRs whose base
        // isn't another local PR (or trunk) are warned and dropped — we
        // deliberately don't auto-expand to the remote chain, since that
        // would silently re-introduce per-PR network cost in large repos.
        let sp = ui::spinner("Fetching PRs for local branches...");
        let prs = fetch_local_prs(state.fetch_remote(), state.repo.as_deref())?;
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
        (graph, None, false)
    };
    validate_managed_adopt_candidates(&state, &candidates, native_stack_number)?;
    let candidate_refs = fetch_and_validate_candidate_refs(state.fetch_remote(), &candidates)?;

    ui::header(&format!("Found {} branch(es) to adopt", candidates.len()));
    for c in &candidates {
        let draft = if c.is_draft { " [draft]" } else { "" };
        let already = if state.is_managed(&c.branch) {
            " (already tracked)"
        } else {
            ""
        };
        let identity = c
            .pr_number
            .map(|number| format!("#{number}"))
            .unwrap_or_else(|| "branch".to_string());
        ui::info(&format!(
            "  {} {} → {} (base: `{}`){draft}{already}",
            identity, c.branch, c.title, c.base
        ));
    }

    let (adopted, skipped, provisioning) = adopt_candidates_transactionally(
        &mut state,
        &candidates,
        no_worktrees,
        &candidate_refs,
        explicit_chain,
    )?;

    if adopted == 0 && skipped > 0 {
        ui::info(&format!("All {skipped} branch(es) were already tracked"));
    } else {
        ui::success(&format!(
            "Adopted {adopted} branch(es), {skipped} already tracked"
        ));
    }

    if no_worktrees {
        ui::hint(
            "Run `ez log` to see the adopted stack, then `ez switch <branch>` to start working",
        );
    } else {
        ui::hint("Worktrees are ready; use `ez switch <branch>` to start working in one");
    }

    ui::receipt(&adopt_receipt_value(
        adopted,
        skipped,
        &candidates,
        native_stack_number,
        &provisioning,
    ));

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
        assert_eq!(graph[0].pr_number, Some(42));
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
    fn build_native_adopt_candidates_uses_native_order_over_reported_bases() {
        let (_, mut base) = make_pr("feat/base", "stale-base", 10);
        base.title = "Base".to_string();
        let (_, mut child) = make_pr("feat/child", "main", 11);
        child.title = "Child".to_string();

        let candidates = build_native_adopt_candidates(
            "main",
            &[
                ("feat/base".to_string(), base),
                ("feat/child".to_string(), child),
            ],
        );

        let branches: Vec<&str> = candidates.iter().map(|c| c.branch.as_str()).collect();
        let bases: Vec<&str> = candidates.iter().map(|c| c.base.as_str()).collect();
        assert_eq!(branches, vec!["feat/base", "feat/child"]);
        assert_eq!(bases, vec!["main", "feat/base"]);
    }

    #[test]
    fn build_native_adopt_candidates_excludes_inactive_entries_and_rechains_to_trunk() {
        let (_, mut closed_base) = make_pr("feat/base", "main", 10);
        closed_base.state = "CLOSED".to_string();
        let (_, child) = make_pr("feat/child", "feat/base", 11);

        let candidates = build_native_adopt_candidates(
            "main",
            &[
                ("feat/base".to_string(), closed_base),
                ("feat/child".to_string(), child),
            ],
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].branch, "feat/child");
        assert_eq!(candidates[0].base, "main");
    }

    #[test]
    fn build_explicit_chain_candidates_uses_argument_order_and_enriches_matching_prs() {
        let mut prs = HashMap::new();
        let (branch, pr) = make_pr("feat/base", "main", 10);
        prs.insert(branch, pr);
        let (branch, pr) = make_pr("feat/child", "feat/base", 11);
        prs.insert(branch, pr);

        let candidates = build_explicit_chain_candidates(
            "main",
            &["feat/base".to_string(), "feat/child".to_string()],
            &prs,
        )
        .expect("explicit chain");

        assert_eq!(
            candidates
                .iter()
                .map(|c| c.branch.as_str())
                .collect::<Vec<_>>(),
            vec!["feat/base", "feat/child"]
        );
        assert_eq!(
            candidates
                .iter()
                .map(|c| c.base.as_str())
                .collect::<Vec<_>>(),
            vec!["main", "feat/base"]
        );
        assert_eq!(
            candidates.iter().map(|c| c.pr_number).collect::<Vec<_>>(),
            vec![Some(10), Some(11)]
        );
    }

    #[test]
    fn build_explicit_chain_candidates_mixes_pr_backed_and_pr_less_and_rejects_base_mismatch() {
        let mut prs = HashMap::new();
        let (branch, mut pr) = make_pr("feat/child", "main", 11);
        pr.title = "Child PR".to_string();
        prs.insert(branch, pr);

        let err = build_explicit_chain_candidates(
            "main",
            &["feat/base".to_string(), "feat/child".to_string()],
            &prs,
        )
        .expect_err("reported PR base must match explicit parent");
        assert!(err.to_string().contains("feat/child"));
        assert!(err.to_string().contains("reported base `main`"));
        assert!(err.to_string().contains("explicit parent is `feat/base`"));

        let mut prs = HashMap::new();
        let (branch, pr) = make_pr("feat/child", "feat/base", 11);
        prs.insert(branch, pr);
        let candidates = build_explicit_chain_candidates(
            "main",
            &["feat/base".to_string(), "feat/child".to_string()],
            &prs,
        )
        .expect("mixed explicit chain");
        assert_eq!(candidates[0].pr_number, None);
        assert_eq!(candidates[1].pr_number, Some(11));
        assert_eq!(candidates[0].title, "feat/base");
        assert_eq!(candidates[1].title, "PR for feat/child");
    }

    #[test]
    fn build_explicit_chain_candidates_rejects_duplicate_branch_names() {
        let err = build_explicit_chain_candidates(
            "main",
            &["feat/base".to_string(), "feat/base".to_string()],
            &HashMap::new(),
        )
        .expect_err("duplicate branch names must not create a self-parenting layer");

        assert!(err.to_string().contains("feat/base"));
        assert!(err.to_string().contains("more than once"));
    }

    fn candidate(branch: &str, base: &str, pr_number: u64) -> AdoptCandidate {
        AdoptCandidate {
            branch: branch.to_string(),
            base: base.to_string(),
            pr_number: Some(pr_number),
            title: format!("PR for {branch}"),
            is_draft: false,
        }
    }

    fn branch_candidate(branch: &str, base: &str) -> AdoptCandidate {
        AdoptCandidate {
            branch: branch.to_string(),
            base: base.to_string(),
            pr_number: None,
            title: branch.to_string(),
            is_draft: false,
        }
    }

    #[test]
    fn validate_managed_adopt_candidates_allows_matching_metadata_and_missing_pr_number() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/base", "main", "base-head", None, None);
        state.get_branch_mut("feat/base").expect("base").pr_number = Some(10);
        state.add_branch("feat/child", "feat/base", "child-head", None, None);

        validate_managed_adopt_candidates(
            &state,
            &[
                candidate("feat/base", "main", 10),
                candidate("feat/child", "feat/base", 11),
            ],
            Some(77),
        )
        .expect("matching metadata should pass");
    }

    #[test]
    fn validate_managed_adopt_candidates_rejects_parent_divergence_before_mutation() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/child", "feat/old-parent", "child-head", None, None);
        state.get_branch_mut("feat/child").expect("child").pr_number = Some(11);

        let err = validate_managed_adopt_candidates(
            &state,
            &[candidate("feat/child", "main", 11)],
            Some(77),
        )
        .expect_err("parent divergence should abort");
        let message = err.to_string();
        assert!(message.contains("native stack #77"));
        assert!(message.contains("feat/child"));
        assert!(message.contains("local parent=`feat/old-parent`"));
        assert!(message.contains("remote parent=`main`"));
        assert!(message.contains("local PR=#11"));
        assert!(message.contains("remote PR=#11"));
    }

    #[test]
    fn validate_managed_adopt_candidates_rejects_pr_number_divergence_before_mutation() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/child", "main", "child-head", None, None);
        state.get_branch_mut("feat/child").expect("child").pr_number = Some(99);

        let err = validate_managed_adopt_candidates(
            &state,
            &[candidate("feat/child", "main", 11)],
            Some(77),
        )
        .expect_err("pr divergence should abort");
        let message = err.to_string();
        assert!(message.contains("native stack #77"));
        assert!(message.contains("feat/child"));
        assert!(message.contains("local parent=`main`"));
        assert!(message.contains("remote parent=`main`"));
        assert!(message.contains("local PR=#99"));
        assert!(message.contains("remote PR=#11"));
    }

    #[test]
    fn validate_managed_adopt_candidates_rejects_parent_and_prless_conflicts_for_any_adoption() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/parent-conflict", "feat/old", "head", None, None);
        state.add_branch("feat/pr-conflict", "main", "head", None, None);
        state
            .get_branch_mut("feat/pr-conflict")
            .expect("branch")
            .pr_number = Some(99);

        let err = validate_managed_adopt_candidates(
            &state,
            &[branch_candidate("feat/parent-conflict", "main")],
            None,
        )
        .expect_err("parent mismatch should abort");
        assert!(err.to_string().contains("feat/parent-conflict"));
        assert!(err.to_string().contains("local parent=`feat/old`"));
        assert!(err.to_string().contains("remote parent=`main`"));

        let err = validate_managed_adopt_candidates(
            &state,
            &[branch_candidate("feat/pr-conflict", "main")],
            None,
        )
        .expect_err("pr-less candidate against local PR metadata should abort");
        assert!(err.to_string().contains("feat/pr-conflict"));
        assert!(err.to_string().contains("local PR=#99"));
        assert!(err.to_string().contains("remote PR=none"));

        validate_managed_adopt_candidates(
            &state,
            &[candidate("feat/pr-conflict", "main", 99)],
            None,
        )
        .expect("matching PR metadata should pass");
    }

    #[test]
    fn fetch_candidates_by_pr_number_prefers_native_stack_order_and_graphql_prs() {
        use crate::test_support::{
            CwdGuard, PathGuard, init_git_repo, install_fake_bin, run_cmd, take_env_lock,
        };

        let _guard = take_env_lock();
        let repo = init_git_repo("adopt-native-fetch");
        let _cwd = CwdGuard::enter(&repo);
        run_cmd(
            &repo,
            "git",
            &["remote", "add", "origin", "https://github.com/org/repo.git"],
        );

        let fake_dir = install_fake_bin(
            "gh-adopt-native-fetch",
            "gh",
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$EZ_FAKE_GH_LOG"
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=20" ]; then
  echo '[{"number":88,"pull_requests":[{"number":10},{"number":20}]}]'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  pr=""
  for arg in "$@"; do
    case "$arg" in
      num=10) pr=10 ;;
      num=20) pr=20 ;;
    esac
  done
  if [ "$pr" = "10" ]; then
    echo '{"data":{"repository":{"pullRequest":{"number":10,"url":"https://github.com/org/repo/pull/10","state":"OPEN","title":"Base","baseRefName":"stale-parent","headRefName":"feat/base","isDraft":false,"mergedAt":null}}}}'
    exit 0
  fi
  if [ "$pr" = "20" ]; then
    echo '{"data":{"repository":{"pullRequest":{"number":20,"url":"https://github.com/org/repo/pull/20","state":"OPEN","title":"Child","baseRefName":"main","headRefName":"feat/child","isDraft":true,"mergedAt":null}}}}'
    exit 0
  fi
fi
exit 2
"#,
        );
        let log_path = fake_dir.join("args.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let (title, native_stack_number, candidates) =
            fetch_candidates_by_pr_number("origin", None, "main", 20, true)
                .expect("fetch native candidates");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }
        assert_eq!(title, "Child");
        assert_eq!(native_stack_number, Some(88));
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.branch.as_str())
                .collect::<Vec<_>>(),
            vec!["feat/base", "feat/child"]
        );
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.base.as_str())
                .collect::<Vec<_>>(),
            vec!["main", "feat/base"]
        );
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.pr_number)
                .collect::<Vec<_>>(),
            vec![Some(10), Some(20)]
        );
        assert!(candidates[1].is_draft);

        let log = std::fs::read_to_string(log_path).expect("gh log");
        assert!(log.contains("api repos/org/repo/stacks?pull_request=20"));
        assert!(log.contains("api graphql"));
        assert!(log.contains("num=10"));
        assert!(log.contains("num=20"));
    }

    #[test]
    fn adopt_receipt_includes_native_prs_and_worktree_paths() {
        let candidates = vec![
            AdoptCandidate {
                branch: "feat/base".to_string(),
                base: "main".to_string(),
                pr_number: Some(10),
                title: "Base".to_string(),
                is_draft: false,
            },
            AdoptCandidate {
                branch: "feat/child".to_string(),
                base: "feat/base".to_string(),
                pr_number: Some(11),
                title: "Child".to_string(),
                is_draft: true,
            },
        ];
        let provisioning = WorktreeProvisioning {
            created: 2,
            paths: vec![
                "/repo/.worktrees/feat-base".to_string(),
                "/repo/.worktrees/feat-child".to_string(),
            ],
            created_paths: vec![
                "/repo/.worktrees/feat-base".to_string(),
                "/repo/.worktrees/feat-child".to_string(),
            ],
        };

        let receipt = adopt_receipt_value(2, 0, &candidates, Some(77), &provisioning);

        assert_eq!(receipt["cmd"], "adopt");
        assert_eq!(receipt["adopted"], 2);
        assert_eq!(receipt["skipped"], 0);
        assert_eq!(receipt["native_stack_number"], 77);
        assert_eq!(
            receipt["branches"],
            serde_json::json!(["feat/base", "feat/child"])
        );
        assert_eq!(receipt["pr_numbers"], serde_json::json!([10, 11]));
        assert_eq!(receipt["worktrees_created"], 2);
        assert_eq!(
            receipt["worktree_paths"],
            serde_json::json!(["/repo/.worktrees/feat-base", "/repo/.worktrees/feat-child"])
        );
    }

    #[test]
    fn adopt_receipt_uses_null_native_stack_number_for_legacy_adoption() {
        let candidates = vec![AdoptCandidate {
            branch: "feat/base".to_string(),
            base: "main".to_string(),
            pr_number: None,
            title: "Base".to_string(),
            is_draft: false,
        }];
        let provisioning = WorktreeProvisioning {
            created: 0,
            paths: Vec::new(),
            created_paths: Vec::new(),
        };

        let receipt = adopt_receipt_value(0, 1, &candidates, None, &provisioning);

        assert!(receipt["native_stack_number"].is_null());
        assert_eq!(receipt["pr_numbers"], serde_json::json!([null]));
        assert_eq!(receipt["worktrees_created"], 0);
        assert_eq!(receipt["worktree_paths"], serde_json::json!([]));
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

    #[test]
    fn provision_worktrees_creates_reuses_and_skips_candidate_worktrees() {
        use crate::test_support::{CwdGuard, init_git_repo, take_env_lock};

        let _guard = take_env_lock();
        let repo = init_git_repo("adopt-worktrees");
        let _cwd = CwdGuard::enter(&repo);
        let main_head = git::rev_parse("main").expect("main sha");
        git::create_branch_at("feat/base", &main_head).expect("create base branch");
        git::create_branch_at("feat/child", &main_head).expect("create child branch");

        let candidates = vec![
            AdoptCandidate {
                branch: "feat/base".to_string(),
                base: "main".to_string(),
                pr_number: Some(10),
                title: "Base".to_string(),
                is_draft: false,
            },
            AdoptCandidate {
                branch: "feat/child".to_string(),
                base: "feat/base".to_string(),
                pr_number: Some(11),
                title: "Child".to_string(),
                is_draft: false,
            },
        ];

        let skipped = provision_worktrees(&candidates, true).expect("skip worktrees");
        assert_eq!(skipped.created, 0);
        assert!(skipped.paths.is_empty());
        assert_eq!(git::worktree_list().expect("worktree list").len(), 1);

        let first = provision_worktrees(&candidates, false).expect("create worktrees");
        assert_eq!(first.created, 2);
        assert_eq!(first.paths.len(), 2);
        assert!(first.paths.iter().all(|path| path.contains("/.worktrees/")));
        assert!(
            first
                .paths
                .iter()
                .all(|path| std::path::Path::new(path).exists())
        );

        let second = provision_worktrees(&candidates, false).expect("reuse worktrees");
        assert_eq!(second.created, 0);
        assert_eq!(second.paths, first.paths);
    }

    #[test]
    fn validate_existing_branch_pr_refs_allows_equal_and_local_ahead_but_rejects_stale_or_diverged()
    {
        use crate::test_support::{CwdGuard, init_git_repo, take_env_lock, write_file};

        let _guard = take_env_lock();
        let repo = init_git_repo("adopt-pr-ref-preflight");
        let _cwd = CwdGuard::enter(&repo);
        let main_head = git::rev_parse("main").expect("main sha");

        git::create_branch_at("pr-head", &main_head).expect("create pr-head");
        git::checkout("pr-head").expect("checkout pr-head");
        write_file(&repo, "pr.txt", "pr\n");
        git::add_all_including_untracked().expect("stage pr");
        git::commit("pr head").expect("commit pr");
        let pr_head = git::rev_parse("pr-head").expect("pr sha");

        git::create_branch_at("feat/equal", &pr_head).expect("create equal");
        git::create_branch_at("feat/ahead", &pr_head).expect("create ahead");
        git::checkout("feat/ahead").expect("checkout ahead");
        write_file(&repo, "ahead.txt", "ahead\n");
        git::add_all_including_untracked().expect("stage ahead");
        git::commit("ahead").expect("commit ahead");

        git::checkout("main").expect("checkout main");
        git::create_branch_at("feat/stale", &main_head).expect("create stale");
        git::create_branch_at("feat/diverged", &main_head).expect("create diverged");
        git::checkout("feat/diverged").expect("checkout diverged");
        write_file(&repo, "diverged.txt", "diverged\n");
        git::add_all_including_untracked().expect("stage diverged");
        git::commit("diverged").expect("commit diverged");
        git::checkout("main").expect("checkout main");

        let mut refs = HashMap::new();
        refs.insert("feat/equal".to_string(), pr_head.clone());
        refs.insert("feat/ahead".to_string(), pr_head.clone());
        refs.insert("feat/stale".to_string(), pr_head.clone());
        refs.insert("feat/diverged".to_string(), pr_head.clone());

        validate_existing_branch_pr_refs(
            &[
                candidate("feat/equal", "main", 10),
                candidate("feat/ahead", "main", 11),
            ],
            &refs,
        )
        .expect("equal and local-ahead branches should pass");

        let stale = validate_existing_branch_pr_refs(&[candidate("feat/stale", "main", 12)], &refs)
            .expect_err("stale local branch should fail");
        assert!(stale.to_string().contains("feat/stale"));
        assert!(stale.to_string().contains("PR #12"));

        let diverged =
            validate_existing_branch_pr_refs(&[candidate("feat/diverged", "main", 13)], &refs)
                .expect_err("diverged local branch should fail");
        assert!(diverged.to_string().contains("feat/diverged"));
        assert!(diverged.to_string().contains("PR #13"));
    }

    #[test]
    fn adoption_transaction_rolls_back_worktrees_branches_and_preserves_state_on_provision_failure()
    {
        use crate::test_support::{CwdGuard, init_git_repo, take_env_lock, write_file};

        let _guard = take_env_lock();
        let repo = init_git_repo("adopt-transaction-rollback");
        let _cwd = CwdGuard::enter(&repo);

        let mut state = StackState::new("main".to_string());
        state.repo = Some("sentinel/repo".to_string());
        state.save().expect("save sentinel state");
        let state_path = StackState::state_path().expect("state path");
        let original_state = std::fs::read(&state_path).expect("read sentinel state");

        let main_head = git::rev_parse("main").expect("main sha");
        let candidates = vec![
            candidate("feat/base", "main", 10),
            candidate("feat/child", "feat/base", 11),
        ];
        let mut refs = HashMap::new();
        refs.insert("feat/base".to_string(), main_head.clone());
        refs.insert("feat/child".to_string(), main_head);

        let child_collision =
            std::path::PathBuf::from(git::worktree_path("feat/child").expect("child path"));
        write_file(&child_collision, "collision.txt", "block worktree add\n");

        let err = adopt_candidates_transactionally(&mut state, &candidates, false, &refs, true)
            .expect_err("second worktree add should fail");
        assert!(
            err.to_string().contains("already exists")
                || err.to_string().contains("exists")
                || err.to_string().contains("not empty"),
            "unexpected error: {err}"
        );

        assert_eq!(
            std::fs::read(&state_path).expect("read state after failure"),
            original_state,
            "stack.json must remain byte-for-byte unchanged"
        );
        assert!(!git::branch_exists("feat/base"));
        assert!(!git::branch_exists("feat/child"));

        let worktrees = git::worktree_list().expect("worktree list");
        assert!(
            worktrees
                .iter()
                .all(|wt| wt.branch.as_deref() != Some("feat/base")
                    && wt.branch.as_deref() != Some("feat/child")),
            "candidate worktrees should be rolled back: {worktrees:?}"
        );
        assert!(
            !std::path::Path::new(&git::worktree_path("feat/base").expect("base path")).exists(),
            "first created worktree path should be removed"
        );
        assert!(
            child_collision.exists(),
            "pre-existing collision directory should not be deleted"
        );
    }

    #[test]
    fn adoption_transaction_rolls_back_earlier_branch_when_later_branch_setup_fails() {
        use crate::test_support::{CwdGuard, init_git_repo, take_env_lock};

        let _guard = take_env_lock();
        let repo = init_git_repo("adopt-branch-rollback");
        let _cwd = CwdGuard::enter(&repo);

        let mut state = StackState::new("main".to_string());
        state.repo = Some("sentinel/repo".to_string());
        state.save().expect("save sentinel state");
        let state_path = StackState::state_path().expect("state path");
        let original_state = std::fs::read(&state_path).expect("read sentinel state");

        let candidates = vec![
            candidate("feat/base", "main", 10),
            candidate("feat/child", "feat/base", 11),
        ];
        let mut refs = HashMap::new();
        refs.insert(
            "feat/base".to_string(),
            git::rev_parse("main").expect("main sha"),
        );

        let err = adopt_candidates_transactionally(&mut state, &candidates, true, &refs, true)
            .expect_err("missing second PR ref should abort");
        assert!(
            err.to_string()
                .contains("missing fetched source ref for `feat/child`")
        );
        assert_eq!(
            std::fs::read(&state_path).expect("read state after failure"),
            original_state,
            "stack.json must remain byte-for-byte unchanged"
        );
        assert!(!git::branch_exists("feat/base"));
        assert!(!git::branch_exists("feat/child"));
    }

    #[test]
    fn managed_candidate_with_missing_local_ref_is_rehydrated_before_worktree_provisioning() {
        use crate::test_support::{CwdGuard, init_git_repo, take_env_lock};

        let _guard = take_env_lock();
        let repo = init_git_repo("adopt-managed-missing-ref");
        let _cwd = CwdGuard::enter(&repo);
        let main_head = git::rev_parse("main").expect("main sha");

        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/remote", "main", &main_head, None, None);
        state.save().expect("save managed metadata");

        let candidates = vec![branch_candidate("feat/remote", "main")];
        let refs = HashMap::from([("feat/remote".to_string(), main_head)]);

        let (adopted, skipped, provisioning) =
            adopt_candidates_transactionally(&mut state, &candidates, false, &refs, true)
                .expect("rehydrate managed branch and provision its worktree");

        assert_eq!((adopted, skipped), (0, 1));
        assert_eq!(provisioning.created, 1);
        assert!(git::branch_exists("feat/remote"));
        assert!(
            std::path::Path::new(&git::worktree_path("feat/remote").expect("worktree path"))
                .exists()
        );
        assert!(StackState::load().expect("state").is_managed("feat/remote"));
    }

    #[test]
    fn explicit_remote_only_branch_fetches_creates_worktree_and_saves_state() {
        use crate::test_support::{
            CwdGuard, init_git_repo, run_cmd, take_env_lock, temp_dir, write_file,
        };

        let _guard = take_env_lock();
        let remote = temp_dir("adopt-remote-only-bare");
        run_cmd(&remote, "git", &["init", "--bare", "-b", "main"]);
        let repo = init_git_repo("adopt-remote-only");
        let _cwd = CwdGuard::enter(&repo);
        run_cmd(
            &repo,
            "git",
            &["remote", "add", "origin", remote.to_str().expect("remote")],
        );
        run_cmd(&repo, "git", &["push", "origin", "main"]);

        git::create_branch("feat/remote").expect("create remote branch");
        write_file(&repo, "remote.txt", "remote\n");
        git::add_all_including_untracked().expect("stage remote");
        git::commit("remote branch").expect("commit remote");
        run_cmd(&repo, "git", &["push", "origin", "feat/remote"]);
        git::checkout("main").expect("checkout main");
        git::delete_branch("feat/remote", true).expect("delete local remote branch");

        let mut state = StackState::new("main".to_string());
        state.remote = "origin".to_string();
        state.save().expect("save state");
        let candidates =
            build_explicit_chain_candidates("main", &["feat/remote".to_string()], &HashMap::new())
                .expect("explicit candidates");
        validate_managed_adopt_candidates(&state, &candidates, None).expect("metadata preflight");
        let refs =
            fetch_and_validate_candidate_refs("origin", &candidates).expect("fetch remote ref");
        let (adopted, skipped, provisioning) =
            adopt_candidates_transactionally(&mut state, &candidates, false, &refs, true)
                .expect("adopt");

        assert_eq!((adopted, skipped), (1, 0));
        assert_eq!(provisioning.created, 1);
        assert!(git::branch_exists("feat/remote"));
        let saved = StackState::load().expect("load state");
        let meta = saved.get_branch("feat/remote").expect("tracked branch");
        assert_eq!(meta.parent, "main");
        assert_eq!(meta.pr_number, None);
        assert!(std::path::Path::new(&git::worktree_path("feat/remote").expect("wt")).exists());
    }

    #[test]
    fn explicit_local_prless_branch_supports_worktree_and_no_worktrees_modes() {
        use crate::test_support::{CwdGuard, init_git_repo, take_env_lock, write_file};

        let _guard = take_env_lock();
        let repo = init_git_repo("adopt-local-prless");
        let _cwd = CwdGuard::enter(&repo);
        git::create_branch("feat/local").expect("create local branch");
        write_file(&repo, "local.txt", "local\n");
        git::add_all_including_untracked().expect("stage local");
        git::commit("local branch").expect("commit local");
        git::checkout("main").expect("checkout main");

        let mut state = StackState::new("main".to_string());
        state.save().expect("save state");
        let candidates =
            build_explicit_chain_candidates("main", &["feat/local".to_string()], &HashMap::new())
                .expect("explicit candidates");
        let refs =
            fetch_and_validate_candidate_refs("origin", &candidates).expect("local-only accepted");
        let (_, _, provisioning) =
            adopt_candidates_transactionally(&mut state, &candidates, false, &refs, true)
                .expect("adopt local");
        assert_eq!(provisioning.created, 1);
        assert!(
            StackState::load()
                .expect("state")
                .get_branch("feat/local")
                .expect("branch")
                .pr_number
                .is_none()
        );

        let repo2 = init_git_repo("adopt-local-prless-no-worktree");
        let _cwd2 = CwdGuard::enter(&repo2);
        git::create_branch("feat/local").expect("create local branch");
        write_file(&repo2, "local.txt", "local\n");
        git::add_all_including_untracked().expect("stage local");
        git::commit("local branch").expect("commit local");
        git::checkout("main").expect("checkout main");
        let mut state2 = StackState::new("main".to_string());
        state2.save().expect("save state");
        let refs =
            fetch_and_validate_candidate_refs("origin", &candidates).expect("local-only accepted");
        let (_, _, provisioning) =
            adopt_candidates_transactionally(&mut state2, &candidates, true, &refs, true)
                .expect("adopt without worktree");
        assert_eq!(provisioning.created, 0);
        assert!(!std::path::Path::new(&git::worktree_path("feat/local").expect("wt")).exists());
    }

    #[test]
    fn explicit_local_branch_adoption_runs_without_github_authentication() {
        use crate::test_support::{
            CwdGuard, PathGuard, init_git_repo, install_fake_bin, take_env_lock, write_file,
        };

        let _guard = take_env_lock();
        let repo = init_git_repo("adopt-local-offline");
        let _cwd = CwdGuard::enter(&repo);
        git::create_branch("feat/offline").expect("create local branch");
        write_file(&repo, "offline.txt", "offline\n");
        git::add_all_including_untracked().expect("stage local branch");
        git::commit("offline branch").expect("commit local branch");
        git::checkout("main").expect("checkout main");

        StackState::new("main".to_string())
            .save()
            .expect("save initial state");
        let fake_dir = install_fake_bin("gh-adopt-offline", "gh", "#!/bin/sh\nexit 1\n");
        let _path = PathGuard::install(&fake_dir);

        run(None, &["feat/offline".to_string()], true)
            .expect("explicit local adoption should not require gh auth");

        let saved = StackState::load().expect("load saved state");
        let meta = saved.get_branch("feat/offline").expect("tracked branch");
        assert_eq!(meta.parent, "main");
        assert_eq!(meta.pr_number, None);
        assert!(
            !std::path::Path::new(&git::worktree_path("feat/offline").expect("worktree path"))
                .exists()
        );
    }

    #[test]
    fn explicit_prless_ref_preflight_allows_equal_ahead_and_rejects_behind_diverged_missing() {
        use crate::test_support::{
            CwdGuard, init_git_repo, run_cmd, take_env_lock, temp_dir, write_file,
        };

        let _guard = take_env_lock();
        let remote = temp_dir("adopt-ref-preflight-bare");
        run_cmd(&remote, "git", &["init", "--bare", "-b", "main"]);
        let repo = init_git_repo("adopt-ref-preflight");
        let _cwd = CwdGuard::enter(&repo);
        run_cmd(
            &repo,
            "git",
            &["remote", "add", "origin", remote.to_str().expect("remote")],
        );
        run_cmd(&repo, "git", &["push", "origin", "main"]);

        git::create_branch("feat/equal").expect("equal");
        write_file(&repo, "equal.txt", "equal\n");
        git::add_all_including_untracked().expect("stage equal");
        git::commit("equal").expect("commit equal");
        run_cmd(&repo, "git", &["push", "origin", "feat/equal"]);

        git::checkout("main").expect("main");
        git::create_branch_at("feat/ahead", "feat/equal").expect("ahead");
        git::checkout("feat/ahead").expect("checkout ahead");
        write_file(&repo, "ahead.txt", "ahead\n");
        git::add_all_including_untracked().expect("stage ahead");
        git::commit("ahead").expect("commit ahead");
        run_cmd(&repo, "git", &["push", "origin", "feat/ahead"]);
        write_file(&repo, "ahead-local.txt", "ahead local\n");
        git::add_all_including_untracked().expect("stage ahead local");
        git::commit("ahead local").expect("commit ahead local");

        git::checkout("main").expect("main");
        git::create_branch_at("feat/behind", "feat/equal").expect("behind");
        run_cmd(&repo, "git", &["push", "origin", "feat/behind"]);
        git::checkout("feat/behind").expect("behind");
        write_file(&repo, "behind-remote.txt", "behind remote\n");
        git::add_all_including_untracked().expect("stage behind remote");
        git::commit("behind remote").expect("commit behind remote");
        run_cmd(&repo, "git", &["push", "origin", "feat/behind"]);
        git::checkout("feat/behind").expect("behind");
        git::hard_reset("HEAD~1").expect("make behind");

        git::checkout("main").expect("main");
        git::create_branch_at("feat/diverged", "feat/equal").expect("diverged");
        run_cmd(&repo, "git", &["push", "origin", "feat/diverged"]);
        git::checkout("feat/diverged").expect("diverged");
        write_file(&repo, "diverged-local.txt", "diverged local\n");
        git::add_all_including_untracked().expect("stage diverged local");
        git::commit("diverged local").expect("commit diverged local");

        let remote_clone_parent = temp_dir("adopt-ref-preflight-remote-clone-parent");
        run_cmd(
            &remote_clone_parent,
            "git",
            &["clone", remote.to_str().expect("remote"), "remote-clone"],
        );
        let remote_clone = remote_clone_parent.join("remote-clone");
        run_cmd(&remote_clone, "git", &["config", "user.name", "Test User"]);
        run_cmd(
            &remote_clone,
            "git",
            &["config", "user.email", "test@example.com"],
        );
        run_cmd(&remote_clone, "git", &["checkout", "feat/diverged"]);
        write_file(&remote_clone, "diverged-remote.txt", "diverged remote\n");
        run_cmd(&remote_clone, "git", &["add", "diverged-remote.txt"]);
        run_cmd(&remote_clone, "git", &["commit", "-m", "diverged remote"]);
        run_cmd(&remote_clone, "git", &["push", "origin", "feat/diverged"]);

        git::checkout("main").expect("main");

        fetch_and_validate_candidate_refs(
            "origin",
            &[
                branch_candidate("feat/equal", "main"),
                branch_candidate("feat/ahead", "feat/equal"),
            ],
        )
        .expect("equal and ahead accepted");

        let behind =
            fetch_and_validate_candidate_refs("origin", &[branch_candidate("feat/behind", "main")])
                .expect_err("behind rejected");
        assert!(
            behind.to_string().contains("behind")
                || behind.to_string().contains("does not contain")
        );

        let diverged = fetch_and_validate_candidate_refs(
            "origin",
            &[branch_candidate("feat/diverged", "main")],
        )
        .expect_err("diverged rejected");
        assert!(diverged.to_string().contains("diverged"));

        let missing = fetch_and_validate_candidate_refs(
            "origin",
            &[branch_candidate("feat/missing", "main")],
        )
        .expect_err("missing rejected");
        assert!(missing.to_string().contains("feat/missing"));
    }

    #[test]
    fn explicit_unrelated_history_aborts_and_rolls_back_state_refs_and_worktrees() {
        use crate::test_support::{
            CwdGuard, init_git_repo, run_cmd, take_env_lock, temp_dir, write_file,
        };

        let _guard = take_env_lock();
        let repo = init_git_repo("adopt-unrelated-rollback");
        let other = temp_dir("adopt-unrelated-other");
        run_cmd(&other, "git", &["init", "-b", "main"]);
        run_cmd(&other, "git", &["config", "user.name", "Test User"]);
        run_cmd(&other, "git", &["config", "user.email", "test@example.com"]);
        write_file(&other, "other.txt", "other\n");
        run_cmd(&other, "git", &["add", "other.txt"]);
        run_cmd(&other, "git", &["commit", "-m", "other root"]);
        run_cmd(&other, "git", &["branch", "feat/unrelated"]);

        let _cwd = CwdGuard::enter(&repo);
        run_cmd(
            &repo,
            "git",
            &["remote", "add", "other", other.to_str().expect("other")],
        );
        run_cmd(
            &repo,
            "git",
            &[
                "fetch",
                "other",
                "feat/unrelated:refs/remotes/origin/feat/unrelated",
            ],
        );
        let unrelated_ref = "origin/feat/unrelated".to_string();
        let mut state = StackState::new("main".to_string());
        state.save().expect("save state");
        let state_path = StackState::state_path().expect("state path");
        let original_state = std::fs::read(&state_path).expect("state bytes");
        let candidates = vec![branch_candidate("feat/unrelated", "main")];
        let mut refs = HashMap::new();
        refs.insert("feat/unrelated".to_string(), unrelated_ref);

        let err = adopt_candidates_transactionally(&mut state, &candidates, false, &refs, true)
            .expect_err("unrelated merge-base should abort explicit adoption");
        assert!(err.to_string().contains("parent") || err.to_string().contains("merge"));
        assert_eq!(
            std::fs::read(&state_path).expect("state bytes"),
            original_state
        );
        assert!(!git::branch_exists("feat/unrelated"));
        assert!(!std::path::Path::new(&git::worktree_path("feat/unrelated").expect("wt")).exists());
    }
}
