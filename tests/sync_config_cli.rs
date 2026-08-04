use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TestRepo {
    path: PathBuf,
    remote: PathBuf,
    fake_bin: PathBuf,
    gh_log: PathBuf,
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = std::fs::remove_dir_all(&self.remote);
        let _ = std::fs::remove_dir_all(&self.fake_bin);
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ez-{prefix}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).expect("create temp dir");
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
    assert_success(&output);
    output
}

fn run_ez(repo: &TestRepo, dir: &Path, args: &[&str]) -> Output {
    let inherited_path = std::env::var("PATH").unwrap_or_default();
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(
            "PATH",
            format!("{}:{inherited_path}", repo.fake_bin.display()),
        )
        .env("GH_LOG", &repo.gh_log)
        .output()
        .expect("run ez")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        stdout_text(output),
        stderr_text(output)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "expected failure\nstdout:\n{}\nstderr:\n{}",
        stdout_text(output),
        stderr_text(output)
    );
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    stdout_text(&run(dir, "git", args)).trim().to_string()
}

fn write_file(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).expect("write file");
}

fn commit_file(dir: &Path, name: &str, contents: &str, message: &str) {
    write_file(dir, name, contents);
    run(dir, "git", &["add", name]);
    run(dir, "git", &["commit", "-m", message]);
}

fn install_strict_fake_gh(prefix: &str) -> (PathBuf, PathBuf) {
    let fake_bin = temp_dir(prefix);
    let gh_log = fake_bin.join("gh.log");
    let script = fake_bin.join("gh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
echo "$@" >> "$GH_LOG"
printf 'unexpected gh invocation: %s\n' "$*" >&2
exit 97
"#,
    )
    .expect("write fake gh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script)
            .expect("fake gh metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod fake gh");
    }
    (fake_bin, gh_log)
}

fn init_repo(prefix: &str) -> TestRepo {
    let path = temp_dir(prefix);
    let remote = temp_dir(&format!("{prefix}-remote"));
    run(&remote, "git", &["init", "--bare", "-b", "main"]);

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

    let (fake_bin, gh_log) = install_strict_fake_gh(&format!("{prefix}-gh"));
    let repo = TestRepo {
        path,
        remote,
        fake_bin,
        gh_log,
    };
    assert_success(&run_ez(&repo, &repo.path, &["init", "--yes"]));
    repo
}

fn stack_path(repo: &TestRepo) -> PathBuf {
    repo.path.join(".git").join("ez").join("stack.json")
}

fn stack_state(repo: &TestRepo) -> Value {
    serde_json::from_slice(&std::fs::read(stack_path(repo)).expect("read stack state"))
        .expect("parse stack state")
}

fn save_stack_state(repo: &TestRepo, value: &Value) {
    std::fs::write(
        stack_path(repo),
        serde_json::to_vec_pretty(value).expect("serialize stack state"),
    )
    .expect("write stack state");
}

fn add_managed_branch(repo: &TestRepo, branch: &str, parent: &str, pr_number: u64) -> PathBuf {
    assert_success(&run_ez(
        repo,
        &repo.path,
        &["create", branch, "--from", parent, "--no-worktree"],
    ));
    run(&repo.path, "git", &["checkout", branch]);
    let filename = format!("{}.txt", branch.replace('/', "-"));
    commit_file(&repo.path, &filename, branch, branch);
    run(&repo.path, "git", &["push", "-u", "origin", branch]);
    run(&repo.path, "git", &["checkout", "main"]);

    let mut state = stack_state(repo);
    state["branches"][branch]["pr_number"] = Value::from(pr_number);
    save_stack_state(repo, &state);

    let worktree = repo.path.join(".worktrees").join(branch.replace('/', "-"));
    run(
        &repo.path,
        "git",
        &[
            "worktree",
            "add",
            worktree.to_str().expect("worktree path"),
            branch,
        ],
    );
    worktree
}

fn gh_log(repo: &TestRepo) -> String {
    std::fs::read_to_string(&repo.gh_log).unwrap_or_default()
}

