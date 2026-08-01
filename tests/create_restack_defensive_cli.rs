use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_DEFENSIVE_REPO_ID: AtomicU64 = AtomicU64::new(1);

struct DefensiveRepo {
    path: PathBuf,
    remote: PathBuf,
}

impl DefensiveRepo {
    fn new(prefix: &str) -> Self {
        let path = defensive_temp_dir(prefix);
        let remote = defensive_temp_dir(&format!("{prefix}-remote"));
        defensive_run(&remote, "git", &["init", "--bare", "-b", "main"]);
        defensive_run(&path, "git", &["init", "-b", "main"]);
        defensive_run(&path, "git", &["config", "user.name", "Test User"]);
        defensive_run(&path, "git", &["config", "user.email", "test@example.com"]);
        defensive_commit_file(&path, "tracked.txt", "initial\n", "initial");
        defensive_run(
            &path,
            "git",
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().expect("remote path"),
            ],
        );
        defensive_run(&path, "git", &["push", "-u", "origin", "main"]);
        defensive_run_ez(&path, &["init", "--yes"]);
        Self { path, remote }
    }
}

impl Drop for DefensiveRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = std::fs::remove_dir_all(&self.remote);
    }
}

fn defensive_temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ez-create-restack-defensive-{prefix}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
        NEXT_DEFENSIVE_REPO_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).expect("create temp directory");
    path
}

fn defensive_run_raw(dir: &Path, program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .output()
        .expect("run command")
}

fn defensive_run(dir: &Path, program: &str, args: &[&str]) -> Output {
    let output = defensive_run_raw(dir, program, args);
    assert!(
        output.status.success(),
        "{program} {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn defensive_run_ez(dir: &Path, args: &[&str]) -> Output {
    defensive_run(dir, env!("CARGO_BIN_EXE_ez"), args)
}

fn defensive_run_ez_raw(dir: &Path, args: &[&str]) -> Output {
    defensive_run_raw(dir, env!("CARGO_BIN_EXE_ez"), args)
}

fn defensive_stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn defensive_stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn defensive_git_output(dir: &Path, args: &[&str]) -> String {
    defensive_stdout(&defensive_run(dir, "git", args))
}

fn defensive_commit_file(dir: &Path, file: &str, contents: &str, message: &str) -> String {
    std::fs::write(dir.join(file), contents).expect("write commit file");
    defensive_run(dir, "git", &["add", file]);
    defensive_run(dir, "git", &["commit", "-m", message]);
    defensive_git_output(dir, &["rev-parse", "HEAD"])
}

fn defensive_commit_file_on_branch(
    repo: &Path,
    branch: &str,
    file: &str,
    contents: &str,
    message: &str,
) -> String {
    defensive_run(repo, "git", &["checkout", branch]);
    defensive_commit_file(repo, file, contents, message)
}

fn defensive_append_file(dir: &Path, file: &str, contents: &str) {
    use std::io::Write;
    let mut handle = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join(file))
        .expect("open file");
    handle.write_all(contents.as_bytes()).expect("append file");
}

fn defensive_common_dir(repo: &Path) -> PathBuf {
    let raw = defensive_git_output(repo, &["rev-parse", "--git-common-dir"]);
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        repo.join(path)
    }
}

fn defensive_stack_path(repo: &Path) -> PathBuf {
    defensive_common_dir(repo).join("ez").join("stack.json")
}

fn defensive_stack_state(repo: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(defensive_stack_path(repo)).expect("read stack state"))
        .expect("parse stack state")
}

fn defensive_write_stack_state(repo: &Path, state: &Value) {
    std::fs::write(
        defensive_stack_path(repo),
        serde_json::to_vec_pretty(state).expect("serialize state"),
    )
    .expect("write stack state");
}

fn defensive_expected_worktree(repo: &Path, branch: &str) -> PathBuf {
    repo.join(".worktrees").join(branch.replace('/', "-"))
}

fn defensive_ref_tip(repo: &Path, rev: &str) -> String {
    defensive_git_output(repo, &["rev-parse", rev])
}

fn defensive_branch_list(repo: &Path, pattern: &str) -> String {
    defensive_git_output(repo, &["branch", "--list", pattern])
}

fn defensive_status(repo: &Path) -> String {
    defensive_git_output(repo, &["status", "--porcelain"])
}

