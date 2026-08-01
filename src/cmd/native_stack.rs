use crate::github;
use crate::stack::StackState;
use crate::ui;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeStackChain {
    pub(crate) branches: Vec<String>,
    pub(crate) pr_numbers: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkippedNativeStackComponent {
    pub(crate) root: String,
    pub(crate) branches: Vec<String>,
    pub(crate) pr_numbers: Vec<u64>,
    pub(crate) reason: &'static str,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NativeStackPlan {
    pub(crate) chains: Vec<NativeStackChain>,
    pub(crate) skipped: Vec<SkippedNativeStackComponent>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct NativeStackInspection {
    pub(crate) provider: &'static str,
    pub(crate) preview: bool,
    pub(crate) state: String,
    pub(crate) local: LocalNativeStackInspection,
    pub(crate) github: Option<GitHubNativeStackInspection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct LocalNativeStackInspection {
    pub(crate) branches: Vec<String>,
    pub(crate) pull_requests: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct GitHubNativeStackInspection {
    pub(crate) number: u64,
    pub(crate) base_ref: Option<String>,
    pub(crate) open: Option<bool>,
    pub(crate) position: Option<usize>,
    pub(crate) size: usize,
    pub(crate) pull_requests: Vec<u64>,
}

/// Project ez's worktree-aware parent graph onto GitHub's linear stack model.
///
/// Each direct child of trunk starts an independent component. Linear components
/// become one or more contiguous PR chains; a managed branch without a PR splits
/// the chain. Branching components are intentionally skipped because flattening
/// them would invent a PR order that does not exist in the local worktree graph.
pub(crate) fn native_stack_plan(state: &StackState) -> NativeStackPlan {
    let mut plan = NativeStackPlan::default();
    let mut visited = HashSet::new();
    let roots = state.children_of(&state.trunk);

    for root in roots {
        let mut component = Vec::new();
        let mut queue = VecDeque::from([root.clone()]);
        let mut branching = false;

        while let Some(branch) = queue.pop_front() {
            if !visited.insert(branch.clone()) {
                continue;
            }
            component.push(branch.clone());
            let children = state.children_of(&branch);
            branching |= children.len() > 1;
            queue.extend(children);
        }

        component.sort();
        if branching {
            plan.skipped.push(SkippedNativeStackComponent {
                root,
                pr_numbers: pr_numbers_for_branches(state, &component),
                branches: component,
                reason: "branching_component",
            });
            continue;
        }

        let mut branch = root;
        let mut segment_branches = Vec::new();
        let mut segment_prs = Vec::new();
        loop {
            let meta = state
                .branches
                .get(&branch)
                .expect("rooted native stack component must contain every branch");
            if let Some(pr_number) = meta.pr_number {
                segment_branches.push(branch.clone());
                segment_prs.push(pr_number);
            } else {
                push_native_stack_segment(
                    &mut plan.chains,
                    &mut segment_branches,
                    &mut segment_prs,
                );
            }

            let children = state.children_of(&branch);
            let Some(child) = children.first() else {
                break;
            };
            branch = child.clone();
        }
        push_native_stack_segment(&mut plan.chains, &mut segment_branches, &mut segment_prs);
    }

    let all_branches = state.branches.keys().cloned().collect::<BTreeSet<_>>();
    let mut remaining = all_branches
        .into_iter()
        .filter(|branch| !visited.contains(branch))
        .collect::<BTreeSet<_>>();
    while let Some(start) = remaining.first().cloned() {
        let mut component = BTreeSet::new();
        let mut queue = VecDeque::from([start.clone()]);
        while let Some(branch) = queue.pop_front() {
            if !remaining.remove(&branch) || !component.insert(branch.clone()) {
                continue;
            }
            if let Some(meta) = state.branches.get(&branch)
                && state.branches.contains_key(&meta.parent)
            {
                queue.push_back(meta.parent.clone());
            }
            queue.extend(state.children_of(&branch));
        }
        let branches = component.into_iter().collect::<Vec<_>>();
        plan.skipped.push(SkippedNativeStackComponent {
            root: start,
            pr_numbers: pr_numbers_for_branches(state, &branches),
            branches,
            reason: "invalid_parent_graph",
        });
    }

    plan
}

pub(crate) fn linkable_chains(plan: &NativeStackPlan) -> Vec<&NativeStackChain> {
    plan.chains
        .iter()
        .filter(|chain| chain.pr_numbers.len() >= 2)
        .collect()
}

fn push_native_stack_segment(
    chains: &mut Vec<NativeStackChain>,
    branches: &mut Vec<String>,
    pr_numbers: &mut Vec<u64>,
) {
    if !pr_numbers.is_empty() {
        chains.push(NativeStackChain {
            branches: std::mem::take(branches),
            pr_numbers: std::mem::take(pr_numbers),
        });
    } else {
        branches.clear();
        pr_numbers.clear();
    }
}

fn pr_numbers_for_branches(state: &StackState, branches: &[String]) -> Vec<u64> {
    branches
        .iter()
        .filter_map(|branch| state.branches.get(branch).and_then(|meta| meta.pr_number))
        .collect()
}

pub(crate) fn inspect_branch(state: &StackState, branch: &str) -> NativeStackInspection {
    if state.is_fork_workflow() {
        return fork_not_applicable_branch_inspection(state, branch);
    }
    inspect_branch_with_lookup(state, branch, |pr_number| {
        github::lookup_native_stack_for_pr(pr_number, state.repo.as_deref())
    })
}

pub(crate) fn inspect_all(state: &StackState) -> HashMap<String, NativeStackInspection> {
    if state.is_fork_workflow() {
        return fork_not_applicable_all_inspections(state);
    }
    inspect_all_with_lookup(state, |pr_number| {
        github::lookup_native_stack_for_pr(pr_number, state.repo.as_deref())
    })
}

fn inspect_branch_with_lookup(
    state: &StackState,
    branch: &str,
    lookup: impl Fn(u64) -> anyhow::Result<github::NativeStackLookup>,
) -> NativeStackInspection {
    if state.is_trunk(branch) || !state.is_managed(branch) {
        return not_applicable_inspection(branch);
    }

    let Some(meta) = state.branches.get(branch) else {
        return not_applicable_inspection(branch);
    };
    let Some(pr_number) = meta.pr_number else {
        return not_applicable_inspection(branch);
    };

    let plan = native_stack_plan(state);
    if let Some(component) = plan
        .skipped
        .iter()
        .find(|component| component.branches.iter().any(|name| name == branch))
    {
        return unrepresentable_inspection(component);
    }

    let Some(chain) = plan
        .chains
        .iter()
        .find(|chain| chain.branches.iter().any(|name| name == branch))
    else {
        return not_applicable_inspection(branch);
    };

    inspection_from_lookup(chain, pr_number, lookup(pr_number))
}

fn inspect_all_with_lookup(
    state: &StackState,
    lookup: impl Fn(u64) -> anyhow::Result<github::NativeStackLookup>,
) -> HashMap<String, NativeStackInspection> {
    let mut inspections = HashMap::new();
    let plan = native_stack_plan(state);

    for component in &plan.skipped {
        for branch in &component.branches {
            inspections.insert(branch.clone(), unrepresentable_inspection(component));
        }
    }

    for chain in &plan.chains {
        let top_pr = *chain
            .pr_numbers
            .last()
            .expect("native stack chain contains at least one PR");
        let lookup_result = lookup(top_pr);
        for (branch, pr_number) in chain.branches.iter().zip(&chain.pr_numbers) {
            inspections.insert(
                branch.clone(),
                inspection_from_lookup(chain, *pr_number, clone_lookup_result(&lookup_result)),
            );
        }
    }

    for branch in state.branches.keys() {
        inspections
            .entry(branch.clone())
            .or_insert_with(|| not_applicable_inspection(branch));
    }

    inspections
}

fn clone_lookup_result(
    result: &anyhow::Result<github::NativeStackLookup>,
) -> anyhow::Result<github::NativeStackLookup> {
    match result {
        Ok(lookup) => Ok(lookup.clone()),
        Err(err) => Err(anyhow::anyhow!(err.to_string())),
    }
}

fn inspection_from_lookup(
    chain: &NativeStackChain,
    pr_number: u64,
    lookup: anyhow::Result<github::NativeStackLookup>,
) -> NativeStackInspection {
    match lookup {
        Ok(github::NativeStackLookup::Found(info)) => {
            let position = info
                .pull_requests
                .iter()
                .position(|remote_pr| *remote_pr == pr_number)
                .map(|idx| idx + 1);
            let state = if info.pull_requests == chain.pr_numbers {
                "in_sync"
            } else {
                "diverged"
            };
            base_inspection(
                state,
                chain.branches.clone(),
                chain.pr_numbers.clone(),
                Some(GitHubNativeStackInspection {
                    number: info.number,
                    base_ref: info.base_ref,
                    open: info.open,
                    position,
                    size: info.pull_requests.len(),
                    pull_requests: info.pull_requests,
                }),
                None,
                None,
            )
        }
        Ok(github::NativeStackLookup::NotLinked) => base_inspection(
            "not_linked",
            chain.branches.clone(),
            chain.pr_numbers.clone(),
            None,
            None,
            None,
        ),
        Ok(github::NativeStackLookup::Unavailable) => base_inspection(
            "unavailable",
            chain.branches.clone(),
            chain.pr_numbers.clone(),
            None,
            None,
            None,
        ),
        Err(err) => base_inspection(
            "error",
            chain.branches.clone(),
            chain.pr_numbers.clone(),
            None,
            None,
            Some(err.to_string()),
        ),
    }
}

fn unrepresentable_inspection(component: &SkippedNativeStackComponent) -> NativeStackInspection {
    base_inspection(
        "unrepresentable",
        component.branches.clone(),
        component.pr_numbers.clone(),
        None,
        Some(component.reason.to_string()),
        None,
    )
}

fn not_applicable_inspection(branch: &str) -> NativeStackInspection {
    base_inspection(
        "not_applicable",
        vec![branch.to_string()],
        Vec::new(),
        None,
        None,
        None,
    )
}

fn fork_not_applicable_reason() -> String {
    "GitHub native stacks require pull requests in one repository; fork/cross-repository workflows keep the ez stack local".to_string()
}

fn fork_not_applicable_inspection(
    branches: Vec<String>,
    pull_requests: Vec<u64>,
) -> NativeStackInspection {
    base_inspection(
        "not_applicable",
        branches,
        pull_requests,
        None,
        Some(fork_not_applicable_reason()),
        None,
    )
}

fn fork_not_applicable_branch_inspection(
    state: &StackState,
    branch: &str,
) -> NativeStackInspection {
    if state.is_trunk(branch) || !state.is_managed(branch) {
        return fork_not_applicable_inspection(vec![branch.to_string()], Vec::new());
    }

    let plan = native_stack_plan(state);
    if let Some(component) = plan
        .skipped
        .iter()
        .find(|component| component.branches.iter().any(|name| name == branch))
    {
        return fork_not_applicable_inspection(
            component.branches.clone(),
            component.pr_numbers.clone(),
        );
    }
    if let Some(chain) = plan
        .chains
        .iter()
        .find(|chain| chain.branches.iter().any(|name| name == branch))
    {
        return fork_not_applicable_inspection(chain.branches.clone(), chain.pr_numbers.clone());
    }

    let pr_number = state
        .branches
        .get(branch)
        .and_then(|meta| meta.pr_number)
        .into_iter()
        .collect();
    fork_not_applicable_inspection(vec![branch.to_string()], pr_number)
}

fn fork_not_applicable_all_inspections(
    state: &StackState,
) -> HashMap<String, NativeStackInspection> {
    let mut inspections = HashMap::new();
    let plan = native_stack_plan(state);

    for component in &plan.skipped {
        let inspection = fork_not_applicable_inspection(
            component.branches.clone(),
            component.pr_numbers.clone(),
        );
        for branch in &component.branches {
            inspections.insert(branch.clone(), inspection.clone());
        }
    }

    for chain in &plan.chains {
        let inspection =
            fork_not_applicable_inspection(chain.branches.clone(), chain.pr_numbers.clone());
        for branch in &chain.branches {
            inspections.insert(branch.clone(), inspection.clone());
        }
    }

    for branch in state.branches.keys() {
        inspections
            .entry(branch.clone())
            .or_insert_with(|| fork_not_applicable_branch_inspection(state, branch));
    }

    inspections
}

fn base_inspection(
    state: &str,
    branches: Vec<String>,
    pull_requests: Vec<u64>,
    github: Option<GitHubNativeStackInspection>,
    reason: Option<String>,
    error: Option<String>,
) -> NativeStackInspection {
    NativeStackInspection {
        provider: "github",
        preview: true,
        state: state.to_string(),
        local: LocalNativeStackInspection {
            branches,
            pull_requests,
        },
        github,
        reason,
        error,
    }
}

pub(crate) fn summary(inspection: &NativeStackInspection) -> String {
    match inspection.state.as_str() {
        "in_sync" => "GitHub native stack is in sync".to_string(),
        "diverged" => "GitHub native stack diverges from local stack".to_string(),
        "not_linked" => "GitHub native stack is not linked".to_string(),
        "unavailable" => "GitHub native stack preview is unavailable".to_string(),
        "unrepresentable" => format!(
            "Local stack cannot be represented as a GitHub native stack ({})",
            inspection.reason.as_deref().unwrap_or("unknown")
        ),
        "not_applicable" => "GitHub native stack does not apply to this branch".to_string(),
        "error" => format!(
            "GitHub native stack inspection failed: {}",
            inspection.error.as_deref().unwrap_or("unknown error")
        ),
        _ => format!("GitHub native stack state: {}", inspection.state),
    }
}

pub(crate) fn report_outcome(outcome: &github::NativeStackOutcome) {
    match outcome {
        github::NativeStackOutcome::NotNeeded => {}
        github::NativeStackOutcome::Created { number } => {
            ui::info(&format!("Linked PRs into GitHub native stack #{number}"));
        }
        github::NativeStackOutcome::Extended { number, added } => {
            ui::info(&format!(
                "Extended GitHub native stack #{number} with {added} PR(s)"
            ));
        }
        github::NativeStackOutcome::Unchanged { number } => {
            ui::info(&format!("GitHub native stack #{number} is up to date"));
        }
        github::NativeStackOutcome::Repaired {
            previous_number,
            number,
        } => {
            ui::info(&format!(
                "Repaired GitHub native stack #{previous_number} as #{number}"
            ));
        }
        github::NativeStackOutcome::Unavailable => {
            ui::info("GitHub native stacks unavailable; ordinary PR chain succeeded");
        }
        github::NativeStackOutcome::NotApplicable { reason } => {
            ui::info(&format!("GitHub native stacks not applicable: {reason}"));
        }
    }
}

pub(crate) fn receipt_value(
    command: &str,
    branches: &[String],
    pr_numbers: &[u64],
    outcome: &github::NativeStackOutcome,
) -> serde_json::Value {
    let mut value = match outcome {
        github::NativeStackOutcome::NotNeeded => serde_json::json!({
            "cmd": command,
            "native_stack_action": "not_needed",
        }),
        github::NativeStackOutcome::Created { number } => serde_json::json!({
            "cmd": command,
            "native_stack_action": "created",
            "native_stack_number": number,
        }),
        github::NativeStackOutcome::Extended { number, added } => serde_json::json!({
            "cmd": command,
            "native_stack_action": "extended",
            "native_stack_number": number,
            "native_stack_added": added,
        }),
        github::NativeStackOutcome::Unchanged { number } => serde_json::json!({
            "cmd": command,
            "native_stack_action": "unchanged",
            "native_stack_number": number,
        }),
        github::NativeStackOutcome::Repaired {
            previous_number,
            number,
        } => serde_json::json!({
            "cmd": command,
            "native_stack_action": "repaired",
            "native_stack_previous_number": previous_number,
            "native_stack_number": number,
        }),
        github::NativeStackOutcome::Unavailable => serde_json::json!({
            "cmd": command,
            "native_stack_action": "unavailable",
        }),
        github::NativeStackOutcome::NotApplicable { reason } => serde_json::json!({
            "cmd": command,
            "native_stack_action": "not_applicable",
            "native_stack_reason": reason,
        }),
    };
    if !branches.is_empty() {
        value["branches"] = serde_json::json!(branches);
    }
    if !pr_numbers.is_empty() {
        value["pull_requests"] = serde_json::json!(pr_numbers);
    }
    value
}

pub(crate) fn error_receipt_value(
    command: &str,
    branches: &[String],
    pr_numbers: &[u64],
    error: &str,
) -> serde_json::Value {
    serde_json::json!({
        "cmd": command,
        "branches": branches,
        "pull_requests": pr_numbers,
        "native_stack_action": "error",
        "native_stack_error": error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::StackState;
    use std::cell::RefCell;

    fn branch_with_pr(state: &mut StackState, name: &str, parent: &str, pr_number: Option<u64>) {
        state.add_branch(name, parent, "parent-head", None, None);
        state.get_branch_mut(name).expect("branch").pr_number = pr_number;
    }

    #[test]
    fn receipt_records_context_and_created_stack_number() {
        let branches = vec!["feat/a".to_string(), "feat/b".to_string()];
        let value = receipt_value(
            "sync",
            &branches,
            &[101, 102],
            &github::NativeStackOutcome::Created { number: 88 },
        );

        assert_eq!(value["cmd"], "sync");
        assert_eq!(value["branches"], serde_json::json!(["feat/a", "feat/b"]));
        assert_eq!(value["pull_requests"], serde_json::json!([101, 102]));
        assert_eq!(value["native_stack_action"], "created");
        assert_eq!(value["native_stack_number"], 88);
    }

    #[test]
    fn receipt_omits_empty_optional_context() {
        let value = receipt_value("submit", &[], &[], &github::NativeStackOutcome::Unavailable);

        assert_eq!(value["native_stack_action"], "unavailable");
        assert!(value.get("branches").is_none());
        assert!(value.get("pull_requests").is_none());
        assert!(value.get("native_stack_number").is_none());
    }

    #[test]
    fn receipt_records_not_needed_without_stack_number() {
        let value = receipt_value("submit", &[], &[], &github::NativeStackOutcome::NotNeeded);

        assert_eq!(value["cmd"], "submit");
        assert_eq!(value["native_stack_action"], "not_needed");
        assert!(value.get("native_stack_number").is_none());
    }

    #[test]
    fn error_receipt_preserves_desired_chain() {
        let value = error_receipt_value(
            "sync",
            &["feat/a".to_string(), "feat/b".to_string()],
            &[101, 102],
            "diverged",
        );

        assert_eq!(value["native_stack_action"], "error");
        assert_eq!(value["native_stack_error"], "diverged");
        assert_eq!(value["pull_requests"], serde_json::json!([101, 102]));
    }

    #[test]
    fn receipt_records_all_mutating_outcome_variants() {
        let unchanged = receipt_value(
            "sync",
            &["feat/a".to_string()],
            &[101],
            &github::NativeStackOutcome::Unchanged { number: 88 },
        );
        assert_eq!(unchanged["native_stack_action"], "unchanged");
        assert_eq!(unchanged["native_stack_number"], 88);

        let extended = receipt_value(
            "sync",
            &["feat/a".to_string(), "feat/b".to_string()],
            &[101, 102],
            &github::NativeStackOutcome::Extended {
                number: 88,
                added: 1,
            },
        );
        assert_eq!(extended["native_stack_action"], "extended");
        assert_eq!(extended["native_stack_number"], 88);
        assert_eq!(extended["native_stack_added"], 1);

        let repaired = receipt_value(
            "sync",
            &["feat/a".to_string(), "feat/b".to_string()],
            &[101, 102],
            &github::NativeStackOutcome::Repaired {
                previous_number: 88,
                number: 89,
            },
        );
        assert_eq!(repaired["native_stack_action"], "repaired");
        assert_eq!(repaired["native_stack_previous_number"], 88);
        assert_eq!(repaired["native_stack_number"], 89);

        let not_applicable = receipt_value(
            "submit",
            &["feat/fork".to_string()],
            &[201],
            &github::NativeStackOutcome::NotApplicable {
                reason: "fork workflow".to_string(),
            },
        );
        assert_eq!(not_applicable["native_stack_action"], "not_applicable");
        assert_eq!(not_applicable["native_stack_reason"], "fork workflow");
    }

    #[test]
    fn report_outcome_accepts_every_user_visible_variant() {
        for outcome in [
            github::NativeStackOutcome::NotNeeded,
            github::NativeStackOutcome::Created { number: 88 },
            github::NativeStackOutcome::Extended {
                number: 88,
                added: 2,
            },
            github::NativeStackOutcome::Unchanged { number: 88 },
            github::NativeStackOutcome::Repaired {
                previous_number: 88,
                number: 89,
            },
            github::NativeStackOutcome::Unavailable,
            github::NativeStackOutcome::NotApplicable {
                reason: "fork workflow".to_string(),
            },
        ] {
            report_outcome(&outcome);
        }
    }

    #[test]
    fn native_stack_plan_keeps_independent_linear_components_separate() {
        let mut state = StackState::new("main".to_string());
        branch_with_pr(&mut state, "feat/a", "main", Some(101));
        branch_with_pr(&mut state, "feat/b", "feat/a", Some(102));
        branch_with_pr(&mut state, "feat/x", "main", Some(201));
        branch_with_pr(&mut state, "feat/y", "feat/x", Some(202));

        let plan = native_stack_plan(&state);

        assert_eq!(
            plan.chains
                .iter()
                .map(|chain| chain.pr_numbers.clone())
                .collect::<Vec<_>>(),
            vec![vec![101, 102], vec![201, 202]]
        );
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn native_stack_plan_retains_single_pr_segments_for_inspection() {
        let mut state = StackState::new("main".to_string());
        branch_with_pr(&mut state, "feat/a", "main", Some(101));
        branch_with_pr(&mut state, "feat/local-only", "feat/a", None);
        branch_with_pr(&mut state, "feat/c", "feat/local-only", Some(103));
        branch_with_pr(&mut state, "feat/d", "feat/c", Some(104));

        let plan = native_stack_plan(&state);

        assert_eq!(plan.chains.len(), 2);
        assert_eq!(plan.chains[0].branches, vec!["feat/a"]);
        assert_eq!(plan.chains[0].pr_numbers, vec![101]);
        assert_eq!(plan.chains[1].branches, vec!["feat/c", "feat/d"]);
        assert_eq!(plan.chains[1].pr_numbers, vec![103, 104]);
        assert_eq!(
            linkable_chains(&plan)
                .into_iter()
                .map(|chain| chain.pr_numbers.clone())
                .collect::<Vec<_>>(),
            vec![vec![103, 104]]
        );
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn native_stack_plan_skips_branching_component_instead_of_flattening_it() {
        let mut state = StackState::new("main".to_string());
        branch_with_pr(&mut state, "feat/a", "main", Some(101));
        branch_with_pr(&mut state, "feat/b", "feat/a", Some(102));
        branch_with_pr(&mut state, "feat/c", "feat/a", Some(103));

        let plan = native_stack_plan(&state);

        assert!(plan.chains.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].root, "feat/a");
        assert_eq!(plan.skipped[0].reason, "branching_component");
        assert_eq!(plan.skipped[0].branches, vec!["feat/a", "feat/b", "feat/c"]);
        assert_eq!(plan.skipped[0].pr_numbers, vec![101, 102, 103]);
    }

    #[test]
    fn native_stack_plan_reports_orphans_and_cycles_as_unrepresentable() {
        let mut state = StackState::new("main".to_string());
        branch_with_pr(&mut state, "feat/orphan", "missing-parent", Some(301));
        branch_with_pr(&mut state, "feat/cycle-a", "feat/cycle-b", Some(401));
        branch_with_pr(&mut state, "feat/cycle-b", "feat/cycle-a", Some(402));

        let plan = native_stack_plan(&state);

        assert!(plan.chains.is_empty());
        assert_eq!(plan.skipped.len(), 2);
        assert_eq!(plan.skipped[0].reason, "invalid_parent_graph");
        assert_eq!(plan.skipped[1].reason, "invalid_parent_graph");
        assert_eq!(
            plan.skipped
                .iter()
                .flat_map(|component| component.branches.clone())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "feat/cycle-a".to_string(),
                "feat/cycle-b".to_string(),
                "feat/orphan".to_string(),
            ])
        );
    }

    #[test]
    fn inspect_branch_reports_all_terminal_states() {
        let mut state = StackState::new("main".to_string());
        branch_with_pr(&mut state, "feat/a", "main", Some(101));
        branch_with_pr(&mut state, "feat/b", "feat/a", Some(102));

        let found = inspect_branch_with_lookup(&state, "feat/b", |_| {
            Ok(github::NativeStackLookup::Found(github::NativeStackInfo {
                number: 88,
                base_ref: Some("main".to_string()),
                open: Some(true),
                pull_requests: vec![101, 102],
            }))
        });
        assert_eq!(found.state, "in_sync");
        assert_eq!(found.github.as_ref().expect("github").position, Some(2));

        let diverged = inspect_branch_with_lookup(&state, "feat/b", |_| {
            Ok(github::NativeStackLookup::Found(github::NativeStackInfo {
                number: 88,
                base_ref: None,
                open: None,
                pull_requests: vec![101, 999],
            }))
        });
        assert_eq!(diverged.state, "diverged");
        assert_eq!(diverged.github.as_ref().expect("github").position, None);

        let not_linked = inspect_branch_with_lookup(&state, "feat/b", |_| {
            Ok(github::NativeStackLookup::NotLinked)
        });
        assert_eq!(not_linked.state, "not_linked");

        let unavailable = inspect_branch_with_lookup(&state, "feat/b", |_| {
            Ok(github::NativeStackLookup::Unavailable)
        });
        assert_eq!(unavailable.state, "unavailable");

        let error =
            inspect_branch_with_lookup(&state, "feat/b", |_| Err(anyhow::anyhow!("auth failed")));
        assert_eq!(error.state, "error");
        assert_eq!(error.error.as_deref(), Some("auth failed"));

        assert_eq!(
            inspect_branch_with_lookup(&state, "main", |_| unreachable!()).state,
            "not_applicable"
        );
        branch_with_pr(&mut state, "feat/local", "feat/b", None);
        assert_eq!(
            inspect_branch_with_lookup(&state, "feat/local", |_| unreachable!()).state,
            "not_applicable"
        );
    }

    #[test]
    fn inspect_branch_reports_unrepresentable_for_skipped_component() {
        let mut state = StackState::new("main".to_string());
        branch_with_pr(&mut state, "feat/a", "main", Some(101));
        branch_with_pr(&mut state, "feat/b", "feat/a", Some(102));
        branch_with_pr(&mut state, "feat/c", "feat/a", Some(103));

        let inspection = inspect_branch_with_lookup(&state, "feat/b", |_| unreachable!());

        assert_eq!(inspection.state, "unrepresentable");
        assert_eq!(inspection.reason.as_deref(), Some("branching_component"));
        assert_eq!(inspection.local.pull_requests, vec![101, 102, 103]);
    }

    #[test]
    fn inspect_branch_in_fork_workflow_never_calls_native_stack_lookup() {
        let mut state = StackState::new("main".to_string());
        state.upstream_remote = Some("upstream".to_string());
        branch_with_pr(&mut state, "feat/a", "main", Some(101));
        branch_with_pr(&mut state, "feat/b", "feat/a", Some(102));
        branch_with_pr(&mut state, "feat/c", "feat/a", Some(103));
        branch_with_pr(&mut state, "feat/local", "main", None);

        let trunk = inspect_branch(&state, "main");
        assert_eq!(trunk.state, "not_applicable");
        assert_eq!(trunk.local.branches, vec!["main"]);
        assert!(trunk.local.pull_requests.is_empty());
        assert_eq!(trunk.reason, Some(fork_not_applicable_reason()));

        let branching = inspect_branch(&state, "feat/b");
        assert_eq!(branching.state, "not_applicable");
        assert_eq!(branching.local.branches, vec!["feat/a", "feat/b", "feat/c"]);
        assert_eq!(branching.local.pull_requests, vec![101, 102, 103]);
        assert_eq!(branching.reason, Some(fork_not_applicable_reason()));

        let local_only = inspect_branch(&state, "feat/local");
        assert_eq!(local_only.state, "not_applicable");
        assert_eq!(local_only.local.branches, vec!["feat/local"]);
        assert!(local_only.local.pull_requests.is_empty());
        assert_eq!(local_only.reason, Some(fork_not_applicable_reason()));
    }

    #[test]
    fn inspect_branch_in_fork_workflow_reports_linear_chain_context() {
        let mut state = StackState::new("main".to_string());
        state.remote = "fork".to_string();
        state.repo = Some("upstream/project".to_string());
        state.fork_repo = Some("fork/project".to_string());
        branch_with_pr(&mut state, "feat/a", "main", Some(101));
        branch_with_pr(&mut state, "feat/b", "feat/a", Some(102));

        let inspection = inspect_branch(&state, "feat/b");

        assert_eq!(inspection.state, "not_applicable");
        assert_eq!(inspection.local.branches, vec!["feat/a", "feat/b"]);
        assert_eq!(inspection.local.pull_requests, vec![101, 102]);
        assert_eq!(inspection.reason, Some(fork_not_applicable_reason()));
    }

    #[test]
    fn public_inspection_helpers_do_not_require_github_for_local_not_applicable_branches() {
        let mut state = StackState::new("main".to_string());
        branch_with_pr(&mut state, "feat/local", "main", None);

        let branch = inspect_branch(&state, "feat/local");
        assert_eq!(branch.state, "not_applicable");
        assert_eq!(branch.local.branches, vec!["feat/local"]);

        let all = inspect_all(&state);
        assert_eq!(all["feat/local"].state, "not_applicable");
        assert_eq!(all["feat/local"].local.branches, vec!["feat/local"]);
    }

    #[test]
    fn inspect_all_in_fork_workflow_classifies_each_local_shape_without_lookup() {
        let mut state = StackState::new("main".to_string());
        state.repo = Some("upstream/project".to_string());
        state.fork_repo = Some("fork/project".to_string());
        branch_with_pr(&mut state, "feat/a", "main", Some(101));
        branch_with_pr(&mut state, "feat/b", "feat/a", Some(102));
        branch_with_pr(&mut state, "feat/x", "main", Some(201));
        branch_with_pr(&mut state, "feat/y", "feat/x", Some(202));
        branch_with_pr(&mut state, "feat/z", "feat/x", Some(203));
        branch_with_pr(&mut state, "feat/local", "main", None);

        let inspections = inspect_all(&state);

        assert_eq!(inspections.len(), 6);
        assert_eq!(inspections["feat/a"].state, "not_applicable");
        assert_eq!(
            inspections["feat/a"].local.branches,
            vec!["feat/a", "feat/b"]
        );
        assert_eq!(inspections["feat/a"].local.pull_requests, vec![101, 102]);
        assert_eq!(inspections["feat/b"], inspections["feat/a"]);

        assert_eq!(inspections["feat/x"].state, "not_applicable");
        assert_eq!(
            inspections["feat/x"].reason,
            Some(fork_not_applicable_reason())
        );
        assert_eq!(
            inspections["feat/x"].local.branches,
            vec!["feat/x", "feat/y", "feat/z"]
        );
        assert_eq!(
            inspections["feat/x"].local.pull_requests,
            vec![201, 202, 203]
        );
        assert_eq!(inspections["feat/y"], inspections["feat/x"]);
        assert_eq!(inspections["feat/z"], inspections["feat/x"]);

        assert_eq!(inspections["feat/local"].state, "not_applicable");
        assert_eq!(inspections["feat/local"].local.branches, vec!["feat/local"]);
        assert!(inspections["feat/local"].local.pull_requests.is_empty());
    }

    #[test]
    fn inspect_all_queries_once_per_segment_and_sets_one_based_positions() {
        let mut state = StackState::new("main".to_string());
        branch_with_pr(&mut state, "feat/a", "main", Some(101));
        branch_with_pr(&mut state, "feat/b", "feat/a", Some(102));

        let calls = RefCell::new(Vec::new());
        let inspections = inspect_all_with_lookup(&state, |pr| {
            calls.borrow_mut().push(pr);
            Ok(github::NativeStackLookup::Found(github::NativeStackInfo {
                number: 88,
                base_ref: Some("main".to_string()),
                open: Some(true),
                pull_requests: vec![101, 102],
            }))
        });

        assert_eq!(calls.into_inner(), vec![102]);
        assert_eq!(
            inspections["feat/a"]
                .github
                .as_ref()
                .expect("github")
                .position,
            Some(1)
        );
        assert_eq!(
            inspections["feat/b"]
                .github
                .as_ref()
                .expect("github")
                .position,
            Some(2)
        );
    }

    #[test]
    fn inspect_all_reuses_lookup_error_for_every_branch_in_chain() {
        let mut state = StackState::new("main".to_string());
        branch_with_pr(&mut state, "feat/a", "main", Some(101));
        branch_with_pr(&mut state, "feat/b", "feat/a", Some(102));

        let calls = RefCell::new(Vec::new());
        let inspections = inspect_all_with_lookup(&state, |pr| {
            calls.borrow_mut().push(pr);
            Err(anyhow::anyhow!("stack endpoint down"))
        });

        assert_eq!(calls.into_inner(), vec![102]);
        assert_eq!(inspections["feat/a"].state, "error");
        assert_eq!(
            inspections["feat/a"].error.as_deref(),
            Some("stack endpoint down")
        );
        assert_eq!(inspections["feat/b"], inspections["feat/a"]);
    }

    #[test]
    fn inspect_all_marks_skipped_components_and_local_only_branches_without_lookup() {
        let mut state = StackState::new("main".to_string());
        branch_with_pr(&mut state, "feat/root", "main", Some(101));
        branch_with_pr(&mut state, "feat/left", "feat/root", Some(102));
        branch_with_pr(&mut state, "feat/right", "feat/root", Some(103));
        branch_with_pr(&mut state, "feat/local", "main", None);

        let inspections = inspect_all_with_lookup(&state, |_| unreachable!());

        assert_eq!(inspections["feat/root"].state, "unrepresentable");
        assert_eq!(
            inspections["feat/root"].reason.as_deref(),
            Some("branching_component")
        );
        assert_eq!(
            inspections["feat/root"].local.branches,
            vec!["feat/left", "feat/right", "feat/root"]
        );
        assert_eq!(
            inspections["feat/root"].local.pull_requests,
            vec![102, 103, 101]
        );
        assert_eq!(inspections["feat/left"], inspections["feat/root"]);
        assert_eq!(inspections["feat/right"], inspections["feat/root"]);

        assert_eq!(inspections["feat/local"].state, "not_applicable");
        assert_eq!(inspections["feat/local"].local.branches, vec!["feat/local"]);
        assert!(inspections["feat/local"].local.pull_requests.is_empty());
    }

    #[test]
    fn inspection_serializes_exact_shape_and_summary_is_compact() {
        let inspection = base_inspection(
            "in_sync",
            vec!["feat/a".to_string(), "feat/b".to_string()],
            vec![101, 102],
            Some(GitHubNativeStackInspection {
                number: 88,
                base_ref: Some("main".to_string()),
                open: Some(true),
                position: Some(1),
                size: 2,
                pull_requests: vec![101, 102],
            }),
            None,
            None,
        );

        assert_eq!(
            serde_json::to_value(&inspection).expect("json"),
            serde_json::json!({
                "provider": "github",
                "preview": true,
                "state": "in_sync",
                "local": {
                    "branches": ["feat/a", "feat/b"],
                    "pull_requests": [101, 102],
                },
                "github": {
                    "number": 88,
                    "base_ref": "main",
                    "open": true,
                    "position": 1,
                    "size": 2,
                    "pull_requests": [101, 102],
                },
            })
        );
        assert_eq!(summary(&inspection), "GitHub native stack is in sync");
    }

    #[test]
    fn summary_covers_terminal_and_unknown_states() {
        let diverged = base_inspection(
            "diverged",
            vec!["feat/a".to_string()],
            vec![101],
            None,
            None,
            None,
        );
        assert_eq!(
            summary(&diverged),
            "GitHub native stack diverges from local stack"
        );

        let not_linked = base_inspection(
            "not_linked",
            vec!["feat/a".to_string()],
            vec![101],
            None,
            None,
            None,
        );
        assert_eq!(summary(&not_linked), "GitHub native stack is not linked");

        let unavailable = base_inspection(
            "unavailable",
            vec!["feat/a".to_string()],
            vec![101],
            None,
            None,
            None,
        );
        assert_eq!(
            summary(&unavailable),
            "GitHub native stack preview is unavailable"
        );

        let unrepresentable = base_inspection(
            "unrepresentable",
            vec!["feat/a".to_string()],
            vec![101],
            None,
            Some("branching_component".to_string()),
            None,
        );
        assert_eq!(
            summary(&unrepresentable),
            "Local stack cannot be represented as a GitHub native stack (branching_component)"
        );

        let unknown_unrepresentable = base_inspection(
            "unrepresentable",
            vec!["feat/a".to_string()],
            vec![101],
            None,
            None,
            None,
        );
        assert_eq!(
            summary(&unknown_unrepresentable),
            "Local stack cannot be represented as a GitHub native stack (unknown)"
        );

        let not_applicable = base_inspection(
            "not_applicable",
            vec!["feat/a".to_string()],
            Vec::new(),
            None,
            None,
            None,
        );
        assert_eq!(
            summary(&not_applicable),
            "GitHub native stack does not apply to this branch"
        );

        let error = base_inspection(
            "error",
            vec!["feat/a".to_string()],
            vec![101],
            None,
            None,
            Some("auth failed".to_string()),
        );
        assert_eq!(
            summary(&error),
            "GitHub native stack inspection failed: auth failed"
        );

        let unknown_error = base_inspection(
            "error",
            vec!["feat/a".to_string()],
            vec![101],
            None,
            None,
            None,
        );
        assert_eq!(
            summary(&unknown_error),
            "GitHub native stack inspection failed: unknown error"
        );

        let future_state = base_inspection(
            "queued",
            vec!["feat/a".to_string()],
            vec![101],
            None,
            None,
            None,
        );
        assert_eq!(summary(&future_state), "GitHub native stack state: queued");
    }
}
