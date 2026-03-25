use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::stack::StackState;

pub fn run(stat: bool, name_only: bool) -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current));
    }

    let meta = state.get_branch(&current)?;
    let parent = &meta.parent;

    // Three-dot diff: what this branch changed relative to where it forked from parent.
    let range = format!("{parent}...HEAD");
    let output = git::diff(&range, stat, name_only)?;
    if !output.is_empty() {
        print!("{output}");
    }

    Ok(())
}
