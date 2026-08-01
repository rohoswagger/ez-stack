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
    use crate::test_support::{PathGuard, install_fake_bin, take_env_lock, temp_dir, write_file};

    fn make_fake_dev_tools(
        prefix: &str,
        lsof_body: &str,
        ps_body: &str,
        kill_body: &str,
    ) -> PathBuf {
        let dir = install_fake_bin(
            prefix,
            "lsof",
            &format!("#!/bin/sh\n{}\n", lsof_body.trim_start()),
        );
        write_file(
            &dir,
            "ps",
            &format!("#!/bin/sh\n{}\n", ps_body.trim_start()),
        );
        write_file(
            &dir,
            "kill",
            &format!("#!/bin/sh\n{}\n", kill_body.trim_start()),
        );
        make_executable(&dir.join("ps"));
        make_executable(&dir.join("kill"));
        dir
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut perms = std::fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("chmod");
    }

    fn log_script(tool: &str, body: &str) -> String {
        format!(
            r#"
echo "{tool}:$*" >> "$EZ_TEST_LOG"
{body}
"#
        )
    }

    fn read_log(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

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

    #[test]
    fn listener_pids_returns_parsed_pids_when_lsof_succeeds() {
        let _guard = take_env_lock();
        let log_dir = temp_dir("dev-listener-success");
        let log_path = log_dir.join("calls.log");
        let fake_dir = make_fake_dev_tools(
            "dev-listener-success-bin",
            &log_script("lsof", "printf '1234\\nnoise\\n5678\\n'"),
            "exit 97",
            "exit 97",
        );
        let _path = PathGuard::install(&fake_dir);

        unsafe {
            std::env::set_var("EZ_TEST_LOG", &log_path);
        }
        let result = listener_pids(4567).expect("listener pids");
        unsafe {
            std::env::remove_var("EZ_TEST_LOG");
        }

        assert_eq!(result, vec![1234, 5678]);
        assert_eq!(read_log(&log_path), "lsof:-nP -iTCP:4567 -sTCP:LISTEN -t\n");
    }

    #[test]
    fn listener_pids_returns_empty_when_lsof_exits_one() {
        let _guard = take_env_lock();
        let fake_dir =
            make_fake_dev_tools("dev-listener-empty-bin", "exit 1", "exit 97", "exit 97");
        let _path = PathGuard::install(&fake_dir);

        assert_eq!(
            listener_pids(4567).expect("listener pids"),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn listener_pids_surfaces_lsof_error_stderr() {
        let _guard = take_env_lock();
        let fake_dir = make_fake_dev_tools(
            "dev-listener-error-bin",
            "echo 'lsof exploded' >&2\nexit 2",
            "exit 97",
            "exit 97",
        );
        let _path = PathGuard::install(&fake_dir);

        let err = listener_pids(4567).expect_err("lsof error");

        assert!(
            err.to_string()
                .contains("failed to query TCP port 4567: lsof exploded")
        );
    }

    #[test]
    fn process_cwd_returns_path_when_lsof_reports_cwd_name() {
        let _guard = take_env_lock();
        let log_dir = temp_dir("dev-cwd-success");
        let log_path = log_dir.join("calls.log");
        let fake_dir = make_fake_dev_tools(
            "dev-cwd-success-bin",
            &log_script("lsof", "printf 'p1234\\nfcwd\\nn/repo/.worktrees/feat\\n'"),
            "exit 97",
            "exit 97",
        );
        let _path = PathGuard::install(&fake_dir);

        unsafe {
            std::env::set_var("EZ_TEST_LOG", &log_path);
        }
        let result = process_cwd(1234).expect("process cwd");
        unsafe {
            std::env::remove_var("EZ_TEST_LOG");
        }

        assert_eq!(result.as_deref(), Some(Path::new("/repo/.worktrees/feat")));
        assert_eq!(read_log(&log_path), "lsof:-a -p 1234 -d cwd -Fn\n");
    }

    #[test]
    fn process_cwd_returns_none_when_lsof_exits_one() {
        let _guard = take_env_lock();
        let fake_dir = make_fake_dev_tools("dev-cwd-empty-bin", "exit 1", "exit 97", "exit 97");
        let _path = PathGuard::install(&fake_dir);

        assert_eq!(process_cwd(1234).expect("process cwd"), None);
    }

    #[test]
    fn process_cwd_surfaces_lsof_error_stderr() {
        let _guard = take_env_lock();
        let fake_dir = make_fake_dev_tools(
            "dev-cwd-error-bin",
            "echo 'cwd denied' >&2\nexit 2",
            "exit 97",
            "exit 97",
        );
        let _path = PathGuard::install(&fake_dir);

        let err = process_cwd(1234).expect_err("cwd error");

        assert!(
            err.to_string()
                .contains("failed to inspect cwd for pid 1234: cwd denied")
        );
    }

    #[test]
    fn process_start_token_returns_trimmed_ps_stdout() {
        let _guard = take_env_lock();
        let log_dir = temp_dir("dev-start-success");
        let log_path = log_dir.join("calls.log");
        let fake_dir = make_fake_dev_tools(
            "dev-start-success-bin",
            "exit 97",
            &log_script("ps", "printf 'Mon Jan  1 00:00:00 2024\\n'"),
            "exit 97",
        );
        let _path = PathGuard::install(&fake_dir);

        unsafe {
            std::env::set_var("EZ_TEST_LOG", &log_path);
        }
        let result = process_start_token(1234).expect("start token");
        unsafe {
            std::env::remove_var("EZ_TEST_LOG");
        }

        assert_eq!(result.as_deref(), Some("Mon Jan  1 00:00:00 2024"));
        assert_eq!(read_log(&log_path), "ps:-o lstart= -p 1234\n");
    }

    #[test]
    fn process_start_token_returns_none_when_ps_stdout_is_empty() {
        let _guard = take_env_lock();
        let fake_dir =
            make_fake_dev_tools("dev-start-empty-bin", "exit 97", "printf '\\n'", "exit 97");
        let _path = PathGuard::install(&fake_dir);

        assert_eq!(process_start_token(1234).expect("start token"), None);
    }

    #[test]
    fn process_start_token_returns_none_when_ps_exits_one() {
        let _guard = take_env_lock();
        let fake_dir = make_fake_dev_tools("dev-start-exit1-bin", "exit 97", "exit 1", "exit 97");
        let _path = PathGuard::install(&fake_dir);

        assert_eq!(process_start_token(1234).expect("start token"), None);
    }

    #[test]
    fn process_start_token_surfaces_ps_error_stderr() {
        let _guard = take_env_lock();
        let fake_dir = make_fake_dev_tools(
            "dev-start-error-bin",
            "exit 97",
            "echo 'ps denied' >&2\nexit 2",
            "exit 97",
        );
        let _path = PathGuard::install(&fake_dir);

        let err = process_start_token(1234).expect_err("ps error");

        assert!(
            err.to_string()
                .contains("failed to inspect start time for pid 1234: ps denied")
        );
    }

    #[test]
    fn terminate_process_sends_term_to_pid() {
        let _guard = take_env_lock();
        let log_dir = temp_dir("dev-terminate-success");
        let log_path = log_dir.join("calls.log");
        let fake_dir = make_fake_dev_tools(
            "dev-terminate-success-bin",
            "exit 97",
            "exit 97",
            &log_script("kill", "exit 0"),
        );
        let _path = PathGuard::install(&fake_dir);

        unsafe {
            std::env::set_var("EZ_TEST_LOG", &log_path);
        }
        terminate_process(1234).expect("terminate");
        unsafe {
            std::env::remove_var("EZ_TEST_LOG");
        }

        assert_eq!(read_log(&log_path), "kill:-TERM 1234\n");
    }

    #[test]
    fn terminate_process_surfaces_kill_error_stderr() {
        let _guard = take_env_lock();
        let fake_dir = make_fake_dev_tools(
            "dev-terminate-error-bin",
            "exit 97",
            "exit 97",
            "echo 'kill denied' >&2\nexit 2",
        );
        let _path = PathGuard::install(&fake_dir);

        let err = terminate_process(1234).expect_err("kill error");

        assert!(
            err.to_string()
                .contains("failed to terminate pid 1234: kill denied")
        );
    }

    #[test]
    fn listener_processes_in_worktree_filters_by_cwd_and_start_token() {
        let _guard = take_env_lock();
        let log_dir = temp_dir("dev-ownership-filter");
        let log_path = log_dir.join("calls.log");
        let fake_dir = make_fake_dev_tools(
            "dev-ownership-filter-bin",
            &log_script(
                "lsof",
                r#"
if [ "$*" = "-nP -iTCP:4321 -sTCP:LISTEN -t" ]; then
  printf '1111\n2222\n3333\n'
elif [ "$*" = "-a -p 1111 -d cwd -Fn" ]; then
  printf 'p1111\nfcwd\nn/repo/.worktrees/feat/app\n'
elif [ "$*" = "-a -p 2222 -d cwd -Fn" ]; then
  printf 'p2222\nfcwd\nn/repo-other\n'
elif [ "$*" = "-a -p 3333 -d cwd -Fn" ]; then
  printf 'p3333\nfcwd\nn/repo/.worktrees/feat\n'
else
  exit 97
fi
"#,
            ),
            &log_script(
                "ps",
                r#"
if [ "$*" = "-o lstart= -p 1111" ]; then
  printf 'Mon Jan  1 00:00:00 2024\n'
elif [ "$*" = "-o lstart= -p 3333" ]; then
  printf '\n'
else
  exit 97
fi
"#,
            ),
            "exit 97",
        );
        let _path = PathGuard::install(&fake_dir);

        unsafe {
            std::env::set_var("EZ_TEST_LOG", &log_path);
        }
        let result =
            listener_processes_in_worktree(4321, "/repo/.worktrees/feat").expect("listeners");
        unsafe {
            std::env::remove_var("EZ_TEST_LOG");
        }

        assert_eq!(
            result,
            vec![ProcessIdentity {
                pid: 1111,
                start_token: "Mon Jan  1 00:00:00 2024".to_string(),
            }]
        );
        assert_eq!(
            read_log(&log_path),
            "lsof:-nP -iTCP:4321 -sTCP:LISTEN -t\n\
lsof:-a -p 1111 -d cwd -Fn\n\
ps:-o lstart= -p 1111\n\
lsof:-a -p 2222 -d cwd -Fn\n\
lsof:-a -p 3333 -d cwd -Fn\n\
ps:-o lstart= -p 3333\n"
        );
    }

    #[test]
    fn terminate_processes_skips_stale_identity_and_kills_current_identity() {
        let _guard = take_env_lock();
        let log_dir = temp_dir("dev-terminate-identities");
        let log_path = log_dir.join("calls.log");
        let fake_dir = make_fake_dev_tools(
            "dev-terminate-identities-bin",
            "exit 97",
            &log_script(
                "ps",
                r#"
if [ "$*" = "-o lstart= -p 1111" ]; then
  printf 'stale-token\n'
elif [ "$*" = "-o lstart= -p 2222" ]; then
  printf 'current-token\n'
else
  exit 97
fi
"#,
            ),
            &log_script("kill", "exit 0"),
        );
        let _path = PathGuard::install(&fake_dir);
        let processes = [
            ProcessIdentity {
                pid: 1111,
                start_token: "original-token".to_string(),
            },
            ProcessIdentity {
                pid: 2222,
                start_token: "current-token".to_string(),
            },
        ];

        unsafe {
            std::env::set_var("EZ_TEST_LOG", &log_path);
        }
        let killed = terminate_processes(&processes).expect("terminate processes");
        unsafe {
            std::env::remove_var("EZ_TEST_LOG");
        }

        assert_eq!(killed, vec![2222]);
        assert_eq!(
            read_log(&log_path),
            "ps:-o lstart= -p 1111\n\
ps:-o lstart= -p 2222\n\
kill:-TERM 2222\n"
        );
    }
}
