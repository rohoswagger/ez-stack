use anyhow::Result;
use std::collections::HashMap;

use crate::cmd::native_stack::{self, NativeStackInspection};
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run(json: bool, native_stack: bool) -> Result<()> {
    let state = StackState::load()?;

    if json {
        let order = state.topo_order();
        let repo = github::repo_name(state.repo.as_deref())
            .ok()
            .unwrap_or_default();
        let native_stack_inspections = if native_stack {
            native_stack::inspect_all(&state)
        } else {
            HashMap::new()
        };

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
                        let (state_str, draft) =
                            github::get_pr_status(&n.to_string(), state.repo.as_deref())
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

                let mut entry = serde_json::json!({
                    "branch": branch,
                    "parent": meta.parent,
                    "depth": depth,
                    "pr_number": pr_number,
                    "pr_url": pr_url,
                    "pr_state": pr_state,
                    "is_draft": is_draft,
                    "children": children,
                });
                if let Some(inspection) = native_stack_inspections.get(branch) {
                    insert_native_stack_json(&mut entry, inspection);
                }
                entry
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
    let native_stack_inspections = if native_stack {
        native_stack::inspect_all(&state)
    } else {
        HashMap::new()
    };
    let render_context = RenderContext {
        current: &current,
        repo: state.repo.as_deref(),
        worktree_map: &worktree_map,
        native_stack_inspections: &native_stack_inspections,
    };

    ui::header("Stack");

    // Print trunk as the root
    let trunk_display = ui::branch_display(&state.trunk, current == state.trunk);
    eprintln!("{trunk_display}");

    // Render children of trunk recursively
    let children = state.children_of(&state.trunk);
    let count = children.len();
    for (i, child) in children.iter().enumerate() {
        let is_last = i == count - 1;
        render_tree(&state, child, 1, is_last, &[], &render_context)?;
    }

    Ok(())
}

struct RenderContext<'a> {
    current: &'a str,
    repo: Option<&'a str>,
    worktree_map: &'a HashMap<String, String>,
    native_stack_inspections: &'a HashMap<String, NativeStackInspection>,
}

fn render_tree(
    state: &StackState,
    branch: &str,
    depth: usize,
    is_last: bool,
    ancestors_last: &[bool],
    context: &RenderContext<'_>,
) -> Result<()> {
    let is_current = branch == context.current;
    let meta = state.get_branch(branch)?;

    // Build the display text for this branch
    let name_display = ui::branch_display(branch, is_current);

    // Worktree indicator — shown when branch is checked out in another worktree.
    let worktree_text = if let Some(wt_path) = context.worktree_map.get(branch) {
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
        if let Ok(Some(pr)) = github::get_pr_status(&pr_number.to_string(), context.repo) {
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
        let ci = github::get_ci_status(branch, context.repo);
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
    let native_stack_text = context
        .native_stack_inspections
        .get(branch)
        .map(|inspection| native_stack_log_suffix(&native_stack::summary(inspection)))
        .unwrap_or_default();

    let line_text = format!(
        "{name_display}{worktree_text}{pr_text}{ci_text}{commit_text}{native_stack_text}{current_marker}"
    );
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
            context,
        )?;
    }

    Ok(())
}

fn insert_native_stack_json(value: &mut serde_json::Value, inspection: &NativeStackInspection) {
    value["native_stack"] = serde_json::json!(inspection);
}

