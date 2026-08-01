use anyhow::Result;

use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;

pub fn run() -> Result<()> {
    let state = StackState::load()?;
    let current = git::current_branch()?;

    if !state.is_managed(&current) {
        anyhow::bail!(EzError::BranchNotInStack(current.clone()));
    }

    let meta = state.get_branch(&current)?;
    let pr_number = meta.pr_number.ok_or_else(|| {
        EzError::UserMessage(format!(
            "No PR found for `{current}` — run `ez push` to create one first"
        ))
    })?;

    // Try to construct URL from configured/repo-discovered name (fast, no extra API call).
    let url = if let Some(repo) = state.repo.as_deref() {
        pr_url(repo, pr_number)
    } else if let Ok(repo) = github::repo_name(None) {
        pr_url(&repo, pr_number)
    } else {
        // Fall back to gh API.
        github::get_pr_status(&pr_number.to_string(), state.repo.as_deref())?
            .ok_or_else(|| EzError::UserMessage(format!("Could not find PR #{pr_number}")))?
            .url
    };

    // stdout (not stderr) — pipeable: open $(ez pr-link)
    println!("{url}");
    Ok(())
}

fn pr_url(repo: &str, pr_number: u64) -> String {
    format!("https://github.com/{repo}/pull/{pr_number}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pr_url_construction() {
        let repo = "owner/repo";
        let pr_number: u64 = 42;
        let url = pr_url(repo, pr_number);
        assert_eq!(url, "https://github.com/owner/repo/pull/42");
    }

    #[test]
    fn configured_repo_url_needs_no_discovery_inputs() {
        assert_eq!(
            pr_url("upstream/project", 123),
            "https://github.com/upstream/project/pull/123"
        );
    }
}
