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
        "ez-audit-create-commit-{prefix}-{}-{}-{}",
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
        .env_remove("EZ_SHELL_INTEGRATION")
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

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&run(dir, "git", args).stdout)
        .trim()
        .to_string()
}

fn write_file(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).expect("write file");
}

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) {
    write_file(dir, file, contents);
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
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
    run_ez(&path, &["init", "--yes"]);

    TestRepo { path, remote }
}

fn current_branch(dir: &Path) -> String {
    git_output(dir, &["branch", "--show-current"])
}

fn rev(dir: &Path, rev: &str) -> String {
    git_output(dir, &["rev-parse", rev])
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}

// Issue #165: `ez create --no-worktree` on a dirty tree left HEAD on the parent,
// so the next `ez commit` landed on the parent.
#[test]
fn create_no_worktree_on_dirty_tree_checks_out_new_branch_with_changes() {
    let repo = init_repo("no-wt-dirty");
    let main_before = rev(&repo.path, "main");

    write_file(&repo.path, "tracked.txt", "layer one\n");
    write_file(&repo.path, "new.txt", "untracked\n");
    run_ez(&repo.path, &["create", "layer-1", "--no-worktree"]);

    assert_eq!(current_branch(&repo.path), "layer-1");
    let status = git_output(&repo.path, &["status", "--short"]);
    assert!(status.contains("M tracked.txt"), "status: {status}");
    assert!(status.contains("?? new.txt"), "status: {status}");

    run_ez(
        &repo.path,
        &["commit", "-m", "layer one", "--", "tracked.txt"],
    );
    assert_eq!(rev(&repo.path, "main"), main_before, "parent must not move");
    assert_ne!(rev(&repo.path, "layer-1"), main_before);

    // Stacking further layers off a dirty tree keeps building a chain, never
    // zero-commit siblings.
    run_ez(&repo.path, &["create", "layer-2", "--no-worktree"]);
    assert_eq!(current_branch(&repo.path), "layer-2");
    let commit = run_ez(&repo.path, &["commit", "-m", "layer two", "--", "new.txt"]);
    assert!(
        !stderr_text(&commit).contains("could not be restacked"),
        "{}",
        stderr_text(&commit)
    );
    assert_eq!(rev(&repo.path, "layer-2~1"), rev(&repo.path, "layer-1"));
    assert_eq!(rev(&repo.path, "layer-1~1"), main_before);
    assert_eq!(
        git_output(&repo.path, &["status", "--short"]),
        "",
        "all dirty changes should be committed across the two layers"
    );
}

#[test]
fn create_from_no_worktree_still_does_not_switch() {
    let repo = init_repo("from-no-wt");
    run_ez(&repo.path, &["create", "base", "--no-worktree"]);
    commit_file(&repo.path, "base.txt", "base\n", "base");

    run_ez(
        &repo.path,
        &["create", "side", "--from", "main", "--no-worktree"],
    );

    assert_eq!(current_branch(&repo.path), "base");
    assert_eq!(rev(&repo.path, "side"), rev(&repo.path, "main"));
}

#[test]
fn commit_with_paths_leaves_other_staged_files_staged() {
    let repo = init_repo("commit-paths-only");
    run_ez(&repo.path, &["create", "feat", "--no-worktree"]);

    write_file(&repo.path, "other.txt", "staged elsewhere\n");
    run(&repo.path, "git", &["add", "other.txt"]);
    write_file(&repo.path, "tracked.txt", "focused change\n");

    run_ez(
        &repo.path,
        &["commit", "-m", "focused", "--", "tracked.txt"],
    );

    let committed = git_output(&repo.path, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(committed, "tracked.txt");
    let status = git_output(&repo.path, &["status", "--short"]);
    assert_eq!(status, "A  other.txt");
}

#[test]
fn commit_with_unknown_path_is_a_usage_error() {
    let repo = init_repo("commit-bad-path");
    run_ez(&repo.path, &["create", "feat", "--no-worktree"]);
    let head = rev(&repo.path, "HEAD");

    let output = run_ez_raw(&repo.path, &["commit", "-m", "nope", "--", "missing.txt"]);

    assert_eq!(exit_code(&output), 5, "{}", stderr_text(&output));
    assert!(stderr_text(&output).contains("git status"));
    assert_eq!(rev(&repo.path, "HEAD"), head);
}

#[test]
fn amend_on_branch_without_own_commits_refuses_to_rewrite_parent() {
    let repo = init_repo("amend-empty-branch");
    run_ez(&repo.path, &["create", "base", "--no-worktree"]);
    commit_file(&repo.path, "base.txt", "base\n", "base");
    run_ez(&repo.path, &["create", "child", "--no-worktree"]);
    let base_tip = rev(&repo.path, "base");

    write_file(&repo.path, "child.txt", "child\n");
    run(&repo.path, "git", &["add", "child.txt"]);
    let output = run_ez_raw(&repo.path, &["amend"]);

    assert_eq!(exit_code(&output), 5, "{}", stderr_text(&output));
    assert!(stderr_text(&output).contains("ez commit"));
    assert_eq!(rev(&repo.path, "child"), base_tip);
    assert_eq!(rev(&repo.path, "base"), base_tip);
}

#[test]
fn create_with_invalid_branch_name_is_a_usage_error() {
    let repo = init_repo("create-bad-name");

    for args in [
        &["create", "bad..name", "--no-worktree"][..],
        &["create", "bad name"][..],
    ] {
        let output = run_ez_raw(&repo.path, args);
        assert_eq!(exit_code(&output), 5, "{}", stderr_text(&output));
        assert!(stderr_text(&output).contains("not a valid branch name"));
    }
    assert_eq!(current_branch(&repo.path), "main");
}

#[test]
fn parent_and_diff_fall_back_to_commit_graph_when_parent_was_deleted() {
    let repo = init_repo("deleted-parent");
    run_ez(&repo.path, &["create", "base", "--no-worktree"]);
    commit_file(&repo.path, "base.txt", "base\n", "base");
    run_ez(&repo.path, &["create", "child", "--no-worktree"]);
    commit_file(&repo.path, "child.txt", "child\n", "child");
    run(&repo.path, "git", &["branch", "-D", "base"]);

    let parent = run_ez(&repo.path, &["parent"]);
    assert_eq!(String::from_utf8_lossy(&parent.stdout).trim(), "main");
    assert!(stderr_text(&parent).contains("no longer exists"));

    let diff = run_ez(&repo.path, &["diff", "--name-only"]);
    let names = String::from_utf8_lossy(&diff.stdout);
    assert!(names.contains("child.txt"), "{names}");
    assert!(names.contains("base.txt"), "{names}");
}

#[test]
fn checkout_without_name_and_without_tty_is_a_usage_error() {
    let repo = init_repo("checkout-no-tty");

    let output = run_ez_raw(&repo.path, &["checkout"]);

    assert_eq!(exit_code(&output), 5, "{}", stderr_text(&output));
    assert!(stderr_text(&output).contains("ez checkout <branch>"));
}

#[test]
fn worktrees_dir_is_excluded_so_commit_all_files_never_stages_it() {
    let repo = init_repo("wt-excluded");
    run_ez(&repo.path, &["create", "in-worktree"]);
    run_ez(&repo.path, &["create", "here", "--no-worktree"]);

    assert_eq!(git_output(&repo.path, &["status", "--short"]), "");
    write_file(&repo.path, "f.txt", "x\n");
    run_ez(&repo.path, &["commit", "-Am", "add f"]);
    let files = git_output(&repo.path, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(files.trim(), "f.txt");
}
