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
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ez-track-cli-{prefix}-{}-{}-{}",
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
        commit_file(&path, "tracked.txt", "initial\n", "initial");
        run_ez(&path, &["init", "--yes"]);
        Self { path }
    }

    fn unmanaged(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ez-track-cli-{prefix}-{}-{}-{}",
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
        commit_file(&path, "tracked.txt", "initial\n", "initial");
        Self { path }
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn run_raw(dir: &Path, program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .output()
        .expect("run command")
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    let output = run_raw(dir, program, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn run_ez_raw(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .output()
        .expect("run ez")
}

fn run_ez(dir: &Path, args: &[&str]) -> Output {
    let output = run_ez_raw(dir, args);
    assert!(
        output.status.success(),
        "ez {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    stdout_text(&run(dir, "git", args))
}

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(dir.join(file), contents).expect("write file");
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
}

fn create_raw_branch(dir: &Path, branch: &str, parent: &str, file: &str) -> String {
    run(dir, "git", &["checkout", parent]);
    let parent_head = git_output(dir, &["rev-parse", "HEAD"]);
    run(dir, "git", &["checkout", "-b", branch]);
    commit_file(dir, file, &format!("{branch}\n"), branch);
    run(dir, "git", &["checkout", "main"]);
    parent_head
}

fn create_empty_raw_branch(dir: &Path, branch: &str, parent: &str) -> String {
    run(dir, "git", &["checkout", parent]);
    let parent_head = git_output(dir, &["rev-parse", "HEAD"]);
    run(dir, "git", &["checkout", "-b", branch]);
    run(dir, "git", &["checkout", "main"]);
    parent_head
}

fn create_unrelated_raw_branch(dir: &Path, branch: &str, file: &str) {
    run(dir, "git", &["checkout", "--orphan", branch]);
    run(dir, "git", &["rm", "-rf", "."]);
    commit_file(dir, file, &format!("{branch}\n"), branch);
    run(dir, "git", &["checkout", "main"]);
}

fn create_managed_branch(dir: &Path, branch: &str, parent: &str, file: &str) -> String {
    run_ez(dir, &["create", branch, "--from", parent, "--no-worktree"]);
    run(dir, "git", &["checkout", branch]);
    commit_file(dir, file, &format!("{branch}\n"), branch);
    run(dir, "git", &["checkout", "main"]);
    stack_state(dir)["branches"][branch]["parent_head"]
        .as_str()
        .expect("parent head")
        .to_string()
}

fn git_common_dir(repo: &Path) -> PathBuf {
    let raw = git_output(repo, &["rev-parse", "--git-common-dir"]);
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        repo.join(path)
    }
}

fn stack_path(repo: &Path) -> PathBuf {
    git_common_dir(repo).join("ez").join("stack.json")
}

fn stack_state(repo: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(stack_path(repo)).expect("read stack state"))
        .expect("parse stack state")
}

fn stack_state_bytes(repo: &Path) -> Vec<u8> {
    std::fs::read(stack_path(repo)).expect("read stack state")
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "expected failure\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn receipt_json(output: &Output) -> Value {
    let stderr = stderr_text(output);
    let line = stderr
        .lines()
        .find(|line| line.contains(r#""cmd":"track""#))
        .unwrap_or_else(|| panic!("missing track receipt in stderr:\n{stderr}"));
    let start = line.find('{').expect("receipt start");
    let end = line.rfind('}').expect("receipt end");
    serde_json::from_str(&line[start..=end]).expect("receipt JSON")
}

#[test]
fn track_records_explicit_parent_for_existing_branch() {
    let repo = TempRepo::new("explicit-parent");
    create_managed_branch(&repo.path, "feat/base", "main", "base.txt");
    let expected_parent_head =
        create_raw_branch(&repo.path, "feat/child", "feat/base", "child.txt");

    let output = run_ez(
        &repo.path,
        &["track", "feat/child", "--parent", "feat/base"],
    );

    let state = stack_state(&repo.path);
    assert_eq!(state["branches"]["feat/child"]["parent"], "feat/base");
    assert_eq!(
        state["branches"]["feat/child"]["parent_head"],
        expected_parent_head
    );
    assert_eq!(state["branches"]["feat/child"]["pr_number"], Value::Null);
    let receipt = receipt_json(&output);
    assert_eq!(receipt["cmd"], "track");
    assert_eq!(receipt["branch"], "feat/child");
    assert_eq!(receipt["parent"], "feat/base");
    assert_eq!(receipt["commits_ahead"], 1);
}

#[test]
fn track_infers_closest_tracked_ancestor_as_parent() {
    let repo = TempRepo::new("infer-parent");
    create_managed_branch(&repo.path, "feat/base", "main", "base.txt");
    let expected_parent_head =
        create_raw_branch(&repo.path, "feat/child", "feat/base", "child.txt");

    run_ez(&repo.path, &["track", "feat/child"]);

    let state = stack_state(&repo.path);
    assert_eq!(state["branches"]["feat/child"]["parent"], "feat/base");
    assert_eq!(
        state["branches"]["feat/child"]["parent_head"],
        expected_parent_head
    );
}

#[test]
fn track_infers_trunk_for_sibling_branch() {
    let repo = TempRepo::new("infer-trunk");
    create_managed_branch(&repo.path, "feat/base", "main", "base.txt");
    let expected_parent_head = create_raw_branch(&repo.path, "feat/sibling", "main", "sibling.txt");

    run_ez(&repo.path, &["track", "feat/sibling"]);

    let state = stack_state(&repo.path);
    assert_eq!(state["branches"]["feat/sibling"]["parent"], "main");
    assert_eq!(
        state["branches"]["feat/sibling"]["parent_head"],
        expected_parent_head
    );
}

#[test]
fn track_defaults_to_current_branch_when_branch_argument_is_omitted() {
    let repo = TempRepo::new("current-branch");
    let expected_parent_head = create_raw_branch(&repo.path, "feat/current", "main", "current.txt");
    run(&repo.path, "git", &["checkout", "feat/current"]);

    run_ez(&repo.path, &["track"]);

    let state = stack_state(&repo.path);
    assert_eq!(state["branches"]["feat/current"]["parent"], "main");
    assert_eq!(
        state["branches"]["feat/current"]["parent_head"],
        expected_parent_head
    );
}

#[test]
fn track_records_empty_branch_with_zero_commits_ahead() {
    let repo = TempRepo::new("empty-branch");
    let expected_parent_head = create_empty_raw_branch(&repo.path, "feat/empty", "main");

    let output = run_ez(&repo.path, &["track", "feat/empty"]);

    let state = stack_state(&repo.path);
    assert_eq!(state["branches"]["feat/empty"]["parent"], "main");
    assert_eq!(
        state["branches"]["feat/empty"]["parent_head"],
        expected_parent_head
    );
    assert!(
        stderr_text(&output).contains("`feat/empty` has no commits beyond `main` yet"),
        "{}",
        stderr_text(&output)
    );
    assert_eq!(receipt_json(&output)["commits_ahead"], 0);
}

#[test]
fn track_rejects_already_tracked_branch_without_mutating_state() {
    let repo = TempRepo::new("already-tracked");
    create_managed_branch(&repo.path, "feat/base", "main", "base.txt");
    let before = stack_state_bytes(&repo.path);

    let output = run_ez_raw(&repo.path, &["track", "feat/base"]);

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(5));
    assert!(
        stderr_text(&output).contains("branch `feat/base` is already tracked"),
        "{}",
        stderr_text(&output)
    );
    assert_eq!(stack_state_bytes(&repo.path), before);
}

#[test]
fn track_rejects_missing_local_parent_branch_without_mutating_state() {
    let repo = TempRepo::new("missing-parent-ref");
    create_managed_branch(&repo.path, "feat/base", "main", "base.txt");
    run(&repo.path, "git", &["branch", "-D", "feat/base"]);
    create_raw_branch(&repo.path, "feat/child", "main", "child.txt");
    let before = stack_state_bytes(&repo.path);

    let output = run_ez_raw(
        &repo.path,
        &["track", "feat/child", "--parent", "feat/base"],
    );

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(5));
    assert!(
        stderr_text(&output).contains("parent `feat/base` does not exist locally"),
        "{}",
        stderr_text(&output)
    );
    assert_eq!(stack_state_bytes(&repo.path), before);
}

#[test]
fn track_rejects_untracked_parent_without_mutating_state() {
    let repo = TempRepo::new("untracked-parent");
    create_raw_branch(&repo.path, "feat/orphan", "main", "orphan.txt");
    create_empty_raw_branch(&repo.path, "feat/unmanaged-parent", "main");
    let before = stack_state_bytes(&repo.path);

    let output = run_ez_raw(
        &repo.path,
        &["track", "feat/orphan", "--parent", "feat/unmanaged-parent"],
    );

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(5));
    assert!(
        stderr_text(&output).contains("parent `feat/unmanaged-parent` is not the trunk"),
        "{}",
        stderr_text(&output)
    );
    assert_eq!(stack_state_bytes(&repo.path), before);
}

