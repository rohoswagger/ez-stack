use anyhow::Result;
use std::collections::HashMap;

use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

/// Compute a deterministic port in range 10000-19999 from a branch name.
fn dev_port(branch: &str) -> u16 {
    let mut hash: u32 = 5381;
    for byte in branch.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(byte as u32);
    }
    10000 + (hash % 10000) as u16
}

fn last_activity(branch: &str) -> String {
    match git::log_oneline_time(branch) {
        Some(secs) if secs < 60 => format!("{}s", secs),
        Some(secs) if secs < 3600 => format!("{}m", secs / 60),
        Some(secs) if secs < 86400 => format!("{}h", secs / 3600),
        Some(secs) => format!("{}d", secs / 86400),
        None => "-".to_string(),
    }
}

fn row(
    marker: &str,
    branch: &str,
    pr: &str,
    ci: &str,
    age: &str,
    port: &str,
    status: &str,
) -> String {
    format!("{marker:<4} {branch:<30} {pr:<8} {ci:<6} {age:<6} {port:<7} {status}")
}

pub fn run(json: bool) -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    let worktree_map: HashMap<String, String> = git::worktree_list()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|wt| wt.branch.map(|b| (b, wt.path)))
        .collect();

    if json {
        return run_json(&state, &current, &worktree_map);
    }

    // Header.
    eprintln!("{}", row("", "BRANCH", "PR", "CI", "AGE", "PORT", "STATUS"));
    eprintln!("{}", "-".repeat(80));

    // Trunk.
    let m = if current == state.trunk { " *" } else { "  " };
    eprintln!(
        "{}",
        row(
            m,
            &state.trunk,
            "-",
            "-",
            &last_activity(&state.trunk),
            "-",
            "(trunk)"
        )
    );

    // Managed branches.
    for branch in &state.topo_order() {
        let meta = state.get_branch(branch)?;
        let m = if *branch == current { " *" } else { "  " };
        let pr = meta
            .pr_number
            .map(|n| format!("#{n}"))
            .unwrap_or_else(|| "-".into());
        let ci_raw = if meta.pr_number.is_some() {
            github::get_ci_status(branch)
        } else {
            String::new()
        };
        let ci = if ci_raw.is_empty() {
            "-".into()
        } else {
            ci_raw
        };
        let age = last_activity(branch);
        let has_wt = worktree_map.contains_key(branch.as_str());
        let port = if has_wt {
            format!("{}", dev_port(branch))
        } else {
            "-".into()
        };

        let status = if has_wt {
            let wt_path = worktree_map.get(branch.as_str()).unwrap();
            let (s, mo, u) = git::working_tree_status_at(wt_path);
            if s == 0 && mo == 0 && u == 0 {
                "clean".into()
            } else {
                let mut p = Vec::new();
                if s > 0 {
                    p.push(format!("{s}S"));
                }
                if mo > 0 {
                    p.push(format!("{mo}M"));
                }
                if u > 0 {
                    p.push(format!("{u}U"));
                }
                p.join(" ")
            }
        } else {
            "no worktree".into()
        };

        eprintln!("{}", row(m, branch, &pr, &ci, &age, &port, &status));
    }

    // Untracked current branch.
    if current != state.trunk && !state.is_managed(&current) {
        eprintln!(
            "{}",
            row(
                " *",
                &current,
                "-",
                "-",
                &last_activity(&current),
                "-",
                "not tracked"
            )
        );
        ui::hint("use `ez create` to track branches");
    }

    Ok(())
}

fn run_json(
    state: &StackState,
    current: &str,
    worktree_map: &HashMap<String, String>,
) -> Result<()> {
    let mut entries = Vec::new();

    entries.push(serde_json::json!({
        "branch": state.trunk,
        "is_trunk": true,
        "is_current": current == state.trunk,
    }));

    for branch in &state.topo_order() {
        let meta = state.get_branch(branch)?;
        let wt_path = worktree_map.get(branch.as_str());
        let has_wt = wt_path.is_some();

        let wt_status = wt_path.map(|p| {
            let (s, m, u) = git::working_tree_status_at(p);
            serde_json::json!({"staged": s, "modified": m, "untracked": u})
        });

        let ci = if meta.pr_number.is_some() {
            let s = github::get_ci_status(branch);
            if s.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(s)
            }
        } else {
            serde_json::Value::Null
        };

        entries.push(serde_json::json!({
            "branch": branch,
            "is_current": *branch == current,
            "parent": meta.parent,
            "pr_number": meta.pr_number,
            "ci_status": ci,
            "last_activity_secs": git::log_oneline_time(branch),
            "dev_port": if has_wt { Some(dev_port(branch)) } else { None },
            "worktree_path": wt_path,
            "working_tree": wt_status,
        }));
    }

    println!("{}", serde_json::to_string_pretty(&entries)?);
    Ok(())
}
