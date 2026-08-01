use anyhow::{Result, bail};
use dialoguer::Select;
use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::path::Path;

use crate::cmd::checkout::{switch_to, worktree_map};
use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

fn worktree_picker_suffix(wt_map: &HashMap<String, String>, name: &str) -> String {
    wt_map
        .get(name)
        .filter(|p| p.contains("/.worktrees/"))
        .map(|path| {
            let label = Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path.as_str());
            format!(" {}", ui::dim(&format!("[wt: {label}]")))
        })
        .unwrap_or_default()
}

fn child_picker_labels(
    state: &StackState,
    children: &[String],
    wt_map: &HashMap<String, String>,
) -> Vec<String> {
    children
        .iter()
        .map(|name| {
            let branch_text = ui::branch_display(name, false);
            let wt = worktree_picker_suffix(wt_map, name);
            if let Some(meta) = state.branches.get(name)
                && let Some(n) = meta.pr_number
            {
                format!("{} {}{}", branch_text, ui::pr_badge(n, "OPEN", false), wt)
            } else {
                format!("{branch_text}{wt}")
            }
        })
        .collect()
}

fn resolve_explicit_child(
    state: &StackState,
    current: &str,
    arg: &str,
    children: &[String],
) -> Result<String> {
    if let Ok(pr_num) = arg.parse::<u64>() {
        let found = state
            .branches
            .values()
            .find(|m| m.pr_number == Some(pr_num) && children.contains(&m.name))
            .map(|m| m.name.clone());

        return found.ok_or_else(|| {
            let listed = children.join(", ");
            EzError::UserMessage(format!(
                "no child of `{current}` has PR #{pr_num}\n  → Child branches: {listed}\n  → Run `ez up <branch>` or `ez up <pr-number>` for one of these"
            ))
            .into()
        });
    }

    if children.iter().any(|c| c == arg) {
        return Ok(arg.to_string());
    }

    let listed = children.join(", ");
    bail!(EzError::UserMessage(format!(
        "`{arg}` is not a child branch of `{current}`\n  → Child branches: {listed}\n  → Run `ez up <branch>`"
    )));
}

fn pick_upstream_child(
    state: &StackState,
    current: &str,
    children: &[String],
    explicit: Option<&str>,
    wt_map: &HashMap<String, String>,
) -> Result<String> {
    if children.is_empty() {
        bail!(EzError::AlreadyAtTop);
    }

    if let Some(arg) = explicit {
        return resolve_explicit_child(state, current, arg, children);
    }

    if children.len() == 1 {
        return Ok(children[0].clone());
    }

    let stdin_tty = io::stdin().is_terminal();
    let stderr_tty = io::stderr().is_terminal();
    if stdin_tty && stderr_tty {
        let labels = child_picker_labels(state, children, wt_map);
        let selection = Select::new()
            .with_prompt(format!(
                "Multiple branches stack on `{}` — move up to",
                current
            ))
            .items(&labels)
            .default(0)
            .interact()?;
        return Ok(children[selection].clone());
    }

    let listed = children.join(", ");
    bail!(EzError::UserMessage(format!(
        "multiple child branches stack on `{current}`: {listed}\n  → Run `ez up <branch>` or `ez up <pr-number>` to choose one (no TTY for interactive pick)"
    )));
}

fn down_target(state: &StackState, current: &str) -> Result<String> {
    if state.is_trunk(current) {
        bail!(EzError::AlreadyAtBottom);
    }
    if !state.is_managed(current) {
        bail!(EzError::BranchNotInStack(current.to_string()));
    }
    Ok(state.get_branch(current)?.parent.clone())
}

fn top_target(state: &StackState, current: &str) -> Result<String> {
    let target = state.stack_top(current);
    if target == current {
        bail!(EzError::AlreadyAtTop);
    }
    Ok(target)
}

fn bottom_target(state: &StackState, current: &str) -> Result<String> {
    if state.is_trunk(current) {
        let children = state.children_of(current);
        if children.is_empty() {
            bail!(EzError::AlreadyAtBottom);
        }
        return pick_upstream_child(state, current, &children, None, &HashMap::new());
    }

    let bottom = state.stack_bottom(current);
    if bottom == current {
        bail!(EzError::AlreadyAtBottom);
    }
    Ok(bottom)
}

pub fn up(explicit_child: Option<&str>) -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;
    let wt_map = worktree_map();

    let children = state.children_of(&current);
    let target = pick_upstream_child(&state, &current, &children, explicit_child, &wt_map)?;
    switch_to(&state, &target, &wt_map)?;
    ui::success(&format!(
        "Moved up: {} → {}",
        ui::branch_display(&current, false),
        ui::branch_display(&target, true),
    ));

    Ok(())
}

pub fn down(explicit_parent: Option<&str>) -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    let parent = down_target(&state, &current)?;
    if let Some(exp) = explicit_parent {
        if exp != parent {
            bail!(EzError::UserMessage(format!(
                "`{exp}` is not the stack parent of `{current}` (expected `{parent}`)\n  → Run `ez down` or `ez down {parent}`"
            )));
        }
    }

    let wt_map = worktree_map();
    switch_to(&state, &parent, &wt_map)?;
    ui::success(&format!(
        "Moved down: {} → {}",
        ui::branch_display(&current, false),
        ui::branch_display(&parent, true),
    ));

    Ok(())
}

