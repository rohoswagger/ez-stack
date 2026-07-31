use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_REPO_ID: AtomicU64 = AtomicU64::new(1);

struct CleanupRepo {
    path: PathBuf,
    remote: PathBuf,
    fake_bin: PathBuf,
    gh_log: PathBuf,
}

impl Drop for CleanupRepo {
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

fn install_fake_gh(prefix: &str) -> (PathBuf, PathBuf) {
    let fake_bin = temp_dir(prefix);
    let gh_log = fake_bin.join("gh.log");
    let script = fake_bin.join("gh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
echo "$@" >> "$GH_LOG"
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '%s\n' "$GH_GRAPHQL_RESPONSE"
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ]; then
  exit 0
fi
printf 'unexpected gh invocation: %s\n' "$*" >&2
exit 1
"#,
    )
    .expect("write fake gh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("make fake gh executable");
    }
    (fake_bin, gh_log)
}

fn init_repo(prefix: &str, trunk: &str, branches: Value) -> CleanupRepo {
    let path = temp_dir(prefix);
    let remote = temp_dir(&format!("{prefix}-remote")).join("origin.git");
    std::fs::create_dir_all(&remote).expect("create remote parent");
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

    if trunk != "main" {
        run(&path, "git", &["checkout", "-b", trunk]);
        write_file(&path, "trunk.txt", "trunk\n");
        run(&path, "git", &["add", "trunk.txt"]);
        run(&path, "git", &["commit", "-m", "trunk"]);
        run(&path, "git", &["push", "-u", "origin", trunk]);
    }

    let ez_dir = path.join(".git/ez");
    std::fs::create_dir_all(&ez_dir).expect("create ez metadata");
    std::fs::write(
        ez_dir.join("stack.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "trunk": trunk,
            "remote": "origin",
            "branches": branches,
        }))
        .expect("serialize state"),
    )
    .expect("write state");

    let (fake_bin, gh_log) = install_fake_gh(&format!("{prefix}-bin"));
    CleanupRepo {
        path,
        remote,
        fake_bin,
        gh_log,
    }
}

fn run_ez(repo: &CleanupRepo, dir: &Path, args: &[&str], graphql_response: &str) -> Output {
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
        .env("GH_GRAPHQL_RESPONSE", graphql_response)
        .output()
        .expect("run ez")
}

fn stack_state(repo: &CleanupRepo) -> Value {
    serde_json::from_slice(
        &std::fs::read(repo.path.join(".git/ez/stack.json")).expect("read stack state"),
    )
    .expect("stack JSON")
}

fn save_stack_state(repo: &CleanupRepo, state: &Value) {
    std::fs::write(
        repo.path.join(".git/ez/stack.json"),
        serde_json::to_vec_pretty(state).expect("serialize stack state"),
    )
    .expect("write stack state");
}

fn add_branch(repo: &CleanupRepo, name: &str, parent: &str, file: &str, pr_number: u64) {
    run(&repo.path, "git", &["checkout", parent]);
    let parent_head = git_output(&repo.path, &["rev-parse", "HEAD"]);
    run(&repo.path, "git", &["checkout", "-b", name]);
    commit_file(&repo.path, file, &format!("{name}\n"), name);
    run(&repo.path, "git", &["push", "-u", "origin", name]);
    run(&repo.path, "git", &["checkout", "main"]);

    let mut state = stack_state(repo);
    state["branches"][name] = serde_json::json!({
        "name": name,
        "parent": parent,
        "parent_head": parent_head,
        "pr_number": pr_number,
    });
    save_stack_state(repo, &state);
}

fn add_worktree(repo: &CleanupRepo, branch: &str) -> PathBuf {
    let path = repo.path.join(".worktrees").join(branch.replace('/', "-"));
    run(
        &repo.path,
        "git",
        &[
            "worktree",
            "add",
            path.to_str().expect("worktree path"),
            branch,
        ],
    );
    path
}

fn branch_exists(repo: &CleanupRepo, branch: &str) -> bool {
    run_raw(
        &repo.path,
        "git",
        &["show-ref", "--verify", &format!("refs/heads/{branch}")],
    )
    .status
    .success()
}

fn current_branch(dir: &Path) -> String {
    git_output(dir, &["branch", "--show-current"])
}

fn status_porcelain(dir: &Path) -> String {
    git_output(dir, &["status", "--porcelain"])
}