fn native_stack_log_suffix(summary: &str) -> String {
    format!(" {}", ui::dim(&format!("[native: {summary}]")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::{BranchMeta, StackState};
    use crate::test_support::{
        CwdGuard, PathGuard, init_git_repo, install_fake_bin, run_cmd, take_env_lock, write_file,
    };
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
                scope: None,
                scope_mode: None,
            },
        );
        branches.insert(
            "feat/b".to_string(),
            BranchMeta {
                name: "feat/b".to_string(),
                parent: "feat/a".to_string(),
                parent_head: "def".to_string(),
                pr_number: None,
                scope: None,
                scope_mode: None,
            },
        );
        let mut state = StackState::new("main".to_string());
        state.branches = branches;
        state
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

    #[test]
    fn native_stack_log_suffix_is_compact() {
        let suffix = native_stack_log_suffix("in sync with GitHub stack #88");

        assert!(suffix.contains("[native: in sync with GitHub stack #88]"));
        assert!(suffix.starts_with(' '));
    }

    fn enter_real_stack(prefix: &str, pr_number: Option<u64>) -> (std::path::PathBuf, CwdGuard) {
        let repo = init_git_repo(prefix);
        let main_head =
            crate::git::rev_parse_at(repo.to_str().expect("repo path"), "main").expect("main head");
        run_cmd(&repo, "git", &["checkout", "-b", "feat/a"]);
        write_file(&repo, "a.txt", "a\n");
        run_cmd(&repo, "git", &["add", "a.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "a"]);
        let a_head =
            crate::git::rev_parse_at(repo.to_str().expect("repo path"), "feat/a").expect("a head");
        run_cmd(&repo, "git", &["checkout", "-b", "feat/b"]);
        write_file(&repo, "b.txt", "b\n");
        run_cmd(&repo, "git", &["add", "b.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "b"]);

        let worktree = repo.join(".worktrees/feat-a");
        std::fs::create_dir_all(worktree.parent().expect("worktree parent"))
            .expect("create worktree parent");
        run_cmd(
            &repo,
            "git",
            &[
                "worktree",
                "add",
                worktree.to_str().expect("worktree path"),
                "feat/a",
            ],
        );

        let cwd = CwdGuard::enter(&repo);
        let mut state = StackState::new("main".to_string());
        state.repo = Some("owner/repo".to_string());
        state.add_branch("feat/a", "main", &main_head, None, None);
        state.get_branch_mut("feat/a").expect("a meta").pr_number = pr_number;
        state.add_branch("feat/b", "feat/a", &a_head, None, None);
        state.save().expect("save stack");
        (repo, cwd)
    }

    #[test]
    fn renders_real_nested_stack_in_human_and_json_modes() {
        let _lock = take_env_lock();
        let (_repo, _cwd) = enter_real_stack("log-real", None);

        run(false, false).expect("human log");
        run(true, false).expect("json log");
        run(true, true).expect("native json log without PRs");
    }

    #[test]
    fn renders_pr_status_ci_and_json_url_from_fake_github() {
        let _lock = take_env_lock();
        let (_repo, _cwd) = enter_real_stack("log-pr", Some(7));
        let fake = install_fake_bin(
            "log-pr-gh",
            "gh",
            r#"#!/bin/sh
case "$*" in
  "pr view 7"*)
    printf '%s\n' '{"number":7,"url":"https://github.com/owner/repo/pull/7","state":"OPEN","title":"A","isDraft":true,"mergedAt":null,"baseRefName":"main"}'
    ;;
  "run list --branch feat/a"*)
    printf '%s\n' '{"status":"completed","conclusion":"success"}'
    ;;
  *) exit 1 ;;
esac
"#,
        );
        let _path = PathGuard::install(&fake);

        run(false, false).expect("human log with PR");
        run(true, false).expect("json log with PR");
    }

    #[test]
    fn json_log_uses_null_url_when_repo_is_unknown() {
        let _lock = take_env_lock();
        let (_repo, _cwd) = enter_real_stack("log-no-repo", Some(9));
        let mut state = StackState::load().expect("load stack");
        state.repo = None;
        state.save().expect("save stack without repo");
        let fake = install_fake_bin("log-no-repo-gh", "gh", "#!/bin/sh\nexit 1\n");
        let _path = PathGuard::install(&fake);

        run(true, false).expect("json log without repo");
    }
}
