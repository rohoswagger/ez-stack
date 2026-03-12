use anyhow::{Context, Result, bail};
use std::path::PathBuf;
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

/// Create a new branch at the tip of `base` without switching branches.
pub fn create_branch_at(name: &str, base: &str) -> Result<()> {
    run_git(&["branch", name, base])?;
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

/// Returns true if `ancestor` is reachable from `descendant` (i.e. is an ancestor of it).
/// Returns false if not, or if either ref does not exist.
pub fn is_ancestor(ancestor: &str, descendant: &str) -> bool {
    let (success, _, _) =
        run_git_with_status(&["merge-base", "--is-ancestor", ancestor, descendant]).unwrap_or((
            false,
            String::new(),
            String::new(),
        ));
    success
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

fn parse_porcelain_dirty(output: &str) -> bool {
    output.lines().any(|l| !l.trim().is_empty())
}

pub fn has_uncommitted_changes() -> Result<bool> {
    let output = run_git(&["status", "--porcelain"])?;
    Ok(parse_porcelain_dirty(&output))
}

pub fn stash_push() -> Result<bool> {
    if !has_uncommitted_changes()? {
        return Ok(false);
    }
    run_git(&["stash", "push", "--include-untracked", "-m", "ez-autostash"])?;
    Ok(true)
}

pub fn stash_pop() -> Result<()> {
    run_git(&["stash", "pop"])?;
    Ok(())
}

/// Returns the path to the shared `.git` directory, even in linked worktrees.
///
/// `git rev-parse --git-common-dir` returns the common git dir but may give a
/// relative path in the main worktree. We resolve relative paths against
/// `--show-toplevel` (always absolute) to handle subdirectory invocations correctly.
pub fn git_common_dir() -> Result<PathBuf> {
    let out = run_git(&["rev-parse", "--git-common-dir"])?;
    let p = PathBuf::from(&out);
    if p.is_absolute() {
        return Ok(p);
    }
    // Relative path (e.g., ".git") — resolve against the worktree root.
    let root = run_git(&["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(root).join(p))
}

/// Information about a single git worktree.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    /// Absolute path to the worktree root.
    pub path: String,
    /// The branch checked out in this worktree, or None if detached HEAD.
    pub branch: Option<String>,
}

fn parse_worktree_list(output: &str) -> Vec<WorktreeInfo> {
    let mut worktrees = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_branch: Option<String> = None;

    for line in output.lines() {
        if line.is_empty() {
            if let Some(path) = current_path.take() {
                worktrees.push(WorktreeInfo {
                    path,
                    branch: current_branch.take(),
                });
            }
        } else if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(path.to_string());
            current_branch = None;
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            current_branch = branch_ref
                .strip_prefix("refs/heads/")
                .map(|s| s.to_string());
        }
        // Ignore HEAD sha, `detached`, `bare` lines — not needed.
    }

    // Handle last block — some git versions omit trailing blank line.
    if let Some(path) = current_path {
        worktrees.push(WorktreeInfo {
            path,
            branch: current_branch,
        });
    }

    worktrees
}

/// Returns all git worktrees for this repository.
pub fn worktree_list() -> Result<Vec<WorktreeInfo>> {
    let output = run_git(&["worktree", "list", "--porcelain"])?;
    Ok(parse_worktree_list(&output))
}

/// If `branch` is checked out in a worktree OTHER than `current_root`, returns that
/// worktree's path. Returns Ok(None) if the branch is safe to rebase in this worktree.
///
/// `current_root` should come from `git::repo_root()` (the current worktree's --show-toplevel).
pub fn branch_checked_out_elsewhere(branch: &str, current_root: &str) -> Result<Option<String>> {
    let worktrees = worktree_list()?;
    for wt in worktrees {
        if wt.branch.as_deref() == Some(branch) && wt.path != current_root {
            return Ok(Some(wt.path));
        }
    }
    Ok(None)
}

/// Updates a local branch ref to match the remote WITHOUT checking it out.
///
/// Equivalent to `git fetch origin main:main`. This is different from `fetch_branch`
/// (which only updates remote-tracking refs). This updates the local branch ref directly,
/// so it works even when the branch is checked out in another worktree.
pub fn fetch_refupdate(remote: &str, branch: &str) -> Result<()> {
    let refspec = format!("{branch}:{branch}");
    run_git(&["fetch", remote, &refspec])?;
    Ok(())
}

/// Remove a linked worktree at `path`. Fails if the worktree has uncommitted changes.
pub fn worktree_remove(path: &str) -> Result<()> {
    run_git(&["worktree", "remove", path])?;
    Ok(())
}

/// Force-remove a linked worktree at `path`, discarding any uncommitted changes.
pub fn worktree_remove_force(path: &str) -> Result<()> {
    run_git(&["worktree", "remove", "--force", path])?;
    Ok(())
}

/// Add a linked worktree at `path` checking out `branch`.
/// The branch must already exist.
pub fn worktree_add(path: &str, branch: &str) -> Result<()> {
    run_git(&["worktree", "add", path, branch])?;
    Ok(())
}

/// Prune stale worktree admin entries (git worktree prune).
pub fn worktree_prune() -> Result<()> {
    run_git(&["worktree", "prune"])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_uncommitted_parses_dirty() {
        assert!(parse_porcelain_dirty(" M file.txt\n?? untracked.txt\n"));
        assert!(parse_porcelain_dirty("M  staged.rs\n"));
        assert!(!parse_porcelain_dirty(""));
        assert!(!parse_porcelain_dirty("\n"));
    }

    #[test]
    fn test_parse_worktree_list_normal() {
        let input = "worktree /repo/main\nHEAD abc123\nbranch refs/heads/main\n\nworktree /repo/feat-wt\nHEAD def456\nbranch refs/heads/feat/x\n\n";
        let result = parse_worktree_list(input);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].path, "/repo/main");
        assert_eq!(result[0].branch.as_deref(), Some("main"));
        assert_eq!(result[1].path, "/repo/feat-wt");
        assert_eq!(result[1].branch.as_deref(), Some("feat/x"));
    }

    #[test]
    fn test_parse_worktree_list_detached() {
        let input = "worktree /repo/detached\nHEAD abc123\ndetached\n\n";
        let result = parse_worktree_list(input);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].branch, None);
    }

    #[test]
    fn test_parse_worktree_list_no_trailing_newline() {
        // Some git versions omit trailing blank line after last block.
        let input = "worktree /repo/main\nHEAD abc123\nbranch refs/heads/main";
        let result = parse_worktree_list(input);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].branch.as_deref(), Some("main"));
    }
}