fn remote_main_change(repo: &CleanupRepo, file: &str, contents: &str, message: &str) -> String {
    let clone = temp_dir("sync-cleanup-remote-writer");
    run(
        &clone,
        "git",
        &[
            "clone",
            "--branch",
            "main",
            repo.remote.to_str().expect("remote path"),
            ".",
        ],
    );
    run(&clone, "git", &["config", "user.name", "Remote User"]);
    run(
        &clone,
        "git",
        &["config", "user.email", "remote@example.com"],
    );
    commit_file(&clone, file, contents, message);
    let head = git_output(&clone, &["rev-parse", "HEAD"]);
    run(&clone, "git", &["push", "origin", "main"]);
    std::fs::remove_dir_all(clone).expect("remove remote writer");
    head
}

fn merged_base_graphql() -> &'static str {
    r#"{"data":{"repository":{"b0":{"nodes":[{"number":101,"url":"https://github.com/org/repo/pull/101","state":"MERGED","title":"base","baseRefName":"main","isDraft":false,"mergedAt":"2026-07-30T00:00:00Z"}]}}}}"#
}

fn merged_base_open_child_graphql() -> &'static str {
    r#"{"data":{"repository":{"b0":{"nodes":[{"number":101,"url":"https://github.com/org/repo/pull/101","state":"MERGED","title":"base","baseRefName":"main","isDraft":false,"mergedAt":"2026-07-30T00:00:00Z"}]},"b1":{"nodes":[{"number":102,"url":"https://github.com/org/repo/pull/102","state":"OPEN","title":"child","baseRefName":"feat/base","isDraft":false,"mergedAt":null}]}}}}"#
}

fn closed_base_open_child_graphql() -> &'static str {
    r#"{"data":{"repository":{"b0":{"nodes":[{"number":101,"url":"https://github.com/org/repo/pull/101","state":"CLOSED","title":"base","baseRefName":"main","isDraft":false,"mergedAt":null}]},"b1":{"nodes":[{"number":102,"url":"https://github.com/org/repo/pull/102","state":"OPEN","title":"child","baseRefName":"feat/base","isDraft":false,"mergedAt":null}]}}}}"#
}

fn open_base_graphql() -> &'static str {
    r#"{"data":{"repository":{"b0":{"nodes":[{"number":101,"url":"https://github.com/org/repo/pull/101","state":"OPEN","title":"base","baseRefName":"main","isDraft":false,"mergedAt":null}]}}}}"#
}

#[test]
fn sync_ignores_unmanaged_local_branch_even_when_github_reports_merged_pr() {
    let repo = init_repo(
        "sync-cleanup-unmanaged-main",
        "release/trunk",
        serde_json::json!({}),
    );

    let output = run_ez(
        &repo,
        &repo.path,
        &["sync"],
        r#"{"data":{"repository":{"b0":{"nodes":[{"number":99,"url":"https://github.com/org/repo/pull/99","state":"MERGED","title":"old main","baseRefName":"release/trunk","isDraft":false,"mergedAt":"2026-07-30T00:00:00Z"}]}}}}"#,
    );

    assert!(
        output.status.success(),
        "sync failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        branch_exists(&repo, "main"),
        "unmanaged main must survive sync"
    );
    let log = std::fs::read_to_string(&repo.gh_log).unwrap_or_default();
    assert!(
        !log.contains("graphql"),
        "sync must not query PRs for unmanaged-only local branches:\n{log}"
    );
}

