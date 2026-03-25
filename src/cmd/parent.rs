use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::stack::StackState;

pub fn run() -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current));
    }

    let meta = state.get_branch(&current)?;
    // Machine output to stdout — pipeable.
    println!("{}", meta.parent);

    Ok(())
}