fn defensive_receipt_with_action(output: &Output, action: &str) -> Value {
    defensive_stderr(output)
        .lines()
        .filter_map(|line| {
            let start = line.find('{')?;
            let end = line.rfind('}')?;
            serde_json::from_str::<Value>(&line[start..=end]).ok()
        })
        .find(|value| value["action"] == action)
        .unwrap_or_else(|| {
            panic!(
                "no receipt action `{action}` in:\n{}",
                defensive_stderr(output)
            )
        })
}

fn defensive_receipt_with_cmd(output: &Output, cmd: &str) -> Value {
    defensive_stderr(output)
        .lines()
        .filter_map(|line| {
            let start = line.find('{')?;
            let end = line.rfind('}')?;
            serde_json::from_str::<Value>(&line[start..=end]).ok()
        })
        .find(|value| value["cmd"] == cmd)
        .unwrap_or_else(|| panic!("no receipt cmd `{cmd}` in:\n{}", defensive_stderr(output)))
}

fn defensive_create_committed_worktree(
    repo: &DefensiveRepo,
    branch: &str,
    parent: &str,
) -> PathBuf {
    defensive_run_ez(&repo.path, &["create", branch, "--from", parent]);
    let worktree = defensive_expected_worktree(&repo.path, branch);
    defensive_commit_file(
        &worktree,
        &format!("{}.txt", branch.replace('/', "-")),
        &format!("{branch}\n"),
        branch,
    );
    worktree
}

fn defensive_assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "expected failure\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn create_hook_without_value_reports_empty_hook_directory_without_loading_stack_state() {
    let repo = DefensiveRepo::new("create-empty-hooks");

    let output = defensive_run_ez(&repo.path, &["create", "feat/no-hooks", "--hook"]);

    assert_eq!(defensive_stdout(&output), "");
    let stderr = defensive_stderr(&output);
    assert!(stderr.contains("No post-create hooks found"), "{stderr}");
    assert!(
        stderr.contains(".ez/hooks/post-create/<name>.md"),
        "{stderr}"
    );
    assert_eq!(
        defensive_git_output(&repo.path, &["branch", "--list", "feat/no-hooks"]),
        ""
    );
    assert!(defensive_stack_state(&repo.path)["branches"]["feat/no-hooks"].is_null());
}

#[test]
fn create_with_invalid_ref_name_restores_stashed_changes_before_branch_creation() {
    let repo = DefensiveRepo::new("create-invalid-ref-rollback");
    defensive_append_file(&repo.path, "tracked.txt", "tracked edit\n");
    std::fs::write(repo.path.join("untracked.txt"), "untracked\n").expect("write untracked");
    defensive_run(&repo.path, "git", &["add", "tracked.txt"]);

    let output = defensive_run_ez_raw(&repo.path, &["create", "bad name", "-m", "bad"]);

    defensive_assert_failure(&output);
    assert_eq!(
        defensive_git_output(&repo.path, &["branch", "--list", "bad name"]),
        ""
    );
    let status = defensive_status(&repo.path);
    assert!(status.contains("M tracked.txt"), "{status}");
    assert!(status.contains("?? untracked.txt"), "{status}");
    assert_eq!(
        defensive_git_output(&repo.path, &["stash", "list"]),
        "",
        "create rollback should not leave its transfer stash behind"
    );
}

#[test]
fn create_worktree_path_collision_rolls_back_branch_state_and_stash() {
    let repo = DefensiveRepo::new("create-worktree-collision");
    let colliding_path = defensive_expected_worktree(&repo.path, "feat/collide");
    std::fs::create_dir_all(colliding_path.parent().expect("worktree parent"))
        .expect("create worktree parent");
    std::fs::write(&colliding_path, "not a directory\n").expect("create colliding file");
    let before_state = defensive_stack_state(&repo.path);

    let output = defensive_run_ez_raw(&repo.path, &["create", "feat/collide"]);

    defensive_assert_failure(&output);
    assert_eq!(
        defensive_git_output(&repo.path, &["branch", "--list", "feat/collide"]),
        ""
    );
    assert_eq!(defensive_stack_state(&repo.path), before_state);
    assert!(colliding_path.is_file());
}

