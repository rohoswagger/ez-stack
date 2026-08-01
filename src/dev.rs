use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pid: u32,
    start_token: String,
}

pub fn dev_port(branch: &str) -> u16 {
    let mut hash: u32 = 5381;
    for byte in branch.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(byte as u32);
    }
    10000 + (hash % 10000) as u16
}

pub fn listener_processes_in_worktree(
    port: u16,
    worktree_path: &str,
) -> Result<Vec<ProcessIdentity>> {
    let pids = listener_pids(port)?;
    let mut owned = Vec::new();
    for pid in pids {
        if process_cwd(pid)?
            .as_deref()
            .is_some_and(|cwd| path_is_within(cwd, Path::new(worktree_path)))
            && let Some(start_token) = process_start_token(pid)?
        {
            owned.push(ProcessIdentity { pid, start_token });
        }
    }
    Ok(owned)
}

pub fn terminate_processes(processes: &[ProcessIdentity]) -> Result<Vec<u32>> {
    let mut killed = Vec::new();
    for process in processes {
        if process_start_token(process.pid)?.as_deref() != Some(process.start_token.as_str()) {
            continue;
        }
        terminate_process(process.pid)?;
        killed.push(process.pid);
    }
    Ok(killed)
}

fn listener_pids(port: u16) -> Result<Vec<u32>> {
    let output = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .output()
        .with_context(|| format!("failed to run lsof for TCP port {port}"))?;

    if output.status.success() {
        return Ok(parse_pid_lines(&String::from_utf8_lossy(&output.stdout)));
    }

    if output.status.code() == Some(1) {
        return Ok(Vec::new());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    bail!("failed to query TCP port {port}: {stderr}");
}

fn terminate_process(pid: u32) -> Result<()> {
    let output = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output()
        .with_context(|| format!("failed to terminate pid {pid}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    bail!("failed to terminate pid {pid}: {stderr}");
}

fn process_cwd(pid: u32) -> Result<Option<PathBuf>> {
    let output = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .with_context(|| format!("failed to inspect cwd for pid {pid}"))?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix('n'))
            .map(PathBuf::from));
    }

    if output.status.code() == Some(1) {
        return Ok(None);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    bail!("failed to inspect cwd for pid {pid}: {stderr}");
}

fn process_start_token(pid: u32) -> Result<Option<String>> {
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .with_context(|| format!("failed to inspect start time for pid {pid}"))?;

    if output.status.success() {
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((!token.is_empty()).then_some(token));
    }

    if output.status.code() == Some(1) {
        return Ok(None);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    bail!("failed to inspect start time for pid {pid}: {stderr}");
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn parse_pid_lines(output: &str) -> Vec<u32> {
    output
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_port_is_stable() {
        assert_eq!(dev_port("feat/auth"), dev_port("feat/auth"));
        assert_ne!(dev_port("feat/auth"), dev_port("feat/api"));
    }

    #[test]
    fn parse_pid_lines_ignores_invalid_rows() {
        let parsed = parse_pid_lines("1234\nnoise\n5678\n");
        assert_eq!(parsed, vec![1234, 5678]);
    }

    #[test]
    fn path_is_within_matches_exact_and_nested_but_not_prefixes() {
        assert!(path_is_within(
            Path::new("/repo/.worktrees/feat"),
            Path::new("/repo/.worktrees/feat")
        ));
        assert!(path_is_within(
            Path::new("/repo/.worktrees/feat/src"),
            Path::new("/repo/.worktrees/feat")
        ));
        assert!(!path_is_within(
            Path::new("/repo/.worktrees/feat-other"),
            Path::new("/repo/.worktrees/feat")
        ));
    }
}
