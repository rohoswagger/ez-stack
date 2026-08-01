use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;
use crate::worktree_lease::{Lease, LeaseMutationGuard, LeaseView, now_unix, parse_ttl};

/// `ez worktree create` is now an alias for `ez create` (worktree is the default).
pub fn create(name: &str, from: Option<&str>) -> Result<()> {
    crate::cmd::create::run(name, None, false, false, from, false, &[], None, None)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct LeaseEntry {
    branch: String,
    path: String,
    lease: Option<LeaseView>,
    foreign_lock_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct LeasesResult {
    cmd: String,
    entries: Vec<LeaseEntry>,
    active_count: usize,
    stale_count: usize,
    foreign_lock_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ClaimResult {
    cmd: String,
    branch: String,
    path: String,
    claimed: bool,
    lease: LeaseView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ReleaseResult {
    cmd: String,
    branch: String,
    path: String,
    released: bool,
}

pub fn claim(
    branch: Option<&str>,
    owner: &str,
    ttl: &str,
    break_stale: bool,
    json: bool,
) -> Result<()> {
    if owner.trim().is_empty() {
        bail!(EzError::UserMessage(
            "lease owner cannot be empty\n  → Pass a stable identity with `--owner <name>`"
                .to_string()
        ));
    }
    let ttl_seconds = parse_ttl(ttl).map_err(|error| {
        EzError::UserMessage(format!(
            "invalid lease TTL `{ttl}`: {error}\n  → Use a positive duration such as `30m`, `4h`, or `1d`"
        ))
    })?;
    let _mutation_guard =
        LeaseMutationGuard::acquire(&format!("claim {}", branch.unwrap_or("<current>")))?;
    let (target, worktree) = linked_worktree_for(branch)?;
    let now = now_unix()?;
    let lease = Lease::new(owner, &target, now, ttl_seconds).map_err(|error| {
        EzError::UserMessage(format!(
            "could not create lease: {error}\n  → Use a non-empty owner and a shorter TTL"
        ))
    })?;
    let reason = lease.reason()?;
    let mut claimed = true;

    if let Some(existing_reason) = worktree.locked_reason.as_deref() {
        let Some(existing) = parse_matching_lease(existing_reason, &target) else {
            bail!(EzError::UserMessage(format!(
                "worktree `{}` is protected by a foreign Git lock: `{}`\n  \
                 → Preserve that lock or remove it manually with `git worktree unlock {}`",
                worktree.path,
                display_lock_reason(existing_reason),
                worktree.path
            )));
        };
        let existing_view = existing.view(now);
        if !existing_view.stale {
            if existing.owner == owner {
                claimed = false;
                return emit_claim_result(
                    ClaimResult {
                        cmd: "worktree.claim".to_string(),
                        branch: target,
                        path: worktree.path,
                        claimed,
                        lease: existing_view,
                    },
                    json,
                );
            }
            bail!(EzError::UserMessage(format!(
                "worktree `{}` is actively claimed by `{}` until Unix timestamp {}\n  \
                 → Coordinate with that owner or wait for the lease to expire",
                worktree.path, existing.owner, existing.expires_at
            )));
        }
        if !break_stale {
            bail!(EzError::UserMessage(format!(
                "worktree `{}` has a stale ez lease owned by `{}`\n  \
                 → Retry with `--break-stale` to claim it explicitly",
                worktree.path, existing.owner
            )));
        }

        git::worktree_unlock_if_reason(&worktree.path, existing_reason)?;
        if let Err(error) = git::worktree_lock(&worktree.path, &reason) {
            let rollback = git::worktree_lock(&worktree.path, existing_reason).err();
            if let Some(rollback_error) = rollback {
                bail!(
                    "could not replace stale worktree lease: {error}\n\
                     restoring the prior stale lease also failed: {rollback_error}"
                );
            }
            return Err(error).context("replace stale worktree lease");
        }
    } else {
        git::worktree_lock(&worktree.path, &reason)?;
    }

    if let Err(error) = verify_exact_lock(&worktree.path, &target, Some(&reason)) {
        release_attempted_lock(&worktree.path, &reason);
        return Err(error);
    }
    emit_claim_result(
        ClaimResult {
            cmd: "worktree.claim".to_string(),
            branch: target,
            path: worktree.path,
            claimed,
            lease: lease.view(now),
        },
        json,
    )
}

pub fn release(branch: Option<&str>, owner: Option<&str>, force: bool, json: bool) -> Result<()> {
    if !force && owner.is_none_or(|value| value.trim().is_empty()) {
        bail!(EzError::UserMessage(
            "lease owner is required for a normal release\n  \
             → Pass `--owner <name>`, or use `--force` only for an ez lease you intend to break"
                .to_string()
        ));
    }
    let _mutation_guard =
        LeaseMutationGuard::acquire(&format!("release {}", branch.unwrap_or("<current>")))?;
    let (target, worktree) = linked_worktree_for(branch)?;
    let Some(reason) = worktree.locked_reason.as_deref() else {
        return emit_release_result(
            ReleaseResult {
                cmd: "worktree.release".to_string(),
                branch: target,
                path: worktree.path,
                released: false,
            },
            json,
        );
    };
    let Some(lease) = parse_matching_lease(reason, &target) else {
        bail!(EzError::UserMessage(format!(
            "worktree `{}` is protected by a foreign Git lock: `{}`\n  \
             → `ez worktree release` never removes foreign locks",
            worktree.path,
            display_lock_reason(reason)
        )));
    };
    if !force && owner != Some(lease.owner.as_str()) {
        bail!(EzError::UserMessage(format!(
            "worktree `{}` is claimed by `{}`, not `{}`\n  \
             → Ask the owner to release it, or use `--force` if takeover is intentional",
            worktree.path,
            lease.owner,
            owner.unwrap_or_default()
        )));
    }

    verify_exact_lock(&worktree.path, &target, Some(reason))?;
    git::worktree_unlock_if_reason(&worktree.path, reason)?;
    verify_exact_lock(&worktree.path, &target, None)?;
    emit_release_result(
        ReleaseResult {
            cmd: "worktree.release".to_string(),
            branch: target,
            path: worktree.path,
            released: true,
        },
        json,
    )
}

pub fn leases(json: bool) -> Result<()> {
    let state = StackState::load()?;
    let worktrees = git::worktree_list()?;
    let main_path = worktrees
        .first()
        .map(|worktree| worktree.path.as_str())
        .ok_or_else(|| anyhow::anyhow!("could not determine main worktree root"))?;
    let worktrees_by_branch: HashMap<&str, &git::WorktreeInfo> = worktrees
        .iter()
        .filter(|worktree| worktree.path != main_path)
        .filter_map(|worktree| worktree.branch.as_deref().map(|branch| (branch, worktree)))
        .collect();
    let now = now_unix()?;
    let entries = deterministic_topo_order(&state)
        .into_iter()
        .filter_map(|branch| {
            worktrees_by_branch
                .get(branch.as_str())
                .map(|worktree| lease_entry(worktree, now))
        })
        .collect::<Vec<_>>();
    let active_count = entries
        .iter()
        .filter(|entry| entry.lease.as_ref().is_some_and(|lease| !lease.stale))
        .count();
    let stale_count = entries
        .iter()
        .filter(|entry| entry.lease.as_ref().is_some_and(|lease| lease.stale))
        .count();
    let foreign_lock_count = entries
        .iter()
        .filter(|entry| entry.foreign_lock_reason.is_some())
        .count();
    let result = LeasesResult {
        cmd: "worktree.leases".to_string(),
        entries,
        active_count,
        stale_count,
        foreign_lock_count,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        for entry in &result.entries {
            let status = match (&entry.lease, &entry.foreign_lock_reason) {
                (Some(lease), _) if lease.stale => {
                    format!(
                        "stale lease: {} (expired {})",
                        lease.owner, lease.expires_at
                    )
                }
                (Some(lease), _) => {
                    format!("claimed: {} (expires {})", lease.owner, lease.expires_at)
                }
                (None, Some(reason)) => format!("foreign lock: {reason}"),
                (None, None) => "available".to_string(),
            };
            eprintln!("{:<30} {:<18} {}", entry.branch, status, entry.path);
        }
    }
    ui::receipt(&serde_json::to_value(&result)?);
    Ok(())
}

fn emit_claim_result(result: ClaimResult, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if result.claimed {
        ui::success(&format!(
            "Claimed `{}` worktree for `{}` until Unix timestamp {}",
            result.branch, result.lease.owner, result.lease.expires_at
        ));
    } else {
        ui::success(&format!(
            "`{}` is already claimed by `{}` until Unix timestamp {}",
            result.branch, result.lease.owner, result.lease.expires_at
        ));
    }
    ui::receipt(&serde_json::to_value(&result)?);
    Ok(())
}

fn emit_release_result(result: ReleaseResult, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if result.released {
        ui::success(&format!("Released `{}` worktree", result.branch));
    } else {
        ui::info(&format!(
            "`{}` worktree is already available",
            result.branch
        ));
    }
    ui::receipt(&serde_json::to_value(&result)?);
    Ok(())
}

fn linked_worktree_for(branch: Option<&str>) -> Result<(String, git::WorktreeInfo)> {
    let state = StackState::load()?;
    let target = branch
        .map(str::to_string)
        .map(Ok)
        .unwrap_or_else(git::current_branch)?;
    if state.is_trunk(&target) {
        bail!(EzError::UserMessage(format!(
            "`{target}` is trunk and cannot be claimed as an agent workspace\n  \
             → Claim a managed stack layer in a linked worktree"
        )));
    }
    if !state.is_managed(&target) {
        bail!(EzError::BranchNotInStack(target));
    }

    let worktrees = git::worktree_list()?;
    let main_path = worktrees
        .first()
        .map(|worktree| worktree.path.as_str())
        .ok_or_else(|| anyhow::anyhow!("could not determine main worktree root"))?;
    let Some(worktree) = worktrees
        .iter()
        .find(|worktree| worktree.branch.as_deref() == Some(target.as_str()))
        .cloned()
    else {
        bail!(EzError::UserMessage(format!(
            "`{target}` has no linked worktree to claim\n  \
             → Run `ez worktree ensure {target}` first"
        )));
    };
    if worktree.path == main_path {
        bail!(EzError::UserMessage(format!(
            "`{target}` is checked out in the main worktree, which is not claimable\n  \
             → Switch main back to trunk, then run `ez worktree ensure {target}`"
        )));
    }
    Ok((target, worktree))
}

fn verify_exact_lock(path: &str, branch: &str, expected_reason: Option<&str>) -> Result<()> {
    let worktrees = git::worktree_list()?;
    let Some(worktree) = worktrees.iter().find(|worktree| worktree.path == path) else {
        bail!("worktree `{path}` disappeared while updating its lease");
    };
    if worktree.branch.as_deref() != Some(branch) {
        bail!(
            "worktree ownership changed at `{path}`: expected `{branch}`, found `{}`",
            worktree.branch.as_deref().unwrap_or("<detached HEAD>")
        );
    }
    if worktree.locked_reason.as_deref() != expected_reason {
        bail!(
            "worktree lock changed at `{path}`: expected `{}`, found `{}`",
            expected_reason.unwrap_or("<unlocked>"),
            worktree
                .locked_reason
                .as_deref()
                .map(display_lock_reason)
                .unwrap_or_else(|| "<unlocked>".to_string())
        );
    }
    Ok(())
}

fn release_attempted_lock(path: &str, attempted_reason: &str) {
    let still_ours = git::worktree_list().is_ok_and(|worktrees| {
        worktrees.iter().any(|worktree| {
            worktree.path == path && worktree.locked_reason.as_deref() == Some(attempted_reason)
        })
    });
    if still_ours {
        let _ = git::worktree_unlock_if_reason(path, attempted_reason);
    }
}

fn lease_entry(worktree: &git::WorktreeInfo, now: u64) -> LeaseEntry {
    let branch = worktree.branch.clone().unwrap_or_default();
    let lease = worktree
        .locked_reason
        .as_deref()
        .and_then(|reason| parse_matching_lease(reason, &branch))
        .map(|lease| lease.view(now));
    let foreign_lock_reason = worktree.locked_reason.as_ref().and_then(|reason| {
        lease
            .is_none()
            .then(|| display_lock_reason(reason.as_str()))
    });
    LeaseEntry {
        branch,
        path: worktree.path.clone(),
        lease,
        foreign_lock_reason,
    }
}

fn parse_matching_lease(reason: &str, branch: &str) -> Option<Lease> {
    Lease::parse_reason(reason).filter(|lease| lease.branch == branch)
}

fn display_lock_reason(reason: &str) -> String {
    if reason.is_empty() {
        "<no reason>".to_string()
    } else {
        reason.to_string()
    }
}

pub(crate) fn guard_branch_worktree(branch: &str, operation: &str) -> Result<()> {
    let worktrees = git::worktree_list()?;
    let worktree = worktrees
        .iter()
        .find(|worktree| worktree.branch.as_deref() == Some(branch));
    let Some(worktree) = worktree else {
        return Ok(());
    };
    guard_registered_worktree(branch, &worktree.path, operation)
}

pub(crate) fn guard_registered_worktree(branch: &str, path: &str, operation: &str) -> Result<()> {
    let worktrees = git::worktree_list()?;
    let worktree = worktrees.iter().find(|worktree| worktree.path == path);
    let Some(worktree) = worktree else {
        bail!(
            "worktree `{path}` disappeared before ez could {operation} `{branch}`\n  \
             → Inspect `git worktree list` and retry"
        );
    };
    if worktree.branch.as_deref() != Some(branch) {
        bail!(
            "worktree ownership changed at `{path}` before ez could {operation} `{branch}`\n  \
             → Inspect `git worktree list` and retry"
        );
    }
    guard_worktree_lock(branch, path, worktree.locked_reason.as_deref(), operation)
}

pub(crate) fn guard_worktree_lock(
    branch: &str,
    path: &str,
    reason: Option<&str>,
    operation: &str,
) -> Result<()> {
    let Some(reason) = reason else {
        return Ok(());
    };
    if let Some(lease) = parse_matching_lease(reason, branch) {
        let stale = lease.is_stale()?;
        let state = if stale { "stale" } else { "active" };
        let release = if stale {
            format!("ez worktree release {branch} --force")
        } else {
            format!("ez worktree release {branch} --owner {}", lease.owner)
        };
        bail!(EzError::UserMessage(format!(
            "cannot {operation} `{branch}` because worktree `{path}` has an {state} lease owned by `{}`\n  \
             → Coordinate with that owner, then run `{release}`",
            lease.owner
        )));
    }
    bail!(EzError::UserMessage(format!(
        "cannot {operation} `{branch}` because worktree `{path}` has a foreign Git lock: `{}`\n  \
         → Preserve that lock or remove it manually with `git worktree unlock {path}`",
        display_lock_reason(reason)
    )));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EnsureStatus {
    Created,
    Reused,
    WouldCreate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EnsureLocation {
    Canonical,
    External,
    Main,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct WorkingTreeCounts {
    staged: usize,
    modified: usize,
    untracked: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct EnsureEntry {
    branch: String,
    path: String,
    status: EnsureStatus,
    location: EnsureLocation,
    working_tree: WorkingTreeCounts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct EnsureResult {
    cmd: String,
    dry_run: bool,
    entries: Vec<EnsureEntry>,
    created_count: usize,
    reused_count: usize,
    would_create_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedWorktree {
    branch: String,
    path: String,
    lock_reason: String,
}

impl EnsureResult {
    fn new(dry_run: bool, entries: Vec<EnsureEntry>) -> Self {
        let created_count = entries
            .iter()
            .filter(|entry| entry.status == EnsureStatus::Created)
            .count();
        let reused_count = entries
            .iter()
            .filter(|entry| entry.status == EnsureStatus::Reused)
            .count();
        let would_create_count = entries
            .iter()
            .filter(|entry| entry.status == EnsureStatus::WouldCreate)
            .count();
        Self {
            cmd: "worktree.ensure".to_string(),
            dry_run,
            entries,
            created_count,
            reused_count,
            would_create_count,
        }
    }
}

pub fn ensure(branches: &[String], dry_run: bool, json: bool) -> Result<()> {
    let result = ensure_with_adder(branches, dry_run, add_owned_worktree)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        render_ensure_result(&result);
    }
    ui::receipt(&serde_json::to_value(&result)?);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ExecStatus {
    Succeeded,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ExecEntry {
    branch: String,
    path: String,
    status: ExecStatus,
    exit_code: Option<i32>,
    duration_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ExecResult {
    cmd: String,
    command: Vec<String>,
    keep_going: bool,
    entries: Vec<ExecEntry>,
    created_count: usize,
    reused_count: usize,
    attempted_count: usize,
    succeeded_count: usize,
    failed_count: usize,
    skipped_count: usize,
    stopped_early: bool,
}

pub fn exec(branches: &[String], command: &[String], keep_going: bool, json: bool) -> Result<()> {
    if command.is_empty() {
        bail!(EzError::UserMessage(
            "a command is required\n  → Run `ez worktree exec -- <command> [args...]`".to_string()
        ));
    }

    let fleet = ensure_with_adder(branches, false, add_owned_worktree)?;
    if fleet.entries.is_empty() {
        bail!(EzError::UserMessage(
            "the stack has no managed branches to execute in\n  → Run `ez create <name>` first"
                .to_string()
        ));
    }
    for fleet_entry in &fleet.entries {
        guard_registered_worktree(
            &fleet_entry.branch,
            &fleet_entry.path,
            "run fleet command in",
        )?;
    }

    let fleet_size = fleet.entries.len();
    let mut entries = Vec::with_capacity(fleet_size);
    for (index, fleet_entry) in fleet.entries.iter().enumerate() {
        if !json {
            ui::info(&format!(
                "Running `{}` in `{}` at `{}`",
                command.join(" "),
                fleet_entry.branch,
                fleet_entry.path
            ));
        }

        let entry = execute_in_worktree(
            &fleet_entry.branch,
            &fleet_entry.path,
            command,
            json,
            index + 1,
            fleet_size,
        );
        let failed = entry.status == ExecStatus::Failed;
        entries.push(entry);
        if failed && !keep_going {
            break;
        }
    }

    for fleet_entry in fleet.entries.iter().skip(entries.len()) {
        entries.push(ExecEntry {
            branch: fleet_entry.branch.clone(),
            path: fleet_entry.path.clone(),
            status: ExecStatus::Skipped,
            exit_code: None,
            duration_ms: 0,
            stdout: json.then(String::new),
            stderr: json.then(String::new),
        });
    }

    let succeeded_count = count_exec_status(&entries, ExecStatus::Succeeded);
    let failed_count = count_exec_status(&entries, ExecStatus::Failed);
    let skipped_count = count_exec_status(&entries, ExecStatus::Skipped);
    let attempted_count = succeeded_count + failed_count;
    let stopped_early = skipped_count > 0;
    let result = ExecResult {
        cmd: "worktree.exec".to_string(),
        command: command.to_vec(),
        keep_going,
        entries,
        created_count: fleet.created_count,
        reused_count: fleet.reused_count,
        attempted_count,
        succeeded_count,
        failed_count,
        skipped_count,
        stopped_early,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if failed_count == 0 {
        ui::success(&format!(
            "Fleet command passed in {} worktree(s)",
            result.succeeded_count
        ));
    } else {
        ui::error(&format!(
            "Fleet command failed in {} of {} attempted worktree(s)",
            result.failed_count, result.attempted_count
        ));
    }
    ui::receipt(&serde_json::json!({
        "cmd": result.cmd,
        "command": result.command,
        "keep_going": result.keep_going,
        "created_count": result.created_count,
        "reused_count": result.reused_count,
        "attempted_count": result.attempted_count,
        "succeeded_count": result.succeeded_count,
        "failed_count": result.failed_count,
        "skipped_count": result.skipped_count,
        "stopped_early": result.stopped_early,
    }));

    if failed_count > 0 {
        let exit_code = result
            .entries
            .iter()
            .find(|entry| entry.status == ExecStatus::Failed)
            .and_then(|entry| entry.exit_code)
            .filter(|code| (1..=255).contains(code))
            .unwrap_or(1);
        bail!(EzError::WorktreeExecFailed {
            count: failed_count,
            exit_code,
        });
    }
    Ok(())
}

fn count_exec_status(entries: &[ExecEntry], status: ExecStatus) -> usize {
    entries
        .iter()
        .filter(|entry| entry.status == status)
        .count()
}

fn execute_in_worktree(
    branch: &str,
    path: &str,
    command: &[String],
    capture: bool,
    stack_index: usize,
    stack_size: usize,
) -> ExecEntry {
    let started = Instant::now();
    let mut process = Command::new(&command[0]);
    process
        .args(&command[1..])
        .current_dir(path)
        .env("EZ_BRANCH", branch)
        .env("EZ_WORKTREE", path)
        .env("EZ_PORT", crate::dev::dev_port(branch).to_string())
        .env("EZ_STACK_INDEX", stack_index.to_string())
        .env("EZ_STACK_SIZE", stack_size.to_string());

    let execution = if capture {
        process.output().map(|output| {
            (
                output.status.success(),
                output.status.code(),
                Some(String::from_utf8_lossy(&output.stdout).into_owned()),
                Some(String::from_utf8_lossy(&output.stderr).into_owned()),
            )
        })
    } else {
        process
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map(|status| (status.success(), status.code(), None, None))
    };

    let (success, exit_code, stdout, stderr) = match execution {
        Ok(result) => result,
        Err(error) => (
            false,
            command_spawn_exit_code(&error),
            capture.then(String::new),
            if capture {
                Some(error.to_string())
            } else {
                ui::error(&format!(
                    "Could not start `{}` in `{branch}`: {error}",
                    command[0]
                ));
                None
            },
        ),
    };

    ExecEntry {
        branch: branch.to_string(),
        path: path.to_string(),
        status: if success {
            ExecStatus::Succeeded
        } else {
            ExecStatus::Failed
        },
        exit_code,
        duration_ms: started.elapsed().as_millis(),
        stdout,
        stderr,
    }
}

fn command_spawn_exit_code(error: &io::Error) -> Option<i32> {
    if error.kind() == io::ErrorKind::NotFound {
        Some(127)
    } else if error.kind() == io::ErrorKind::PermissionDenied {
        Some(126)
    } else {
        None
    }
}

fn ensure_with_adder<F>(
    requested: &[String],
    dry_run: bool,
    mut add_worktree: F,
) -> Result<EnsureResult>
where
    F: FnMut(&str, &str, &str) -> Result<()>,
{
    let state = StackState::load()?;
    let selected = select_managed_branches(&state, requested)?;
    let worktrees = git::worktree_list()?;
    let main_root = git::main_worktree_root()?;
    let mut entries = plan_entries(&selected, &worktrees, &main_root)?;

    if dry_run {
        return Ok(EnsureResult::new(true, entries));
    }

    let mut created_worktrees = Vec::new();
    for entry in &mut entries {
        if entry.status != EnsureStatus::WouldCreate {
            continue;
        }

        let owned = OwnedWorktree {
            branch: entry.branch.clone(),
            path: entry.path.clone(),
            lock_reason: ownership_reason(),
        };
        if let Err(error) = add_worktree(&entry.path, &entry.branch, &owned.lock_reason) {
            let mut rollback_candidates = created_worktrees.clone();
            rollback_candidates.push(owned);
            let rollback_errors = rollback_created_worktrees(&rollback_candidates);
            if rollback_errors.is_empty() {
                return Err(error).with_context(|| {
                    format!(
                        "create worktree for `{}`; earlier worktrees were rolled back",
                        entry.branch
                    )
                });
            }
            bail!(
                "{error:#}\nWorktree fleet rollback was incomplete:\n  - {}\n  → Inspect `git worktree list` before retrying",
                rollback_errors.join("\n  - ")
            );
        }

        let owns_worktree = match worktree_matches_ownership(&owned) {
            Ok(owns_worktree) => owns_worktree,
            Err(error) => {
                let mut rollback_candidates = created_worktrees.clone();
                rollback_candidates.push(owned.clone());
                let rollback_errors = rollback_created_worktrees(&rollback_candidates);
                bail!(
                    "verify ez ownership after creating `{}`: {error:#}\nWorktree fleet rollback results: {}",
                    entry.branch,
                    if rollback_errors.is_empty() {
                        "all created worktrees removed".to_string()
                    } else {
                        rollback_errors.join("; ")
                    }
                );
            }
        };
        if !owns_worktree {
            let rollback_errors = rollback_created_worktrees(&created_worktrees);
            bail!(
                "worktree add for `{}` returned success without ez ownership lock `{}`; rollback errors: {}",
                entry.branch,
                owned.lock_reason,
                if rollback_errors.is_empty() {
                    "none".to_string()
                } else {
                    rollback_errors.join("; ")
                }
            );
        }

        created_worktrees.push(owned);
        entry.status = EnsureStatus::Created;
    }

    let unlock_errors = unlock_created_worktrees(&created_worktrees);
    if !unlock_errors.is_empty() {
        bail!(
            "Worktree fleet was created but ownership locks could not be released:\n  - {}\n  → Run `git worktree unlock <path>` for each listed worktree",
            unlock_errors.join("\n  - ")
        );
    }

    Ok(EnsureResult::new(false, entries))
}

fn add_owned_worktree(path: &str, branch: &str, lock_reason: &str) -> Result<()> {
    git::worktree_add_locked_no_checkout(path, branch, lock_reason)?;
    git::worktree_checkout(path, branch)
}

fn ownership_reason() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_OWNERSHIP_ID: AtomicU64 = AtomicU64::new(1);
    format!(
        "ez-worktree-ensure:{}:{}",
        std::process::id(),
        NEXT_OWNERSHIP_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn select_managed_branches(state: &StackState, requested: &[String]) -> Result<Vec<String>> {
    let order = deterministic_topo_order(state);
    if requested.is_empty() {
        return Ok(order);
    }

    let mut wanted = HashSet::new();
    for branch in requested {
        if state.is_trunk(branch) {
            bail!(EzError::UserMessage(format!(
                "`{branch}` is trunk, not a managed stack layer\n  → Omit trunk from the worktree branch selection"
            )));
        }
        if !state.is_managed(branch) {
            bail!(EzError::BranchNotInStack(branch.clone()));
        }
        wanted.insert(branch.clone());
    }

    Ok(order
        .into_iter()
        .filter(|branch| wanted.contains(branch))
        .collect())
}

fn deterministic_topo_order(state: &StackState) -> Vec<String> {
    fn visit(
        branch: &str,
        state: &StackState,
        visited: &mut HashSet<String>,
        result: &mut Vec<String>,
    ) {
        if state.is_trunk(branch) || !state.branches.contains_key(branch) {
            return;
        }
        if !visited.insert(branch.to_string()) {
            return;
        }
        if let Some(meta) = state.branches.get(branch) {
            visit(&meta.parent, state, visited, result);
        }
        result.push(branch.to_string());
    }

    let mut names: Vec<String> = state.branches.keys().cloned().collect();
    names.sort();
    let mut visited = HashSet::new();
    let mut result = Vec::with_capacity(names.len());
    for name in names {
        visit(&name, state, &mut visited, &mut result);
    }
    result
}

fn plan_entries(
    branches: &[String],
    worktrees: &[git::WorktreeInfo],
    main_root: &str,
) -> Result<Vec<EnsureEntry>> {
    let mut branch_worktrees: HashMap<String, String> = HashMap::new();
    let mut path_owners: HashMap<PathBuf, String> = HashMap::new();
    for worktree in worktrees {
        let owner = worktree
            .branch
            .clone()
            .unwrap_or_else(|| "<detached HEAD>".to_string());
        path_owners.insert(path_key(Path::new(&worktree.path)), owner);
        if let Some(branch) = &worktree.branch {
            branch_worktrees.insert(branch.clone(), worktree.path.clone());
        }
    }

    let main_key = path_key(Path::new(main_root));
    let mut planned_paths: HashMap<PathBuf, String> = HashMap::new();
    let mut entries = Vec::with_capacity(branches.len());

    for branch in branches {
        if !git::branch_exists(branch) {
            bail!(EzError::UserMessage(format!(
                "managed branch `{branch}` is missing locally\n  → Run `ez adopt {branch}` to rehydrate it before ensuring worktrees"
            )));
        }

        let canonical = git::worktree_path(branch)?;
        let canonical_key = path_key(Path::new(&canonical));
        if let Some(existing) = branch_worktrees.get(branch) {
            if !Path::new(existing).is_dir() {
                bail!(EzError::UserMessage(format!(
                    "managed branch `{branch}` is registered at missing worktree path `{existing}`\n  → Run `git worktree prune`, then retry `ez worktree ensure`"
                )));
            }
            let existing_key = path_key(Path::new(existing));
            let location = if existing_key == main_key {
                EnsureLocation::Main
            } else if existing_key == canonical_key {
                EnsureLocation::Canonical
            } else {
                EnsureLocation::External
            };
            let (staged, modified, untracked) = git::working_tree_status_at(existing);
            entries.push(EnsureEntry {
                branch: branch.clone(),
                path: existing.clone(),
                status: EnsureStatus::Reused,
                location,
                working_tree: WorkingTreeCounts {
                    staged,
                    modified,
                    untracked,
                },
            });
            continue;
        }

        if let Some(owner) = path_owners.get(&canonical_key) {
            bail!(EzError::UserMessage(format!(
                "canonical worktree path `{canonical}` for `{branch}` is already owned by `{owner}`\n  → Move or remove the conflicting worktree before retrying"
            )));
        }
        if let Some(other) = planned_paths.get(&canonical_key) {
            bail!(EzError::UserMessage(format!(
                "branches `{other}` and `{branch}` map to the same canonical worktree path `{canonical}`\n  → Materialize one branch at an external path, then retry"
            )));
        }
        if path_is_occupied(Path::new(&canonical))? {
            bail!(EzError::UserMessage(format!(
                "canonical worktree path `{canonical}` for `{branch}` already exists but is not a registered worktree\n  → Move that file or directory aside, then retry"
            )));
        }

        planned_paths.insert(canonical_key, branch.clone());
        entries.push(planned_entry(branch, &canonical));
    }

    Ok(entries)
}

fn planned_entry(branch: &str, canonical_path: &str) -> EnsureEntry {
    EnsureEntry {
        branch: branch.to_string(),
        path: canonical_path.to_string(),
        status: EnsureStatus::WouldCreate,
        location: EnsureLocation::Canonical,
        working_tree: WorkingTreeCounts {
            staged: 0,
            modified: 0,
            untracked: 0,
        },
    }
}

fn worktree_matches_ownership(expected: &OwnedWorktree) -> Result<bool> {
    let expected_path_key = path_key(Path::new(&expected.path));
    Ok(git::worktree_list()?.iter().any(|worktree| {
        path_key(Path::new(&worktree.path)) == expected_path_key
            && worktree.branch.as_deref() == Some(expected.branch.as_str())
            && worktree.locked_reason.as_deref() == Some(expected.lock_reason.as_str())
    }))
}

fn unlock_created_worktrees(created: &[OwnedWorktree]) -> Vec<String> {
    let mut errors = Vec::new();
    for owned in created {
        match worktree_matches_ownership(owned) {
            Ok(true) => {
                if let Err(error) = git::worktree_unlock(&owned.path) {
                    errors.push(format!(
                        "unlock `{}` worktree at `{}`: {error:#}",
                        owned.branch, owned.path
                    ));
                }
            }
            Ok(false) => errors.push(format!(
                "refuse to unlock `{}` at `{}` because its ownership lock changed",
                owned.branch, owned.path
            )),
            Err(error) => errors.push(format!(
                "verify `{}` worktree at `{}` before unlock: {error:#}",
                owned.branch, owned.path
            )),
        }
    }
    errors
}

fn rollback_created_worktrees(created: &[OwnedWorktree]) -> Vec<String> {
    let mut errors = Vec::new();
    for owned in created.iter().rev() {
        let branch = &owned.branch;
        let path = &owned.path;
        let worktrees = match git::worktree_list() {
            Ok(worktrees) => worktrees,
            Err(error) => {
                errors.push(format!(
                    "verify `{branch}` worktree at `{path}` before removal: {error:#}"
                ));
                continue;
            }
        };
        let expected_path_key = path_key(Path::new(path));
        let registered = worktrees
            .iter()
            .find(|worktree| path_key(Path::new(&worktree.path)) == expected_path_key);
        let Some(registered) = registered else {
            match path_is_occupied(Path::new(path)) {
                Ok(false) => continue,
                Ok(true) => errors.push(format!(
                    "refuse to remove unregistered path `{path}` while rolling back `{branch}`"
                )),
                Err(error) => errors.push(format!(
                    "inspect rollback path `{path}` for `{branch}`: {error:#}"
                )),
            }
            continue;
        };
        if registered.branch.as_deref() != Some(branch.as_str()) {
            let owner = registered.branch.as_deref().unwrap_or("<detached HEAD>");
            errors.push(format!(
                "refuse to remove `{path}`: expected `{branch}`, found `{owner}`"
            ));
            continue;
        }
        if registered.locked_reason.as_deref() != Some(owned.lock_reason.as_str()) {
            let actual = registered.locked_reason.as_deref().unwrap_or("<unlocked>");
            errors.push(format!(
                "refuse to remove `{path}`: expected ownership lock `{}`, found `{actual}`",
                owned.lock_reason
            ));
            continue;
        }
        let (staged, modified, untracked) = match git::working_tree_status_at_checked(path) {
            Ok(counts) => counts,
            Err(error) => {
                errors.push(format!(
                    "inspect `{branch}` worktree at `{path}` before removal: {error:#}"
                ));
                continue;
            }
        };
        if staged + modified + untracked > 0 {
            errors.push(format!(
                "refuse to remove dirty `{branch}` worktree at `{path}` ({staged} staged, {modified} modified, {untracked} untracked)"
            ));
            continue;
        }
        if let Err(error) = git::worktree_unlock(path) {
            errors.push(format!(
                "unlock `{branch}` worktree at `{path}` before removal: {error:#}"
            ));
            continue;
        }
        if let Err(error) = git::worktree_remove(path) {
            errors.push(format!("remove `{branch}` worktree at `{path}`: {error:#}"));
        }
    }
    errors
}

fn render_ensure_result(result: &EnsureResult) {
    for entry in &result.entries {
        let action = match entry.status {
            EnsureStatus::Created => "Created",
            EnsureStatus::Reused => "Reused",
            EnsureStatus::WouldCreate => "Would create",
        };
        let counts = &entry.working_tree;
        let dirty = if counts.staged + counts.modified + counts.untracked == 0 {
            String::new()
        } else {
            format!(
                " ({} staged, {} modified, {} untracked)",
                counts.staged, counts.modified, counts.untracked
            )
        };
        ui::info(&format!(
            "{action} `{}` worktree at `{}`{dirty}",
            entry.branch, entry.path
        ));
    }
    if result.dry_run {
        ui::success(&format!(
            "Fleet plan: {} existing, {} to create",
            result.reused_count, result.would_create_count
        ));
    } else {
        ui::success(&format!(
            "Worktree fleet ready: {} created, {} reused",
            result.created_count, result.reused_count
        ));
    }
}

fn path_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn path_is_occupied(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect path `{}`", path.display())),
    }
}

// `ez worktree delete` → routed to cmd::delete::run() in main.rs
// `ez worktree list`   → routed to cmd::list::run() in main.rs

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        CwdGuard, init_git_repo, run_cmd, take_env_lock, temp_dir, write_file,
    };

    struct FleetRepo {
        repo: PathBuf,
    }

    impl FleetRepo {
        fn new(name: &str) -> Self {
            let repo = init_git_repo(name);
            run_cmd(&repo, "git", &["checkout", "-b", "feat/base"]);
            write_file(&repo, "base.txt", "base\n");
            run_cmd(&repo, "git", &["add", "base.txt"]);
            run_cmd(&repo, "git", &["commit", "-m", "base"]);
            let base_sha = command_output(&repo, &["rev-parse", "feat/base"]);
            run_cmd(&repo, "git", &["checkout", "-b", "feat/child"]);
            write_file(&repo, "child.txt", "child\n");
            run_cmd(&repo, "git", &["add", "child.txt"]);
            run_cmd(&repo, "git", &["commit", "-m", "child"]);
            run_cmd(&repo, "git", &["checkout", "main"]);

            let _cwd = CwdGuard::enter(&repo);
            let main_sha = git::rev_parse("main").expect("main sha");
            let mut state = StackState::new("main".to_string());
            state.add_branch("feat/base", "main", &main_sha, None, None);
            state.add_branch("feat/child", "feat/base", &base_sha, None, None);
            state.save().expect("save stack");

            Self { repo }
        }
    }

    fn command_output(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {:?} failed", args);
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn planned_missing_branch_uses_canonical_would_create_status() {
        let entry = planned_entry("feat/auth", "/repo/.worktrees/feat-auth");
        assert_eq!(entry.status, EnsureStatus::WouldCreate);
        assert_eq!(entry.location, EnsureLocation::Canonical);
    }

    #[test]
    fn explicit_selection_is_deduplicated_and_topological() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/base", "main", "m", None, None);
        state.add_branch("feat/child", "feat/base", "b", None, None);

        let selected = select_managed_branches(
            &state,
            &[
                "feat/child".to_string(),
                "feat/base".to_string(),
                "feat/child".to_string(),
            ],
        )
        .expect("select");
        assert_eq!(selected, vec!["feat/base", "feat/child"]);
    }

    #[test]
    fn explicit_selection_rejects_trunk_and_unmanaged_branches() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/base", "main", "m", None, None);

        let trunk = select_managed_branches(&state, &["main".to_string()])
            .expect_err("trunk must not be selected");
        assert!(trunk.to_string().contains("is trunk"));

        let unmanaged = select_managed_branches(&state, &["feat/other".to_string()])
            .expect_err("unmanaged branch must not be selected");
        assert!(unmanaged.to_string().contains("not tracked by ez"));
    }

    #[test]
    fn destructive_guard_distinguishes_active_stale_and_foreign_locks() {
        let active = Lease::new("agent-a", "feat/base", now_unix().expect("time"), 3600)
            .expect("active lease")
            .reason()
            .expect("active reason");
        let active_error = guard_worktree_lock("feat/base", "/repo/base", Some(&active), "delete")
            .expect_err("active lease must block");
        assert!(active_error.to_string().contains("agent-a"));
        assert!(active_error.to_string().contains("--owner agent-a"));

        let stale = Lease::new("gone-agent", "feat/base", 1, 1)
            .expect("stale lease")
            .reason()
            .expect("stale reason");
        let stale_error = guard_worktree_lock("feat/base", "/repo/base", Some(&stale), "fold")
            .expect_err("stale lease must block");
        assert!(stale_error.to_string().contains("stale lease"));
        assert!(stale_error.to_string().contains("--force"));

        let foreign = guard_worktree_lock("feat/base", "/repo/base", Some("maintenance"), "merge")
            .expect_err("foreign lock must block");
        assert!(foreign.to_string().contains("foreign Git lock"));
        assert!(guard_worktree_lock("feat/base", "/repo/base", None, "delete").is_ok());
    }

    #[test]
    fn deterministic_order_never_materializes_an_unmanaged_parent() {
        let mut state = StackState::new("main".to_string());
        state.add_branch("feat/child", "outside-parent", "m", None, None);

        assert_eq!(
            select_managed_branches(&state, &[]).expect("managed selection"),
            vec!["feat/child"]
        );
    }

    #[test]
    fn ensure_all_creates_worktrees_and_is_idempotent() {
        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-create-all");
        let _cwd = CwdGuard::enter(&fixture.repo);

        let first = ensure_with_adder(&[], false, add_owned_worktree).expect("first ensure");
        assert_eq!(first.created_count, 2);
        assert_eq!(first.reused_count, 0);

        let second = ensure_with_adder(&[], false, add_owned_worktree).expect("second ensure");
        assert_eq!(second.created_count, 0);
        assert_eq!(second.reused_count, 2);
        assert!(Path::new(&git::worktree_path("feat/base").expect("base path")).exists());
        assert!(Path::new(&git::worktree_path("feat/child").expect("child path")).exists());
    }

    #[test]
    fn ensure_reuses_external_dirty_and_main_worktrees() {
        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-reuse");
        let external = temp_dir("fleet-external").join("base");
        run_cmd(
            &fixture.repo,
            "git",
            &[
                "worktree",
                "add",
                external.to_str().expect("external path"),
                "feat/base",
            ],
        );
        write_file(&external, "dirty.txt", "dirty\n");
        let _cwd = CwdGuard::enter(&fixture.repo);

        let external_result =
            ensure_with_adder(&["feat/base".to_string()], false, add_owned_worktree)
                .expect("reuse external");
        assert_eq!(
            external_result.entries[0].location,
            EnsureLocation::External
        );
        assert_eq!(external_result.entries[0].working_tree.untracked, 1);

        run_cmd(
            &fixture.repo,
            "git",
            &[
                "worktree",
                "remove",
                "--force",
                external.to_str().expect("external"),
            ],
        );
        run_cmd(&fixture.repo, "git", &["checkout", "feat/base"]);

        let main_result = ensure_with_adder(&["feat/base".to_string()], false, add_owned_worktree)
            .expect("reuse main");
        assert_eq!(main_result.entries[0].location, EnsureLocation::Main);
        assert_eq!(main_result.reused_count, 1);
    }

    #[test]
    fn stale_registered_worktree_is_not_reported_as_reused() {
        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-stale-registration");
        let external = temp_dir("fleet-stale-external").join("base");
        run_cmd(
            &fixture.repo,
            "git",
            &[
                "worktree",
                "add",
                external.to_str().expect("external path"),
                "feat/base",
            ],
        );
        std::fs::remove_dir_all(&external).expect("remove worktree directory only");
        let _cwd = CwdGuard::enter(&fixture.repo);

        let error = ensure_with_adder(&["feat/base".to_string()], false, add_owned_worktree)
            .expect_err("stale worktree registration must fail");
        assert!(
            error
                .to_string()
                .contains("registered at missing worktree path")
        );
        assert!(error.to_string().contains("git worktree prune"));
    }

    #[test]
    fn detached_registration_at_canonical_path_blocks_all_mutation() {
        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-detached-registration");
        let _cwd = CwdGuard::enter(&fixture.repo);
        let canonical = git::worktree_path("feat/base").expect("canonical path");
        run_cmd(
            &fixture.repo,
            "git",
            &["worktree", "add", "--detach", &canonical, "main"],
        );

        let error = ensure_with_adder(&["feat/base".to_string()], false, add_owned_worktree)
            .expect_err("detached registration must block ensure");
        assert!(error.to_string().contains("<detached HEAD>"));
        assert!(!Path::new(&git::worktree_path("feat/child").expect("child path")).exists());
    }

    #[test]
    fn another_branch_registered_at_canonical_path_is_preserved() {
        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-owned-path");
        let _cwd = CwdGuard::enter(&fixture.repo);
        let canonical = git::worktree_path("feat/base").expect("canonical path");
        run_cmd(
            &fixture.repo,
            "git",
            &["worktree", "add", &canonical, "feat/child"],
        );

        let error = ensure_with_adder(&["feat/base".to_string()], false, add_owned_worktree)
            .expect_err("owned path must block ensure");
        assert!(error.to_string().contains("already owned by `feat/child`"));
        assert!(Path::new(&canonical).exists());
        assert!(
            git::worktree_list()
                .expect("worktree list")
                .iter()
                .any(|worktree| {
                    worktree.path == canonical && worktree.branch.as_deref() == Some("feat/child")
                })
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_collision_blocks_all_mutation_and_is_preserved() {
        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-dangling-symlink");
        let _cwd = CwdGuard::enter(&fixture.repo);
        let canonical = PathBuf::from(git::worktree_path("feat/base").expect("canonical path"));
        std::fs::create_dir_all(canonical.parent().expect("worktrees dir"))
            .expect("create worktrees dir");
        std::os::unix::fs::symlink("missing-target", &canonical).expect("create dangling symlink");

        let error = ensure_with_adder(&[], false, add_owned_worktree)
            .expect_err("dangling symlink must block ensure");
        assert!(error.to_string().contains("already exists"));
        assert!(
            std::fs::symlink_metadata(&canonical)
                .expect("symlink preserved")
                .file_type()
                .is_symlink()
        );
        assert!(!Path::new(&git::worktree_path("feat/child").expect("child path")).exists());
    }

    #[test]
    fn dry_run_does_not_create_worktrees() {
        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-dry-run");
        let _cwd = CwdGuard::enter(&fixture.repo);

        let result = ensure_with_adder(&[], true, |_path, _branch, _reason| {
            bail!("adder must not run in dry-run")
        })
        .expect("dry run");
        assert_eq!(result.would_create_count, 2);
        assert_eq!(result.created_count, 0);
        assert!(!Path::new(&git::worktree_path("feat/base").expect("base path")).exists());
    }

    #[test]
    fn missing_ref_preflight_prevents_partial_creation() {
        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-missing-ref");
        let _cwd = CwdGuard::enter(&fixture.repo);
        let mut state = StackState::load().expect("load state");
        state.add_branch(
            "feat/missing",
            "feat/child",
            &git::rev_parse("feat/child").expect("child sha"),
            None,
            None,
        );
        state.save().expect("save missing metadata");

        let error =
            ensure_with_adder(&[], false, add_owned_worktree).expect_err("missing ref must fail");
        assert!(error.to_string().contains("missing locally"));
        assert!(!Path::new(&git::worktree_path("feat/base").expect("base path")).exists());
    }

    #[test]
    fn same_name_tag_does_not_satisfy_local_branch_preflight() {
        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-tag-shadow");
        run_cmd(&fixture.repo, "git", &["branch", "-D", "feat/base"]);
        run_cmd(&fixture.repo, "git", &["tag", "feat/base", "main"]);
        let _cwd = CwdGuard::enter(&fixture.repo);

        let error = ensure_with_adder(&["feat/base".to_string()], false, add_owned_worktree)
            .expect_err("tag must not satisfy local branch preflight");
        assert!(error.to_string().contains("missing locally"));
        assert!(!Path::new(&git::worktree_path("feat/base").expect("base path")).exists());
    }

    #[test]
    fn filesystem_collision_prevents_partial_creation_and_is_preserved() {
        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-filesystem-collision");
        let _cwd = CwdGuard::enter(&fixture.repo);
        let collision = PathBuf::from(git::worktree_path("feat/child").expect("child path"));
        write_file(&collision, "owner.txt", "keep\n");

        let error =
            ensure_with_adder(&[], false, add_owned_worktree).expect_err("collision must fail");
        assert!(error.to_string().contains("already exists"));
        assert!(!Path::new(&git::worktree_path("feat/base").expect("base path")).exists());
        assert_eq!(
            std::fs::read_to_string(collision.join("owner.txt")).expect("collision contents"),
            "keep\n"
        );
    }

    #[test]
    fn sanitized_branch_path_collision_is_rejected_before_creation() {
        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-sanitized-collision");
        let _cwd = CwdGuard::enter(&fixture.repo);
        run_cmd(&fixture.repo, "git", &["branch", "feat/a-b", "main"]);
        run_cmd(&fixture.repo, "git", &["branch", "feat/a/b", "main"]);
        let mut state = StackState::load().expect("load state");
        let main_sha = git::rev_parse("main").expect("main sha");
        state.add_branch("feat/a-b", "main", &main_sha, None, None);
        state.add_branch("feat/a/b", "main", &main_sha, None, None);
        state.save().expect("save collision state");

        let error = ensure_with_adder(
            &["feat/a-b".to_string(), "feat/a/b".to_string()],
            false,
            add_owned_worktree,
        )
        .expect_err("sanitized collision must fail");
        assert!(error.to_string().contains("same canonical worktree path"));
        assert!(!Path::new(&git::worktree_path("feat/a-b").expect("path")).exists());
    }

    #[test]
    fn second_add_failure_rolls_back_first_created_worktree() {
        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-add-rollback");
        let _cwd = CwdGuard::enter(&fixture.repo);
        let mut calls = 0;

        let error = ensure_with_adder(&[], false, |path, branch, lock_reason| {
            calls += 1;
            if calls == 2 {
                bail!("injected add failure");
            }
            add_owned_worktree(path, branch, lock_reason)
        })
        .expect_err("second add must fail");
        assert!(error.to_string().contains("rolled back"));
        assert!(!Path::new(&git::worktree_path("feat/base").expect("base path")).exists());
        assert!(!Path::new(&git::worktree_path("feat/child").expect("child path")).exists());
    }

    #[cfg(unix)]
    #[test]
    fn side_effectful_post_checkout_failure_rolls_back_failing_worktree() {
        use std::os::unix::fs::PermissionsExt;

        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-hook-failure");
        let hook = fixture.repo.join(".git/hooks/post-checkout");
        write_file(
            &fixture.repo,
            ".git/hooks/post-checkout",
            "#!/bin/sh\nexit 1\n",
        );
        let mut permissions = std::fs::metadata(&hook)
            .expect("hook metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).expect("executable hook");
        let _cwd = CwdGuard::enter(&fixture.repo);
        let base_path = git::worktree_path("feat/base").expect("base path");

        let error = ensure_with_adder(&["feat/base".to_string()], false, add_owned_worktree)
            .expect_err("post-checkout failure must surface");
        assert!(error.to_string().contains("rolled back"));
        assert!(!Path::new(&base_path).exists());
        assert!(
            git::worktree_list()
                .expect("worktree list")
                .iter()
                .all(|worktree| worktree.branch.as_deref() != Some("feat/base"))
        );
    }

    #[test]
    fn rollback_refuses_to_remove_a_same_branch_replacement_without_ownership_lock() {
        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-rollback-owner");
        let _cwd = CwdGuard::enter(&fixture.repo);
        let base_path = git::worktree_path("feat/base").expect("base path");
        let mut calls = 0;

        let error = ensure_with_adder(&[], false, |path, branch, lock_reason| {
            calls += 1;
            if calls == 2 {
                git::worktree_unlock(&base_path).expect("unlock first worktree");
                git::worktree_remove(&base_path).expect("remove first worktree");
                git::worktree_add(&base_path, "feat/base").expect("install replacement");
                bail!("injected add failure after concurrent replacement");
            }
            add_owned_worktree(path, branch, lock_reason)
        })
        .expect_err("replacement must make rollback incomplete");

        assert!(error.to_string().contains("rollback was incomplete"));
        assert!(error.to_string().contains("expected ownership lock"));
        assert!(error.to_string().contains("found `<unlocked>`"));
        assert!(Path::new(&base_path).exists());
        assert!(
            git::worktree_list()
                .expect("worktree list")
                .iter()
                .any(|worktree| {
                    worktree.path == base_path
                        && worktree.branch.as_deref() == Some("feat/base")
                        && worktree.locked_reason.is_none()
                })
        );
    }

    #[test]
    fn failing_add_never_claims_an_unlocked_same_branch_worktree_as_its_own() {
        let _lock = take_env_lock();
        let fixture = FleetRepo::new("fleet-current-owner-race");
        let _cwd = CwdGuard::enter(&fixture.repo);
        let base_path = git::worktree_path("feat/base").expect("base path");

        let error = ensure_with_adder(
            &["feat/base".to_string()],
            false,
            |path, branch, _lock_reason| {
                git::worktree_add(path, branch).expect("simulate concurrent add winner");
                bail!("our add lost the race")
            },
        )
        .expect_err("unlocked concurrent worktree must survive");

        assert!(error.to_string().contains("rollback was incomplete"));
        assert!(error.to_string().contains("expected ownership lock"));
        assert!(Path::new(&base_path).exists());
        assert!(
            git::worktree_list()
                .expect("worktree list")
                .iter()
                .any(|worktree| {
                    worktree.path == base_path
                        && worktree.branch.as_deref() == Some("feat/base")
                        && worktree.locked_reason.is_none()
                })
        );
    }

    #[test]
    fn result_json_schema_is_deterministic() {
        let mut entry = planned_entry("feat/base", "/repo/wt");
        entry.status = EnsureStatus::Created;
        let result = EnsureResult::new(false, vec![entry]);
        let value = serde_json::to_value(result).expect("serialize");
        assert_eq!(value["cmd"], "worktree.ensure");
        assert_eq!(value["created_count"], 1);
        assert_eq!(value["entries"][0]["status"], "created");
        assert_eq!(value["entries"][0]["location"], "canonical");
    }

    #[test]
    fn exec_rejects_an_empty_command_before_loading_repository_state() {
        let error = exec(&[], &[], false, true).expect_err("empty command");
        assert!(error.to_string().contains("a command is required"));
    }

    #[test]
    fn execute_in_worktree_captures_context_output_and_failure() {
        let path = temp_dir("fleet-exec-context");
        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s|%s|%s|%s|%s' \"$EZ_BRANCH\" \"$EZ_WORKTREE\" \"$EZ_PORT\" \"$EZ_STACK_INDEX\" \"$EZ_STACK_SIZE\"; printf 'problem' >&2; exit 9".to_string(),
        ];

        let entry = execute_in_worktree(
            "feat/context",
            path.to_str().expect("path"),
            &command,
            true,
            2,
            4,
        );

        assert_eq!(entry.status, ExecStatus::Failed);
        assert_eq!(entry.exit_code, Some(9));
        assert_eq!(entry.stderr.as_deref(), Some("problem"));
        let stdout = entry.stdout.expect("captured stdout");
        let fields: Vec<&str> = stdout.split('|').collect();
        assert_eq!(fields[0], "feat/context");
        assert_eq!(fields[1], path.to_str().expect("path"));
        assert_eq!(fields[2], crate::dev::dev_port("feat/context").to_string());
        assert_eq!(fields[3], "2");
        assert_eq!(fields[4], "4");
    }

    #[test]
    fn execute_in_worktree_maps_spawn_errors_and_status_counts() {
        let path = temp_dir("fleet-exec-spawn");
        let missing = execute_in_worktree(
            "feat/missing",
            path.to_str().expect("path"),
            &["ez-command-that-does-not-exist".to_string()],
            true,
            1,
            1,
        );
        let succeeded = ExecEntry {
            branch: "feat/ok".to_string(),
            path: "/tmp/ok".to_string(),
            status: ExecStatus::Succeeded,
            exit_code: Some(0),
            duration_ms: 1,
            stdout: None,
            stderr: None,
        };
        let skipped = ExecEntry {
            status: ExecStatus::Skipped,
            ..succeeded.clone()
        };

        assert_eq!(missing.status, ExecStatus::Failed);
        assert_eq!(missing.exit_code, Some(127));
        assert_eq!(
            count_exec_status(&[missing, succeeded, skipped], ExecStatus::Failed),
            1
        );
    }

    #[test]
    fn spawn_error_exit_codes_follow_shell_conventions() {
        assert_eq!(
            command_spawn_exit_code(&io::Error::from(io::ErrorKind::NotFound)),
            Some(127)
        );
        assert_eq!(
            command_spawn_exit_code(&io::Error::from(io::ErrorKind::PermissionDenied)),
            Some(126)
        );
        assert_eq!(
            command_spawn_exit_code(&io::Error::from(io::ErrorKind::Other)),
            None
        );
    }
}