#[test]
fn create_tracked_only_commit_moves_untracked_file_without_committing_it() {
    let repo = DefensiveRepo::new("create-tracked-only");
    defensive_append_file(&repo.path, "tracked.txt", "tracked edit\n");
    std::fs::write(repo.path.join("new.txt"), "new\n").expect("write untracked");

    let output = defensive_run_ez(&repo.path, &["create", "feat/tracked", "-am", "tracked"]);

    let stderr = defensive_stderr(&output);
    assert!(stderr.contains("`-A`/`-Am`"), "{stderr}");
    let worktree = defensive_expected_worktree(&repo.path, "feat/tracked");
    assert!(worktree.join("new.txt").exists());
    assert!(defensive_status(&worktree).contains("?? new.txt"));
    let committed = defensive_git_output(&worktree, &["show", "--name-only", "--format=", "HEAD"]);
    assert!(committed.contains("tracked.txt"));
    assert!(!committed.contains("new.txt"));
}

#[test]
fn create_branch_only_receipt_reports_warn_scope_mode_by_default() {
    let repo = DefensiveRepo::new("create-scope-warn");

    let output = defensive_run_ez(
        &repo.path,
        &[
            "create",
            "feat/scoped",
            "--no-worktree",
            "--scope",
            " src/** ",
        ],
    );

    let receipt = defensive_receipt_with_cmd(&output, "create");
    assert_eq!(receipt["scope_defined"], true);
    assert_eq!(receipt["scope_mode"], "warn");
    assert_eq!(
        defensive_stack_state(&repo.path)["branches"]["feat/scoped"]["scope"],
        serde_json::json!(["src/**"])
    );
}

#[test]
fn create_from_current_managed_branch_creates_child_worktree_and_receipt() {
    let repo = DefensiveRepo::new("create-current-managed-worktree");
    defensive_run_ez(&repo.path, &["create", "feat/base", "--from", "main"]);
    let base = defensive_expected_worktree(&repo.path, "feat/base");
    let base_tip = defensive_commit_file(&base, "base.txt", "base\n", "base");

    let output = defensive_run_ez(&base, &["create", "feat/child"]);

    let receipt = defensive_receipt_with_cmd(&output, "create");
    assert_eq!(receipt["branch"], "feat/child");
    assert_eq!(receipt["parent"], "feat/base");
    assert_eq!(receipt["scope_defined"], false);
    let child = defensive_expected_worktree(&repo.path, "feat/child");
    assert_eq!(
        std::fs::canonicalize(defensive_stdout(&output)).expect("canonical stdout worktree"),
        std::fs::canonicalize(&child).expect("canonical expected worktree")
    );
    assert!(child.is_dir(), "child worktree should be created");
    assert_eq!(
        defensive_stack_state(&repo.path)["branches"]["feat/child"]["parent"],
        "feat/base"
    );
    assert_eq!(
        defensive_stack_state(&repo.path)["branches"]["feat/child"]["parent_head"],
        base_tip
    );
    assert_eq!(
        defensive_git_output(&base, &["branch", "--show-current"]),
        "feat/base",
        "create from a linked parent must not switch the parent worktree"
    );
}

#[test]
fn create_duplicate_from_current_managed_branch_leaves_existing_stack_unchanged() {
    let repo = DefensiveRepo::new("create-current-managed-duplicate");
    defensive_run_ez(&repo.path, &["create", "feat/base", "--from", "main"]);
    let base = defensive_expected_worktree(&repo.path, "feat/base");
    defensive_run_ez(&base, &["create", "feat/child", "--no-worktree"]);
    let before_state = defensive_stack_state(&repo.path);
    let before_child = defensive_ref_tip(&repo.path, "feat/child");

    let output = defensive_run_ez_raw(&base, &["create", "feat/child"]);

    defensive_assert_failure(&output);
    let stderr = defensive_stderr(&output);
    assert!(stderr.contains("Use `ez switch feat/child`"), "{stderr}");
    assert_eq!(defensive_stack_state(&repo.path), before_state);
    assert_eq!(defensive_ref_tip(&repo.path, "feat/child"), before_child);
    assert_eq!(
        defensive_branch_list(&repo.path, "feat/child")
            .lines()
            .count(),
        1
    );
}

