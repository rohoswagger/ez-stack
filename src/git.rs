use anyhow::{Context, Result, bail};
use std::process::Command;

use crate::error::EzError;

fn run_git(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(EzError::GitError(stderr).into())
    }
}

fn run_git_with_status(args: &[&str]) -> Result<(bool, String, String)> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Ok((output.status.success(), stdout, stderr))
}

pub fn is_repo() -> bool {
    run_git(&["rev-parse", "--is-inside-work-tree"]).is_ok()
}

pub fn repo_root() -> Result<String> {
    run_git(&["rev-parse", "--show-toplevel"])
}

pub fn current_branch() -> Result<String> {
    run_git(&["rev-parse", "--abbrev-ref", "HEAD"])
}

pub fn rev_parse(refspec: &str) -> Result<String> {
    run_git(&["rev-parse", refspec])
}

pub fn branch_exists(name: &str) -> bool {
    run_git(&["rev-parse", "--verify", name]).is_ok()
}

pub fn create_branch(name: &str) -> Result<()> {
    run_git(&["checkout", "-b", name])?;
    Ok(())
}

pub fn checkout(branch: &str) -> Result<()> {
    run_git(&["checkout", branch])?;
    Ok(())
}

pub fn commit(message: &str) -> Result<()> {
    run_git(&["commit", "-m", message])?;
    Ok(())
}

pub fn commit_amend(message: Option<&str>) -> Result<()> {
    match message {
        Some(msg) => run_git(&["commit", "--amend", "-m", msg])?,
        None => run_git(&["commit", "--amend", "--no-edit"])?,
    };
    Ok(())
}

pub fn add_all() -> Result<()> {
    run_git(&["add", "-A"])?;
    Ok(())
}

pub fn has_staged_changes() -> Result<bool> {
    let (success, _, _) = run_git_with_status(&["diff", "--cached", "--quiet"])?;
    Ok(!success) // exit code 1 means there ARE diffs
}

pub fn fetch(remote: &str) -> Result<()> {
    run_git(&["fetch", remote])?;
    Ok(())
}

pub fn rebase_onto(new_base: &str, old_base: &str, branch: &str) -> Result<bool> {
    let (success, _, stderr) =
        run_git_with_status(&["rebase", "--onto", new_base, old_base, branch])?;
    if success {
        Ok(true)
    } else if stderr.contains("CONFLICT") || stderr.contains("conflict") {
        // Abort the rebase so we leave the repo in a clean state
        let _ = run_git(&["rebase", "--abort"]);
        Ok(false)
    } else {
        // Some other rebase failure — try to abort and report
        let _ = run_git(&["rebase", "--abort"]);
        bail!(EzError::GitError(stderr));
    }
}

pub fn fast_forward_merge(remote_ref: &str) -> Result<()> {
    run_git(&["merge", "--ff-only", remote_ref])?;
    Ok(())
}

pub fn push(remote: &str, branch: &str, force: bool) -> Result<()> {
    let mut args = vec!["push", remote, branch];
    if force {
        args.push("--force-with-lease");
    }
    let (success, _, stderr) = run_git_with_status(&args)?;
    if success {
        return Ok(());
    }
    if stderr.contains("stale info") || stderr.contains("(stale)") {
        bail!(crate::error::EzError::StaleRemoteRef(branch.to_string()));
    }
    bail!(crate::error::EzError::GitError(stderr));
}

pub fn delete_branch(branch: &str, force: bool) -> Result<()> {
    let flag = if force { "-D" } else { "-d" };
    run_git(&["branch", flag, branch])?;
    Ok(())
}

pub fn delete_remote_branch(remote: &str, branch: &str) -> Result<()> {
    let _ = run_git(&["push", remote, "--delete", branch]);
    Ok(())
}

pub fn merge_base(a: &str, b: &str) -> Result<String> {
    run_git(&["merge-base", a, b])
}

pub fn default_branch() -> Result<String> {
    // Try to detect from remote
    if let Ok(out) = run_git(&["symbolic-ref", "refs/remotes/origin/HEAD"])
        && let Some(branch) = out.strip_prefix("refs/remotes/origin/")
    {
        return Ok(branch.to_string());
    }

    // Fallback: check common names
    for name in &["main", "master"] {
        if branch_exists(name) {
            return Ok(name.to_string());
        }
    }

    bail!("could not detect default branch — set it manually with `ez init --trunk <branch>`")
}

pub fn log_oneline(range: &str, max: usize) -> Result<Vec<(String, String)>> {
    let output = run_git(&["log", "--oneline", &format!("--max-count={max}"), range])?;
    Ok(output
        .lines()
        .map(|line| {
            let (sha, msg) = line.split_once(' ').unwrap_or((line, ""));
            (sha.to_string(), msg.to_string())
        })
        .collect())
}

pub fn remote_branch_exists(remote: &str, branch: &str) -> bool {
    run_git(&["ls-remote", "--heads", remote, branch])
        .map(|out| !out.is_empty())
        .unwrap_or(false)
}

pub fn branch_list() -> Result<Vec<String>> {
    let output = run_git(&["branch", "--format=%(refname:short)"])?;
    Ok(output.lines().map(|s| s.to_string()).collect())
}

pub fn fetch_branch(remote: &str, branch: &str) -> Result<()> {
    // Silently update the remote-tracking ref for this branch before force-push.
    // Ignore errors (branch may not exist on remote yet).
    let _ = run_git(&["fetch", remote, branch]);
    Ok(())
}