#[test]
fn track_rejects_unrelated_branch_with_explicit_parent_without_mutating_state() {
    let repo = TempRepo::new("unrelated-explicit-parent");
    create_unrelated_raw_branch(&repo.path, "feat/unrelated", "orphan.txt");
    let before = stack_state_bytes(&repo.path);

    let output = run_ez_raw(&repo.path, &["track", "feat/unrelated", "--parent", "main"]);

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(5));
    assert!(
        stderr_text(&output).contains("`feat/unrelated` and `main` have no common history"),
        "{}",
        stderr_text(&output)
    );
    assert_eq!(stack_state_bytes(&repo.path), before);
}

#[test]
fn track_rejects_unrelated_branch_when_inference_has_no_trunk_merge_base() {
    let repo = TempRepo::new("unrelated-inferred-parent");
    create_unrelated_raw_branch(&repo.path, "feat/unrelated", "orphan.txt");
    let before = stack_state_bytes(&repo.path);

    let output = run_ez_raw(&repo.path, &["track", "feat/unrelated"]);

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(5));
    assert!(
        stderr_text(&output).contains("could not find a merge-base between `feat/unrelated`"),
        "{}",
        stderr_text(&output)
    );
    assert_eq!(stack_state_bytes(&repo.path), before);
}

#[test]
fn track_rejects_missing_branch_without_mutating_state() {
    let repo = TempRepo::new("missing-branch");
    let before = stack_state_bytes(&repo.path);

    let output = run_ez_raw(&repo.path, &["track", "feat/missing"]);

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(5));
    assert!(
        stderr_text(&output).contains("branch `feat/missing` does not exist locally"),
        "{}",
        stderr_text(&output)
    );
    assert_eq!(stack_state_bytes(&repo.path), before);
}

