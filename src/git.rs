use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;

use crate::error::EzError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseConflict {
    pub conflicting_files: Vec<String>,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseOutcome {
    RebasingComplete,
    Conflict(RebaseConflict),
}

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

fn stream_to_terminal<R, W>(mut reader: R, mut writer: W) -> std::io::Result<Vec<u8>>
where
    R: Read + Send + 'static,
    W: Write,
{
    let mut captured = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        writer.write_all(&chunk[..read])?;
        writer.flush()?;
        captured.extend_from_slice(&chunk[..read]);
    }
    Ok(captured)
}

fn run_git_streaming(args: &[&str]) -> Result<()> {
    let mut child = Command::new("git")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    let stdout = child
        .stdout
        .take()
        .context("failed to capture git stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture git stderr")?;

    let stdout_handle = thread::spawn(|| stream_to_terminal(stdout, std::io::stdout()));
    let stderr_handle = thread::spawn(|| stream_to_terminal(stderr, std::io::stderr()));

    let status = child.wait()?;
    let stdout_capture = stdout_handle
        .join()
        .map_err(|_| anyhow::anyhow!("failed to join git stdout stream"))??;
    let stderr_capture = stderr_handle
        .join()
        .map_err(|_| anyhow::anyhow!("failed to join git stderr stream"))??;

    if status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&stderr_capture).trim().to_string();
        let stdout = String::from_utf8_lossy(&stdout_capture).trim().to_string();
        let message = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("git command failed: git {}", args.join(" "))
        };
        Err(EzError::GitError(message).into())
    }
}

pub fn is_repo() -> bool {
    run_git(&["rev-parse", "--is-inside-work-tree"]).is_ok()
}

pub fn repo_root() -> Result<String> {
    run_git(&["rev-parse", "--show-toplevel"])
}

