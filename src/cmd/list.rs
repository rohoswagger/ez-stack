use anyhow::Result;
use std::collections::HashMap;
use std::thread;

use crate::dev;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;
use crate::worktree_lease::{Lease, now_unix};

fn format_age(secs: Option<u64>) -> String {
    match secs {
        Some(s) if s < 60 => format!("{}s", s),
        Some(s) if s < 3600 => format!("{}m", s / 60),
        Some(s) if s < 86400 => format!("{}h", s / 3600),
        Some(s) => format!("{}d", s / 86400),
        None => "-".to_string(),
    }
}

fn row(m: &str, b: &str, pr: &str, ci: &str, age: &str, port: &str, st: &str) -> String {
    format!("{m:<4} {b:<30} {pr:<8} {ci:<6} {age:<6} {port:<7} {st}")
}

fn combined_branch_order(
    trunk: &str,
    managed_order: &[String],
    local_branches: &[String],
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut order = Vec::new();

    for branch in managed_order {
        if branch != trunk && seen.insert(branch.clone()) {
            order.push(branch.clone());
        }
    }

    for branch in local_branches {
        if branch != trunk && seen.insert(branch.clone()) {
            order.push(branch.clone());
        }
    }

    order
}

fn branch_status_label(
    is_managed: bool,
    has_worktree: bool,
    wt_status: (usize, usize, usize),
) -> String {
    let (staged, modified, untracked) = wt_status;
    let base = if has_worktree {
        if staged == 0 && modified == 0 && untracked == 0 {
            "clean".to_string()
        } else {
            let mut parts = Vec::new();
            if staged > 0 {
                parts.push(format!("{staged}S"));
            }
            if modified > 0 {
                parts.push(format!("{modified}M"));
            }
            if untracked > 0 {
                parts.push(format!("{untracked}U"));
            }
            parts.join(" ")
        }
    } else if is_managed {
        "no worktree".to_string()
    } else {
        "not tracked".to_string()
    };

    if !is_managed && has_worktree {
        format!("{base}; not tracked")
    } else {
        base
    }
}

/// Fetched data for one branch.
struct BranchData {
    name: String,
    is_managed: bool,
    pr_number: Option<u64>,
    parent: Option<String>,
    wt_path: Option<String>,
    wt_lock_reason: Option<String>,
    ci: String,
    age: Option<u64>,
    wt_status: (usize, usize, usize),
}