#[test]
fn restack_warns_when_dirty_trunk_blocks_fast_forward_update() {
    let repo = DefensiveRepo::new("restack-dirty-trunk-update");
    defensive_create_committed_worktree(&repo, "feat/topic", "main");
    let writer = defensive_temp_dir("restack-dirty-trunk-writer");
    defensive_run(
        &writer,
        "git",
        &["clone", repo.remote.to_str().expect("remote path"), "."],
    );
    defensive_run(&writer, "git", &["config", "user.name", "Test User"]);
    defensive_run(
        &writer,
        "git",
        &["config", "user.email", "test@example.com"],
    );
    defensive_commit_file(&writer, "tracked.txt", "remote edit\n", "remote edit");
    defensive_run(&writer, "git", &["push", "origin", "main"]);
    std::fs::write(repo.path.join("tracked.txt"), "local dirty edit\n").expect("dirty trunk");
    let main_before = defensive_ref_tip(&repo.path, "main");

    let output = defensive_run_ez(&repo.path, &["restack"]);

    let stderr = defensive_stderr(&output);
    assert!(stderr.contains("Could not update `main`"), "{stderr}");
    assert_eq!(defensive_ref_tip(&repo.path, "main"), main_before);
    assert_eq!(
        std::fs::read_to_string(repo.path.join("tracked.txt")).expect("read dirty trunk"),
        "local dirty edit\n"
    );
    let _ = std::fs::remove_dir_all(writer);
}

#[test]
fn restack_aligns_branch_when_patch_is_already_applied_on_parent() {
    let repo = DefensiveRepo::new("restack-already-applied");
    defensive_run_ez(&repo.path, &["create", "feat/topic", "--from", "main"]);
    let topic = defensive_expected_worktree(&repo.path, "feat/topic");
    let topic_before = defensive_commit_file(&topic, "topic.txt", "topic\n", "topic");
    defensive_run(&repo.path, "git", &["checkout", "main"]);
    defensive_commit_file(&repo.path, "main-before.txt", "main\n", "main before");
    defensive_run(&repo.path, "git", &["cherry-pick", "feat/topic"]);
    let main_tip = defensive_ref_tip(&repo.path, "main");
    assert_ne!(
        topic_before, main_tip,
        "cherry-pick should create a new commit"
    );

    let output = defensive_run_ez(&repo.path, &["restack"]);

    let stderr = defensive_stderr(&output);
    assert!(stderr.contains("already applied"), "{stderr}");
    let receipt = defensive_receipt_with_action(&output, "restacked");
    assert_eq!(receipt["branch"], "feat/topic");
    assert_eq!(receipt["method"], "already_applied");
    assert_eq!(receipt["redundant_commits"], 1);
    assert_eq!(defensive_ref_tip(&repo.path, "feat/topic"), main_tip);
    assert_eq!(
        defensive_stack_state(&repo.path)["branches"]["feat/topic"]["parent_head"],
        main_tip
    );
    assert_eq!(
        defensive_status(&topic),
        "",
        "linked worktree should remain clean after alignment"
    );
}

#[test]
fn restack_refuses_dirty_stale_worktree_without_state_mutation() {
    let repo = DefensiveRepo::new("restack-dirty-stale-worktree");
    defensive_run_ez(&repo.path, &["create", "feat/topic", "--from", "main"]);
    let topic = defensive_expected_worktree(&repo.path, "feat/topic");
    let topic_before = defensive_commit_file(&topic, "topic.txt", "topic\n", "topic");
    defensive_run(&repo.path, "git", &["checkout", "main"]);
    defensive_commit_file(&repo.path, "main-before.txt", "main\n", "main before");
    defensive_run(&repo.path, "git", &["cherry-pick", "feat/topic"]);
    let main_tip = defensive_commit_file(
        &repo.path,
        "main-only.txt",
        "tracked on main\n",
        "main-only file",
    );
    assert_ne!(
        topic_before, main_tip,
        "cherry-pick should create a different main tip"
    );
    std::fs::write(topic.join("main-only.txt"), "untracked topic file\n")
        .expect("dirty topic worktree");
    assert!(
        defensive_status(&topic).contains("?? main-only.txt"),
        "topic worktree must start with an untracked collision"
    );
    assert!(
        defensive_git_output(&repo.path, &["worktree", "list", "--porcelain"])
            .contains("branch refs/heads/feat/topic"),
        "topic branch must be checked out in its linked worktree"
    );
    assert_eq!(
        defensive_git_output(&repo.path, &["branch", "--show-current"]),
        "main",
        "restack must start from the main worktree"
    );
    let state_before = defensive_stack_state(&repo.path);

    let output = defensive_run_ez_raw(&repo.path, &["restack"]);

    defensive_assert_failure(&output);
    assert_eq!(output.status.code(), Some(3));
    let stderr = defensive_stderr(&output);
    assert!(
        stderr.contains("Could not align `feat/topic` to `main`"),
        "{stderr}"
    );
    assert!(stderr.contains("would be overwritten by merge"), "{stderr}");
    let incomplete = defensive_receipt_with_action(&output, "restack_incomplete");
    assert_eq!(incomplete["failed"][0]["branch"], "feat/topic");
    assert_eq!(incomplete["failed"][0]["reason"], "error");
    assert_eq!(
        defensive_ref_tip(&repo.path, "feat/topic"),
        topic_before,
        "dirty linked worktree should keep the branch at its original tip"
    );
    assert_eq!(
        defensive_stack_state(&repo.path),
        state_before,
        "failed alignment should not advance parent_head"
    );
    assert_eq!(
        std::fs::read_to_string(topic.join("main-only.txt")).expect("read untracked topic file"),
        "untracked topic file\n"
    );
    assert_eq!(
        defensive_git_output(&repo.path, &["branch", "--show-current"]),
        "main",
        "restack should return to the original branch after reporting the failure"
    );
}

