use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempRepo {
    path: PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ez-worktree-ensure-cli-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create temp repo");
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
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .output()
        .expect("run command");
    assert!(
        output.status.success(),
        "{program} {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn run_ez(dir: &Path, args: &[&str]) -> Output {
    run(dir, env!("CARGO_BIN_EXE_ez"), args)
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout JSON")
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
