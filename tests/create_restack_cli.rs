use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_REPO_ID: AtomicU64 = AtomicU64::new(1);

struct TestRepo {
    path: PathBuf,
    remote: PathBuf,
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = std::fs::remove_dir_all(&self.remote);
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ez-create-restack-{prefix}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
        NEXT_REPO_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).expect("create temp directory");
    path
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

fn run_ez(dir: &Path, args: &[&str]) -> Output {
    run(dir, env!("CARGO_BIN_EXE_ez"), args)
}

fn run_ez_raw(dir: &Path, args: &[&str]) -> Output {
    run_raw(dir, env!("CARGO_BIN_EXE_ez"), args)
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

fn write_file(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).expect("write file");
}

fn append_file(dir: &Path, name: &str, contents: &str) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join(name))
        .expect("open file");
    file.write_all(contents.as_bytes()).expect("append file");
}

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) -> String {
    write_file(dir, file, contents);
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
    git_output(dir, &["rev-parse", "HEAD"])
}

fn init_repo(prefix: &str) -> TestRepo {
    let path = temp_dir(prefix);
    let remote = temp_dir(&format!("{prefix}-remote"));
    run(&remote, "git", &["init", "--bare"]);

    run(&path, "git", &["init", "-b", "main"]);
    run(&path, "git", &["config", "user.name", "Test User"]);
    run(&path, "git", &["config", "user.email", "test@example.com"]);
    commit_file(&path, "tracked.txt", "initial\n", "initial");
    run(
        &path,
        "git",
        &[
            "remote",
            "add",
            "origin",
            remote.to_str().expect("remote path"),
        ],
    );
    run(&path, "git", &["push", "-u", "origin", "main"]);
    run_ez(&path, &["init", "--yes"]);

    TestRepo { path, remote }
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

fn write_stack_state(repo: &Path, state: &Value) {
    std::fs::write(
        stack_path(repo),
        serde_json::to_vec_pretty(state).expect("serialize stack state"),
    )
    .expect("write stack state");
}

fn expected_worktree(repo: &Path, branch: &str) -> PathBuf {
    repo.join(".worktrees").join(branch.replace('/', "-"))
}

fn current_branch(repo: &Path) -> String {
    git_output(repo, &["branch", "--show-current"])
}

fn ref_tip(repo: &Path, name: &str) -> String {
    git_output(repo, &["rev-parse", name])
}

fn status_porcelain(repo: &Path) -> String {
    git_output(repo, &["status", "--porcelain"])
}

fn receipt_with_action(output: &Output, action: &str) -> Value {
    stderr_text(output)
        .lines()
        .filter_map(|line| {
            let start = line.find('{')?;
            let end = line.rfind('}')?;
            serde_json::from_str::<Value>(&line[start..=end]).ok()
        })
        .find(|value| value["action"] == action)
        .unwrap_or_else(|| panic!("no receipt action `{action}` in:\n{}", stderr_text(output)))
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "expected failure\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn create_branch_worktree(repo: &TestRepo, branch: &str, parent: &str, file: &str) -> PathBuf {
    run_ez(&repo.path, &["create", branch, "--from", parent]);
    let worktree = expected_worktree(&repo.path, branch);
    commit_file(&worktree, file, &format!("{branch}\n"), branch);
    worktree
}

fn clone_writer(repo: &TestRepo, prefix: &str) -> PathBuf {
    let writer = temp_dir(prefix);
    run(
        &writer,
        "git",
        &["clone", repo.remote.to_str().expect("remote"), "."],
    );
    run(&writer, "git", &["config", "user.name", "Test User"]);
    run(
        &writer,
        "git",
        &["config", "user.email", "test@example.com"],
    );
    writer
}

#[test]
fn create_hook_without_value_lists_available_hooks_and_does_not_create_branch() {
    let repo = init_repo("hook-list");
    let hook_dir = repo.path.join(".ez/hooks/post-create");
    std::fs::create_dir_all(&hook_dir).expect("create hook dir");
    write_file(&hook_dir, "setup-node.md", "node setup\n");
    write_file(&hook_dir, "setup-rust.md", "rust setup\n");
    write_file(&hook_dir, "notes.txt", "ignored\n");

    let output = run_ez(&repo.path, &["create", "feat/hooked", "--hook"]);

    assert_eq!(stdout_text(&output), "setup-node\n  setup-rust");
    let stderr = stderr_text(&output);
    assert!(stderr.contains("Available post-create hooks"));
    assert!(stderr.contains("ez create <branch> --hook <name>"));
    assert_eq!(
        git_output(&repo.path, &["branch", "--list", "feat/hooked"]),
        ""
    );
    assert!(stack_state(&repo.path)["branches"]["feat/hooked"].is_null());
}

#[test]
fn create_scope_and_named_hook_are_recorded_for_branch_only_create() {
    let repo = init_repo("scope-hook");
    let hook_dir = repo.path.join(".ez/hooks/post-create");
    std::fs::create_dir_all(&hook_dir).expect("create hook dir");
    write_file(&hook_dir, "setup-rust.md", "Run cargo test.\n");

    let output = run_ez(
        &repo.path,
        &[
            "create",
            "feat/scoped",
            "--no-worktree",
            "--scope",
            " src/** ",
            "--scope",
            "src/**",
            "--scope-mode",
            "strict",
            "--hook",
            "setup-rust",
        ],
    );

    let stderr = stderr_text(&output);
    assert!(stderr.contains("Hook: post-create/setup-rust"));
    assert!(stderr.contains("Run cargo test."));
    let state = stack_state(&repo.path);
    assert_eq!(
        state["branches"]["feat/scoped"]["scope"],
        serde_json::json!(["src/**"])
    );
    assert_eq!(state["branches"]["feat/scoped"]["scope_mode"], "strict");
    assert_eq!(current_branch(&repo.path), "main");
}

#[test]
fn create_on_trunk_uses_valid_default_from_parent() {
    let repo = init_repo("default-from-valid");
    run_ez(&repo.path, &["create", "feat/base", "--no-worktree"]);
    let mut state = stack_state(&repo.path);
    state["default_from"] = Value::from("feat/base");
    write_stack_state(&repo.path, &state);

    let output = run_ez(&repo.path, &["create", "feat/child", "--no-worktree"]);

    assert!(stderr_text(&output).contains("Using default parent `feat/base`"));
    assert_eq!(
        stack_state(&repo.path)["branches"]["feat/child"]["parent"],
        "feat/base"
    );
}

#[test]
fn create_on_trunk_warns_and_uses_trunk_when_default_from_is_untracked() {
    let repo = init_repo("default-from-invalid");
    let mut state = stack_state(&repo.path);
    state["default_from"] = Value::from("scratch");
    write_stack_state(&repo.path, &state);

    let output = run_ez(&repo.path, &["create", "feat/child", "--no-worktree"]);

    assert!(stderr_text(&output).contains("using trunk"));
    assert_eq!(
        stack_state(&repo.path)["branches"]["feat/child"]["parent"],
        "main"
    );
}

#[test]
fn create_am_commits_tracked_change_and_moves_untracked_file_without_staging_it() {
    let repo = init_repo("create-am-transfer");
    append_file(&repo.path, "tracked.txt", "tracked edit\n");
    write_file(&repo.path, "untracked.txt", "untracked\n");

    let output = run_ez(&repo.path, &["create", "feat/am", "-am", "tracked only"]);

    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("`-A`/`-Am`"),
        "expected tracked-only hint:\n{stderr}"
    );
    let worktree = expected_worktree(&repo.path, "feat/am");
    assert!(worktree.join("untracked.txt").exists());
    assert!(status_porcelain(&worktree).contains("?? untracked.txt"));
    assert_eq!(status_porcelain(&repo.path), "?? .worktrees/");
    assert!(
        git_output(&worktree, &["show", "--name-only", "--format=%s", "HEAD"])
            .contains("tracked.txt")
    );
}

