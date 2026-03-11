use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

/// `ready = true` → mark ready for review; `ready = false` → mark as draft.
pub fn run(ready: bool) -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current.clone()));
    }

    let meta = state.get_branch(&current)?;
    let pr_number = meta.pr_number.ok_or_else(|| {
        anyhow::anyhow!("No PR found for `{current}` — run `ez push` to create one first")
    })?;

    github::set_pr_ready(pr_number, ready)?;

    if ready {
        ui::success(&format!("PR #{pr_number} marked as ready for review"));
    } else {
        ui::success(&format!("PR #{pr_number} marked as draft"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_ready_semantics() {
        // ready=true → set_pr_ready(n, true); ready=false → set_pr_ready(n, false)
        // Compile-time check that the function signature is correct.
        let f: fn(bool) -> anyhow::Result<()> = super::run;
        let _ = f;
    }
}