#[test]
fn restack_reparents_child_when_recorded_parent_branch_was_deleted() {
    let repo = DefensiveRepo::new("restack-reparent-deleted-parent");
    let main_initial = defensive_ref_tip(&repo.path, "main");
    defensive_run_ez(
        &repo.path,
        &["create", "feat/base", "--from", "main", "--no-worktree"],
    );
    let base_tip =
        defensive_commit_file_on_branch(&repo.path, "feat/base", "base.txt", "base\n", "base");
    defensive_run_ez(
        &repo.path,
        &[
            "create",
            "feat/child",
            "--from",
            "feat/base",
            "--no-worktree",
        ],
    );
    defensive_commit_file_on_branch(&repo.path, "feat/child", "child.txt", "child\n", "child");
    defensive_run(&repo.path, "git", &["checkout", "main"]);
    defensive_run(&repo.path, "git", &["branch", "-D", "feat/base"]);
    let main_tip = defensive_commit_file(&repo.path, "main2.txt", "main2\n", "main2");
    assert_ne!(main_initial, main_tip);
    assert_eq!(
        defensive_stack_state(&repo.path)["branches"]["feat/child"]["parent_head"],
        base_tip
    );

    let output = defensive_run_ez(&repo.path, &["restack"]);

    let stderr = defensive_stderr(&output);
    assert!(stderr.contains("reparenting onto `main`"), "{stderr}");
    assert_eq!(
        defensive_stack_state(&repo.path)["branches"]["feat/child"]["parent"],
        "main"
    );
    assert_eq!(
        defensive_stack_state(&repo.path)["branches"]["feat/child"]["parent_head"],
        main_tip
    );
    assert_eq!(
        defensive_git_output(&repo.path, &["merge-base", "main", "feat/child"]),
        main_tip
    );
}

#[test]
fn restack_conflict_reports_incomplete_and_continues_to_independent_sibling() {
    let repo = DefensiveRepo::new("restack-conflict-continues");
    defensive_run_ez(&repo.path, &["create", "feat/bad", "--from", "main"]);
    let bad = defensive_expected_worktree(&repo.path, "feat/bad");
    defensive_commit_file(&bad, "tracked.txt", "branch edit\n", "bad");
    defensive_run_ez(&repo.path, &["create", "feat/good", "--from", "main"]);
    let good = defensive_expected_worktree(&repo.path, "feat/good");
    defensive_commit_file(&good, "good.txt", "good\n", "good");
    defensive_commit_file(&repo.path, "tracked.txt", "trunk edit\n", "trunk");
    let main_tip = defensive_ref_tip(&repo.path, "main");
    let bad_before = defensive_ref_tip(&repo.path, "feat/bad");

    let output = defensive_run_ez_raw(&repo.path, &["restack"]);

    defensive_assert_failure(&output);
    assert_eq!(output.status.code(), Some(3));
    let stderr = defensive_stderr(&output);
    assert!(
        stderr.contains("Rebase conflict while updating `feat/bad` onto `main`"),
        "{stderr}"
    );
    assert!(stderr.contains("Left `feat/bad` where it was"), "{stderr}");
    let incomplete = defensive_receipt_with_action(&output, "restack_incomplete");
    assert_eq!(incomplete["restacked"], 1);
    assert_eq!(incomplete["failed"][0]["branch"], "feat/bad");
    assert_eq!(incomplete["failed"][0]["reason"], "conflict");
    assert_eq!(defensive_ref_tip(&repo.path, "feat/bad"), bad_before);
    assert_eq!(
        defensive_stack_state(&repo.path)["branches"]["feat/bad"]["parent_head"],
        defensive_ref_tip(&repo.path, "main~1"),
        "failed branch should keep stale parent_head for retry"
    );
    assert_eq!(
        defensive_stack_state(&repo.path)["branches"]["feat/good"]["parent_head"],
        main_tip,
        "sibling should still restack successfully"
    );
    assert_eq!(
        defensive_git_output(&repo.path, &["merge-base", "main", "feat/good"]),
        main_tip
    );
    assert_eq!(
        defensive_status(&bad),
        "",
        "failed linked worktree should be clean after rebase abort"
    );
}