#[test]
fn create_all_files_commits_untracked_file_in_new_worktree() {
    let repo = init_repo("create-all-files");
    write_file(&repo.path, "new.txt", "new\n");

    let output = run_ez(&repo.path, &["create", "feat/all-files", "-Am", "add new"]);

    assert_success(&output);
    let worktree = expected_worktree(&repo.path, "feat/all-files");
    assert!(worktree.join("new.txt").exists());
    assert_eq!(status_porcelain(&worktree), "");
    assert!(
        git_output(&worktree, &["show", "--name-only", "--format=%s", "HEAD"]).contains("new.txt")
    );
}

#[test]
fn create_message_without_staged_changes_fails_before_creating_branch() {
    let repo = init_repo("create-nothing");

    let output = run_ez_raw(&repo.path, &["create", "feat/empty", "-m", "empty"]);

    assert_failure(&output);
    assert!(stderr_text(&output).contains("Stage changes first"));
    assert_eq!(
        git_output(&repo.path, &["branch", "--list", "feat/empty"]),
        ""
    );
}

#[test]
fn restack_when_everything_is_current_reports_nothing_to_do() {
    let repo = init_repo("restack-current");
    create_branch_worktree(&repo, "feat/current", "main", "current.txt");

    let output = run_ez(&repo.path, &["restack"]);

    assert!(stderr_text(&output).contains("All branches are up to date"));
    assert_eq!(current_branch(&repo.path), "main");
}