#[test]
fn track_rejects_trunk_without_mutating_state() {
    let repo = TempRepo::new("trunk");
    let before = stack_state_bytes(&repo.path);

    let output = run_ez_raw(&repo.path, &["track", "main"]);

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(5));
    assert!(
        stderr_text(&output).contains("`main` is the trunk branch"),
        "{}",
        stderr_text(&output)
    );
    assert_eq!(stack_state_bytes(&repo.path), before);
}

#[test]
fn track_rejects_branch_as_its_own_parent_without_mutating_state() {
    let repo = TempRepo::new("self-parent");
    create_raw_branch(&repo.path, "feat/self", "main", "self.txt");
    let before = stack_state_bytes(&repo.path);

    let output = run_ez_raw(&repo.path, &["track", "feat/self", "--parent", "feat/self"]);

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(5));
    assert!(
        stderr_text(&output).contains("cannot set `feat/self` as its own parent"),
        "{}",
        stderr_text(&output)
    );
    assert_eq!(stack_state_bytes(&repo.path), before);
}

#[test]
fn track_requires_initialized_ez_state() {
    let repo = TempRepo::unmanaged("unmanaged");
    create_raw_branch(&repo.path, "feat/raw", "main", "raw.txt");

    let output = run_ez_raw(&repo.path, &["track", "feat/raw"]);

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(5));
    assert!(
        stderr_text(&output).contains("ez is not initialized"),
        "{}",
        stderr_text(&output)
    );
    assert!(!repo.path.join(".git/ez/stack.json").exists());
}
