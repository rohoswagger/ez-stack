use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::stack::StackState;

fn diff_range(parent: &str) -> String {
    format!("{parent}...HEAD")
}

pub fn run(stat: bool, name_only: bool) -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current));
    }

    let parent = crate::cmd::track::effective_parent(&current, &state)?;

    // Three-dot diff: what this branch changed relative to where it forked from parent.
    let range = diff_range(&parent);
    let output = git::diff(&range, stat, name_only)?;
    if !output.is_empty() {
        print!("{output}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_range_uses_three_dot_parent_to_head() {
        assert_eq!(diff_range("main"), "main...HEAD");
    }
}