#[test]
fn restack_current_stack_reports_nothing_to_do() {
    let repo = DefensiveRepo::new("restack-current-defensive");
    defensive_create_committed_worktree(&repo, "feat/current", "main");

    let output = defensive_run_ez(&repo.path, &["restack"]);

    assert!(defensive_stderr(&output).contains("All branches are up to date"));
    assert_eq!(
        defensive_git_output(&repo.path, &["branch", "--show-current"]),
        "main"
    );
}

#[test]
fn restack_success_summary_updates_child_parent_head() {
    let repo = DefensiveRepo::new("restack-success-defensive");
    let base = defensive_create_committed_worktree(&repo, "feat/base", "main");
    defensive_create_committed_worktree(&repo, "feat/child", "feat/base");
    defensive_commit_file(&base, "base2.txt", "base2\n", "base2");

    let output = defensive_run_ez(&repo.path, &["restack"]);

    assert!(defensive_stderr(&output).contains("Restacked 1 branch(es)"));
    let base_tip = defensive_ref_tip(&repo.path, "feat/base");
    assert_eq!(
        defensive_stack_state(&repo.path)["branches"]["feat/child"]["parent_head"],
        Value::from(base_tip.clone())
    );
    assert_eq!(
        defensive_git_output(&repo.path, &["merge-base", "feat/base", "feat/child"]),
        base_tip
    );
}

#[test]
fn restack_unresolvable_orphan_parent_reports_incomplete_without_rewriting_branch() {
    let repo = DefensiveRepo::new("restack-orphan-parent");
    let main_tip = defensive_ref_tip(&repo.path, "main");
    defensive_run(&repo.path, "git", &["checkout", "--orphan", "feat/orphan"]);
    defensive_run(&repo.path, "git", &["rm", "-rf", "."]);
    defensive_commit_file(&repo.path, "orphan.txt", "orphan\n", "orphan");
    let orphan_tip = defensive_ref_tip(&repo.path, "feat/orphan");
    defensive_run(&repo.path, "git", &["checkout", "main"]);
    defensive_run(&repo.path, "git", &["branch", "gone-parent", "main"]);
    defensive_run(&repo.path, "git", &["checkout", "gone-parent"]);
    let gone_parent_tip = defensive_commit_file(&repo.path, "gone-parent.txt", "gone\n", "gone");
    defensive_run(&repo.path, "git", &["checkout", "main"]);
    defensive_run(&repo.path, "git", &["branch", "-D", "gone-parent"]);
    let mut state = defensive_stack_state(&repo.path);
    state["branches"]["feat/orphan"] = serde_json::json!({
        "name": "feat/orphan",
        "parent": "gone-parent",
        "parent_head": gone_parent_tip,
    });
    defensive_write_stack_state(&repo.path, &state);
    assert_eq!(defensive_ref_tip(&repo.path, "main"), main_tip);

    let output = defensive_run_ez_raw(&repo.path, &["restack"]);

    defensive_assert_failure(&output);
    assert_eq!(output.status.code(), Some(3));
    let failed = defensive_receipt_with_action(&output, "restack_failed");
    assert_eq!(failed["reason"], "unresolvable_parent");
    assert_eq!(defensive_ref_tip(&repo.path, "feat/orphan"), orphan_tip);
    let incomplete = defensive_receipt_with_action(&output, "restack_incomplete");
    assert_eq!(incomplete["failed"][0]["branch"], "feat/orphan");
}
