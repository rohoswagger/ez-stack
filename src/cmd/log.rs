use anyhow::Result;

use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run(json: bool) -> Result<()> {
    let state = StackState::load()?;

    if json {
        let order = state.topo_order();
        let repo = github::repo_name().ok().unwrap_or_default();

        let entries: Vec<serde_json::Value> = order
            .iter()
            .map(|branch| {
                let meta = state.branches.get(branch).unwrap();
                let children = state.children_of(branch);
                let depth = state.path_to_trunk(branch).len().saturating_sub(1);

                let (pr_number, pr_url, pr_state, is_draft) = match meta.pr_number {
                    Some(n) => {
                        let url = if repo.is_empty() {
                            serde_json::Value::Null
                        } else {
                            serde_json::Value::String(format!("https://github.com/{repo}/pull/{n}"))
                        };
                        let (state_str, draft) = github::get_pr_status(branch)
                            .ok()
                            .flatten()
                            .map(|pr| (pr.state, pr.is_draft))
                            .unwrap_or_else(|| ("OPEN".to_string(), false));
                        (
                            serde_json::Value::Number(n.into()),
                            url,
                            serde_json::Value::String(state_str),
                            draft,
                        )
                    }
                    None => (
                        serde_json::Value::Null,
                        serde_json::Value::Null,
                        serde_json::Value::Null,
                        false,
                    ),
                };

                serde_json::json!({
                    "branch": branch,
                    "parent": meta.parent,
                    "depth": depth,
                    "pr_number": pr_number,
                    "pr_url": pr_url,
                    "pr_state": pr_state,
                    "is_draft": is_draft,
                    "children": children,
                })
            })
            .collect();

        println!("{}", serde_json::json!(entries));
        return Ok(());
    }

    let current = git::current_branch()?;

    // Build map of branch → worktree_path for branches checked out in .worktrees/.
    // Called once here so render_tree doesn't make O(n) subprocess calls.
    let worktree_map: std::collections::HashMap<String, String> = git::worktree_list()
        .unwrap_or_default()
        .into_iter()
        .filter(|wt| wt.path.contains("/.worktrees/"))
        .filter_map(|wt| wt.branch.map(|b| (b, wt.path)))
        .collect();

    ui::header("Stack");

    // Print trunk as the root
    let trunk_display = ui::branch_display(&state.trunk, current == state.trunk);
    eprintln!("{trunk_display}");

    // Render children of trunk recursively
    let children = state.children_of(&state.trunk);
    let count = children.len();
    for (i, child) in children.iter().enumerate() {
        let is_last = i == count - 1;
        render_tree(&state, child, 1, is_last, &[], &current, &worktree_map)?;
    }

    Ok(())
}

