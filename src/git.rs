use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use crate::error::EzError;

static NEXT_WORKTREE_LOCK_CAS_ID: AtomicU64 = AtomicU64::new(1);

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
    let reference = format!("refs/heads/{name}");
    Command::new("git")
        .args(["show-ref", "--verify", "--quiet", reference.as_str()])
        .status()
        .is_ok_and(|status| status.success())
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

/// Move an existing local branch ref without checking it out, but only if it
/// still points at `expected_old`.
///
/// Callers must not use this for a branch that is checked out in a worktree,
/// because updating the ref behind that worktree would leave its index and files
/// out of sync with HEAD.
pub fn compare_and_swap_local_branch_ref(
    name: &str,
    target: &str,
    expected_old: &str,
) -> Result<()> {
    let local_ref = format!("refs/heads/{name}");
    run_git(&["update-ref", &local_ref, target, expected_old])?;
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

/// Run `git commit -m <message>` inside `dir`.
pub fn commit_at(dir: &str, message: &str) -> Result<()> {
    run_git(&["-C", dir, "commit", "-m", message])?;
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

/// Run `git cherry <upstream> <branch> <limit>` to inspect only commits after `limit`.
pub fn cherry_from(upstream: &str, branch: &str, limit: &str) -> Result<String> {
    run_git(&["cherry", upstream, branch, limit])
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

/// Stage all tracked modified/deleted files in `dir`.
pub fn add_all_at(dir: &str) -> Result<()> {
    run_git(&["-C", dir, "add", "-u"])?;
    Ok(())
}

/// Stage all changes (including untracked) in `dir`.
pub fn add_all_including_untracked_at(dir: &str) -> Result<()> {
    run_git(&["-C", dir, "add", "-A"])?;
    Ok(())
}

/// Return counts of (staged, modified, untracked) files in the working tree.
pub fn working_tree_status() -> (usize, usize, usize) {
    working_tree_status_for_args(&["status", "--porcelain"]).unwrap_or_default()
}

fn working_tree_status_for_args(args: &[&str]) -> Result<(usize, usize, usize)> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(EzError::GitError(stderr));
    }
    Ok(parse_working_tree_status(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_working_tree_status(output: &str) -> (usize, usize, usize) {
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

/// Returns true if `dir` has staged changes.
pub fn has_staged_changes_at(dir: &str) -> Result<bool> {
    let (success, _, _) = run_git_with_status(&["-C", dir, "diff", "--cached", "--quiet"])?;
    Ok(!success)
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

fn rebase_onto_impl(
    scope: Option<&str>,
    new_base: &str,
    old_base: &str,
    branch: Option<&str>,
) -> Result<RebaseOutcome> {
    if rebase_in_progress_impl(scope) {
        let location = scope.unwrap_or("the current worktree");
        bail!(EzError::UserMessage(format!(
            "a rebase is already in progress in `{location}`\n  → Finish or abort that rebase before retrying"
        )));
    }

    let mut args: Vec<&str> = Vec::new();
    if let Some(dir) = scope {
        args.extend(["-C", dir]);
    }
    args.extend([
        "-c",
        "rebase.autoStash=false",
        "rebase",
        "--onto",
        new_base,
        old_base,
    ]);
    if let Some(branch_name) = branch {
        args.push(branch_name);
    }

    let (success, _, stderr) = run_git_with_status(&args)?;

    let mut abort_args: Vec<&str> = Vec::new();
    if let Some(dir) = scope {
        abort_args.extend(["-C", dir]);
    }
    abort_args.extend(["rebase", "--abort"]);

    if success {
        Ok(RebaseOutcome::RebasingComplete)
    } else if stderr.contains("CONFLICT") || stderr.contains("conflict") {
        // Abort the rebase so we leave the repo in a clean state
        let _ = run_git(&abort_args);
        Ok(RebaseOutcome::Conflict(parse_rebase_conflict(&stderr)))
    } else {
        if rebase_in_progress_impl(scope) && !reports_existing_rebase(&stderr) {
            let _ = run_git(&abort_args);
        }
        bail!(EzError::GitError(stderr));
    }
}

pub fn rebase_onto(new_base: &str, old_base: &str, branch: &str) -> Result<RebaseOutcome> {
    rebase_onto_impl(None, new_base, old_base, Some(branch))
}

/// Rebase the branch checked out in `dir` onto `new_base`, dropping commits up to `old_base`.
pub fn rebase_onto_at(dir: &str, new_base: &str, old_base: &str) -> Result<RebaseOutcome> {
    rebase_onto_impl(Some(dir), new_base, old_base, None)
}

/// Rebase `branch` onto `new_base`, running the rebase in its worktree when checked out elsewhere.
pub fn rebase_onto_for_branch(
    new_base: &str,
    old_base: &str,
    branch: &str,
    current_root: &str,
) -> Result<RebaseOutcome> {
    if let Some(wt_path) = branch_checked_out_elsewhere(branch, current_root)? {
        verify_worktree_branch_at(&wt_path, branch)?;
        match rebase_onto_at(&wt_path, new_base, old_base) {
            Ok(outcome) => {
                verify_worktree_branch_at(&wt_path, branch)?;
                Ok(outcome)
            }
            Err(error) => Err(error),
        }
    } else {
        rebase_onto(new_base, old_base, branch)
    }
}

fn rebase_in_progress_impl(scope: Option<&str>) -> bool {
    for state_dir in ["rebase-merge", "rebase-apply"] {
        let mut args: Vec<&str> = Vec::new();
        if let Some(dir) = scope {
            args.extend(["-C", dir]);
        }
        args.extend(["rev-parse", "--git-path", state_dir]);

        let Ok(out) = run_git(&args) else { continue };
        let raw = out.trim();
        if raw.is_empty() {
            continue;
        }

        // `--git-path` returns a path relative to the worktree it was resolved in.
        let path = std::path::Path::new(raw);
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::path::Path::new(scope.unwrap_or(".")).join(path)
        };
        if resolved.exists() {
            return true;
        }
    }
    false
}

/// Abort a rebase left in progress for `branch`, in its worktree when it is checked out
/// elsewhere. Returns true if one was found and aborted.
///
/// `rebase_onto_impl` already aborts on the failures it recognizes; this is the belt-and-braces
/// cleanup for the cases it can't (abort itself failing, a rebase started outside ez) so that a
/// single stuck branch cannot make every later rebase in the same run fail too.
pub fn abort_rebase_for_branch(branch: &str, current_root: &str) -> Result<bool> {
    let worktree = branch_checked_out_elsewhere(branch, current_root)?;
    let scope = worktree.as_deref();

    if !rebase_in_progress_impl(scope) {
        return Ok(false);
    }

    let mut args: Vec<&str> = Vec::new();
    if let Some(dir) = scope {
        args.extend(["-C", dir]);
    }
    args.extend(["rebase", "--abort"]);
    let _ = run_git(&args);

    Ok(!rebase_in_progress_impl(scope))
}

fn parse_rebase_conflict(stderr: &str) -> RebaseConflict {
    RebaseConflict {
        conflicting_files: parse_conflicting_files(stderr),
        stderr: stderr.trim().to_string(),
    }
}

fn reports_existing_rebase(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("already a rebase-")
        || stderr.contains("another rebase")
        || stderr.contains("rebase already in progress")
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

fn rebase_impl(scope: Option<&str>, upstream: &str, branch: Option<&str>) -> Result<bool> {
    if rebase_in_progress_impl(scope) {
        let location = scope.unwrap_or("the current worktree");
        bail!(EzError::UserMessage(format!(
            "a rebase is already in progress in `{location}`\n  → Finish or abort that rebase before retrying"
        )));
    }

    let mut args: Vec<&str> = Vec::new();
    if let Some(dir) = scope {
        args.extend(["-C", dir]);
    }
    args.extend(["-c", "rebase.autoStash=false"]);
    args.push("rebase");
    args.push(upstream);
    if let Some(branch_name) = branch {
        args.push(branch_name);
    }

    let (success, _, stderr) = run_git_with_status(&args)?;

    let mut abort_args: Vec<&str> = Vec::new();
    if let Some(dir) = scope {
        abort_args.extend(["-C", dir]);
    }
    abort_args.extend(["rebase", "--abort"]);

    if success {
        Ok(true)
    } else if stderr.contains("CONFLICT") || stderr.contains("conflict") {
        let _ = run_git(&abort_args);
        Ok(false)
    } else {
        if rebase_in_progress_impl(scope) && !reports_existing_rebase(&stderr) {
            let _ = run_git(&abort_args);
        }
        bail!(EzError::GitError(stderr));
    }
}

/// Plain `git rebase <upstream> <branch>` — uses git's built-in patch-id detection
/// to auto-skip commits already applied upstream. Returns true on success.
pub fn rebase(upstream: &str, branch: &str) -> Result<bool> {
    rebase_impl(None, upstream, Some(branch))
}

/// Rebase the branch checked out in `dir` onto `upstream`.
pub fn rebase_at(dir: &str, upstream: &str) -> Result<bool> {
    rebase_impl(Some(dir), upstream, None)
}

/// Rebase `branch` onto `upstream`, running the rebase in its worktree when checked out elsewhere.
pub fn rebase_for_branch(upstream: &str, branch: &str, current_root: &str) -> Result<bool> {
    if let Some(wt_path) = branch_checked_out_elsewhere(branch, current_root)? {
        verify_worktree_branch_at(&wt_path, branch)?;
        match rebase_at(&wt_path, upstream) {
            Ok(outcome) => {
                verify_worktree_branch_at(&wt_path, branch)?;
                Ok(outcome)
            }
            Err(error) => Err(error),
        }
    } else {
        rebase(upstream, branch)
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

/// Move a checked-out branch while preserving concurrent worktree edits.
///
/// Unlike `reset --hard`, `reset --keep` refuses any move that would overwrite
/// local changes and retains compatible edits across the ref move.
pub fn reset_keep_at(dir: &str, target: &str) -> Result<()> {
    run_git(&["-C", dir, "reset", "--keep", target])?;
    Ok(())
}

/// Move a branch to `target` without detaching or dirtying its owning worktree.
///
/// Checked-out branches move through `reset --keep` in their owning worktree so
/// git preserves compatible local edits and refuses destructive moves. Branches
/// not checked out anywhere move through an expected-old `update-ref`.
pub fn align_branch_to_target(
    branch: &str,
    target: &str,
    expected_old: &str,
    current_root: &str,
) -> Result<()> {
    let current_tip = rev_parse(branch)?;
    if current_tip != expected_old {
        bail!(EzError::StaleRemoteRef(branch.to_string()));
    }

    let current_branch = current_branch().unwrap_or_default();
    if current_branch == branch {
        verify_worktree_branch_at(current_root, branch)?;
        reset_keep_at(current_root, target)?;
        verify_worktree_branch_at(current_root, branch)?;
        if rev_parse(branch)? != target {
            bail!(EzError::GitError(format!(
                "`{branch}` did not align to expected target `{target}`"
            )));
        }
        return Ok(());
    }

    if let Some(wt_path) = branch_checked_out_elsewhere(branch, current_root)? {
        verify_worktree_branch_at(&wt_path, branch)?;
        reset_keep_at(&wt_path, target)?;
        verify_worktree_branch_at(&wt_path, branch)?;
        if rev_parse(branch)? != target {
            bail!(EzError::GitError(format!(
                "`{branch}` did not align to expected target `{target}`"
            )));
        }
        return Ok(());
    }

    compare_and_swap_local_branch_ref(branch, target, expected_old)?;
    if rev_parse(branch)? != target {
        bail!(EzError::GitError(format!(
            "`{branch}` did not align to expected target `{target}`"
        )));
    }
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
    if is_stale_ref_error(&stderr) {
        bail!(crate::error::EzError::StaleRemoteRef(branch.to_string()));
    }
    bail!(crate::error::EzError::GitError(stderr));
}

pub fn push_atomic(remote: &str, branches: &[&str]) -> Result<()> {
    if branches.is_empty() {
        return Ok(());
    }

    let mut args = vec!["push", "--atomic", "--force-with-lease", remote];
    args.extend_from_slice(branches);

    let (success, _, stderr) = run_git_with_status(&args)?;
    if success {
        return Ok(());
    }
    if is_stale_ref_error(&stderr) {
        let branch =
            first_stale_branch_from_stderr(&stderr, branches).unwrap_or_else(|| branches.join(","));
        bail!(crate::error::EzError::StaleRemoteRef(branch));
    }
    bail!(crate::error::EzError::GitError(stderr));
}

fn first_stale_branch_from_stderr(stderr: &str, branches: &[&str]) -> Option<String> {
    stderr.lines().find_map(|line| {
        if !is_stale_ref_error(line) {
            return None;
        }
        branches
            .iter()
            .find(|branch| line.contains(**branch))
            .map(|branch| (*branch).to_string())
    })
}

fn is_stale_ref_error(stderr: &str) -> bool {
    stderr.contains("stale info")
        || stderr.contains("(stale)")
        || (stderr.contains("cannot lock ref") && stderr.contains("but expected"))
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

/// Count commits reachable from `tip` that are not reachable from `base`
/// (i.e. `git rev-list --count base..tip`). Returns 0 on parse or git error.
pub fn rev_list_count(base: &str, tip: &str) -> Result<u64> {
    let out = run_git(&["rev-list", "--count", &format!("{base}..{tip}")])?;
    Ok(out.trim().parse().unwrap_or(0))
}

/// Count merge commits reachable from `tip` that are not reachable from `base`.
pub fn rev_list_merge_count(base: &str, tip: &str) -> Result<u64> {
    let out = run_git(&["rev-list", "--merges", "--count", &format!("{base}..{tip}")])?;
    Ok(out.trim().parse().unwrap_or(0))
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

/// Read the configured URL for a remote (`git remote get-url <remote>`).
///
/// Returns the URL as configured in `.git/config`. Used to derive the
/// GitHub owner/repo without a network round-trip.
pub fn remote_url(remote: &str) -> Result<String> {
    run_git(&["remote", "get-url", remote])
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

pub fn fetch_pr_head(remote: &str, pr_number: u64) -> Result<String> {
    let remote_ref = format!("{remote}/pr/{pr_number}");
    let refspec = format!("refs/pull/{pr_number}/head:refs/remotes/{remote}/pr/{pr_number}");
    run_git(&["fetch", remote, &refspec])?;
    Ok(remote_ref)
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

/// Stash everything (staged + unstaged + untracked) with a custom label.
/// Returns `Ok(true)` when a stash was created, `Ok(false)` when there was
/// nothing to stash.
pub fn stash_push_with_untracked(message: &str) -> Result<bool> {
    if !has_uncommitted_changes()? {
        return Ok(false);
    }
    run_git(&["stash", "push", "--include-untracked", "-m", message])?;
    Ok(true)
}

pub fn stash_pop() -> Result<()> {
    run_git(&["stash", "pop"])?;
    Ok(())
}

/// `git stash pop --index` — restores staged-vs-unstaged distinction.
pub fn stash_pop_index() -> Result<()> {
    run_git(&["stash", "pop", "--index"])?;
    Ok(())
}

/// `git -C <dir> rev-parse <refspec>` — resolve a ref inside a specific worktree.
pub fn rev_parse_at(dir: &str, refspec: &str) -> Result<String> {
    run_git(&["-C", dir, "rev-parse", refspec])
}

/// Pop the latest stash inside `dir`, preserving the index state via `--index`.
/// Falls back to a plain pop if `--index` fails (e.g. when the staged tree no
/// longer cleanly applies). Returns Err only when neither attempt succeeds.
pub fn stash_pop_index_at(dir: &str) -> Result<()> {
    let (success, _, _) = run_git_with_status(&["-C", dir, "stash", "pop", "--index"]).unwrap_or((
        false,
        String::new(),
        String::new(),
    ));
    if success {
        return Ok(());
    }
    run_git(&["-C", dir, "stash", "pop"])?;
    Ok(())
}

/// Returns the path to the shared `.git` directory, even in linked worktrees.
///
/// `git rev-parse --git-common-dir` may return a relative path. That path is
/// relative to the current working directory, not the repo root, so nested
/// subdirectory invocations must resolve it against `std::env::current_dir()`.
pub fn git_common_dir() -> Result<PathBuf> {
    let out = run_git(&["rev-parse", "--git-common-dir"])?;
    let p = PathBuf::from(&out);
    if p.is_absolute() {
        return Ok(p);
    }
    let cwd = std::env::current_dir()?;
    Ok(cwd.join(p))
}

/// Information about a single git worktree.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    /// Absolute path to the worktree root.
    pub path: String,
    /// The branch checked out in this worktree, or None if detached HEAD.
    pub branch: Option<String>,
    /// Git's lock reason when this worktree is locked.
    pub locked_reason: Option<String>,
}

fn parse_worktree_list(output: &str) -> Vec<WorktreeInfo> {
    let mut worktrees = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_branch: Option<String> = None;
    let mut current_locked_reason: Option<String> = None;
    let fields: Vec<&str> = if output.contains('\0') {
        output.split('\0').collect()
    } else {
        output.lines().collect()
    };

    for line in fields {
        if line.is_empty() {
            if let Some(path) = current_path.take() {
                worktrees.push(WorktreeInfo {
                    path,
                    branch: current_branch.take(),
                    locked_reason: current_locked_reason.take(),
                });
            }
        } else if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(path.to_string());
            current_branch = None;
            current_locked_reason = None;
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            current_branch = branch_ref
                .strip_prefix("refs/heads/")
                .map(|s| s.to_string());
        } else if line == "locked" {
            current_locked_reason = Some(String::new());
        } else if let Some(reason) = line.strip_prefix("locked ") {
            current_locked_reason = Some(reason.to_string());
        }
        // Ignore HEAD sha, `detached`, and `bare` lines.
    }

    // Handle last block — some git versions omit trailing blank line.
    if let Some(path) = current_path {
        worktrees.push(WorktreeInfo {
            path,
            branch: current_branch,
            locked_reason: current_locked_reason,
        });
    }

    worktrees
}

/// Returns all git worktrees for this repository.
pub fn worktree_list() -> Result<Vec<WorktreeInfo>> {
    let output = run_git(&["worktree", "list", "--porcelain", "-z"])?;
    Ok(parse_worktree_list(&output))
}

pub fn worktree_add_locked_no_checkout(path: &str, branch: &str, reason: &str) -> Result<()> {
    run_git(&[
        "worktree",
        "add",
        "--no-checkout",
        "--lock",
        "--reason",
        reason,
        path,
        branch,
    ])?;
    Ok(())
}

pub fn worktree_checkout(path: &str, branch: &str) -> Result<()> {
    run_git(&["-C", path, "checkout", "--force", branch])?;
    Ok(())
}

pub fn worktree_unlock(path: &str) -> Result<()> {
    run_git(&["worktree", "unlock", path])?;
    Ok(())
}

/// Remove a worktree lock only when the lock file still contains `expected_reason`.
///
/// Git's `worktree unlock` is unconditional. Lease release and takeover instead
/// quarantine the administrative lock file atomically, validate the exact file
/// that was moved, and discard only that validated file. A raw Git process that
/// replaces the lock before this operation is therefore preserved.
pub fn worktree_unlock_if_reason(path: &str, expected_reason: &str) -> Result<()> {
    let resolved = PathBuf::from(run_git(&["-C", path, "rev-parse", "--git-path", "locked"])?);
    let lock_path = if resolved.is_absolute() {
        resolved
    } else {
        Path::new(path).join(resolved)
    };
    let quarantine = quarantine_worktree_lock(&lock_path).with_context(|| {
        format!(
            "worktree lock changed at `{path}` before it could be released (expected `{expected_reason}`)"
        )
    })?;

    let contents = match std::fs::read_to_string(&quarantine) {
        Ok(contents) => contents,
        Err(error) => {
            restore_quarantined_worktree_lock(&lock_path, &quarantine)?;
            return Err(error).context("read quarantined worktree lock");
        }
    };
    let actual_reason = contents.trim_end_matches(['\r', '\n']);
    if actual_reason != expected_reason {
        let preservation = match restore_quarantined_worktree_lock(&lock_path, &quarantine) {
            Ok(()) => "the replacement lock was restored".to_string(),
            Err(error) => format!(
                "a newer lock also exists; the replaced lock remains preserved at `{}` ({error})",
                quarantine.display()
            ),
        };
        bail!(
            "worktree lock changed at `{path}`: expected `{expected_reason}`, found `{actual_reason}`; {preservation}"
        );
    }

    std::fs::remove_file(&quarantine).with_context(|| {
        format!(
            "remove conditionally released worktree lock `{}`",
            quarantine.display()
        )
    })
}

fn quarantine_worktree_lock(lock_path: &Path) -> Result<PathBuf> {
    for _ in 0..100 {
        let quarantine = lock_path.with_file_name(format!(
            "locked.ez-cas-{}-{}",
            std::process::id(),
            NEXT_WORKTREE_LOCK_CAS_ID.fetch_add(1, Ordering::Relaxed)
        ));
        match rename_no_replace(lock_path, &quarantine) {
            Ok(()) => return Ok(quarantine),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!(
        "could not reserve a unique quarantine beside `{}`",
        lock_path.display()
    )
}

fn restore_quarantined_worktree_lock(lock_path: &Path, quarantine: &Path) -> Result<()> {
    rename_no_replace(quarantine, lock_path).with_context(|| {
        format!(
            "restore quarantined worktree lock from `{}` to `{}`",
            quarantine.display(),
            lock_path.display()
        )
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn c_path(path: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path contains a NUL byte: `{}`", path.display()),
        )
    })
}

#[cfg(target_os = "macos")]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    unsafe extern "C" {
        fn renamex_np(
            from: *const std::ffi::c_char,
            to: *const std::ffi::c_char,
            flags: std::ffi::c_uint,
        ) -> std::ffi::c_int;
    }

    const RENAME_EXCL: std::ffi::c_uint = 0x0000_0004;
    let from = c_path(from)?;
    let to = c_path(to)?;
    let result = unsafe { renamex_np(from.as_ptr(), to.as_ptr(), RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    unsafe extern "C" {
        fn syscall(number: std::ffi::c_long, ...) -> std::ffi::c_long;
    }

    #[cfg(target_arch = "x86_64")]
    const SYS_RENAMEAT2: std::ffi::c_long = 316;
    #[cfg(target_arch = "aarch64")]
    const SYS_RENAMEAT2: std::ffi::c_long = 276;
    const AT_FDCWD: std::ffi::c_int = -100;
    const RENAME_NOREPLACE: std::ffi::c_uint = 1;
    let from = c_path(from)?;
    let to = c_path(to)?;
    let result = unsafe {
        syscall(
            SYS_RENAMEAT2,
            AT_FDCWD,
            from.as_ptr(),
            AT_FDCWD,
            to.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
)))]
fn rename_no_replace(_from: &Path, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "conditional worktree unlock requires macOS or Linux on x86_64/aarch64",
    ))
}

pub fn worktree_lock(path: &str, reason: &str) -> Result<()> {
    run_git(&["worktree", "lock", "--reason", reason, path])?;
    Ok(())
}

pub fn worktree_repair(path: &str) -> Result<()> {
    run_git(&["worktree", "repair", path])?;
    Ok(())
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

fn verify_worktree_branch_at(path: &str, expected_branch: &str) -> Result<()> {
    let actual_branch = run_git(&["-C", path, "branch", "--show-current"])?;
    if actual_branch == expected_branch {
        return Ok(());
    }

    let actual = if actual_branch.is_empty() {
        "detached HEAD".to_string()
    } else {
        format!("`{actual_branch}`")
    };
    bail!(EzError::UserMessage(format!(
        "worktree ownership changed at `{path}`: expected `{expected_branch}`, found {actual}\n  → Retry after the worktree is attached to `{expected_branch}`"
    )));
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

/// Align a local branch to the fetched remote-tracking ref.
///
/// Checked-out branches move through `reset --keep` in their owning worktree so
/// compatible dirty edits survive and destructive moves are refused. Branches
/// not checked out anywhere are still ref-updated directly.
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
        verify_worktree_branch_at(current_root, branch)?;
        reset_keep_at(current_root, &remote_tracking)?;
        verify_worktree_branch_at(current_root, branch)?;
    } else if let Some(branch_worktree) = branch_checked_out_elsewhere(branch, current_root)? {
        verify_worktree_branch_at(&branch_worktree, branch)?;
        reset_keep_at(&branch_worktree, &remote_tracking)?;
        verify_worktree_branch_at(&branch_worktree, branch)?;
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

/// Strictly fetch a remote branch into its remote-tracking ref and return that ref.
///
/// Unlike `fetch_branch`, this does not swallow errors. It updates
/// `refs/remotes/<remote>/<branch>` from `refs/heads/<branch>` and fails when the
/// remote branch is missing or inaccessible.
pub fn fetch_remote_branch_ref(remote: &str, branch: &str) -> Result<String> {
    let remote_ref = format!("{remote}/{branch}");
    let refspec = format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}");
    run_git(&["fetch", remote, &refspec])?;
    Ok(remote_ref)
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
    working_tree_status_at_checked(dir).unwrap_or_default()
}

pub fn working_tree_status_at_checked(dir: &str) -> Result<(usize, usize, usize)> {
    working_tree_status_for_args(&["-C", dir, "status", "--porcelain"])
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
        let input = "worktree /repo/main\nHEAD abc123\nbranch refs/heads/main\n\nworktree /repo/feat-wt\nHEAD def456\nbranch refs/heads/feat/x\nlocked ez-worktree-ensure:1:1\n\n";
        let result = parse_worktree_list(input);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].path, "/repo/main");
        assert_eq!(result[0].branch.as_deref(), Some("main"));
        assert_eq!(result[0].locked_reason, None);
        assert_eq!(result[1].path, "/repo/feat-wt");
        assert_eq!(result[1].branch.as_deref(), Some("feat/x"));
        assert_eq!(
            result[1].locked_reason.as_deref(),
            Some("ez-worktree-ensure:1:1")
        );
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
    fn test_parse_worktree_list_nul_preserves_json_lock_reason() {
        let reason = r#"ez-lease:{"version":1,"owner":"agent a","branch":"feat/x","created_at":1,"expires_at":2}"#;
        let input = format!(
            "worktree /repo/main\0HEAD abc123\0branch refs/heads/main\0\0\
             worktree /repo/feature path\0HEAD def456\0branch refs/heads/feat/x\0locked {reason}\0\0"
        );

        let result = parse_worktree_list(&input);

        assert_eq!(result.len(), 2);
        assert_eq!(result[1].path, "/repo/feature path");
        assert_eq!(result[1].branch.as_deref(), Some("feat/x"));
        assert_eq!(result[1].locked_reason.as_deref(), Some(reason));
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
    fn rebase_onto_at_uses_worktree_directory() {
        let _guard = take_env_lock();
        let log_dir = crate::test_support::temp_dir("git-rebase-at");
        let log_path = log_dir.join("calls.log");
        let fake_dir = install_fake_bin(
            "git-rebase-at-bin",
            "git",
            &format!(
                r#"#!/bin/sh
echo "$@" >> "{}"
exit 0
"#,
                log_path.display()
            ),
        );
        let _path = PathGuard::install(&fake_dir);

        rebase_onto_at("/repo/.worktrees/feat-a", "main", "old-base").expect("rebase at");
        assert_eq!(
            std::fs::read_to_string(log_path).expect("log"),
            "-C /repo/.worktrees/feat-a rev-parse --git-path rebase-merge\n\
-C /repo/.worktrees/feat-a rev-parse --git-path rebase-apply\n\
-C /repo/.worktrees/feat-a -c rebase.autoStash=false rebase --onto main old-base\n"
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
if [ "$1" = "-c" ] && [ "$2" = "rebase.autoStash=false" ] && [ "$3" = "rebase" ] && [ "$4" = "--onto" ]; then
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
    fn rebase_onto_does_not_abort_state_that_won_the_preflight_race() {
        let _guard = take_env_lock();
        let fixture_dir = crate::test_support::temp_dir("git-rebase-race");
        let state_dir = fixture_dir.join("rebase-merge");
        let abort_log = fixture_dir.join("abort.log");
        let fake_dir = install_fake_bin(
            "git-rebase-race-bin",
            "git",
            &format!(
                r#"#!/bin/sh
if [ "$1" = "rev-parse" ] && [ "$2" = "--git-path" ] && [ "$3" = "rebase-merge" ]; then
  echo "{}"
  exit 0
fi
if [ "$1" = "rev-parse" ] && [ "$2" = "--git-path" ] && [ "$3" = "rebase-apply" ]; then
  echo "{}/rebase-apply"
  exit 0
fi
if [ "$1" = "-c" ] && [ "$2" = "rebase.autoStash=false" ] && [ "$3" = "rebase" ]; then
  mkdir -p "{}"
  echo "fatal: It seems that there is already a rebase-merge directory, and I wonder if you are in the middle of another rebase." >&2
  exit 1
fi
if [ "$1" = "rebase" ] && [ "$2" = "--abort" ]; then
  echo abort >> "{}"
  exit 0
fi
exit 0
"#,
                state_dir.display(),
                fixture_dir.display(),
                state_dir.display(),
                abort_log.display(),
            ),
        );
        let _path = PathGuard::install(&fake_dir);

        let error = rebase_onto("main", "old-base", "feature")
            .expect_err("racing rebase state should fail");

        assert!(error.to_string().contains("another rebase"));
        assert!(state_dir.exists(), "concurrent rebase state must survive");
        assert!(!abort_log.exists(), "ez must not abort the racing rebase");
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
    fn push_atomic_uses_one_force_with_lease_atomic_invocation() {
        let _guard = take_env_lock();
        let log_dir = crate::test_support::temp_dir("git-push-atomic");
        let log_path = log_dir.join("calls.log");
        let fake_dir = install_fake_bin(
            "git-push-atomic-bin",
            "git",
            &format!(
                r#"#!/bin/sh
echo "$@" >> "{}"
exit 0
"#,
                log_path.display()
            ),
        );
        let _path = PathGuard::install(&fake_dir);

        push_atomic("origin", &["feat/a", "feat/b"]).expect("atomic push");

        assert_eq!(
            std::fs::read_to_string(log_path).expect("log"),
            "push --atomic --force-with-lease origin feat/a feat/b\n"
        );
    }

    #[test]
    fn push_atomic_empty_branch_list_is_noop() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "git-push-atomic-empty-bin",
            "git",
            r#"#!/bin/sh
echo "git should not be invoked" >&2
exit 1
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        push_atomic("origin", &[]).expect("empty atomic push");
    }

    #[test]
    fn push_atomic_maps_stale_ref_to_first_mentioned_branch() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "git-push-atomic-stale-bin",
            "git",
            r#"#!/bin/sh
printf '%s\n' '! [rejected] feat/b -> feat/b (stale info)' >&2
printf '%s\n' '! [rejected] feat/a -> feat/a (stale info)' >&2
exit 1
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        let err = push_atomic("origin", &["feat/a", "feat/b"]).expect_err("stale ref");
        assert!(
            matches!(
                err.downcast_ref::<EzError>(),
                Some(EzError::StaleRemoteRef(branch)) if branch == "feat/b"
            ),
            "unexpected error: {err:#}"
        );
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
        run_cmd(&repo, "git", &["tag", "tag-only", "main"]);
        assert!(!branch_exists("tag-only"));
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
        assert_eq!(working_tree_status(), (0, 1, 1));

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
        verify_worktree_branch_at(&wt_path, "feat/test").expect("verified worktree owner");

        create_branch_at("feat/other", "main").expect("create alternate branch");
        run_cmd(
            std::path::Path::new(&wt_path),
            "git",
            &["checkout", "feat/other"],
        );
        let mismatch = verify_worktree_branch_at(&wt_path, "feat/test")
            .expect_err("worktree owner change must be detected");
        assert!(mismatch.to_string().contains("expected `feat/test`"));
        assert!(mismatch.to_string().contains("found `feat/other`"));
        run_cmd(
            std::path::Path::new(&wt_path),
            "git",
            &["checkout", "feat/test"],
        );

        worktree_remove_force(&wt_path).expect("remove worktree");
        worktree_prune().expect("prune");
    }

    #[test]
    fn conditional_worktree_unlock_removes_only_the_expected_real_git_lock() {
        let _guard = take_env_lock();
        let repo = init_git_repo("git-worktree-conditional-unlock");
        let _cwd = CwdGuard::enter(&repo);
        create_branch_at("feat/lease", "main").expect("create branch");
        let wt_path = worktree_path("feat/lease").expect("worktree path");
        worktree_add(&wt_path, "feat/lease").expect("worktree add");
        worktree_lock(&wt_path, "ez-lease:expected").expect("lock worktree");

        worktree_unlock_if_reason(&wt_path, "ez-lease:expected")
            .expect("unlock exact expected reason");

        let worktree = worktree_list()
            .expect("list worktrees")
            .into_iter()
            .find(|worktree| worktree.path == wt_path)
            .expect("linked worktree");
        assert_eq!(worktree.locked_reason, None);
    }

    #[test]
    fn conditional_worktree_unlock_preserves_a_raw_git_replacement_lock() {
        let _guard = take_env_lock();
        let repo = init_git_repo("git-worktree-conditional-race");
        let _cwd = CwdGuard::enter(&repo);
        create_branch_at("feat/lease", "main").expect("create branch");
        let wt_path = worktree_path("feat/lease").expect("worktree path");
        worktree_add(&wt_path, "feat/lease").expect("worktree add");
        worktree_lock(&wt_path, "ez-lease:observed").expect("lock observed lease");

        run_cmd(&repo, "git", &["worktree", "unlock", &wt_path]);
        run_cmd(
            &repo,
            "git",
            &[
                "worktree",
                "lock",
                "--reason",
                "raw-git replacement",
                &wt_path,
            ],
        );

        let error = worktree_unlock_if_reason(&wt_path, "ez-lease:observed")
            .expect_err("replacement lock must survive");
        assert!(
            error.to_string().contains("raw-git replacement"),
            "unexpected error: {error:#}"
        );
        let worktree = worktree_list()
            .expect("list worktrees")
            .into_iter()
            .find(|worktree| worktree.path == wt_path)
            .expect("linked worktree");
        assert_eq!(
            worktree.locked_reason.as_deref(),
            Some("raw-git replacement")
        );
    }

    #[test]
    fn quarantined_lock_restore_never_clobbers_a_newer_lock() {
        let dir = temp_dir("git-worktree-lock-restore-no-clobber");
        let lock_path = dir.join("locked");
        let quarantine = dir.join("locked.ez-cas");
        std::fs::write(&lock_path, "newer foreign lock\n").expect("write newer lock");
        std::fs::write(&quarantine, "quarantined foreign lock\n").expect("write quarantined lock");

        restore_quarantined_worktree_lock(&lock_path, &quarantine)
            .expect_err("restore must not replace a newer lock");

        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("newer lock remains"),
            "newer foreign lock\n"
        );
        assert_eq!(
            std::fs::read_to_string(&quarantine).expect("quarantine remains"),
            "quarantined foreign lock\n"
        );
    }

    #[test]
    fn fetch_remote_branch_ref_strictly_updates_remote_tracking_ref() {
        let _guard = take_env_lock();
        let remote = temp_dir("git-fetch-remote-branch-bare");
        run_cmd(&remote, "git", &["init", "--bare", "-b", "main"]);
        let repo = init_git_repo("git-fetch-remote-branch");
        let _cwd = CwdGuard::enter(&repo);
        run_cmd(
            &repo,
            "git",
            &["remote", "add", "origin", remote.to_str().expect("remote")],
        );
        run_cmd(&repo, "git", &["push", "origin", "main"]);

        create_branch("feat/remote").expect("create remote branch");
        write_file(&repo, "remote.txt", "remote\n");
        add_all_including_untracked().expect("stage remote");
        commit("remote branch").expect("commit remote branch");
        let remote_head = rev_parse("feat/remote").expect("remote head");
        run_cmd(&repo, "git", &["push", "origin", "feat/remote"]);
        checkout("main").expect("checkout main");
        delete_branch("feat/remote", true).expect("delete local branch");

        let remote_ref = fetch_remote_branch_ref("origin", "feat/remote").expect("fetch branch");

        assert_eq!(remote_ref, "origin/feat/remote");
        assert_eq!(rev_parse(&remote_ref).expect("remote ref"), remote_head);
        assert!(fetch_remote_branch_ref("origin", "feat/missing").is_err());
    }

    #[test]
    fn git_common_dir_resolves_from_nested_subdirectory() {
        let _guard = take_env_lock();
        let repo = init_git_repo("git-common-dir-subdir");
        std::fs::create_dir_all(repo.join("backend/api")).expect("create nested dirs");
        let _cwd = CwdGuard::enter(&repo.join("backend/api"));

        assert_eq!(
            std::fs::canonicalize(git_common_dir().expect("git common dir"))
                .expect("canonicalized common dir"),
            std::fs::canonicalize(repo.join(".git")).expect("canonicalized repo git dir")
        );
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

    #[test]
    fn reset_branch_to_latest_remote_preserves_compatible_dirty_tracked_edit() {
        let _guard = take_env_lock();
        let repo = init_git_repo("git-reset-remote-dirty-compatible");
        let remote = temp_dir("git-reset-remote-dirty-compatible-origin");
        run_cmd(&remote, "git", &["init", "--bare", "--initial-branch=main"]);
        run_cmd(
            &repo,
            "git",
            &["remote", "add", "origin", remote.to_str().expect("remote")],
        );
        write_file(&repo, "other.txt", "base other\n");
        run_cmd(&repo, "git", &["add", "other.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "add other"]);
        run_cmd(&repo, "git", &["push", "-u", "origin", "main"]);

        let updater = temp_dir("git-reset-remote-dirty-compatible-updater");
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
        write_file(&updater, "other.txt", "remote other\n");
        run_cmd(&updater, "git", &["add", "other.txt"]);
        run_cmd(&updater, "git", &["commit", "-m", "remote advance other"]);
        run_cmd(&updater, "git", &["push", "origin", "main"]);

        let _cwd = CwdGuard::enter(&repo);
        let old_main = rev_parse("main").expect("old main");
        write_file(&repo, "tracked.txt", "compatible dirty edit\n");

        fetch("origin").expect("fetch origin");
        let updated =
            reset_branch_to_latest_remote("origin", "main", "main", &repo_root().expect("root"))
                .expect("reset branch");

        assert!(updated);
        assert_ne!(rev_parse("main").expect("post-reset"), old_main);
        assert_eq!(
            rev_parse("main").expect("main"),
            rev_parse("origin/main").expect("origin/main")
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked.txt")).expect("tracked"),
            "compatible dirty edit\n"
        );
        let status = run_git(&["status", "--porcelain"]).expect("status");
        assert!(
            status.lines().any(|line| line.ends_with(" tracked.txt")),
            "{status}"
        );
    }

    #[test]
    fn default_branch_uses_origin_head_before_local_fallbacks() {
        let _guard = take_env_lock();
        let remote = temp_dir("git-default-branch-origin-head-remote");
        run_cmd(
            &remote,
            "git",
            &["init", "--bare", "--initial-branch=trunk"],
        );

        let seed = temp_dir("git-default-branch-origin-head-seed");
        run_cmd(
            &seed,
            "git",
            &["clone", remote.to_str().expect("remote"), "."],
        );
        run_cmd(&seed, "git", &["config", "user.name", "Test User"]);
        run_cmd(&seed, "git", &["config", "user.email", "test@example.com"]);
        write_file(&seed, "tracked.txt", "hello\n");
        run_cmd(&seed, "git", &["add", "tracked.txt"]);
        run_cmd(&seed, "git", &["commit", "-m", "initial"]);
        run_cmd(&seed, "git", &["push", "-u", "origin", "trunk"]);

        let repo = temp_dir("git-default-branch-origin-head");
        run_cmd(
            &std::env::temp_dir(),
            "git",
            &[
                "clone",
                remote.to_str().expect("remote"),
                repo.to_str().expect("repo"),
            ],
        );
        let _cwd = CwdGuard::enter(&repo);

        assert_eq!(default_branch().expect("default branch"), "trunk");
    }

    #[test]
    fn default_branch_errors_when_remote_head_and_common_branches_are_absent() {
        let _guard = take_env_lock();
        let repo = temp_dir("git-default-branch-missing");
        run_cmd(&repo, "git", &["init", "-b", "develop"]);
        run_cmd(&repo, "git", &["config", "user.name", "Test User"]);
        run_cmd(&repo, "git", &["config", "user.email", "test@example.com"]);
        write_file(&repo, "tracked.txt", "hello\n");
        run_cmd(&repo, "git", &["add", "tracked.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "initial"]);
        let _cwd = CwdGuard::enter(&repo);

        let error = default_branch().expect_err("default branch should be unknown");

        assert!(
            error
                .to_string()
                .contains("could not detect default branch"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn align_branch_to_target_rejects_stale_expected_ref() {
        let _guard = take_env_lock();
        let repo = init_git_repo("git-align-stale-expected");
        let _cwd = CwdGuard::enter(&repo);

        let expected_old = rev_parse("main").expect("old main");
        create_branch_at("target", "main").expect("create target");
        checkout("target").expect("checkout target");
        write_file(&repo, "target.txt", "target\n");
        add_all_including_untracked().expect("stage target");
        commit("target advance").expect("commit target");
        let target = rev_parse("target").expect("target sha");
        checkout("main").expect("checkout main");
        write_file(&repo, "local.txt", "local\n");
        add_all_including_untracked().expect("stage local");
        commit("local advance").expect("commit local");

        let error =
            align_branch_to_target("main", &target, &expected_old, &repo_root().expect("root"))
                .expect_err("stale expected ref should fail");

        assert!(
            matches!(
                error.downcast_ref::<EzError>(),
                Some(EzError::StaleRemoteRef(branch)) if branch == "main"
            ),
            "unexpected error: {error:#}"
        );
        assert_ne!(rev_parse("main").expect("main"), target);
    }

    #[test]
    fn align_branch_to_target_refuses_to_overwrite_dirty_checked_out_file() {
        let _guard = take_env_lock();
        let repo = init_git_repo("git-align-dirty-current");
        let _cwd = CwdGuard::enter(&repo);

        let expected_old = rev_parse("main").expect("old main");
        create_branch_at("target", "main").expect("create target");
        checkout("target").expect("checkout target");
        write_file(&repo, "tracked.txt", "target version\n");
        add_paths(&["tracked.txt".to_string()]).expect("stage target");
        commit("target edits tracked").expect("commit target");
        let target = rev_parse("target").expect("target sha");
        checkout("main").expect("checkout main");
        write_file(&repo, "tracked.txt", "dirty local version\n");

        let error =
            align_branch_to_target("main", &target, &expected_old, &repo_root().expect("root"))
                .expect_err("dirty checked out file should block reset --keep");

        assert!(
            error
                .to_string()
                .contains("Entry 'tracked.txt' not uptodate")
                || error.to_string().contains("local changes"),
            "unexpected error: {error:#}"
        );
        assert_eq!(rev_parse("main").expect("main"), expected_old);
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked.txt")).expect("tracked"),
            "dirty local version\n"
        );
    }

    #[test]
    fn align_branch_to_target_moves_unchecked_out_branch_with_compare_and_swap() {
        let _guard = take_env_lock();
        let repo = init_git_repo("git-align-unchecked-branch");
        let _cwd = CwdGuard::enter(&repo);

        create_branch_at("feat/unowned", "main").expect("create feature");
        let expected_old = rev_parse("feat/unowned").expect("feature old");
        write_file(&repo, "target.txt", "target\n");
        add_all_including_untracked().expect("stage target");
        commit("advance main").expect("commit target");
        let target = rev_parse("main").expect("target");

        align_branch_to_target(
            "feat/unowned",
            &target,
            &expected_old,
            &repo_root().expect("root"),
        )
        .expect("align unchecked branch");

        assert_eq!(rev_parse("feat/unowned").expect("feature"), target);
        assert_eq!(current_branch().expect("current branch"), "main");
    }

    #[test]
    fn align_branch_to_target_moves_branch_in_linked_worktree_without_detaching_it() {
        let _guard = take_env_lock();
        let repo = init_git_repo("git-align-linked-worktree");
        let _cwd = CwdGuard::enter(&repo);

        create_branch_at("feat/linked", "main").expect("create feature");
        let expected_old = rev_parse("feat/linked").expect("feature old");
        let wt_path = worktree_path("feat/linked").expect("worktree path");
        worktree_add(&wt_path, "feat/linked").expect("worktree add");
        write_file(&repo, "target.txt", "target\n");
        add_all_including_untracked().expect("stage target");
        commit("advance main").expect("commit target");
        let target = rev_parse("main").expect("target");

        align_branch_to_target(
            "feat/linked",
            &target,
            &expected_old,
            &repo_root().expect("root"),
        )
        .expect("align linked worktree branch");

        assert_eq!(rev_parse("feat/linked").expect("feature"), target);
        assert_eq!(
            rev_parse_at(&wt_path, "HEAD").expect("worktree head"),
            target
        );
        assert_eq!(
            run_git(&["-C", &wt_path, "branch", "--show-current"]).expect("worktree branch"),
            "feat/linked"
        );
        assert_eq!(
            std::fs::read_to_string(std::path::Path::new(&wt_path).join("target.txt"))
                .expect("target file"),
            "target\n"
        );
    }

    #[test]
    fn current_linked_worktree_root_returns_none_in_main_and_some_in_linked_worktree() {
        let _guard = take_env_lock();
        let repo = init_git_repo("git-current-linked-worktree");
        let _cwd = CwdGuard::enter(&repo);

        create_branch_at("feat/current-linked", "main").expect("create feature");
        let wt_path = worktree_path("feat/current-linked").expect("worktree path");
        worktree_add(&wt_path, "feat/current-linked").expect("worktree add");

        assert_eq!(
            current_linked_worktree_root().expect("main linked root"),
            None
        );

        let _linked_cwd = CwdGuard::enter(std::path::Path::new(&wt_path));
        assert_eq!(
            std::fs::canonicalize(
                current_linked_worktree_root()
                    .expect("linked root")
                    .expect("linked worktree")
            )
            .expect("canonical actual"),
            std::fs::canonicalize(&wt_path).expect("canonical expected")
        );
    }

    #[test]
    fn push_maps_stale_rejection_to_branch_error() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "git-push-stale-bin",
            "git",
            r#"#!/bin/sh
if [ "$1" = "push" ]; then
  printf '%s\n' '! [rejected] feat/a -> feat/a (stale info)' >&2
  exit 1
fi
exit 0
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        let error = push("origin", "feat/a", true).expect_err("stale push");

        assert!(
            matches!(
                error.downcast_ref::<EzError>(),
                Some(EzError::StaleRemoteRef(branch)) if branch == "feat/a"
            ),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn stash_pop_index_at_falls_back_to_plain_pop_when_index_restore_fails() {
        let _guard = take_env_lock();
        let log_dir = temp_dir("git-stash-pop-index-fallback");
        let log_path = log_dir.join("calls.log");
        let fake_dir = install_fake_bin(
            "git-stash-pop-index-fallback-bin",
            "git",
            &format!(
                r#"#!/bin/sh
echo "$@" >> "{}"
if [ "$1" = "-C" ] && [ "$3" = "stash" ] && [ "$4" = "pop" ] && [ "$5" = "--index" ]; then
  exit 1
fi
exit 0
"#,
                log_path.display()
            ),
        );
        let _path = PathGuard::install(&fake_dir);

        stash_pop_index_at("/repo/worktree").expect("fallback pop");

        assert_eq!(
            std::fs::read_to_string(log_path).expect("call log"),
            "-C /repo/worktree stash pop --index\n-C /repo/worktree stash pop\n"
        );
    }
}

/// Commits in `range`, oldest first, as (full sha, subject).
///
/// Oldest-first is the order a stack is built in — the bottom layer is the first commit — and full
/// SHAs are what stack metadata records, so callers do not have to re-resolve abbreviations.
pub fn log_commits_oldest_first(range: &str) -> Result<Vec<(String, String)>> {
    let output = run_git(&["log", "--reverse", "--format=%H%x1f%s", range])?;
    Ok(output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (sha, subject) = line.split_once('\x1f').unwrap_or((line, ""));
            (sha.trim().to_string(), subject.to_string())
        })
        .collect())
}