pub fn run(json: bool) -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    let worktree_map: HashMap<String, git::WorktreeInfo> = git::worktree_list()?
        .into_iter()
        .filter_map(|worktree| worktree.branch.clone().map(|branch| (branch, worktree)))
        .collect();

    let local_branches = git::branch_list()?;
    let order = combined_branch_order(&state.trunk, &state.topo_order(), &local_branches);

    // Collect what we need per branch, then fetch everything in parallel.
    #[allow(clippy::type_complexity)]
    let branch_specs: Vec<(
        String,
        bool,
        Option<u64>,
        Option<String>,
        Option<(String, Option<String>)>,
    )> = order
        .iter()
        .map(|b| {
            let meta = state.get_branch(b).ok();
            let worktree = worktree_map
                .get(b.as_str())
                .map(|worktree| (worktree.path.clone(), worktree.locked_reason.clone()));
            (
                b.clone(),
                meta.is_some(),
                meta.and_then(|m| m.pr_number),
                meta.map(|m| m.parent.clone()),
                worktree,
            )
        })
        .collect();

    // One API call for all PR statuses (instead of scanning every PR in the repo).
    let has_any_branches = !branch_specs.is_empty();
    let branch_names_for_prs: Vec<String> =
        branch_specs.iter().map(|(name, ..)| name.clone()).collect();
    let remote_for_prs = state.fetch_remote().to_string();
    let repo_for_prs = state.repo.clone();
    let pr_handle = thread::spawn(move || {
        if has_any_branches {
            let refs: Vec<&str> = branch_names_for_prs.iter().map(String::as_str).collect();
            github::get_pr_statuses_for(&remote_for_prs, repo_for_prs.as_deref(), &refs)
        } else {
            HashMap::new()
        }
    });
    let repo_for_ci = state.repo.clone();
    let ci_handle = thread::spawn(move || {
        if has_any_branches {
            github::get_all_ci_statuses(repo_for_ci.as_deref())
        } else {
            HashMap::new()
        }
    });

    // Parallel git calls: age + working tree status per branch.
    let git_handles: Vec<_> = branch_specs
        .iter()
        .map(|(name, _is_managed, _pr_num, _parent, worktree)| {
            let name = name.clone();
            let wt = worktree.as_ref().map(|(path, _)| path.clone());
            thread::spawn(move || {
                let age = git::log_oneline_time(&name);
                let wt_status = wt
                    .as_ref()
                    .map(|p| git::working_tree_status_at(p))
                    .unwrap_or((0, 0, 0));
                (age, wt_status)
            })
        })
        .collect();

    // Trunk age (runs in parallel with the above).
    let trunk_age = format_age(git::log_oneline_time(&state.trunk));

    // Collect results.
    let pr_map = pr_handle.join().unwrap_or_default();
    let ci_map = ci_handle.join().unwrap_or_default();
    let git_results: Vec<(Option<u64>, (usize, usize, usize))> = git_handles
        .into_iter()
        .map(|h| h.join().unwrap_or((None, (0, 0, 0))))
        .collect();

    // Merge into final results.
    #[allow(clippy::type_complexity)]
    let results: Vec<(String, Option<u64>, (usize, usize, usize))> = branch_specs
        .iter()
        .enumerate()
        .map(|(i, (name, _, _, _, _))| {
            let ci = ci_map.get(name.as_str()).cloned().unwrap_or_default();
            let (age, wt_status) = git_results[i];
            (ci, age, wt_status)
        })
        .collect();

    let branch_data: Vec<BranchData> = branch_specs
        .into_iter()
        .zip(results)
        .map(
            |((name, is_managed, stored_pr_number, parent, worktree), (ci, age, wt_status))| {
                let pr_number = pr_map.get(&name).map(|pr| pr.number).or(stored_pr_number);
                let (wt_path, wt_lock_reason) = worktree
                    .map(|(path, reason)| (Some(path), reason))
                    .unwrap_or((None, None));
                BranchData {
                    name,
                    is_managed,
                    pr_number,
                    parent,
                    wt_path,
                    wt_lock_reason,
                    ci,
                    age,
                    wt_status,
                }
            },
        )
        .collect();

    if json {
        return run_json(&state, &current, &branch_data);
    }

    // Render table.
    eprintln!("{}", row("", "BRANCH", "PR", "CI", "AGE", "PORT", "STATUS"));
    eprintln!("{}", "-".repeat(80));

    let m = if current == state.trunk { " *" } else { "  " };
    let trunk_label = format!("{} (trunk)", state.trunk);
    eprintln!("{}", row(m, &trunk_label, "-", "-", &trunk_age, "-", "-"));

    for b in &branch_data {
        let m = if b.name == current { " *" } else { "  " };
        let pr = b.pr_number.map(|n| format!("#{n}")).unwrap_or("-".into());
        let ci = if b.ci.is_empty() { "-" } else { &b.ci };
        let age = format_age(b.age);
        let has_wt = b.wt_path.is_some();
        let port = if has_wt {
            format!("{}", dev::dev_port(&b.name))
        } else {
            "-".into()
        };
        let mut status = branch_status_label(b.is_managed, has_wt, b.wt_status);
        if let Some(reason) = b.wt_lock_reason.as_deref() {
            status.push_str("; ");
            status.push_str(&worktree_lock_label(&b.name, reason));
        }

        eprintln!("{}", row(m, &b.name, &pr, ci, &age, &port, &status));
    }

    if branch_data.iter().any(|b| !b.is_managed) {
        ui::hint("untracked local branches are shown with status `not tracked`");
    }

    Ok(())
}

fn run_json(state: &StackState, current: &str, branches: &[BranchData]) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&json_entries(state, current, branches))?
    );
    Ok(())
}

