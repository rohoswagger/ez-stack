use anyhow::{Result, bail};

use crate::cmd::push::push_or_update_pr;
use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

fn branches_to_submit(path_to_trunk: &[String], trunk: &str) -> Vec<String> {
    path_to_trunk
        .iter()
        .rev()
        .filter(|b| b.as_str() != trunk)
        .cloned()
        .collect()
}

pub fn run(
    draft: bool,
    title: Option<&str>,
    body: Option<&str>,
    body_file: Option<&str>,
) -> Result<()> {
    let mut state = StackState::load()?;
    if let Some(root) = git::current_linked_worktree_root()? {
        ui::linked_worktree_warning(&root);
    }
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current.clone()));
    }

    let resolved_body: Option<String> = match body_file {
        Some(path) => Some(github::body_from_file(path)?),
        None => body.map(|s| s.to_string()),
    };

    // path_to_trunk returns [current, ..., trunk].
    // We want to iterate bottom-to-top (trunk-side first), skipping trunk itself.
    let path = state.path_to_trunk(&current);
    let branches_to_submit = branches_to_submit(&path, &state.trunk);

    if branches_to_submit.is_empty() {
        ui::info("No branches to submit.");
        return Ok(());
    }

    let remote = state.remote.clone();
    let body_explicitly_set = body.is_some() || body_file.is_some();
    let mut pr_urls: Vec<(String, String)> = Vec::new();

    for branch in &branches_to_submit {
        let parent = state.get_branch(branch)?.parent.clone();

        // Push with force-with-lease.
        let sp = ui::spinner(&format!("Pushing `{branch}`..."));
        git::fetch_branch(&remote, branch)?;
        git::push(&remote, branch, true)?;
        sp.finish_and_clear();

        // Create or update the PR.
        let pr_url = push_or_update_pr(
            &mut state,
            branch,
            &parent,
            draft,
            title,
            resolved_body.as_deref(),
            body_explicitly_set,
        )?;

        let pr_number = state.get_branch(branch).ok().and_then(|m| m.pr_number);
        ui::receipt(&serde_json::json!({
            "cmd": "submit",
            "branch": branch,
            "pr_number": pr_number,
            "pr_url": pr_url,
        }));

        pr_urls.push((branch.clone(), pr_url));
    }

    state.save()?;

    // Print summary.
    ui::success(&format!("Submitted {} PR(s):", pr_urls.len()));
    for (branch, url) in &pr_urls {
        ui::info(&format!("  {branch} -> {url}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branches_to_submit_orders_bottom_to_top_and_skips_trunk() {
        let path = vec![
            "feat/c".to_string(),
            "feat/b".to_string(),
            "feat/a".to_string(),
            "main".to_string(),
        ];
        assert_eq!(
            branches_to_submit(&path, "main"),
            vec![
                "feat/a".to_string(),
                "feat/b".to_string(),
                "feat/c".to_string()
            ]
        );
    }

    #[test]
    fn branches_to_submit_handles_trunk_only_path() {
        let path = vec!["main".to_string()];
        assert!(branches_to_submit(&path, "main").is_empty());
    }
}
