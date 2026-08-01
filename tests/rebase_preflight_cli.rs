use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_REPO_ID: AtomicU64 = AtomicU64::new(1);

struct PreflightRepo {
    path: PathBuf,
    remote: PathBuf,
    fake_bin: PathBuf,
    gh_log: PathBuf,
}

impl Drop for PreflightRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = std::fs::remove_dir_all(&self.remote);
        let _ = std::fs::remove_dir_all(&self.fake_bin);
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ez-rebase-preflight-{prefix}-{}-{}-{}",
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

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) -> String {
    write_file(dir, file, contents);
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
    git_output(dir, &["rev-parse", "HEAD"])
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
        std::fs::set_permissions(&script, permissions).expect("chmod fake gh");
    }
    (fake_bin, gh_log)
}

fn init_repo(prefix: &str) -> PreflightRepo {
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

    let ez_dir = path.join(".git/ez");
    std::fs::create_dir_all(&ez_dir).expect("create ez metadata");
    std::fs::write(
        ez_dir.join("stack.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "trunk": "main",
            "remote": "origin",
            "branches": {},
        }))
        .expect("serialize state"),
    )
    .expect("write state");

    let (fake_bin, gh_log) = install_fake_gh(&format!("{prefix}-bin"));
    PreflightRepo {
        path,
        remote,
        fake_bin,
        gh_log,
    }
}

fn run_ez(repo: &PreflightRepo, dir: &Path, args: &[&str]) -> Output {
    run_ez_with_env(repo, dir, args, &[])
}

fn run_ez_with_env(
    repo: &PreflightRepo,
    dir: &Path,
    args: &[&str],
    extra_env: &[(&str, String)],
) -> Output {
    let inherited_path = std::env::var("PATH").unwrap_or_default();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ez"));
    cmd.args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(
            "PATH",
            format!("{}:{inherited_path}", repo.fake_bin.display()),
        )
        .env("GH_LOG", &repo.gh_log)
        .env("GH_GRAPHQL_RESPONSE", open_prs_graphql());
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.output().expect("run ez")
}

fn stack_state(repo: &PreflightRepo) -> Value {
    serde_json::from_slice(
        &std::fs::read(repo.path.join(".git/ez/stack.json")).expect("read stack state"),
    )
    .expect("stack JSON")
}

fn save_stack_state(repo: &PreflightRepo, state: &Value) {
    std::fs::write(
        repo.path.join(".git/ez/stack.json"),
        serde_json::to_vec_pretty(state).expect("serialize stack state"),
    )
    .expect("write stack state");
}

fn add_branch(
    repo: &PreflightRepo,
    name: &str,
    parent: &str,
    file: &str,
    pr_number: u64,
) -> String {
    run(&repo.path, "git", &["checkout", parent]);
    let parent_head = git_output(&repo.path, &["rev-parse", "HEAD"]);
    run(&repo.path, "git", &["checkout", "-b", name]);
    let tip = commit_file(&repo.path, file, &format!("{name}\n"), name);
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
    tip
}

fn add_worktree(repo: &PreflightRepo, branch: &str) -> PathBuf {
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

fn advance_main(repo: &PreflightRepo, file: &str) -> String {
    run(&repo.path, "git", &["checkout", "main"]);
    let tip = commit_file(&repo.path, file, "main advanced\n", "advance main");
    run(&repo.path, "git", &["push", "origin", "main"]);
    tip
}

fn add_merge_commit_on_branch(repo: &PreflightRepo, branch: &str) -> String {
    run(
        &repo.path,
        "git",
        &["checkout", "-b", "side/merge-source", "main"],
    );
    commit_file(&repo.path, "side.txt", "side\n", "side");
    run(&repo.path, "git", &["checkout", branch]);
    run(
        &repo.path,
        "git",
        &["merge", "--no-ff", "side/merge-source", "-m", "merge side"],
    );
    git_output(&repo.path, &["rev-parse", "HEAD"])
}

fn current_branch(dir: &Path) -> String {
    git_output(dir, &["branch", "--show-current"])
}

fn status_porcelain(dir: &Path) -> String {
    git_output(dir, &["status", "--porcelain"])
}

fn open_prs_graphql() -> &'static str {
    r#"{"data":{"repository":{"b0":{"nodes":[{"number":101,"url":"https://github.com/org/repo/pull/101","state":"OPEN","title":"base","baseRefName":"main","isDraft":false,"mergedAt":null}]},"b1":{"nodes":[{"number":102,"url":"https://github.com/org/repo/pull/102","state":"OPEN","title":"child","baseRefName":"feat/base","isDraft":false,"mergedAt":null}]},"b2":{"nodes":[{"number":103,"url":"https://github.com/org/repo/pull/103","state":"OPEN","title":"target","baseRefName":"main","isDraft":false,"mergedAt":null}]}}}}"#
}

