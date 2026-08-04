use anyhow::{Context, Result, bail};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::EzError;
use crate::git;
use crate::github;
use crate::stack::StackState;
use crate::ui;

static NEXT_EDIT_BUFFER_ID: AtomicU64 = AtomicU64::new(0);

struct EditBuffer {
    path: PathBuf,
}

impl EditBuffer {
    fn create(pr_number: u64, contents: &str) -> Result<Self> {
        Self::create_in(&std::env::temp_dir(), pr_number, contents)
    }

    fn create_in(directory: &Path, pr_number: u64, contents: &str) -> Result<Self> {
        for _ in 0..100 {
            let id = NEXT_EDIT_BUFFER_ID.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!("ez-pr-{pr_number}-{}-{id}.md", std::process::id()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }

            match options.open(&path) {
                Ok(mut file) => {
                    let buffer = Self { path };
                    file.write_all(contents.as_bytes())
                        .context("failed to write temporary PR edit buffer")?;
                    file.flush()
                        .context("failed to flush temporary PR edit buffer")?;
                    return Ok(buffer);
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).context("failed to create temporary PR edit buffer");
                }
            }
        }

        bail!("failed to create a unique temporary PR edit buffer after 100 attempts")
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for EditBuffer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

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
        let edit_buffer = EditBuffer::create(pr_number, &body_current)?;
        let status = std::process::Command::new(&editor)
            .arg(edit_buffer.path())
            .status()
            .with_context(|| {
                format!("failed to launch editor `{editor}` — set $EDITOR or $VISUAL")
            })?;

        if !status.success() {
            anyhow::bail!("Editor exited with non-zero status");
        }

        let new_body = std::fs::read_to_string(edit_buffer.path())?;
        drop(edit_buffer);

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
    use super::*;

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

    #[test]
    fn edit_buffer_avoids_collisions_is_private_and_cleans_up() {
        let directory = crate::test_support::temp_dir("pr-edit-buffer");
        let collision_id = NEXT_EDIT_BUFFER_ID.load(Ordering::Relaxed);
        let collision =
            directory.join(format!("ez-pr-42-{}-{collision_id}.md", std::process::id()));
        std::fs::write(&collision, "do not overwrite").expect("seed collision");

        let buffer = EditBuffer::create_in(&directory, 42, "PR body").expect("create edit buffer");
        let buffer_path = buffer.path().to_path_buf();

        assert_ne!(buffer_path, collision);
        assert_eq!(
            std::fs::read_to_string(&collision).expect("read collision"),
            "do not overwrite"
        );
        assert_eq!(
            std::fs::read_to_string(&buffer_path).expect("read edit buffer"),
            "PR body"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&buffer_path)
                    .expect("edit buffer metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        drop(buffer);
        assert!(!buffer_path.exists());

        let exhausted_directory = crate::test_support::temp_dir("pr-edit-buffer-exhausted");
        let first_exhausted_id = NEXT_EDIT_BUFFER_ID.load(Ordering::Relaxed);
        for id in first_exhausted_id..first_exhausted_id + 100 {
            let path = exhausted_directory.join(format!("ez-pr-42-{}-{id}.md", std::process::id()));
            std::fs::write(path, "occupied").expect("seed exhausted collision");
        }
        let error = EditBuffer::create_in(&exhausted_directory, 42, "body")
            .err()
            .expect("exhausted names should fail");
        assert!(error.to_string().contains("after 100 attempts"));

        let missing_directory = directory.join("missing");
        let error = EditBuffer::create_in(&missing_directory, 42, "body")
            .err()
            .expect("missing temp directory should fail");
        assert!(
            error
                .to_string()
                .contains("failed to create temporary PR edit buffer")
        );

        let _ = std::fs::remove_dir_all(directory);
        let _ = std::fs::remove_dir_all(exhausted_directory);
    }
}