#[test]
fn sync_preserves_dirty_linked_managed_worktree_without_force() {
    let repo = init_repo("sync-cleanup-dirty-preserve", "main", serde_json::json!({}));
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    let worktree = add_worktree(&repo, "feat/base");
    write_file(&worktree, "dirty.txt", "dirty\n");

    let output = run_ez(&repo, &repo.path, &["sync"], merged_base_graphql());

    assert!(
        output.status.success(),
        "sync should keep dirty cleanup retryable:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(branch_exists(&repo, "feat/base"), "branch must remain");
    assert!(worktree.exists(), "dirty worktree must remain");
    assert!(
        stack_state(&repo)["branches"].get("feat/base").is_some(),
        "stack state must keep branch tracked"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(r#""action":"cleanup_skipped""#), "{stderr}");
    assert!(
        stderr.contains(r#""reason":"worktree_remove_failed""#),
        "{stderr}"
    );
}

#[test]
fn sync_force_removes_dirty_linked_worktree_and_reparents_surviving_child() {
    let repo = init_repo("sync-cleanup-force-reparent", "main", serde_json::json!({}));
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    add_branch(&repo, "feat/child", "feat/base", "child.txt", 102);
    let worktree = add_worktree(&repo, "feat/base");
    write_file(&worktree, "dirty.txt", "dirty\n");

    let output = run_ez(
        &repo,
        &repo.path,
        &["sync", "--force"],
        merged_base_open_child_graphql(),
    );

    assert!(
        output.status.success(),
        "force sync failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!branch_exists(&repo, "feat/base"), "merged parent removed");
    assert!(
        branch_exists(&repo, "feat/child"),
        "surviving child remains"
    );
    assert!(!worktree.exists(), "dirty worktree removed by --force");
    let state = stack_state(&repo);
    assert!(state["branches"].get("feat/base").is_none());
    assert_eq!(state["branches"]["feat/child"]["parent"], "main");
    let log = std::fs::read_to_string(&repo.gh_log).expect("gh log");
    assert!(
        log.contains("pr edit 102 --base main"),
        "child PR base should be repaired after reparent:\n{log}"
    );
}

#[test]
fn sync_from_cleaned_linked_worktree_prints_main_worktree_navigation_target() {
    let repo = init_repo(
        "sync-cleanup-current-worktree",
        "main",
        serde_json::json!({}),
    );
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    let worktree = add_worktree(&repo, "feat/base");

    let output = run_ez(&repo, &worktree, &["sync"], merged_base_graphql());

    assert!(
        output.status.success(),
        "sync from linked worktree failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!branch_exists(&repo, "feat/base"), "branch removed");
    assert!(!worktree.exists(), "current linked worktree removed");
    let printed = std::fs::canonicalize(String::from_utf8_lossy(&output.stdout).trim())
        .expect("canonicalize printed navigation target");
    let expected = std::fs::canonicalize(&repo.path).expect("canonicalize repo path");
    assert_eq!(
        printed, expected,
        "sync should print shell navigation target"
    );
}

#[test]
fn sync_reparents_child_and_repairs_pr_base_when_managed_parent_branch_was_deleted_externally() {
    let repo = init_repo("sync-cleanup-deleted-parent", "main", serde_json::json!({}));
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    add_branch(&repo, "feat/child", "feat/base", "child.txt", 102);
    run(&repo.path, "git", &["branch", "-D", "feat/base"]);

    let output = run_ez(
        &repo,
        &repo.path,
        &["sync"],
        merged_base_open_child_graphql(),
    );

    assert!(
        output.status.success(),
        "sync failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let state = stack_state(&repo);
    assert!(state["branches"].get("feat/base").is_none());
    assert_eq!(state["branches"]["feat/child"]["parent"], "main");
    assert!(branch_exists(&repo, "feat/child"));
    let log = std::fs::read_to_string(&repo.gh_log).expect("gh log");
    assert!(
        log.contains("pr edit 102 --base main"),
        "child PR base should be repaired after external parent deletion:\n{log}"
    );
}

#[test]
fn sync_reparents_child_to_trunk_when_deleted_parent_record_points_to_missing_parent() {
    let repo = init_repo(
        "sync-cleanup-deleted-parent-missing-parent",
        "main",
        serde_json::json!({}),
    );
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    add_branch(&repo, "feat/child", "feat/base", "child.txt", 102);
    let mut state = stack_state(&repo);
    state["branches"]["feat/base"]["parent"] = Value::from("feat/missing-parent");
    save_stack_state(&repo, &state);
    run(&repo.path, "git", &["branch", "-D", "feat/base"]);

    let output = run_ez(
        &repo,
        &repo.path,
        &["sync"],
        merged_base_open_child_graphql(),
    );

    assert!(
        output.status.success(),
        "sync failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let state = stack_state(&repo);
    assert!(state["branches"].get("feat/base").is_none());
    assert_eq!(state["branches"]["feat/child"]["parent"], "main");
    let log = std::fs::read_to_string(&repo.gh_log).expect("gh log");
    assert!(
        log.contains("pr edit 102 --base main"),
        "child PR base should be repaired to trunk when deleted parent had no surviving parent:\n{log}"
    );
}

#[test]
fn sync_autostash_restores_tracked_and_untracked_changes_after_restack_conflict() {
    let repo = init_repo(
        "sync-cleanup-autostash-conflict",
        "main",
        serde_json::json!({}),
    );
    commit_file(&repo.path, "user.txt", "clean\n", "add user file");
    run(&repo.path, "git", &["push", "origin", "main"]);
    add_branch(&repo, "feat/base", "main", "tracked.txt", 101);
    run(&repo.path, "git", &["checkout", "main"]);
    let old_parent_head = stack_state(&repo)["branches"]["feat/base"]["parent_head"]
        .as_str()
        .expect("parent head")
        .to_string();
    let remote_main = remote_main_change(
        &repo,
        "tracked.txt",
        "upstream version\n",
        "conflicting upstream change",
    );
    write_file(&repo.path, "user.txt", "dirty tracked user change\n");
    write_file(&repo.path, "untracked-note.txt", "dirty untracked note\n");

    let output = run_ez(
        &repo,
        &repo.path,
        &["sync", "--autostash"],
        open_base_graphql(),
    );

    assert_eq!(
        output.status.code(),
        Some(3),
        "restack conflict should use documented exit code:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Stashed uncommitted changes"), "{stderr}");
    assert!(stderr.contains("Restored stashed changes"), "{stderr}");
    assert!(
        stderr.contains(r#""action":"restack_incomplete""#),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path.join("user.txt")).expect("read tracked dirty file"),
        "dirty tracked user change\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path.join("untracked-note.txt"))
            .expect("read untracked dirty file"),
        "dirty untracked note\n"
    );
    assert_eq!(current_branch(&repo.path), "main");
    assert_eq!(git_output(&repo.path, &["rev-parse", "main"]), remote_main);
    assert!(
        branch_exists(&repo, "feat/base"),
        "branch remains retryable"
    );
    assert_eq!(
        stack_state(&repo)["branches"]["feat/base"]["parent_head"]
            .as_str()
            .expect("parent head after failed sync"),
        old_parent_head,
        "failed restack must leave stale parent_head for retry"
    );
    let status = status_porcelain(&repo.path);
    assert!(
        status.lines().any(|line| line.ends_with(" user.txt")),
        "{status}"
    );
    assert!(status.contains("?? untracked-note.txt"), "{status}");
}

#[test]
fn sync_cleans_closed_unmerged_pr_with_pr_closed_reason_and_reparents_child() {
    let repo = init_repo("sync-cleanup-closed-pr", "main", serde_json::json!({}));
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    add_branch(&repo, "feat/child", "feat/base", "child.txt", 102);

    let output = run_ez(
        &repo,
        &repo.path,
        &["sync"],
        closed_base_open_child_graphql(),
    );

    assert!(
        output.status.success(),
        "sync failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(r#""action":"cleaned""#), "{stderr}");
    assert!(stderr.contains(r#""reason":"pr_closed""#), "{stderr}");
    assert!(!branch_exists(&repo, "feat/base"));
    assert_eq!(
        stack_state(&repo)["branches"]["feat/child"]["parent"],
        "main"
    );
    let log = std::fs::read_to_string(&repo.gh_log).expect("gh log");
    assert!(
        log.contains("pr edit 102 --base main"),
        "child PR base should be repaired after closed-PR cleanup:\n{log}"
    );
}

#[test]
fn sync_skips_cleanup_for_leased_worktree_and_preserves_retryable_state() {
    let repo = init_repo(
        "sync-cleanup-leased-worktree",
        "main",
        serde_json::json!({}),
    );
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    let worktree = add_worktree(&repo, "feat/base");
    run(
        &repo.path,
        env!("CARGO_BIN_EXE_ez"),
        &["worktree", "claim", "feat/base", "--owner", "agent-a"],
    );

    let output = run_ez(&repo, &repo.path, &["sync"], merged_base_graphql());

    assert!(
        output.status.success(),
        "sync should skip leased worktree cleanup without failing local sync:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(branch_exists(&repo, "feat/base"), "branch must remain");
    assert!(worktree.exists(), "leased worktree must remain");
    assert!(
        stack_state(&repo)["branches"].get("feat/base").is_some(),
        "stack state must keep branch tracked"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(r#""action":"cleanup_skipped""#), "{stderr}");
    assert!(stderr.contains(r#""reason":"worktree_locked""#), "{stderr}");
}