pub fn top() -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    let target = top_target(&state, &current)?;
    let wt_map = worktree_map();
    switch_to(&state, &target, &wt_map)?;
    ui::success(&format!(
        "Jumped to top: {} → {}",
        ui::branch_display(&current, false),
        ui::branch_display(&target, true),
    ));

    Ok(())
}

pub fn bottom() -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    let target = bottom_target(&state, &current)?;
    let wt_map = worktree_map();
    switch_to(&state, &target, &wt_map)?;
    ui::success(&format!(
        "Jumped to bottom: {} → {}",
        ui::branch_display(&current, false),
        ui::branch_display(&target, true),
    ));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> StackState {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.add_branch("feat/b", "feat/a", "bbb", None, None);
        state.add_branch("feat/c", "feat/b", "ccc", None, None);
        state
    }

    fn fork_state() -> StackState {
        let mut state = StackState::new("main".to_string());
        state.add_branch("line-a", "main", "aaa", None, None);
        state.add_branch("line-b", "main", "bbb", None, None);
        state.branches.get_mut("line-a").unwrap().pr_number = Some(10);
        state.branches.get_mut("line-b").unwrap().pr_number = Some(20);
        state
    }

    #[test]
    fn pick_upstream_errors_without_children() {
        let state = sample_state();
        let err = pick_upstream_child(&state, "feat/c", &[], None, &HashMap::new())
            .expect_err("expected no children");
        assert!(err.to_string().contains("already at the top"));
    }

    #[test]
    fn pick_upstream_single_child_without_explicit() {
        let state = sample_state();
        let children = state.children_of("feat/b");
        let got = pick_upstream_child(&state, "feat/b", &children, None, &HashMap::new())
            .expect("one child");
        assert_eq!(got, "feat/c");
    }

    #[test]
    fn resolve_explicit_child_by_name() {
        let state = fork_state();
        let children = state.children_of("main");
        let got = resolve_explicit_child(&state, "main", "line-b", &children).expect("resolve");
        assert_eq!(got, "line-b");
    }

    #[test]
    fn child_picker_labels_include_pr_badges_and_linked_worktree_suffixes() {
        let state = fork_state();
        let children = state.children_of("main");
        let wt_map = HashMap::from([
            ("line-a".to_string(), "/repo/.worktrees/line-a".to_string()),
            ("line-b".to_string(), "/repo/line-b".to_string()),
        ]);

        let labels = child_picker_labels(&state, &children, &wt_map);

        assert_eq!(labels.len(), 2);
        assert!(labels[0].contains("line-a"), "{labels:?}");
        assert!(labels[0].contains("#10"), "{labels:?}");
        assert!(labels[0].contains("[wt: line-a]"), "{labels:?}");
        assert!(labels[1].contains("line-b"), "{labels:?}");
        assert!(labels[1].contains("#20"), "{labels:?}");
        assert!(!labels[1].contains("[wt:"), "{labels:?}");
    }

    #[test]
    fn child_picker_labels_omit_pr_badges_for_prless_children() {
        let state = sample_state();
        let children = state.children_of("feat/a");

        let labels = child_picker_labels(&state, &children, &HashMap::new());

        assert_eq!(labels.len(), 1);
        assert!(labels[0].contains("feat/b"), "{labels:?}");
        assert!(!labels[0].contains('#'), "{labels:?}");
    }

    #[test]
    fn resolve_explicit_child_by_pr() {
        let state = fork_state();
        let children = state.children_of("main");
        let got = resolve_explicit_child(&state, "main", "20", &children).expect("resolve pr");
        assert_eq!(got, "line-b");
    }

    #[test]
    fn resolve_explicit_child_rejects_wrong_name() {
        let state = fork_state();
        let children = state.children_of("main");
        assert!(resolve_explicit_child(&state, "main", "nope", &children).is_err());
    }

    #[test]
    fn down_target_validates_trunk_and_unmanaged() {
        let state = sample_state();
        assert_eq!(down_target(&state, "feat/b").expect("parent"), "feat/a");
        assert!(down_target(&state, "main").is_err());
        assert!(down_target(&state, "scratch").is_err());
    }

    #[test]
    fn top_and_bottom_targets_follow_stack_shape() {
        let state = sample_state();
        assert_eq!(top_target(&state, "feat/a").expect("top"), "feat/c");
        assert_eq!(bottom_target(&state, "feat/c").expect("bottom"), "feat/a");
        assert_eq!(
            bottom_target(&state, "main").expect("bottom from trunk"),
            "feat/a"
        );
        assert!(top_target(&state, "feat/c").is_err());
        assert!(bottom_target(&state, "feat/a").is_err());
    }

    #[test]
    fn bottom_from_trunk_errors_when_no_children() {
        let state = StackState::new("main".to_string());
        assert!(bottom_target(&state, "main").is_err());
    }
}