fn json_entries(
    state: &StackState,
    current: &str,
    branches: &[BranchData],
) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();

    entries.push(serde_json::json!({
        "branch": state.trunk,
        "is_trunk": true,
        "is_current": current == state.trunk,
    }));

    for b in branches {
        let has_wt = b.wt_path.is_some();
        let (s, m, u) = b.wt_status;
        let wt_status = if has_wt {
            Some(serde_json::json!({"staged": s, "modified": m, "untracked": u}))
        } else {
            None
        };
        let ci = if b.ci.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(b.ci.clone())
        };

        entries.push(serde_json::json!({
            "branch": b.name,
            "is_managed": b.is_managed,
            "is_current": b.name == current,
            "parent": b.parent,
            "pr_number": b.pr_number,
            "ci_status": ci,
            "last_activity_secs": b.age,
            "dev_port": if has_wt { Some(dev::dev_port(&b.name)) } else { None },
            "worktree_path": b.wt_path,
            "working_tree": wt_status,
            "worktree_lock": b.wt_lock_reason.as_deref().map(|reason| {
                worktree_lock_json(&b.name, reason)
            }),
        }));
    }

    entries
}

fn worktree_lock_label(branch: &str, reason: &str) -> String {
    match Lease::parse_reason(reason).filter(|lease| lease.branch == branch) {
        Some(lease) if lease.is_stale().unwrap_or(false) => {
            format!("stale lease: {}", lease.owner)
        }
        Some(lease) => format!("claimed: {}", lease.owner),
        None if reason.is_empty() => "locked".to_string(),
        None => format!("locked: {reason}"),
    }
}

fn worktree_lock_json(branch: &str, reason: &str) -> serde_json::Value {
    if let Some(lease) = Lease::parse_reason(reason).filter(|lease| lease.branch == branch) {
        let view = lease.view(now_unix().unwrap_or(0));
        serde_json::json!({
            "kind": "lease",
            "owner": view.owner,
            "branch": view.branch,
            "created_at": view.created_at,
            "expires_at": view.expires_at,
            "stale": view.stale,
        })
    } else {
        serde_json::json!({
            "kind": "foreign",
            "reason": if reason.is_empty() { "<no reason>" } else { reason },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::StackState;

    #[test]
    fn combined_branch_order_appends_unmanaged_locals_once() {
        let managed = vec!["feat/a".to_string(), "feat/b".to_string()];
        let local = vec![
            "main".to_string(),
            "feat/b".to_string(),
            "scratch".to_string(),
            "hotfix".to_string(),
        ];

        assert_eq!(
            combined_branch_order("main", &managed, &local),
            vec![
                "feat/a".to_string(),
                "feat/b".to_string(),
                "scratch".to_string(),
                "hotfix".to_string()
            ]
        );
    }

    #[test]
    fn branch_status_label_handles_managed_and_unmanaged_variants() {
        assert_eq!(branch_status_label(true, false, (0, 0, 0)), "no worktree");
        assert_eq!(branch_status_label(false, false, (0, 0, 0)), "not tracked");
        assert_eq!(branch_status_label(true, true, (0, 0, 0)), "clean");
        assert_eq!(
            branch_status_label(false, true, (1, 2, 3)),
            "1S 2M 3U; not tracked"
        );
    }

    #[test]
    fn format_age_handles_boundaries() {
        assert_eq!(format_age(None), "-");
        assert_eq!(format_age(Some(59)), "59s");
        assert_eq!(format_age(Some(60)), "1m");
        assert_eq!(format_age(Some(3_599)), "59m");
        assert_eq!(format_age(Some(3_600)), "1h");
        assert_eq!(format_age(Some(86_400)), "1d");
    }

    #[test]
    fn json_entries_include_unmanaged_branch_without_worktree_fields() {
        let state = StackState::new("main".to_string());
        let entries = json_entries(
            &state,
            "scratch",
            &[BranchData {
                name: "scratch".to_string(),
                is_managed: false,
                pr_number: None,
                parent: None,
                wt_path: None,
                wt_lock_reason: None,
                ci: String::new(),
                age: None,
                wt_status: (0, 0, 0),
            }],
        );

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["branch"], "main");
        assert_eq!(entries[1]["branch"], "scratch");
        assert_eq!(entries[1]["is_managed"], false);
        assert!(entries[1]["ci_status"].is_null());
        assert!(entries[1]["dev_port"].is_null());
        assert!(entries[1]["worktree_path"].is_null());
        assert!(entries[1]["working_tree"].is_null());
        assert!(entries[1]["worktree_lock"].is_null());
    }
}
