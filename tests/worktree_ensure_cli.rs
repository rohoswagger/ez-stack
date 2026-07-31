use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_REPO_ID: AtomicU64 = AtomicU64::new(1);

struct TempRepo {
    path: PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ez-worktree-ensure-cli-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            NEXT_TEMP_REPO_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create unique temp repo");
        run(&path, "git", &["init", "-b", "main"]);
        run(&path, "git", &["config", "user.name", "Test User"]);
        run(&path, "git", &["config", "user.email", "test@example.com"]);
        std::fs::write(path.join("tracked.txt"), "hello\n").expect("write tracked file");
        run(&path, "git", &["add", "tracked.txt"]);
        run(&path, "git", &["commit", "-m", "initial"]);
        Self { path }
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    let output = run_raw(dir, program, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn run_raw(dir: &Path, program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .output()
        .expect("run command")
}

fn run_ez(dir: &Path, args: &[&str]) -> Output {
    run(dir, env!("CARGO_BIN_EXE_ez"), args)
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout JSON")
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn canonical_stdout_path(output: &Output) -> PathBuf {
    PathBuf::from(stdout_text(output))
        .canonicalize()
        .expect("canonical stdout path")
}

fn receipt_json(output: &Output) -> Value {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .find(|line| line.contains(r#"{"cmd":"worktree.ensure""#))
        .expect("worktree ensure receipt");
    let start = line.find('{').expect("receipt start");
    let end = line.rfind('}').expect("receipt end");
    serde_json::from_str(&line[start..=end]).expect("receipt JSON")
}

#[test]
fn ensure_cli_reports_dry_run_creation_and_idempotent_reuse() {
    let repo = TempRepo::new();
    run_ez(&repo.path, &["init", "--yes"]);
    run_ez(
        &repo.path,
        &["create", "feat/base", "--from", "main", "--no-worktree"],
    );

    let dry_run = run_ez(&repo.path, &["worktree", "ensure", "--dry-run", "--json"]);
    let dry_stdout = stdout_json(&dry_run);
    let dry_receipt = receipt_json(&dry_run);
    assert_eq!(dry_stdout["would_create_count"], 1);
    assert_eq!(dry_stdout["entries"][0]["status"], "would_create");
    assert_eq!(dry_receipt["dry_run"], true);
    assert_eq!(dry_receipt["entries"][0]["branch"], "feat/base");

    let create = run_ez(&repo.path, &["worktree", "ensure", "--json"]);
    let create_stdout = stdout_json(&create);
    assert_eq!(create_stdout["created_count"], 1);
    assert_eq!(receipt_json(&create)["created_count"], 1);
    let path = create_stdout["entries"][0]["path"]
        .as_str()
        .expect("worktree path");
    assert!(Path::new(path).is_dir());

    let reuse = run_ez(&repo.path, &["worktree", "ensure", "--json"]);
    let reuse_stdout = stdout_json(&reuse);
    assert_eq!(reuse_stdout["created_count"], 0);
    assert_eq!(reuse_stdout["reused_count"], 1);
    assert_eq!(reuse_stdout["entries"][0]["status"], "reused");
}

#[test]
fn offline_stack_inspection_scope_and_navigation_commands_work_end_to_end() {
    let repo = TempRepo::new();
    run_ez(&repo.path, &["init", "--yes"]);

    let trunk_status = stdout_json(&run_ez(&repo.path, &["status", "--json"]));
    assert_eq!(trunk_status["branch"], "main");
    assert_eq!(trunk_status["depth"], 0);
    run_ez(&repo.path, &["status"]);
    assert_eq!(
        stdout_json(&run_ez(&repo.path, &["log", "--json"])),
        Value::Array(vec![])
    );

    run_ez(
        &repo.path,
        &["create", "feat/base", "--from", "main", "--no-worktree"],
    );
    run(&repo.path, "git", &["checkout", "feat/base"]);
    std::fs::write(repo.path.join("base.txt"), "base\n").expect("write base");
    run(&repo.path, "git", &["add", "base.txt"]);
    run(&repo.path, "git", &["commit", "-m", "base"]);

    let no_scope = run_ez(&repo.path, &["scope", "show"]);
    assert!(String::from_utf8_lossy(&no_scope.stderr).contains("No scope configured"));
    run_ez(&repo.path, &["scope", "set", "--mode", "strict", "src/**"]);
    run_ez(&repo.path, &["scope", "add", "tests/**", "src/**"]);
    let scope_status = stdout_json(&run_ez(&repo.path, &["status", "--json"]));
    assert_eq!(scope_status["scope_mode"], "strict");
    assert_eq!(
        scope_status["scope"],
        serde_json::json!(["src/**", "tests/**"])
    );
    run_ez(&repo.path, &["scope", "show"]);

    let log = stdout_json(&run_ez(&repo.path, &["log", "--json"]));
    assert_eq!(log[0]["branch"], "feat/base");
    assert_eq!(log[0]["parent"], "main");
    let list = stdout_json(&run_ez(&repo.path, &["list", "--json"]));
    assert_eq!(list[0]["branch"], "main");
    assert!(
        list.as_array()
            .expect("list entries")
            .iter()
            .any(|entry| entry["branch"] == "feat/base")
    );
    assert_eq!(stdout_text(&run_ez(&repo.path, &["parent"])), "main");
    assert_eq!(
        stdout_text(&run_ez(&repo.path, &["diff", "--name-only"])),
        "base.txt"
    );
    assert!(stdout_text(&run_ez(&repo.path, &["diff", "--stat"])).contains("base.txt"));
    run_ez(&repo.path, &["status"]);
    run_ez(&repo.path, &["log"]);
    run_ez(&repo.path, &["list"]);
    run_ez(&repo.path, &["worktree", "list"]);

    run_ez(
        &repo.path,
        &[
            "create",
            "feat/child",
            "--from",
            "feat/base",
            "--no-worktree",
        ],
    );
    let child_path = PathBuf::from(stdout_text(&run_ez(&repo.path, &["up"])));
    assert!(child_path.is_dir());
    assert_eq!(
        stdout_json(&run_ez(&child_path, &["status", "--json"]))["branch"],
        "feat/child"
    );
    assert_eq!(
        canonical_stdout_path(&run_ez(&child_path, &["down"])),
        repo.path.canonicalize().expect("canonical repo path")
    );
    assert_eq!(
        canonical_stdout_path(&run_ez(&repo.path, &["top"])),
        child_path.canonicalize().expect("canonical child path")
    );
    assert_eq!(
        canonical_stdout_path(&run_ez(&child_path, &["bottom"])),
        repo.path.canonicalize().expect("canonical repo path")
    );
    assert_eq!(
        canonical_stdout_path(&run_ez(
            &child_path,
            &["switch", "feat/base", "--no-cd-required"],
        )),
        repo.path.canonicalize().expect("canonical repo path")
    );
    run_ez(&repo.path, &["scope", "clear"]);
    let cleared = stdout_json(&run_ez(&repo.path, &["status", "--json"]));
    assert_eq!(cleared["scope_defined"], false);
}

#[test]
fn fleet_exec_materializes_and_runs_parent_first_in_each_stack_worktree() {
    let repo = TempRepo::new();
    run_ez(&repo.path, &["init", "--yes"]);
    run_ez(
        &repo.path,
        &["create", "feat/base", "--from", "main", "--no-worktree"],
    );
    run_ez(
        &repo.path,
        &[
            "create",
            "feat/child",
            "--from",
            "feat/base",
            "--no-worktree",
        ],
    );

    let output = run_ez(
        &repo.path,
        &[
            "worktree",
            "exec",
            "--json",
            "--",
            "git",
            "branch",
            "--show-current",
        ],
    );
    let report = stdout_json(&output);
    assert_eq!(report["cmd"], "worktree.exec");
    assert_eq!(report["succeeded_count"], 2);
    assert_eq!(report["failed_count"], 0);
    assert_eq!(report["entries"][0]["branch"], "feat/base");
    assert_eq!(report["entries"][0]["stdout"], "feat/base\n");
    assert_eq!(report["entries"][1]["branch"], "feat/child");
    assert_eq!(report["entries"][1]["stdout"], "feat/child\n");
    assert!(
        report["entries"]
            .as_array()
            .expect("exec entries")
            .iter()
            .all(|entry| Path::new(entry["path"].as_str().expect("worktree path")).is_dir())
    );
}

#[test]
fn fleet_exec_normalizes_selection_and_exposes_worktree_context() {
    let repo = TempRepo::new();
    run_ez(&repo.path, &["init", "--yes"]);
    run_ez(
        &repo.path,
        &["create", "feat/base", "--from", "main", "--no-worktree"],
    );
    run_ez(
        &repo.path,
        &[
            "create",
            "feat/child",
            "--from",
            "feat/base",
            "--no-worktree",
        ],
    );

    let output = run_ez(
        &repo.path,
        &[
            "worktree",
            "exec",
            "feat/child",
            "feat/base",
            "--json",
            "--",
            "sh",
            "-c",
            "printf '%s|%s|%s|%s|%s\\n' \"$EZ_BRANCH\" \"$EZ_WORKTREE\" \"$EZ_PORT\" \"$EZ_STACK_INDEX\" \"$EZ_STACK_SIZE\"",
        ],
    );
    let entries = stdout_json(&output)["entries"]
        .as_array()
        .expect("exec entries")
        .clone();
    assert_eq!(entries[0]["branch"], "feat/base");
    assert_eq!(entries[1]["branch"], "feat/child");
    for (index, entry) in entries.iter().enumerate() {
        let fields: Vec<&str> = entry["stdout"]
            .as_str()
            .expect("stdout")
            .trim()
            .split('|')
            .collect();
        assert_eq!(fields[0], entry["branch"]);
        assert_eq!(fields[1], entry["path"]);
        assert!(fields[2].parse::<u16>().is_ok());
        assert_eq!(fields[3], (index + 1).to_string());
        assert_eq!(fields[4], "2");
    }
}

#[test]
fn fleet_exec_fail_fast_and_keep_going_report_partial_failure() {
    let repo = TempRepo::new();
    run_ez(&repo.path, &["init", "--yes"]);
    run_ez(
        &repo.path,
        &["create", "feat/base", "--from", "main", "--no-worktree"],
    );
    run_ez(
        &repo.path,
        &[
            "create",
            "feat/child",
            "--from",
            "feat/base",
            "--no-worktree",
        ],
    );
    let command =
        "if [ \"$EZ_BRANCH\" = feat/base ]; then echo base-failed >&2; exit 7; fi; echo child-ran";

    let fail_fast = run_raw(
        &repo.path,
        env!("CARGO_BIN_EXE_ez"),
        &["worktree", "exec", "--json", "--", "sh", "-c", command],
    );
    assert!(!fail_fast.status.success());
    assert_eq!(fail_fast.status.code(), Some(7));
    let fail_fast_report = stdout_json(&fail_fast);
    assert_eq!(fail_fast_report["failed_count"], 1);
    assert_eq!(fail_fast_report["succeeded_count"], 0);
    assert_eq!(fail_fast_report["skipped_count"], 1);
    assert_eq!(fail_fast_report["stopped_early"], true);
    assert_eq!(fail_fast_report["entries"].as_array().unwrap().len(), 2);
    assert_eq!(fail_fast_report["entries"][0]["exit_code"], 7);
    assert_eq!(fail_fast_report["entries"][0]["stderr"], "base-failed\n");
    assert_eq!(fail_fast_report["entries"][1]["status"], "skipped");

    let keep_going = run_raw(
        &repo.path,
        env!("CARGO_BIN_EXE_ez"),
        &[
            "worktree",
            "exec",
            "--keep-going",
            "--json",
            "--",
            "sh",
            "-c",
            command,
        ],
    );
    assert!(!keep_going.status.success());
    assert_eq!(keep_going.status.code(), Some(7));
    let keep_going_report = stdout_json(&keep_going);
    assert_eq!(keep_going_report["failed_count"], 1);
    assert_eq!(keep_going_report["succeeded_count"], 1);
    assert_eq!(keep_going_report["skipped_count"], 0);
    assert_eq!(keep_going_report["stopped_early"], false);
    assert_eq!(keep_going_report["entries"].as_array().unwrap().len(), 2);
    assert_eq!(keep_going_report["entries"][1]["stdout"], "child-ran\n");
}

#[test]
fn fleet_exec_rejects_invalid_selection_before_running_the_command() {
    let repo = TempRepo::new();
    run_ez(&repo.path, &["init", "--yes"]);
    let marker = repo.path.join("must-not-run");

    let output = run_raw(
        &repo.path,
        env!("CARGO_BIN_EXE_ez"),
        &[
            "worktree",
            "exec",
            "main",
            "--",
            "sh",
            "-c",
            "touch must-not-run",
        ],
    );

    assert_eq!(output.status.code(), Some(5));
    assert!(!marker.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("trunk"));
}

#[test]
fn fleet_exec_reports_spawn_failures_and_preserves_conventional_exit_code() {
    let repo = TempRepo::new();
    run_ez(&repo.path, &["init", "--yes"]);
    run_ez(
        &repo.path,
        &["create", "feat/base", "--from", "main", "--no-worktree"],
    );
    run_ez(
        &repo.path,
        &[
            "create",
            "feat/child",
            "--from",
            "feat/base",
            "--no-worktree",
        ],
    );

    let output = run_raw(
        &repo.path,
        env!("CARGO_BIN_EXE_ez"),
        &[
            "worktree",
            "exec",
            "--keep-going",
            "--json",
            "--",
            "ez-command-that-does-not-exist",
        ],
    );
    assert_eq!(output.status.code(), Some(127));
    let report = stdout_json(&output);
    assert_eq!(report["failed_count"], 2);
    assert_eq!(report["skipped_count"], 0);
    assert!(
        report["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .all(|entry| entry["exit_code"] == 127
                && entry["stderr"]
                    .as_str()
                    .expect("spawn stderr")
                    .contains("No such file"))
    );
}

#[test]
fn fleet_exec_human_mode_streams_child_output() {
    let repo = TempRepo::new();
    run_ez(&repo.path, &["init", "--yes"]);
    run_ez(
        &repo.path,
        &["create", "feat/base", "--from", "main", "--no-worktree"],
    );

    let output = run_ez(
        &repo.path,
        &[
            "worktree",
            "exec",
            "feat/base",
            "--",
            "sh",
            "-c",
            "echo human-stdout; echo human-stderr >&2",
        ],
    );

    assert_eq!(String::from_utf8_lossy(&output.stdout), "human-stdout\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("human-stderr"));
    assert!(stderr.contains("Fleet command passed in 1 worktree"));
}
