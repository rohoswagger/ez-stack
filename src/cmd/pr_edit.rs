use anyhow::{Context, Result, bail};

use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

pub fn run(title: Option<&str>, body: Option<&str>, body_file: Option<&str>) -> Result<()> {
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
        anyhow::anyhow!("No PR found for branch `{current}` — run `ez push` to create one first")
    })?;

    // If no explicit edits, open $EDITOR with the current PR body.
    if title.is_none() && body.is_none() && body_file.is_none() {
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_string());

        let body_current = github::get_pr_body(pr_number, state.repo.as_deref())?;
        let tmp_path = format!("/tmp/ez-pr-{pr_number}.md");
        std::fs::write(&tmp_path, &body_current)?;

        let status = std::process::Command::new(&editor)
            .arg(&tmp_path)
            .status()
            .with_context(|| {
                format!("failed to launch editor `{editor}` — set $EDITOR or $VISUAL")
            })?;

        if !status.success() {
            anyhow::bail!("Editor exited with non-zero status");
        }

        let new_body = std::fs::read_to_string(&tmp_path)?;
        let _ = std::fs::remove_file(&tmp_path);

        if new_body == body_current {
            ui::info("No changes made");
            return Ok(());
        }

        github::edit_pr(pr_number, None, Some(&new_body), state.repo.as_deref())?;

        if let Ok(Some(pr)) = github::get_pr_status(&pr_number.to_string(), state.repo.as_deref()) {
            ui::success(&format!("Updated PR #{}: {}", pr.number, pr.url));
        } else {
            ui::success(&format!("Updated PR #{pr_number} body"));
        }
        return Ok(());
    }

    let resolved_body: Option<String> = if let Some(path) = body_file {
        Some(github::body_from_file(path)?)
    } else {
        body.map(|s| s.to_string())
    };

    github::edit_pr(
        pr_number,
        title,
        resolved_body.as_deref(),
        state.repo.as_deref(),
    )?;

    if let Ok(Some(pr)) = github::get_pr_status(&pr_number.to_string(), state.repo.as_deref()) {
        ui::success(&format!("Updated PR #{}: {}", pr.number, pr.url));
    } else {
        ui::success(&format!("Updated PR #{pr_number}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_editor_resolution() {
        // Test the fallback chain: VISUAL > EDITOR > vi
        // We simulate by testing Option chaining (not env vars, which are global state).
        fn resolve(visual: Option<&str>, editor: Option<&str>) -> String {
            visual
                .or(editor)
                .map(|s| s.to_string())
                .unwrap_or_else(|| "vi".to_string())
        }
        assert_eq!(resolve(Some("code"), Some("vim")), "code");
        assert_eq!(resolve(None, Some("vim")), "vim");
        assert_eq!(resolve(None, None), "vi");
    }
}