#[test]
fn restack_fetches_remote_trunk_and_rebases_linked_worktree() {
    let repo = init_repo("restack-fetch");
    let worktree = create_branch_worktree(&repo, "feat/topic", "main", "topic.txt");
    let writer = clone_writer(&repo, "restack-fetch-writer");
    commit_file(&writer, "remote.txt", "remote\n", "remote main");
    run(&writer, "git", &["push", "origin", "main"]);
    let remote_tip = git_output(&writer, &["rev-parse", "HEAD"]);

    let output = run_ez(&repo.path, &["restack"]);

    assert_success(&output);
    assert!(stderr_text(&output).contains("Updated `main` to latest"));
    assert_eq!(ref_tip(&repo.path, "main"), remote_tip);
    assert_eq!(
        git_output(&repo.path, &["merge-base", "main", "feat/topic"]),
        remote_tip
    );
    assert!(worktree.join("remote.txt").exists());
    assert_eq!(
        stack_state(&repo.path)["branches"]["feat/topic"]["parent_head"],
        Value::from(remote_tip)
    );
    let _ = std::fs::remove_dir_all(writer);
}

#[test]
fn restack_dirty_linked_worktree_returns_incomplete_and_leaves_branch_retryable() {
    let repo = init_repo("restack-dirty");
    let parent = create_branch_worktree(&repo, "feat/base", "main", "base.txt");
    let child = create_branch_worktree(&repo, "feat/child", "feat/base", "child.txt");
    append_file(&child, "child.txt", "dirty\n");
    let before_child = ref_tip(&repo.path, "feat/child");
    commit_file(&parent, "base2.txt", "base2\n", "base2");

    let output = run_ez_raw(&repo.path, &["restack"]);

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(3));
    let stderr = stderr_text(&output);
    assert!(stderr.contains("restack_incomplete"), "{stderr}");
    assert!(
        stderr.contains("cannot rebase: You have unstaged changes"),
        "{stderr}"
    );
    let receipt = receipt_with_action(&output, "restack_failed");
    assert_eq!(receipt["reason"], "git_error");
    assert_eq!(ref_tip(&repo.path, "feat/child"), before_child);
    assert_ne!(
        stack_state(&repo.path)["branches"]["feat/child"]["parent_head"],
        Value::from(ref_tip(&repo.path, "feat/base"))
    );
}

#[test]
fn restack_from_linked_worktree_restores_original_branch_after_success() {
    let repo = init_repo("restack-linked-origin");
    let base = create_branch_worktree(&repo, "feat/base", "main", "base.txt");
    let child = create_branch_worktree(&repo, "feat/child", "feat/base", "child.txt");
    commit_file(&base, "base2.txt", "base2\n", "base2");

    let output = run_ez(&child, &["restack"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("linked worktree"));
    assert!(stderr.contains("Restacked 1 branch(es)"));
    assert_eq!(current_branch(&child), "feat/child");
    let base_tip = ref_tip(&repo.path, "feat/base");
    assert_eq!(
        git_output(&repo.path, &["merge-base", "feat/base", "feat/child"]),
        base_tip
    );
}

#[test]
fn restack_merge_commit_preflight_blocks_before_rewriting_branch_or_state() {
    let repo = init_repo("restack-merge-preflight");
    let worktree = create_branch_worktree(&repo, "feat/topic", "main", "topic.txt");
    run(&worktree, "git", &["checkout", "-b", "side"]);
    commit_file(&worktree, "side.txt", "side\n", "side");
    run(&worktree, "git", &["checkout", "feat/topic"]);
    run(
        &worktree,
        "git",
        &["merge", "--no-ff", "side", "-m", "merge side"],
    );
    commit_file(&repo.path, "trunk.txt", "trunk\n", "trunk");
    let before_tip = ref_tip(&repo.path, "feat/topic");
    let before_state = stack_state(&repo.path);

    let output = run_ez_raw(&repo.path, &["restack"]);

    assert_failure(&output);
    assert_eq!(
        output.status.code(),
        Some(5),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        stderr_text(&output)
    );
    assert_eq!(ref_tip(&repo.path, "feat/topic"), before_tip);
    assert_eq!(stack_state(&repo.path), before_state);
    let receipt = receipt_with_action(&output, "rebase_preflight");
    assert_eq!(receipt["status"], "blocked");
    assert_eq!(receipt["merge_commits"], 1);
}
