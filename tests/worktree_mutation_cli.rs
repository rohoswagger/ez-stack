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
            "ez-worktree-mutation-cli-{}-{}-{}",
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
        std::fs::write(path.join("tracked.txt"), "initial\n").expect("write tracked file");
        run(&path, "git", &["add", "tracked.txt"]);
        run(&path, "git", &["commit", "-m", "initial"]);
        run(&path, "git", &["remote", "add", "origin", "."]);
        run_ez(&path, &["init", "--yes"]);
        Self { path }
    }

    fn create_worktree(&self, branch: &str, parent: &str) -> PathBuf {
        run_ez(
            &self.path,
            &["create", branch, "--from", parent, "--no-worktree"],
        );
        let report = stdout_json(&run_ez(
            &self.path,
            &["worktree", "ensure", branch, "--json"],
        ));
        PathBuf::from(
            report["entries"][0]["path"]
                .as_str()
                .expect("worktree path"),
        )
    }

    fn stack_state(&self) -> Value {
        let common_dir = stdout_text(&run(&self.path, "git", &["rev-parse", "--git-common-dir"]));
        let common_dir = PathBuf::from(common_dir);
        let common_dir = if common_dir.is_absolute() {
            common_dir
        } else {
            self.path.join(common_dir)
        };
        serde_json::from_slice(
            &std::fs::read(common_dir.join("ez/stack.json")).expect("read stack state"),
        )
        .expect("parse stack state")
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct LinearStack {
    repo: TempRepo,
    base_worktree: PathBuf,
    child_worktree: PathBuf,
}

impl LinearStack {
    fn new() -> Self {
        let repo = TempRepo::new();
        let base_worktree = repo.create_worktree("feat/base", "main");
        commit_file(&base_worktree, "base.txt", "base\n", "base");
        let child_worktree = repo.create_worktree("feat/child", "feat/base");
        commit_file(&child_worktree, "child.txt", "child\n", "child");
        Self {
            repo,
            base_worktree,
            child_worktree,
        }
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

fn run_ez_raw(dir: &Path, args: &[&str]) -> Output {
    run_raw(dir, env!("CARGO_BIN_EXE_ez"), args)
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout JSON")
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(dir.join(file), contents).expect("write commit file");
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
}

fn branch_tip(repo: &Path, branch: &str) -> String {
    stdout_text(&run(repo, "git", &["rev-parse", branch]))
}

fn current_branch(worktree: &Path) -> String {
    stdout_text(&run(worktree, "git", &["branch", "--show-current"]))
}

fn status_porcelain(worktree: &Path) -> String {
    stdout_text(&run(worktree, "git", &["status", "--porcelain"]))
}

fn assert_ancestor(repo: &Path, ancestor: &str, descendant: &str) {
    run(
        repo,
        "git",
        &["merge-base", "--is-ancestor", ancestor, descendant],
    );
}

fn assert_no_rebase_state(worktree: &Path) {
    assert!(
        !rebase_state_exists(worktree),
        "rebase state should be cleaned up"
    );
}

fn rebase_state_exists(worktree: &Path) -> bool {
    for state_dir in ["rebase-merge", "rebase-apply"] {
        let raw = stdout_text(&run(
            worktree,
            "git",
            &["rev-parse", "--git-path", state_dir],
        ));
        let path = PathBuf::from(raw);
        let path = if path.is_absolute() {
            path
        } else {
            worktree.join(path)
        };
        if path.exists() {
            return true;
        }
    }
    false
}

#[test]
fn restack_rebases_branches_inside_their_linked_worktrees() {
    let stack = LinearStack::new();
    let child_before = branch_tip(&stack.repo.path, "feat/child");
    commit_file(
        &stack.base_worktree,
        "base-advance.txt",
        "advance\n",
        "advance base",
    );

    run_ez(&stack.repo.path, &["restack"]);

    let child_after = branch_tip(&stack.repo.path, "feat/child");
    assert_ne!(child_after, child_before);
    assert_ancestor(&stack.repo.path, "feat/base", "feat/child");
    assert_eq!(current_branch(&stack.base_worktree), "feat/base");
    assert_eq!(current_branch(&stack.child_worktree), "feat/child");
    assert_eq!(status_porcelain(&stack.base_worktree), "");
    assert_eq!(status_porcelain(&stack.child_worktree), "");
    assert_eq!(
        stack.repo.stack_state()["branches"]["feat/child"]["parent_head"],
        branch_tip(&stack.repo.path, "feat/base")
    );
}

#[test]
fn restack_preserves_dirty_child_worktree_and_leaves_it_retryable() {
    let stack = LinearStack::new();
    run(
        &stack.repo.path,
        "git",
        &["config", "rebase.autoStash", "true"],
    );
    commit_file(
        &stack.base_worktree,
        "tracked.txt",
        "base changed\n",
        "advance base",
    );
    let child_before = branch_tip(&stack.repo.path, "feat/child");
    let parent_head_before =
        stack.repo.stack_state()["branches"]["feat/child"]["parent_head"].clone();
    std::fs::write(
        stack.child_worktree.join("tracked.txt"),
        "dirty child edit\n",
    )
    .expect("write dirty edit");

    let output = run_ez_raw(&stack.repo.path, &["restack"]);

    assert!(!output.status.success());
    assert_eq!(branch_tip(&stack.repo.path, "feat/child"), child_before);
    assert_eq!(
        std::fs::read_to_string(stack.child_worktree.join("tracked.txt")).expect("read dirty edit"),
        "dirty child edit\n"
    );
    assert_eq!(current_branch(&stack.child_worktree), "feat/child");
    assert_eq!(
        stack.repo.stack_state()["branches"]["feat/child"]["parent_head"],
        parent_head_before
    );
    assert_ne!(
        stack.repo.stack_state()["branches"]["feat/child"]["parent_head"],
        branch_tip(&stack.repo.path, "feat/base")
    );
    assert_no_rebase_state(&stack.child_worktree);
}

#[test]
fn restack_cleans_up_a_conflicting_worktree_and_continues_to_a_sibling() {
    let repo = TempRepo::new();
    let bad_worktree = repo.create_worktree("feat/bad", "main");
    commit_file(
        &bad_worktree,
        "tracked.txt",
        "bad branch\n",
        "conflicting branch",
    );
    let good_worktree = repo.create_worktree("feat/good", "main");
    commit_file(&good_worktree, "good.txt", "good\n", "good branch");
    let bad_before = branch_tip(&repo.path, "feat/bad");
    let good_before = branch_tip(&repo.path, "feat/good");
    commit_file(
        &repo.path,
        "tracked.txt",
        "main branch\n",
        "conflicting trunk",
    );

    let output = run_ez_raw(&repo.path, &["restack"]);

    assert!(!output.status.success());
    assert_eq!(branch_tip(&repo.path, "feat/bad"), bad_before);
    assert_ne!(branch_tip(&repo.path, "feat/good"), good_before);
    assert_ancestor(&repo.path, "main", "feat/good");
    assert_eq!(current_branch(&bad_worktree), "feat/bad");
    assert_eq!(current_branch(&good_worktree), "feat/good");
    assert_eq!(status_porcelain(&bad_worktree), "");
    assert_eq!(status_porcelain(&good_worktree), "");
    assert_no_rebase_state(&bad_worktree);
    let state = repo.stack_state();
    assert_ne!(
        state["branches"]["feat/bad"]["parent_head"],
        branch_tip(&repo.path, "main")
    );
    assert_eq!(
        state["branches"]["feat/good"]["parent_head"],
        branch_tip(&repo.path, "main")
    );
}

#[test]
fn restack_does_not_abort_a_rebase_started_in_another_worktree() {
    let repo = TempRepo::new();
    let base_worktree = repo.create_worktree("feat/base", "main");
    commit_file(
        &base_worktree,
        "tracked.txt",
        "base baseline\n",
        "base baseline",
    );
    let child_worktree = repo.create_worktree("feat/child", "feat/base");
    commit_file(
        &child_worktree,
        "tracked.txt",
        "child change\n",
        "child change",
    );
    commit_file(
        &base_worktree,
        "tracked.txt",
        "base changed\n",
        "base changed",
    );
    let external_rebase = run_raw(&child_worktree, "git", &["rebase", "feat/base"]);
    assert!(!external_rebase.status.success());
    assert!(rebase_state_exists(&child_worktree));
    let status_before = status_porcelain(&child_worktree);
    let child_tip_before = branch_tip(&repo.path, "feat/child");
    let state_before = repo.stack_state();

    let output = run_ez_raw(&repo.path, &["restack"]);

    assert!(!output.status.success());
    assert!(rebase_state_exists(&child_worktree));
    assert_eq!(status_porcelain(&child_worktree), status_before);
    assert_eq!(branch_tip(&repo.path, "feat/child"), child_tip_before);
    assert_eq!(repo.stack_state(), state_before);
}

#[test]
fn restack_aborts_rebase_state_created_by_a_generic_git_failure() {
    let stack = LinearStack::new();
    commit_file(
        &stack.base_worktree,
        "base-advance.txt",
        "advance\n",
        "advance base",
    );
    let child_tip_before = branch_tip(&stack.repo.path, "feat/child");
    let state_before = stack.repo.stack_state();
    run(
        &stack.repo.path,
        "git",
        &["config", "commit.gpgSign", "true"],
    );
    run(&stack.repo.path, "git", &["config", "gpg.program", "false"]);

    let output = run_ez_raw(&stack.repo.path, &["restack"]);

    assert!(!output.status.success());
    assert_eq!(branch_tip(&stack.repo.path, "feat/child"), child_tip_before);
    assert_eq!(stack.repo.stack_state(), state_before);
    assert_eq!(current_branch(&stack.child_worktree), "feat/child");
    assert_eq!(status_porcelain(&stack.child_worktree), "");
    assert_no_rebase_state(&stack.child_worktree);
}

#[test]
fn move_from_a_linked_worktree_restacks_descendant_worktrees() {
    let repo = TempRepo::new();
    let a_worktree = repo.create_worktree("feat/a", "main");
    commit_file(&a_worktree, "a.txt", "a\n", "a");
    let b_worktree = repo.create_worktree("feat/b", "feat/a");
    commit_file(&b_worktree, "b.txt", "b\n", "b");
    let c_worktree = repo.create_worktree("feat/c", "feat/b");
    commit_file(&c_worktree, "c.txt", "c\n", "c");
    let c_before = branch_tip(&repo.path, "feat/c");

    run_ez(&b_worktree, &["move", "--onto", "main"]);

    let state = repo.stack_state();
    assert_eq!(state["branches"]["feat/b"]["parent"], "main");
    assert_eq!(state["branches"]["feat/c"]["parent"], "feat/b");
    assert_eq!(
        state["branches"]["feat/c"]["parent_head"],
        branch_tip(&repo.path, "feat/b")
    );
    assert_ne!(branch_tip(&repo.path, "feat/c"), c_before);
    assert_ancestor(&repo.path, "feat/b", "feat/c");
    assert_eq!(current_branch(&b_worktree), "feat/b");
    assert_eq!(current_branch(&c_worktree), "feat/c");
    assert_eq!(status_porcelain(&b_worktree), "");
    assert_eq!(status_porcelain(&c_worktree), "");
}

#[test]
fn move_derives_the_replay_range_after_external_worktree_history_changes() {
    let repo = TempRepo::new();
    let a_worktree = repo.create_worktree("feat/a", "main");
    commit_file(&a_worktree, "a.txt", "a\n", "a");
    let b_worktree = repo.create_worktree("feat/b", "feat/a");
    commit_file(&b_worktree, "b.txt", "b\n", "b");
    commit_file(&a_worktree, "a-advance.txt", "parent only\n", "advance a");
    run(&b_worktree, "git", &["rebase", "feat/a"]);
    assert!(b_worktree.join("a-advance.txt").exists());

    run_ez(&b_worktree, &["move", "--onto", "main"]);

    assert!(!b_worktree.join("a.txt").exists());
    assert!(
        !b_worktree.join("a-advance.txt").exists(),
        "move must not replay a former parent's commits when parent_head metadata is stale"
    );
    assert!(b_worktree.join("b.txt").exists());
    assert_eq!(repo.stack_state()["branches"]["feat/b"]["parent"], "main");
}

#[test]
fn move_conflict_keeps_the_linked_worktree_attached_and_state_unchanged() {
    let repo = TempRepo::new();
    let a_worktree = repo.create_worktree("feat/a", "main");
    commit_file(&a_worktree, "tracked.txt", "a changed\n", "conflicting a");
    let target_worktree = repo.create_worktree("feat/target", "main");
    commit_file(
        &target_worktree,
        "tracked.txt",
        "target changed\n",
        "conflicting target",
    );
    let branch_before = branch_tip(&repo.path, "feat/a");
    let state_before = repo.stack_state();

    let output = run_ez_raw(&a_worktree, &["move", "--onto", "feat/target"]);

    assert!(!output.status.success());
    assert_eq!(branch_tip(&repo.path, "feat/a"), branch_before);
    assert_eq!(repo.stack_state(), state_before);
    assert_eq!(current_branch(&a_worktree), "feat/a");
    assert_eq!(status_porcelain(&a_worktree), "");
    assert_no_rebase_state(&a_worktree);
}

#[test]
fn move_rejects_a_dirty_linked_worktree_without_losing_edits_or_state() {
    let repo = TempRepo::new();
    run(&repo.path, "git", &["config", "rebase.autoStash", "true"]);
    let a_worktree = repo.create_worktree("feat/a", "main");
    commit_file(&a_worktree, "a.txt", "a\n", "a");
    let target_worktree = repo.create_worktree("feat/target", "main");
    commit_file(
        &target_worktree,
        "tracked.txt",
        "target changed\n",
        "target",
    );
    let branch_before = branch_tip(&repo.path, "feat/a");
    let state_before = repo.stack_state();
    std::fs::write(a_worktree.join("tracked.txt"), "dirty a edit\n").expect("write dirty edit");

    let output = run_ez_raw(&a_worktree, &["move", "--onto", "feat/target"]);

    assert!(!output.status.success());
    assert_eq!(branch_tip(&repo.path, "feat/a"), branch_before);
    assert_eq!(repo.stack_state(), state_before);
    assert_eq!(current_branch(&a_worktree), "feat/a");
    assert_eq!(
        std::fs::read_to_string(a_worktree.join("tracked.txt")).expect("read dirty edit"),
        "dirty a edit\n"
    );
    assert_no_rebase_state(&a_worktree);
}