fn normalize_path_for_compare(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
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

/// Returns the `--stat` summary for HEAD (files changed, insertions, deletions).
pub fn show_stat_head() -> Result<String> {
    run_git(&["show", "--stat", "--no-patch", "--format=", "HEAD"])
}

/// Parse the shortstat for HEAD into (files_changed, insertions, deletions).
pub fn diff_stat_numbers() -> (u64, u64, u64) {
    let output = run_git(&["diff", "--shortstat", "HEAD~1..HEAD"]).unwrap_or_default();
    parse_shortstat(&output)
}

/// Parse `git diff --shortstat` output into (files, insertions, deletions).
fn parse_shortstat(s: &str) -> (u64, u64, u64) {
    let mut files = 0u64;
    let mut ins = 0u64;
    let mut del = 0u64;
    for part in s.split(',') {
        let part = part.trim();
        let num: u64 = part
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        if part.contains("file") {
            files = num;
        } else if part.contains("insertion") {
            ins = num;
        } else if part.contains("deletion") {
            del = num;
        }
    }
    (files, ins, del)
}

/// Run `git diff` with the given range and optional flags.
/// Returns the raw output (may be empty if no changes).
pub fn diff(range: &str, stat: bool, name_only: bool) -> Result<String> {
    let mut args = vec!["diff"];
    if stat {
        args.push("--stat");
    }
    if name_only {
        args.push("--name-only");
    }
    args.push(range);
    run_git(&args)
}

/// Run `git cherry <upstream> <branch>` to find commits not yet applied upstream.
/// Output lines starting with `- ` are already upstream; `+ ` are unique to the branch.
pub fn cherry(upstream: &str, branch: &str) -> Result<String> {
    run_git(&["cherry", upstream, branch])
}

/// Stage all tracked modified/deleted files. Uses `git add -u` (NOT `git add -A`)
/// so untracked files are never accidentally staged by the -a flag.
pub fn add_all() -> Result<()> {
    run_git(&["add", "-u"])?;
    Ok(())
}

/// Stage all changes, including untracked files.
pub fn add_all_including_untracked() -> Result<()> {
    run_git(&["add", "-A"])?;
    Ok(())
}

/// Return counts of (staged, modified, untracked) files in the working tree.
pub fn working_tree_status() -> (usize, usize, usize) {
    let output = run_git(&["status", "--porcelain"]).unwrap_or_default();
    let mut staged = 0;
    let mut modified = 0;
    let mut untracked = 0;
    for line in output.lines() {
        if line.len() < 2 {
            continue;
        }
        let index = line.as_bytes()[0];
        let worktree = line.as_bytes()[1];
        if line.starts_with("??") {
            untracked += 1;
        } else {
            if index != b' ' && index != b'?' {
                staged += 1;
            }
            if worktree != b' ' && worktree != b'?' {
                modified += 1;
            }
        }
    }
    (staged, modified, untracked)
}

/// List files modified in the working tree (unstaged changes).
pub fn modified_files() -> Vec<String> {
    run_git(&["diff", "--name-only"])
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// Stage specific paths.
pub fn add_paths(paths: &[String]) -> Result<()> {
    let mut args = vec!["add", "--"];
    let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
    args.extend(refs);
    run_git(&args)?;
    Ok(())
}

pub fn has_staged_changes() -> Result<bool> {
    let (success, _, _) = run_git_with_status(&["diff", "--cached", "--quiet"])?;
    Ok(!success) // exit code 1 means there ARE diffs
}

pub fn staged_files() -> Result<Vec<String>> {
    Ok(run_git(&["diff", "--cached", "--name-only"])?
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect())
}

pub fn staged_files_matching_scope(patterns: &[String]) -> Result<Vec<String>> {
    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let normalized: Vec<String> = patterns
        .iter()
        .map(|pattern| git_scope_pattern(pattern))
        .collect();
    let mut args: Vec<&str> = vec!["diff", "--cached", "--name-only", "--"];
    args.extend(normalized.iter().map(String::as_str));

    Ok(run_git(&args)?
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect())
}

fn git_scope_pattern(pattern: &str) -> String {
    if pattern.starts_with(":(") {
        pattern.to_string()
    } else {
        format!(":(glob){pattern}")
    }
}

pub fn fetch(remote: &str) -> Result<()> {
    let args = fetch_args(remote);
    run_git_streaming(&args)?;
    Ok(())
}

fn fetch_args(remote: &str) -> [&str; 3] {
    ["fetch", "--progress", remote]
}

pub fn rebase_onto(new_base: &str, old_base: &str, branch: &str) -> Result<RebaseOutcome> {
    let (success, _, stderr) =
        run_git_with_status(&["rebase", "--onto", new_base, old_base, branch])?;
    if success {
        Ok(RebaseOutcome::RebasingComplete)
    } else if stderr.contains("CONFLICT") || stderr.contains("conflict") {
        // Abort the rebase so we leave the repo in a clean state
        let _ = run_git(&["rebase", "--abort"]);
        Ok(RebaseOutcome::Conflict(parse_rebase_conflict(&stderr)))
    } else {
        // Some other rebase failure — try to abort and report
        let _ = run_git(&["rebase", "--abort"]);
        bail!(EzError::GitError(stderr));
    }
}

fn parse_rebase_conflict(stderr: &str) -> RebaseConflict {
    RebaseConflict {
        conflicting_files: parse_conflicting_files(stderr),
        stderr: stderr.trim().to_string(),
    }
}

fn parse_conflicting_files(stderr: &str) -> Vec<String> {
    let mut files = BTreeSet::new();

    for line in stderr.lines().map(str::trim) {
        if let Some(path) = line.split("Merge conflict in ").nth(1) {
            if !path.is_empty() {
                files.insert(path.to_string());
            }
            continue;
        }

        if let Some(detail) = line.strip_prefix("CONFLICT ")
            && let Some(after_colon) = detail.split(": ").nth(1)
        {
            if let Some(path) = after_colon.split(" deleted in ").next()
                && after_colon.contains(" deleted in ")
                && !path.is_empty()
            {
                files.insert(path.to_string());
            } else if let Some(path) = after_colon.split(" added in ").next()
                && after_colon.contains(" added in ")
                && !path.is_empty()
            {
                files.insert(path.to_string());
            }
        }
    }

    files.into_iter().collect()
}

/// Plain `git rebase <upstream> <branch>` — uses git's built-in patch-id detection
/// to auto-skip commits already applied upstream. Returns true on success.
pub fn rebase(upstream: &str, branch: &str) -> Result<bool> {
    let (success, _, stderr) = run_git_with_status(&["rebase", upstream, branch])?;
    if success {
        Ok(true)
    } else {
        let _ = run_git(&["rebase", "--abort"]);
        if stderr.contains("CONFLICT") || stderr.contains("conflict") {
            Ok(false)
        } else {
            bail!(EzError::GitError(stderr));
        }
    }
}

pub fn fast_forward_merge(remote_ref: &str) -> Result<()> {
    run_git(&["merge", "--ff-only", remote_ref])?;
    Ok(())
}

pub fn fast_forward_merge_at(dir: &str, remote_ref: &str) -> Result<()> {
    run_git(&["-C", dir, "merge", "--ff-only", remote_ref])?;
    Ok(())
}

pub fn hard_reset(remote_ref: &str) -> Result<()> {
    run_git(&["reset", "--hard", remote_ref])?;
    Ok(())
}

pub fn hard_reset_at(dir: &str, remote_ref: &str) -> Result<()> {
    run_git(&["-C", dir, "reset", "--hard", remote_ref])?;
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

/// Get seconds since the last commit on a branch. Returns None if no commits or error.
pub fn log_oneline_time(branch: &str) -> Option<u64> {
    let output = run_git(&["log", "-1", "--format=%ct", branch]).ok()?;
    let timestamp: u64 = output.trim().parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(timestamp))
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

/// Update a local branch to the latest fetched remote-tracking ref without requiring checkout.
///
/// Returns `Ok(true)` when the branch moved, `Ok(false)` when it was already up to date.
pub fn update_branch_to_latest_remote(
    remote: &str,
    branch: &str,
    current_branch: &str,
    current_root: &str,
) -> Result<bool> {
    let remote_tracking = format!("{remote}/{branch}");
    let branch_is_behind =
        is_ancestor(branch, &remote_tracking) && !is_ancestor(&remote_tracking, branch);

    if !branch_is_behind {
        return Ok(false);
    }

    if current_branch == branch {
        fast_forward_merge(&remote_tracking)?;
    } else if let Some(branch_worktree) = branch_checked_out_elsewhere(branch, current_root)? {
        fast_forward_merge_at(&branch_worktree, &remote_tracking)?;
    } else {
        fetch_refupdate(remote, branch)?;
    }

    Ok(true)
}

/// Force-align a local branch to the fetched remote-tracking ref, discarding local
/// divergence. This is intentionally stronger than `update_branch_to_latest_remote`
/// and is used by `ez sync` so trunk matches the latest remote state exactly.
pub fn reset_branch_to_latest_remote(
    remote: &str,
    branch: &str,
    current_branch: &str,
    current_root: &str,
) -> Result<bool> {
    let remote_tracking = format!("{remote}/{branch}");

    if rev_parse(branch)? == rev_parse(&remote_tracking)? {
        return Ok(false);
    }

    if current_branch == branch {
        hard_reset(&remote_tracking)?;
    } else if let Some(branch_worktree) = branch_checked_out_elsewhere(branch, current_root)? {
        hard_reset_at(&branch_worktree, &remote_tracking)?;
    } else {
        fetch_refupdate(remote, branch)?;
    }

    Ok(true)
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

/// Resolve the main worktree root path.
/// Uses the first entry from `git worktree list` which is always the main worktree.
pub fn main_worktree_root() -> Result<String> {
    let worktrees = worktree_list()?;
    worktrees
        .first()
        .map(|wt| wt.path.clone())
        .ok_or_else(|| anyhow::anyhow!("could not determine main worktree root"))
}

/// The directory agents should edit within for the current checkout.
pub fn active_edit_root() -> Result<String> {
    repo_root()
}

/// Returns the current linked worktree root if the active checkout is not the main worktree.
pub fn current_linked_worktree_root() -> Result<Option<String>> {
    let current_root = repo_root()?;
    let main_root = main_worktree_root().unwrap_or_else(|_| current_root.clone());

    if normalize_path_for_compare(&current_root) == normalize_path_for_compare(&main_root) {
        Ok(None)
    } else {
        Ok(Some(current_root))
    }
}

/// Compute the `.worktrees/<name>` path relative to the main worktree root.
pub fn worktree_path(name: &str) -> Result<String> {
    let root = main_worktree_root()?;
    let safe_name = name.replace('/', "-");
    Ok(format!("{root}/.worktrees/{safe_name}"))
}

/// Run `git -C <dir> status --porcelain` and return counts of (staged, modified, untracked).
pub fn working_tree_status_at(dir: &str) -> (usize, usize, usize) {
    let output = run_git(&["-C", dir, "status", "--porcelain"]).unwrap_or_default();
    let mut staged = 0;
    let mut modified = 0;
    let mut untracked = 0;
    for line in output.lines() {
        if line.len() < 2 {
            continue;
        }
        let index = line.as_bytes()[0];
        let worktree = line.as_bytes()[1];
        if line.starts_with("??") {
            untracked += 1;
        } else {
            if index != b' ' && index != b'?' {
                staged += 1;
            }
            if worktree != b' ' && worktree != b'?' {
                modified += 1;
            }
        }
    }
    (staged, modified, untracked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        CwdGuard, PathGuard, init_git_repo, install_fake_bin, run_cmd, take_env_lock, temp_dir,
        write_file,
    };

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

    #[test]
    fn test_git_scope_pattern_uses_glob_magic() {
        assert_eq!(git_scope_pattern("src/auth/**"), ":(glob)src/auth/**");
        assert_eq!(
            git_scope_pattern(":(glob)src/auth/**"),
            ":(glob)src/auth/**"
        );
    }

    #[test]
    fn parse_shortstat_handles_partial_and_empty_sections() {
        assert_eq!(
            parse_shortstat(" 1 file changed, 3 insertions(+)"),
            (1, 3, 0)
        );
        assert_eq!(
            parse_shortstat(" 2 files changed, 4 deletions(-)"),
            (2, 0, 4)
        );
        assert_eq!(parse_shortstat(""), (0, 0, 0));
    }

    #[test]
    fn test_parse_conflicting_files_extracts_merge_conflict_paths() {
        let stderr = "\
Rebasing (1/6)\n\
Auto-merging src/data/competitors.ts\n\
CONFLICT (content): Merge conflict in src/data/competitors.ts\n\
CONFLICT (modify/delete): src/old.ts deleted in HEAD and modified in abc123.\n";

        assert_eq!(
            parse_conflicting_files(stderr),
            vec![
                "src/data/competitors.ts".to_string(),
                "src/old.ts".to_string()
            ]
        );
    }

    #[test]
    fn staged_files_matching_scope_short_circuits_empty_patterns() {
        assert_eq!(
            staged_files_matching_scope(&[]).expect("empty scope should succeed"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn rebase_onto_aborts_and_returns_conflict_details() {
        let _guard = take_env_lock();
        let log_dir = crate::test_support::temp_dir("git-rebase-conflict");
        let log_path = log_dir.join("calls.log");
        let fake_dir = install_fake_bin(
            "git-rebase-conflict-bin",
            "git",
            &format!(
                r#"#!/bin/sh
if [ "$1" = "rebase" ] && [ "$2" = "--onto" ]; then
  echo "CONFLICT (content): Merge conflict in src/lib.rs" >&2
  exit 1
fi
if [ "$1" = "rebase" ] && [ "$2" = "--abort" ]; then
  echo abort >> "{}"
  exit 0
fi
exit 0
"#,
                log_path.display()
            ),
        );
        let _path = PathGuard::install(&fake_dir);

        let outcome = rebase_onto("main", "old-base", "feature").expect("conflict result");
        assert_eq!(
            outcome,
            RebaseOutcome::Conflict(RebaseConflict {
                conflicting_files: vec!["src/lib.rs".to_string()],
                stderr: "CONFLICT (content): Merge conflict in src/lib.rs".to_string(),
            })
        );
        assert_eq!(
            std::fs::read_to_string(log_path).expect("abort log"),
            "abort\n"
        );
    }

    #[test]
    fn fetch_surfaces_git_stderr_from_failed_subprocess() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "git-fetch-fail",
            "git",
            r#"#!/bin/sh
echo "fatal: simulated fetch failure" >&2
exit 1
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        let err = fetch("origin").expect_err("fetch should fail");
        assert!(
            err.to_string().contains("fatal: simulated fetch failure"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn fetch_args_force_progress_output() {
        assert_eq!(fetch_args("origin"), ["fetch", "--progress", "origin"]);
    }

    #[test]
    fn has_staged_changes_treats_exit_code_one_as_dirty() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "git-diff-quiet",
            "git",
            r#"#!/bin/sh
if [ "$1" = "diff" ] && [ "$2" = "--cached" ] && [ "$3" = "--quiet" ]; then
  exit 1
fi
exit 0
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        assert!(has_staged_changes().expect("staged changes"));
    }

    #[test]
    fn repo_and_branch_helpers_work_in_real_repo() {
        let _guard = take_env_lock();
        let repo = init_git_repo("git-basics");
        let _cwd = CwdGuard::enter(&repo);

        assert!(is_repo());
        assert_eq!(
            std::fs::canonicalize(repo_root().expect("root")).expect("canonicalized root"),
            std::fs::canonicalize(&repo).expect("canonicalized repo")
        );
        assert_eq!(current_branch().expect("branch"), "main");
        assert_eq!(default_branch().expect("default"), "main");
        assert!(branch_exists("main"));
        assert_eq!(branch_list().expect("branches"), vec!["main".to_string()]);
    }

    #[test]
    fn staging_and_status_helpers_track_real_changes() {
        let _guard = take_env_lock();
        let repo = init_git_repo("git-staging");
        let _cwd = CwdGuard::enter(&repo);

        write_file(&repo, "tracked.txt", "changed\n");
        write_file(&repo, "new.txt", "new\n");

        assert_eq!(modified_files(), vec!["tracked.txt".to_string()]);
        let (staged, modified, untracked) = working_tree_status();
        assert_eq!(untracked, 1);
        assert_eq!(staged + modified, 1);

        add_paths(&["tracked.txt".to_string()]).expect("stage tracked");
        assert!(has_staged_changes().expect("staged"));
        assert_eq!(
            staged_files().expect("staged files"),
            vec!["tracked.txt".to_string()]
        );
        assert_eq!(working_tree_status(), (1, 0, 1));

        add_all().expect("stage tracked changes");
        assert_eq!(
            staged_files().expect("staged files"),
            vec!["tracked.txt".to_string()]
        );

        add_all_including_untracked().expect("stage all changes");
        assert_eq!(
            staged_files().expect("staged files"),
            vec!["new.txt".to_string(), "tracked.txt".to_string()]
        );
    }

    #[test]
    fn branch_log_and_worktree_helpers_operate_on_temp_repo() {
        let _guard = take_env_lock();
        let repo = init_git_repo("git-worktree");
        let _cwd = CwdGuard::enter(&repo);

        create_branch_at("feat/test", "main").expect("create branch");
        assert!(branch_exists("feat/test"));
        checkout("feat/test").expect("checkout feat");
        write_file(&repo, "feature.txt", "feature\n");
        add_paths(&["feature.txt".to_string()]).expect("stage feature");
        commit("feat: add feature").expect("commit");

        let log = log_oneline("main..feat/test", 1).expect("log");
        assert_eq!(log.len(), 1);
        assert!(log[0].1.contains("feat: add feature"));
        assert!(log_oneline_time("feat/test").is_some());

        let wt_path = worktree_path("feat/test").expect("worktree path");
        assert!(wt_path.ends_with(".worktrees/feat-test"));

        checkout("main").expect("back to main");
        worktree_add(&wt_path, "feat/test").expect("worktree add");

        let worktrees = worktree_list().expect("worktree list");
        assert_eq!(worktrees.len(), 2);
        assert_eq!(
            std::fs::canonicalize(main_worktree_root().expect("main root"))
                .expect("canonicalized main root"),
            std::fs::canonicalize(&repo).expect("canonicalized repo")
        );
        assert_eq!(working_tree_status_at(&wt_path), (0, 0, 0));
        let repo_canonical = std::fs::canonicalize(&repo)
            .expect("canonical repo")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            branch_checked_out_elsewhere("feat/test", &repo_canonical).expect("checked elsewhere"),
            Some(wt_path.clone())
        );

        worktree_remove_force(&wt_path).expect("remove worktree");
        worktree_prune().expect("prune");
    }

    #[test]
    fn reset_branch_to_latest_remote_discards_local_divergence() {
        let _guard = take_env_lock();
        let repo = init_git_repo("git-reset-remote");
        let remote = temp_dir("git-reset-remote-origin");
        run_cmd(&remote, "git", &["init", "--bare", "--initial-branch=main"]);
        run_cmd(
            &repo,
            "git",
            &["remote", "add", "origin", remote.to_str().expect("remote")],
        );
        run_cmd(&repo, "git", &["push", "-u", "origin", "main"]);

        let updater = temp_dir("git-reset-remote-updater");
        run_cmd(
            &std::env::temp_dir(),
            "git",
            &[
                "clone",
                remote.to_str().expect("remote"),
                updater.to_str().expect("updater"),
            ],
        );
        run_cmd(&updater, "git", &["config", "user.name", "Test User"]);
        run_cmd(
            &updater,
            "git",
            &["config", "user.email", "test@example.com"],
        );
        write_file(&updater, "tracked.txt", "remote version\n");
        run_cmd(&updater, "git", &["add", "tracked.txt"]);
        run_cmd(&updater, "git", &["commit", "-m", "remote advance"]);
        run_cmd(&updater, "git", &["push", "origin", "main"]);

        let _cwd = CwdGuard::enter(&repo);
        write_file(&repo, "tracked.txt", "local divergence\n");
        add_paths(&["tracked.txt".to_string()]).expect("stage local divergence");
        commit("local divergence").expect("commit local divergence");
        let local_diverged = rev_parse("main").expect("local diverged");

        fetch("origin").expect("fetch origin");
        let updated =
            reset_branch_to_latest_remote("origin", "main", "main", &repo_root().expect("root"))
                .expect("reset branch");
        assert!(updated);
        assert_ne!(rev_parse("main").expect("post-reset"), local_diverged);
        assert_eq!(
            rev_parse("main").expect("main"),
            rev_parse("origin/main").expect("origin/main")
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked.txt")).expect("tracked"),
            "remote version\n"
        );
    }
}