fn advance_remote_main(repo: &TestRepo, prefix: &str) -> String {
    let writer = temp_dir(prefix);
    run(
        &writer,
        "git",
        &["clone", repo.remote.to_str().expect("remote path"), "."],
    );
    run(&writer, "git", &["config", "user.name", "Test User"]);
    run(
        &writer,
        "git",
        &["config", "user.email", "test@example.com"],
    );
    commit_file(
        &writer,
        "remote.txt",
        "remote-only\n",
        "remote-only advance",
    );
    run(&writer, "git", &["push", "origin", "main"]);
    let head = git_output(&writer, &["rev-parse", "HEAD"]);
    std::fs::remove_dir_all(writer).expect("remove remote writer");
    head
}

#[test]
fn config_list_reports_required_and_optional_values() {
    let repo = init_repo("config-list-values");
    assert_success(&run_ez(
        &repo,
        &repo.path,
        &["config", "set", "repo", "owner/project"],
    ));

    let output = run_ez(&repo, &repo.path, &["config", "list"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("trunk           = main"), "{stderr}");
    assert!(stderr.contains("remote          = origin"), "{stderr}");
    assert!(
        stderr.contains("repo            = owner/project"),
        "{stderr}"
    );
    assert!(stderr.contains("fork_repo       = (not set)"), "{stderr}");
}

#[test]
fn config_get_prints_set_values_to_stdout_only() {
    let repo = init_repo("config-get-stdout");
    assert_success(&run_ez(
        &repo,
        &repo.path,
        &["config", "set", "default_from", "main"],
    ));

    let output = run_ez(&repo, &repo.path, &["config", "get", "default_from"]);

    assert_success(&output);
    assert_eq!(stdout_text(&output).trim(), "main");
    assert!(
        stderr_text(&output).contains("[ok |"),
        "status line stays on stderr"
    );
}

#[test]
fn config_get_fails_for_unset_optional_key_without_mutating_state() {
    let repo = init_repo("config-get-unset");
    let before = std::fs::read_to_string(stack_path(&repo)).expect("state before");

    let output = run_ez(&repo, &repo.path, &["config", "get", "fork_repo"]);

    assert_failure(&output);
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("config key `fork_repo` is not set"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(stack_path(&repo)).expect("state after"),
        before
    );
}

#[test]
fn config_set_rejects_invalid_bool_and_preserves_previous_value() {
    let repo = init_repo("config-set-invalid-bool");
    assert_success(&run_ez(
        &repo,
        &repo.path,
        &["config", "set", "draft", "yes"],
    ));

    let output = run_ez(&repo, &repo.path, &["config", "set", "draft", "maybe"]);

    assert_failure(&output);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("invalid boolean value `maybe`"), "{stderr}");
    assert_eq!(stack_state(&repo)["draft"], Value::from(true));
}

#[test]
fn config_unset_required_key_fails_without_mutating_state() {
    let repo = init_repo("config-unset-required");
    let before = std::fs::read_to_string(stack_path(&repo)).expect("state before");

    let output = run_ez(&repo, &repo.path, &["config", "unset", "remote"]);

    assert_failure(&output);
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("config key `remote` cannot be unset"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(stack_path(&repo)).expect("state after"),
        before
    );
}