fn merged_base_open_child_graphql() -> &'static str {
    r#"{"data":{"repository":{"b0":{"nodes":[{"number":101,"url":"https://github.com/org/repo/pull/101","state":"MERGED","title":"base","baseRefName":"main","isDraft":false,"mergedAt":"2026-07-30T00:00:00Z"}]},"b1":{"nodes":[{"number":102,"url":"https://github.com/org/repo/pull/102","state":"OPEN","title":"child","baseRefName":"feat/base","isDraft":false,"mergedAt":null}]}}}}"#
}

fn json_receipts(output: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .map(strip_ansi)
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .collect()
}

fn strip_ansi(line: &str) -> String {
    let mut stripped = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            stripped.push(ch);
        }
    }
    stripped
}

fn receipt_with_action(output: &Output, action: &str) -> Value {
    json_receipts(output)
        .into_iter()
        .find(|receipt| receipt["action"] == action)
        .unwrap_or_else(|| {
            panic!(
                "missing receipt action={action}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        })
}

fn assert_preflight_blocked(output: &Output, cmd: &str) {
    assert_eq!(
        output.status.code(),
        Some(5),
        "preflight block should be usage exit:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt = receipt_with_action(output, "rebase_preflight");
    assert_eq!(receipt["cmd"], cmd);
    assert_eq!(receipt["status"], "blocked");
    assert_eq!(receipt["forced"], false);
    assert!(
        receipt["merge_commits"].as_u64().unwrap_or_default() >= 1,
        "receipt should count merge commits: {receipt}"
    );
    assert!(
        receipt["hint"].as_str().expect("hint").contains("--force"),
        "receipt should include --force hint: {receipt}"
    );
}

fn assert_preflight_forced(output: &Output, cmd: &str) {
    let receipt = receipt_with_action(output, "rebase_preflight");
    assert_eq!(receipt["cmd"], cmd);
    assert_eq!(receipt["status"], "forced");
    assert_eq!(receipt["forced"], true);
    assert!(
        receipt["merge_commits"].as_u64().unwrap_or_default() >= 1,
        "receipt should count merge commits: {receipt}"
    );
}

#[test]
fn restack_blocks_merge_commit_before_rewriting_branch_or_state() {
    let repo = init_repo("restack-blocks-merge");
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    let merge_tip = add_merge_commit_on_branch(&repo, "feat/base");
    let state_before = stack_state(&repo);
    advance_main(&repo, "main-after-merge.txt");

    let output = run_ez(&repo, &repo.path, &["restack"]);

    assert_preflight_blocked(&output, "restack");
    assert_eq!(
        git_output(&repo.path, &["rev-parse", "feat/base"]),
        merge_tip
    );
    assert_eq!(stack_state(&repo), state_before);
}

#[test]
fn restack_force_allows_merge_commit_linearization() {
    let repo = init_repo("restack-force-merge");
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    add_merge_commit_on_branch(&repo, "feat/base");
    let new_parent = advance_main(&repo, "main-force.txt");

    let output = run_ez(&repo, &repo.path, &["restack", "--force"]);

    assert!(
        output.status.success(),
        "forced restack failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_preflight_forced(&output, "restack");
    assert_eq!(
        git_output(
            &repo.path,
            &["rev-list", "--merges", "--count", "main..feat/base"]
        ),
        "0",
        "forced restack should linearize merge commits"
    );
    assert_eq!(
        stack_state(&repo)["branches"]["feat/base"]["parent_head"],
        new_parent
    );
}

#[test]
fn restack_preflights_child_that_only_becomes_stale_after_parent_moves() {
    let repo = init_repo("restack-transitive-preflight");
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    add_branch(&repo, "feat/child", "feat/base", "child.txt", 102);
    let child_merge_tip = add_merge_commit_on_branch(&repo, "feat/child");
    let base_tip = git_output(&repo.path, &["rev-parse", "feat/base"]);
    let state_before = stack_state(&repo);
    advance_main(&repo, "main-before-transitive-preflight.txt");

    let output = run_ez(&repo, &repo.path, &["restack"]);

    assert_preflight_blocked(&output, "restack");
    assert_eq!(
        git_output(&repo.path, &["rev-parse", "feat/base"]),
        base_tip,
        "command-level preflight must block before rewriting the parent"
    );
    assert_eq!(
        git_output(&repo.path, &["rev-parse", "feat/child"]),
        child_merge_tip
    );
    assert_eq!(stack_state(&repo), state_before);
}

#[test]
fn move_preflights_descendants_before_rewriting_current_branch_or_pr_base() {
    let repo = init_repo("move-descendant-preflight");
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    add_branch(&repo, "feat/child", "feat/base", "child.txt", 102);
    add_branch(&repo, "feat/target", "main", "target.txt", 103);
    let child_merge_tip = add_merge_commit_on_branch(&repo, "feat/child");
    let base_tip = git_output(&repo.path, &["rev-parse", "feat/base"]);
    let state_before = stack_state(&repo);
    let base_worktree = add_worktree(&repo, "feat/base");

    let output = run_ez(&repo, &base_worktree, &["move", "--onto", "feat/target"]);

    assert_preflight_blocked(&output, "move");
    assert_eq!(
        git_output(&repo.path, &["rev-parse", "feat/base"]),
        base_tip
    );
    assert_eq!(
        git_output(&repo.path, &["rev-parse", "feat/child"]),
        child_merge_tip
    );
    assert_eq!(stack_state(&repo), state_before);
    assert_eq!(current_branch(&base_worktree), "feat/base");
    let gh_log = std::fs::read_to_string(&repo.gh_log).unwrap_or_default();
    assert!(
        !gh_log.contains("pr edit 101"),
        "blocked move must not update PR base:\n{gh_log}"
    );
}

#[test]
fn move_force_allows_descendant_merge_linearization() {
    let repo = init_repo("move-force-descendant");
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    add_branch(&repo, "feat/child", "feat/base", "child.txt", 102);
    add_branch(&repo, "feat/target", "main", "target.txt", 103);
    add_merge_commit_on_branch(&repo, "feat/child");
    let target_tip = git_output(&repo.path, &["rev-parse", "feat/target"]);
    let base_worktree = add_worktree(&repo, "feat/base");

    let output = run_ez(
        &repo,
        &base_worktree,
        &["move", "--onto", "feat/target", "--force"],
    );

    assert!(
        output.status.success(),
        "forced move failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_preflight_forced(&output, "move");
    assert_eq!(
        stack_state(&repo)["branches"]["feat/base"]["parent"],
        "feat/target"
    );
    assert_eq!(
        stack_state(&repo)["branches"]["feat/base"]["parent_head"],
        target_tip
    );
    assert_eq!(
        git_output(
            &repo.path,
            &["rev-list", "--merges", "--count", "feat/base..feat/child"]
        ),
        "0",
        "forced move should linearize descendant merge commits"
    );
}

#[test]
fn sync_blocks_merge_commit_before_rewriting_feature_branch_or_state() {
    let repo = init_repo("sync-blocks-merge");
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    let merge_tip = add_merge_commit_on_branch(&repo, "feat/base");
    let state_before = stack_state(&repo);
    let remote_main = {
        let clone = temp_dir("sync-blocks-merge-remote-writer");
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
        let head = commit_file(&clone, "remote.txt", "remote\n", "remote main");
        run(&clone, "git", &["push", "origin", "main"]);
        std::fs::remove_dir_all(clone).expect("remove remote writer");
        head
    };

    let output = run_ez(&repo, &repo.path, &["sync"]);

    assert_preflight_blocked(&output, "sync");
    assert_eq!(git_output(&repo.path, &["rev-parse", "main"]), remote_main);
    assert_eq!(
        git_output(&repo.path, &["rev-parse", "feat/base"]),
        merge_tip
    );
    assert_eq!(stack_state(&repo), state_before);
}

#[test]
fn sync_force_allows_merge_commit_linearization() {
    let repo = init_repo("sync-force-merge");
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    add_merge_commit_on_branch(&repo, "feat/base");
    let remote_main = {
        let clone = temp_dir("sync-force-merge-remote-writer");
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
        let head = commit_file(&clone, "remote-force.txt", "remote\n", "remote main");
        run(&clone, "git", &["push", "origin", "main"]);
        std::fs::remove_dir_all(clone).expect("remove remote writer");
        head
    };

    let output = run_ez(&repo, &repo.path, &["sync", "--force"]);

    assert!(
        output.status.success(),
        "forced sync failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_preflight_forced(&output, "sync");
    assert_eq!(
        stack_state(&repo)["branches"]["feat/base"]["parent_head"],
        remote_main
    );
    assert_eq!(
        git_output(
            &repo.path,
            &["rev-list", "--merges", "--count", "main..feat/base"]
        ),
        "0",
        "forced sync should linearize merge commits"
    );
}

#[test]
fn sync_persists_cleanup_then_blocks_surviving_child_merge_preflight() {
    let repo = init_repo("sync-cleanup-then-preflight");
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    add_branch(&repo, "feat/child", "feat/base", "child.txt", 102);
    let child_merge_tip = add_merge_commit_on_branch(&repo, "feat/child");
    let remote_main = {
        let clone = temp_dir("sync-cleanup-preflight-remote-writer");
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
        let head = commit_file(&clone, "cleanup-remote.txt", "remote\n", "remote main");
        run(&clone, "git", &["push", "origin", "main"]);
        std::fs::remove_dir_all(clone).expect("remove remote writer");
        head
    };

    let output = run_ez_with_env(
        &repo,
        &repo.path,
        &["sync"],
        &[(
            "GH_GRAPHQL_RESPONSE",
            merged_base_open_child_graphql().to_string(),
        )],
    );

    assert_preflight_blocked(&output, "sync");
    assert_eq!(git_output(&repo.path, &["rev-parse", "main"]), remote_main);
    assert_eq!(
        git_output(&repo.path, &["rev-parse", "feat/child"]),
        child_merge_tip,
        "surviving child must remain retryable after preflight block"
    );
    let state = stack_state(&repo);
    assert!(
        state["branches"].get("feat/base").is_none(),
        "merged parent cleanup should persist despite later preflight block"
    );
    assert_eq!(state["branches"]["feat/child"]["parent"], "main");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(r#""action":"cleaned""#), "{stderr}");
    let gh_log = std::fs::read_to_string(&repo.gh_log).expect("gh log");
    assert!(
        gh_log.contains("pr edit 102 --base main"),
        "cleanup should repair surviving child PR base before the rebase preflight blocks:\n{gh_log}"
    );
}

#[test]
fn stale_parent_head_reports_derived_base_and_restack_proceeds_without_force() {
    let repo = init_repo("stale-parent-head-derived");
    add_branch(&repo, "feat/base", "main", "base.txt", 101);
    let first_main = advance_main(&repo, "first-main.txt");
    let state_before_stale = stack_state(&repo);
    let output = run_ez(&repo, &repo.path, &["restack"]);
    assert!(
        output.status.success(),
        "initial restack failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let mut stale_state = stack_state(&repo);
    stale_state["branches"]["feat/base"]["parent_head"] =
        state_before_stale["branches"]["feat/base"]["parent_head"].clone();
    save_stack_state(&repo, &stale_state);
    let second_main = advance_main(&repo, "second-main.txt");

    let output = run_ez(&repo, &repo.path, &["restack"]);

    assert!(
        output.status.success(),
        "stale metadata should be derived, not blocked:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt = receipt_with_action(&output, "rebase_preflight");
    assert_eq!(receipt["cmd"], "restack");
    assert_eq!(receipt["status"], "ok");
    assert_eq!(receipt["forced"], false);
    assert!(
        receipt["derived_bases"].as_u64().unwrap_or_default() >= 1,
        "receipt should report derived bases: {receipt}"
    );
    assert_eq!(
        stack_state(&repo)["branches"]["feat/base"]["parent_head"],
        second_main
    );
    assert_ne!(first_main, second_main);
}

#[test]
fn all_redundant_branch_aligns_to_parent_without_invoking_git_rebase() {
    let repo = init_repo("redundant-no-rebase");
    run(&repo.path, "git", &["checkout", "-b", "feat/redundant"]);
    let effective_old_base = commit_file(
        &repo.path,
        "old-branch-only.txt",
        "old branch-only base\n",
        "old branch-only base",
    );
    commit_file(&repo.path, "already.txt", "same patch\n", "same patch");
    run(&repo.path, "git", &["checkout", "main"]);
    commit_file(
        &repo.path,
        "already.txt",
        "same patch\n",
        "same patch on main",
    );
    let main_tip = git_output(&repo.path, &["rev-parse", "main"]);
    run(&repo.path, "git", &["push", "origin", "main"]);

    let mut state = stack_state(&repo);
    state["branches"]["feat/redundant"] = serde_json::json!({
        "name": "feat/redundant",
        "parent": "main",
        "parent_head": effective_old_base,
        "pr_number": 101,
    });
    save_stack_state(&repo, &state);
    let worktree = add_worktree(&repo, "feat/redundant");
    let real_git =
        String::from_utf8_lossy(&run(&repo.path, "sh", &["-c", "command -v git"]).stdout)
            .trim()
            .to_string();
    let fake_git_bin = temp_dir("fake-git-no-rebase");
    let fake_git = fake_git_bin.join("git");
    std::fs::write(
        &fake_git,
        r#"#!/bin/sh
for arg in "$@"; do
  if [ "$arg" = "rebase" ]; then
    echo "git rebase must not be invoked for already-applied branches" >&2
    exit 97
  fi
done
exec "$EZ_TEST_REAL_GIT" "$@"
"#,
    )
    .expect("write fake git");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&fake_git)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_git, permissions).expect("chmod fake git");
    }

    let inherited_path = std::env::var("PATH").unwrap_or_default();
    let fake_path = format!(
        "{}:{}:{inherited_path}",
        fake_git_bin.display(),
        repo.fake_bin.display()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(["restack"])
        .current_dir(&worktree)
        .env("NO_COLOR", "1")
        .env("PATH", fake_path)
        .env("GH_LOG", &repo.gh_log)
        .env("GH_GRAPHQL_RESPONSE", open_prs_graphql())
        .env("EZ_TEST_REAL_GIT", real_git)
        .output()
        .expect("run ez");

    assert!(
        output.status.success(),
        "already-applied restack should not call git rebase:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt = receipt_with_action(&output, "restacked");
    assert_eq!(receipt["cmd"], "restack");
    assert_eq!(receipt["branch"], "feat/redundant");
    assert_eq!(receipt["method"], "already_applied");
    assert!(
        receipt["redundant_commits"].as_u64().unwrap_or_default() >= 1,
        "receipt should report redundant commits: {receipt}"
    );
    assert_eq!(
        git_output(&repo.path, &["rev-parse", "feat/redundant"]),
        main_tip
    );
    assert_eq!(
        stack_state(&repo)["branches"]["feat/redundant"]["parent_head"],
        main_tip
    );
    assert_eq!(current_branch(&worktree), "feat/redundant");
    assert_eq!(status_porcelain(&worktree), "");
}
