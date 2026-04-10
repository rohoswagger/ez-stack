use anyhow::{Result, bail};

use crate::cmd::checkout::{switch_to, worktree_map};
use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

fn up_target(children: &[String]) -> Result<String> {
    children
        .first()
        .cloned()
        .ok_or_else(|| EzError::AlreadyAtTop.into())
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
        return up_target(&children).map_err(|_| EzError::AlreadyAtBottom.into());
    }

    let bottom = state.stack_bottom(current);
    if bottom == current {
        bail!(EzError::AlreadyAtBottom);
    }
    Ok(bottom)
}

pub fn up() -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    let children = state.children_of(&current);
    let target = up_target(&children)?;
    switch_to(&state, &target, &worktree_map())?;
    ui::success(&format!(
        "Moved up: {} → {}",
        ui::branch_display(&current, false),
        ui::branch_display(&target, true),
    ));

    Ok(())
}

pub fn down() -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    let parent = down_target(&state, &current)?;
    switch_to(&state, &parent, &worktree_map())?;
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
    switch_to(&state, &target, &worktree_map())?;
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
    switch_to(&state, &target, &worktree_map())?;
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

    #[test]
    fn up_target_errors_without_children() {
        let err = up_target(&[]).expect_err("expected no children");
        assert!(err.to_string().contains("already at the top"));
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
}