#[test]
fn config_unset_optional_key_is_idempotent() {
    let repo = init_repo("config-unset-idempotent");
    assert_success(&run_ez(
        &repo,
        &repo.path,
        &["config", "set", "fork_repo", "owner/fork"],
    ));
    assert_success(&run_ez(
        &repo,
        &repo.path,
        &["config", "unset", "fork_repo"],
    ));

    let output = run_ez(&repo, &repo.path, &["config", "unset", "fork_repo"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("fork_repo is already unset"), "{stderr}");
    assert!(stack_state(&repo)["fork_repo"].is_null());
}

#[test]
fn sync_dry_run_reports_fork_workflow_preview_without_fetching_or_calling_github() {
    let repo = init_repo("sync-dry-run-fork-preview");
    let worktree = add_managed_branch(&repo, "feat/topic", "main", 42);
    assert_success(&run_ez(
        &repo,
        &repo.path,
        &["config", "set", "repo", "owner/project"],
    ));
    assert_success(&run_ez(
        &repo,
        &repo.path,
        &["config", "set", "fork_repo", "owner/fork"],
    ));
    let tracking_before = git_output(&repo.path, &["rev-parse", "origin/main"]);
    let remote_head = advance_remote_main(&repo, "sync-dry-run-fork-writer");
    assert_ne!(tracking_before, remote_head, "remote must be ahead locally");

    let before = std::fs::read_to_string(stack_path(&repo)).expect("state before");
    let output = run_ez(&repo, &repo.path, &["sync", "--dry-run"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("Sync preview (--dry-run"), "{stderr}");
    assert!(stderr.contains("Would fetch from `origin`"), "{stderr}");
    assert!(
        stderr.contains("Would check if PR for `feat/topic` is merged or closed"),
        "{stderr}"
    );
    let worktree = std::fs::canonicalize(worktree).expect("canonical worktree");
    assert!(
        stderr.contains(&format!(
            "Would remove worktree at `{}`",
            worktree.display()
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains("GitHub native stacks are not applicable"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(stack_path(&repo)).expect("state after"),
        before
    );
    assert_eq!(
        git_output(&repo.path, &["branch", "--show-current"]),
        "main"
    );
    assert_eq!(
        git_output(&repo.path, &["rev-parse", "origin/main"]),
        tracking_before,
        "dry-run must not fetch the remote-only commit"
    );
    assert_eq!(
        git_output(&repo.remote, &["rev-parse", "main"]),
        remote_head
    );
    assert_eq!(gh_log(&repo), "");
}

#[test]
fn sync_dry_run_reports_native_stack_chain_without_calling_github() {
    let repo = init_repo("sync-dry-run-native-preview");
    add_managed_branch(&repo, "feat/base", "main", 41);
    add_managed_branch(&repo, "feat/topic", "feat/base", 42);
    let tracking_before = git_output(&repo.path, &["rev-parse", "origin/main"]);
    let remote_head = advance_remote_main(&repo, "sync-dry-run-native-writer");
    assert_ne!(tracking_before, remote_head, "remote must be ahead locally");

    let output = run_ez(&repo, &repo.path, &["sync", "--dry-run"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains(
            "Would reconcile GitHub native stack for PRs [41, 42] (feat/base -> feat/topic)"
        ),
        "{stderr}"
    );
    assert_eq!(
        git_output(&repo.path, &["rev-parse", "origin/main"]),
        tracking_before,
        "dry-run must not fetch the remote-only commit"
    );
    assert_eq!(
        git_output(&repo.remote, &["rev-parse", "main"]),
        remote_head
    );
    assert_eq!(gh_log(&repo), "");
}

#[test]
fn sync_dry_run_reports_native_stack_branching_skip_without_calling_github() {
    let repo = init_repo("sync-dry-run-native-branching-preview");
    add_managed_branch(&repo, "feat/base", "main", 41);
    add_managed_branch(&repo, "feat/left", "feat/base", 42);
    add_managed_branch(&repo, "feat/right", "feat/base", 43);
    let tracking_before = git_output(&repo.path, &["rev-parse", "origin/main"]);
    let remote_head = advance_remote_main(&repo, "sync-dry-run-native-branching-writer");
    assert_ne!(tracking_before, remote_head, "remote must be ahead locally");

    let before = std::fs::read_to_string(stack_path(&repo)).expect("state before");
    let output = run_ez(&repo, &repo.path, &["sync", "--dry-run"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("Would skip GitHub native stack for `feat/base` (branching_component)"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("Would reconcile GitHub native stack"),
        "branching graph must not be flattened into a native stack preview:\n{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(stack_path(&repo)).expect("state after"),
        before
    );
    assert_eq!(
        git_output(&repo.path, &["rev-parse", "origin/main"]),
        tracking_before,
        "dry-run must not fetch the remote-only commit"
    );
    assert_eq!(
        git_output(&repo.remote, &["rev-parse", "main"]),
        remote_head
    );
    assert_eq!(gh_log(&repo), "");
}

#[test]
fn sync_autostash_without_dirty_changes_runs_cleanly_and_leaves_no_stash() {
    let repo = init_repo("sync-autostash-clean");

    let output = run_ez(&repo, &repo.path, &["sync", "--autostash"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("Everything is up to date"), "{stderr}");
    assert!(
        !stderr.contains("Stashed uncommitted changes"),
        "clean autostash must not create a stash:\n{stderr}"
    );
    assert_eq!(
        run_raw(&repo.path, "git", &["stash", "list"])
            .stdout
            .as_slice(),
        b""
    );
    assert_eq!(gh_log(&repo), "");
}
