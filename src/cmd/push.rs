use anyhow::{Result, bail};

use crate::cmd::mutation_guard;
use crate::cmd::mutation_guard::StageMode;
use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

fn stack_ancestors(
    state: &StackState,
    branch: &str,
    repo: &str,
) -> Vec<crate::stack_body::AncestorPr> {
    let path = state.path_to_trunk(branch);
    let len = path.len();
    if len < 2 {
        return vec![];
    }

    path[1..len - 1]
        .iter()
        .rev()
        .map(|b| {
            let pr_number = state.branches.get(b).and_then(|m| m.pr_number);
            let pr_url = pr_number.and_then(|n| {
                if repo.is_empty() {
                    None
                } else {
                    Some(format!("https://github.com/{repo}/pull/{n}"))
                }
            });
            crate::stack_body::AncestorPr {
                branch: b.clone(),
                pr_number,
                pr_url,
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    draft: bool,
    title: Option<&str>,
    body: Option<&str>,
    body_file: Option<&str>,
    base_override: Option<&str>,
    stack: bool,
    stage_all: bool,
    stage_all_files: bool,
    commit_message: Option<&str>,
) -> Result<()> {
    if stack {
        return crate::cmd::submit::run(draft, title, body, body_file);
    }

    let mut commit_scope_defined = false;
    let mut commit_scope_mode: Option<String> = None;
    let mut commit_out_of_scope_files: Vec<String> = Vec::new();

    // If -a or -m was provided, do the commit first.
    if stage_all || stage_all_files || commit_message.is_some() {
        if let Some(msg) = commit_message {
            let stage_mode = if stage_all_files {
                Some(StageMode::All)
            } else if stage_all {
                Some(StageMode::Tracked)
            } else {
                None
            };
            let outcome = mutation_guard::commit_with_guard(msg, stage_mode, false, &[])?
                .expect("commit_with_guard returns Some when --if-changed is false");
            commit_scope_defined = outcome.scope.scope_defined;
            commit_scope_mode = outcome.scope.scope_mode.clone();
            commit_out_of_scope_files = outcome.scope.out_of_scope_files.clone();
            let current = outcome.current;
            ui::info(&format!("Committed on `{current}`: {msg}"));

            if let Ok(stat) = git::show_stat_head() {
                let stat = stat.trim();
                if !stat.is_empty() {
                    eprintln!("{stat}");
                }
            }

            // Restack children (same as ez commit).
            let state = StackState::load()?;
            if state.is_managed(&current) {
                let children = state.children_of(&current);
                if !children.is_empty() {
                    crate::cmd::restack::run()?;
                }
            }
        }
    }

    let mut state = StackState::load()?;
    let current = git::current_branch()?;

    if state.is_trunk(&current) {
        bail!(EzError::OnTrunk);
    }

    if !state.is_managed(&current) {
        bail!(EzError::BranchNotInStack(current.clone()));
    }

    let remote = &state.remote.clone();

    let resolved_body: Option<String> = match body_file {
        Some(path) => Some(github::body_from_file(path)?),
        None => body.map(|s| s.to_string()),
    };

    let parent = if let Some(b) = base_override {
        b.to_string()
    } else {
        state.get_branch(&current)?.parent.clone()
    };

    // Push the branch with force-with-lease.
    let sp = ui::spinner(&format!("Pushing `{current}`..."));
    git::fetch_branch(remote, &current)?;
    git::push(remote, &current, true)?;
    sp.finish_and_clear();
    ui::info(&format!("Pushed `{current}`"));

    let body_explicitly_set = body.is_some() || body_file.is_some();

    // Create or update the PR.
    let had_pr_before = state
        .get_branch(&current)
        .ok()
        .and_then(|m| m.pr_number)
        .is_some();

    let pr_url = push_or_update_pr(
        &mut state,
        &current,
        &parent,
        draft,
        title,
        resolved_body.as_deref(),
        body_explicitly_set,
    )?;

    let pr_number = state.get_branch(&current).ok().and_then(|m| m.pr_number);

    state.save()?;
    ui::success(&format!("PR: {pr_url}"));

    ui::receipt(&serde_json::json!({
        "cmd": "push",
        "branch": current,
        "pr_number": pr_number,
        "pr_url": pr_url,
        "created": !had_pr_before,
        "scope_defined": commit_scope_defined,
        "scope_mode": commit_scope_mode,
        "out_of_scope_count": commit_out_of_scope_files.len(),
        "out_of_scope_files": commit_out_of_scope_files,
    }));

    Ok(())
}

/// Push-or-update logic shared with the `submit` command.
///
/// Returns the PR URL.
pub fn push_or_update_pr(
    state: &mut StackState,
    branch: &str,
    parent: &str,
    draft: bool,
    title_override: Option<&str>,
    body_override: Option<&str>,
    body_explicitly_set: bool,
) -> Result<String> {
    // Collect upstream ancestor PRs for the stack section.
    // path_to_trunk returns [branch, ..., trunk]; we want ancestors only.
    let ancestors = stack_ancestors(state, branch, &github::repo_name().unwrap_or_default());

    let existing_pr = github::get_pr_status(branch)?;

    let pr_url = match existing_pr {
        Some(pr) => {
            state.get_branch_mut(branch)?.pr_number = Some(pr.number);

            // Update PR base only when the stack parent is genuinely an ancestor of this branch.
            // If the branch was rebased onto a different base outside of ez (bypassing stack
            // metadata), is_ancestor returns false and we leave the PR base alone so we don't
            // clobber a manual `gh pr edit --base` change.
            if pr.base != parent {
                if git::is_ancestor(parent, branch) {
                    if let Err(e) = github::update_pr_base(pr.number, parent) {
                        ui::warn(&format!(
                            "Push succeeded but PR #{} base could not be updated to `{parent}`: {e}",
                            pr.number
                        ));
                    } else {
                        ui::info(&format!("Updated PR #{} base to `{parent}`", pr.number));
                    }
                } else {
                    ui::warn(&format!(
                        "PR #{} base not updated: `{parent}` is not an ancestor of `{branch}` \
                         (stack metadata may be stale — run `ez sync` or update manually)",
                        pr.number
                    ));
                }
            }

            // Only update body if user explicitly passed --body/--body-file.
            if body_explicitly_set {
                let raw_body = body_override.unwrap_or("Part of a stack managed by `ez`.");
                let body = crate::stack_body::build_stack_body(&ancestors, raw_body);
                if let Err(e) = github::edit_pr(pr.number, title_override, Some(&body)) {
                    ui::warn(&format!(
                        "Push succeeded but PR #{} could not be updated: {e}",
                        pr.number
                    ));
                } else {
                    ui::info(&format!("Updated PR #{}", pr.number));
                }
            } else if title_override.is_some() {
                if let Err(e) = github::edit_pr(pr.number, title_override, None) {
                    ui::warn(&format!(
                        "Push succeeded but PR #{} title could not be updated: {e}",
                        pr.number
                    ));
                } else {
                    ui::info(&format!("Updated PR #{} title", pr.number));
                }
            }

            pr.url
        }
        None => {
            // Derive the PR title from the first commit on this branch.
            let range = format!("{parent}..{branch}");
            let commits = git::log_oneline(&range, 1)?;
            let derived_title = commits
                .first()
                .map(|(_, msg)| msg.clone())
                .unwrap_or_else(|| branch.to_string());

            let title = title_override.unwrap_or(&derived_title);
            let default_body = "Part of a stack managed by `ez`.";
            let raw_body = body_override.unwrap_or(default_body);

            // Always append stack section to new PRs.
            let body = crate::stack_body::build_stack_body(&ancestors, raw_body);

            let pr = github::create_pr(title, &body, parent, branch, draft)?;
            state.get_branch_mut(branch)?.pr_number = Some(pr.number);
            ui::info(&format!("Created PR #{}: {}", pr.number, pr.url));
            pr.url
        }
    };

    Ok(pr_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_ancestors_orders_trunk_closest_first_and_builds_urls() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.add_branch("feat/b", "feat/a", "bbb", None, None);
        state.add_branch("feat/c", "feat/b", "ccc", None, None);
        state.get_branch_mut("feat/a").expect("a").pr_number = Some(10);
        state.get_branch_mut("feat/b").expect("b").pr_number = Some(20);

        let ancestors = stack_ancestors(&state, "feat/c", "org/repo");
        assert_eq!(ancestors.len(), 2);
        assert_eq!(ancestors[0].branch, "feat/a");
        assert_eq!(ancestors[1].branch, "feat/b");
        assert_eq!(
            ancestors[0].pr_url.as_deref(),
            Some("https://github.com/org/repo/pull/10")
        );
        assert_eq!(
            ancestors[1].pr_url.as_deref(),
            Some("https://github.com/org/repo/pull/20")
        );
    }

    #[test]
    fn stack_ancestors_handles_empty_repo_name() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/a", "main", "aaa", None, None);
        state.add_branch("feat/b", "feat/a", "bbb", None, None);
        state.get_branch_mut("feat/a").expect("a").pr_number = Some(10);

        let ancestors = stack_ancestors(&state, "feat/b", "");
        assert_eq!(ancestors.len(), 1);
        assert!(ancestors[0].pr_url.is_none());
    }
}
