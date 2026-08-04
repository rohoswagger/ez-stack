use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct DispatchTempDir {
    path: PathBuf,
}

impl DispatchTempDir {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ez-dispatch-status-{prefix}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DispatchTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn run_ez(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .output()
        .expect("run ez")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn bare_ez_and_bare_worktree_are_successful_discovery_commands() {
    let cwd = DispatchTempDir::new("discovery");

    for args in [Vec::<&str>::new(), vec!["worktree"]] {
        let output = run_ez(cwd.path(), &args);
        assert!(
            output.status.success(),
            "ez {args:?} should be discovery-successful\nstdout:\n{}\nstderr:\n{}",
            stdout(&output),
            stderr(&output)
        );
        assert!(
            stdout(&output).contains("Commands:"),
            "discovery output should include help:\n{}",
            stdout(&output)
        );
        assert!(
            stderr(&output).contains("[ok |"),
            "discovery output should include successful status line:\n{}",
            stderr(&output)
        );
    }
}

#[test]
fn unknown_subcommand_uses_agent_friendly_usage_exit() {
    let cwd = DispatchTempDir::new("unknown");

    let output = run_ez(cwd.path(), &["definitely-not-a-command"]);

    assert_eq!(output.status.code(), Some(5));
    let err = stderr(&output);
    assert!(err.contains("unrecognized subcommand"), "{err}");
    assert!(
        err.contains("Run `ez --help` to see available commands"),
        "{err}"
    );
    assert!(err.contains("[exit:5 |"), "{err}");
}

#[test]
fn missing_required_argument_points_to_command_help() {
    let cwd = DispatchTempDir::new("missing-arg");

    let output = run_ez(cwd.path(), &["create"]);

    assert_eq!(output.status.code(), Some(5));
    let err = stderr(&output);
    assert!(
        err.contains("required arguments were not provided"),
        "{err}"
    );
    assert!(
        err.contains("Run `ez <command> --help` for usage details"),
        "{err}"
    );
    assert!(err.contains("[exit:5 |"), "{err}");
}

#[test]
fn clap_conflicts_still_receive_ez_status_lines() {
    let cwd = DispatchTempDir::new("conflict");

    let output = run_ez(cwd.path(), &["push", "--no-pr", "--draft"]);

    assert_eq!(output.status.code(), Some(5));
    let err = stderr(&output);
    assert!(err.contains("cannot be used with"), "{err}");
    assert!(err.contains("[exit:5 |"), "{err}");
}