fn render_tree(
    state: &StackState,
    branch: &str,
    depth: usize,
    is_last: bool,
    ancestors_last: &[bool],
    current: &str,
    worktree_map: &std::collections::HashMap<String, String>,
) -> Result<()> {
    let is_current = branch == current;
    let meta = state.get_branch(branch)?;

    // Build the display text for this branch
    let name_display = ui::branch_display(branch, is_current);

    // Worktree indicator — shown when branch is checked out in another worktree.
    let worktree_text = if let Some(wt_path) = worktree_map.get(branch) {
        let label = std::path::Path::new(wt_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(wt_path.as_str());
        format!(" {}", ui::dim(&format!("[wt: {label}]")))
    } else {
        String::new()
    };

    // Get PR badge if available
    let pr_text = if let Some(pr_number) = meta.pr_number {
        if let Ok(Some(pr)) = github::get_pr_status(branch) {
            let badge = ui::pr_badge(pr.number, &pr.state, pr.is_draft);
            let state_label = if pr.is_draft {
                "draft".to_string()
            } else {
                pr.state.clone()
            };
            format!(" ({badge} {state_label})")
        } else {
            format!(" ({})", ui::pr_badge(pr_number, "OPEN", false))
        }
    } else {
        String::new()
    };

    // Get CI status (best-effort, empty string if unavailable).
    let ci_text = if meta.pr_number.is_some() {
        let ci = github::get_ci_status(branch);
        if ci.is_empty() {
            String::new()
        } else {
            format!(" {ci}")
        }
    } else {
        String::new()
    };

    // Count commits on this branch
    let range = format!("{}..{}", meta.parent, branch);
    let commits = git::log_oneline(&range, 100).unwrap_or_default();
    let commit_count = commits.len();
    let commit_text = if commit_count == 1 {
        ui::dim(" 1 commit")
    } else {
        ui::dim(&format!(" {commit_count} commits"))
    };

    // Current branch indicator
    let current_marker = if is_current {
        format!("     {}", ui::dim("← current"))
    } else {
        String::new()
    };

    let line_text =
        format!("{name_display}{worktree_text}{pr_text}{ci_text}{commit_text}{current_marker}");
    let line = ui::tree_line(depth, is_last, ancestors_last, &line_text);
    eprintln!("{line}");

    // Recurse into children
    let children = state.children_of(branch);
    let child_count = children.len();
    let mut child_ancestors = ancestors_last.to_vec();
    child_ancestors.push(is_last);
    for (i, child) in children.iter().enumerate() {
        let child_is_last = i == child_count - 1;
        render_tree(
            state,
            child,
            depth + 1,
            child_is_last,
            &child_ancestors,
            current,
            worktree_map,
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::stack::{BranchMeta, StackState};
    use std::collections::HashMap;

    fn make_state() -> StackState {
        let mut branches = HashMap::new();
        branches.insert(
            "feat/a".to_string(),
            BranchMeta {
                name: "feat/a".to_string(),
                parent: "main".to_string(),
                parent_head: "abc".to_string(),
                pr_number: Some(1),
            },
        );
        branches.insert(
            "feat/b".to_string(),
            BranchMeta {
                name: "feat/b".to_string(),
                parent: "feat/a".to_string(),
                parent_head: "def".to_string(),
                pr_number: None,
            },
        );
        StackState {
            trunk: "main".to_string(),
            remote: "origin".to_string(),
            branches,
        }
    }

    #[test]
    fn test_log_topo_order() {
        let state = make_state();
        let order = state.topo_order();
        let idx_a = order.iter().position(|s| s == "feat/a").unwrap();
        let idx_b = order.iter().position(|s| s == "feat/b").unwrap();
        assert!(
            idx_a < idx_b,
            "feat/a (parent) must come before feat/b (child)"
        );
    }

    #[test]
    fn test_log_children_of() {
        let state = make_state();
        assert_eq!(state.children_of("feat/a"), vec!["feat/b"]);
        assert!(state.children_of("feat/b").is_empty());
    }

    #[test]
    fn test_worktree_map_only_includes_dot_worktrees() {
        use std::collections::HashMap;

        let mock_worktrees: Vec<(String, Option<String>)> = vec![
            ("/repo".to_string(), Some("main".to_string())),
            (
                "/repo/.worktrees/feat-x".to_string(),
                Some("feat/x".to_string()),
            ),
            ("/somewhere/else".to_string(), Some("stray".to_string())),
            ("/repo/.worktrees/detached".to_string(), None),
        ];

        let map: HashMap<String, String> = mock_worktrees
            .into_iter()
            .filter(|(path, _)| path.contains("/.worktrees/"))
            .filter_map(|(path, branch)| branch.map(|b| (b, path)))
            .collect();

        assert!(
            !map.contains_key("main"),
            "main worktree must not appear in map"
        );
        assert!(
            !map.contains_key("stray"),
            "worktrees outside .worktrees/ must not appear"
        );
        assert_eq!(
            map.get("feat/x").map(String::as_str),
            Some("/repo/.worktrees/feat-x")
        );
        assert_eq!(map.len(), 1, "only the .worktrees/ branch should be in map");
    }
}
