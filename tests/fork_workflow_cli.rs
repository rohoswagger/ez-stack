use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct ForkRepo {
    path: PathBuf,
    fork_remote: PathBuf,
    upstream_remote: PathBuf,
    fake_bin: PathBuf,
    gh_log: PathBuf,
}

struct TempPath(PathBuf);

impl Drop for ForkRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = std::fs::remove_dir_all(&self.fork_remote);
        let _ = std::fs::remove_dir_all(&self.upstream_remote);
        let _ = std::fs::remove_dir_all(&self.fake_bin);
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
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

fn run_ez_with_fake_gh(repo: &ForkRepo, dir: &Path, args: &[&str]) -> Output {
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
        .expect("run ez with fake gh")
}

fn write_file(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).expect("write file");
}

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) {
    write_file(dir, file, contents);
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
}

fn stack_path(repo: &Path) -> PathBuf {
    repo.join(".git").join("ez").join("stack.json")
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

fn install_fake_gh(prefix: &str, body: &str) -> (PathBuf, PathBuf) {
    let fake_bin = temp_dir(prefix);
    let gh_log = fake_bin.join("gh.log");
    let script = fake_bin.join("gh");
    let contents = format!(
        r#"#!/bin/sh
echo "$@" >> "$GH_LOG"
{body}
printf 'unexpected gh invocation: %s\n' "$*" >&2
exit 1
"#
    );
    std::fs::write(&script, contents).expect("write fake gh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod fake gh");
    }
    (fake_bin, gh_log)
}

fn init_repo(prefix: &str, fake_body: &str) -> ForkRepo {
    let path = temp_dir(prefix);
    let fork_remote = temp_dir(&format!("{prefix}-fork"));
    let upstream_remote = temp_dir(&format!("{prefix}-upstream"));
    let (fake_bin, gh_log) = install_fake_gh(&format!("{prefix}-bin"), fake_body);

    run(&fork_remote, "git", &["init", "--bare"]);
    run(&upstream_remote, "git", &["init", "--bare"]);
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
            "fork",
            fork_remote.to_str().expect("fork path"),
        ],
    );
    run(
        &path,
        "git",
        &[
            "remote",
            "add",
            "upstream",
            upstream_remote.to_str().expect("upstream path"),
        ],
    );
    run(&path, "git", &["push", "fork", "main"]);
    run(&path, "git", &["push", "upstream", "main"]);
    run_ez(&path, &["init", "--yes"]);

    ForkRepo {
        path,
        fork_remote,
        upstream_remote,
        fake_bin,
        gh_log,
    }
}

fn configure_fork_state(repo: &ForkRepo) {
    let mut state = stack_state(&repo.path);
    state["remote"] = Value::from("fork");
    state["upstream_remote"] = Value::from("upstream");
    state["repo"] = Value::from("upstream-owner/project");
    state["fork_repo"] = Value::from("fork-owner/project");
    write_stack_state(&repo.path, &state);
}

fn configure_same_repo_state(repo: &ForkRepo) {
    let mut state = stack_state(&repo.path);
    state["remote"] = Value::from("fork");
    state["repo"] = Value::from("owner/project");
    state["upstream_remote"] = Value::Null;
    state["fork_repo"] = Value::Null;
    write_stack_state(&repo.path, &state);
}

fn add_feature(repo: &ForkRepo) {
    run_ez(
        &repo.path,
        &["create", "feat/fork", "--from", "main", "--no-worktree"],
    );
    run(&repo.path, "git", &["checkout", "feat/fork"]);
    commit_file(&repo.path, "feature.txt", "feature\n", "feature");
}

fn add_two_branch_stack(repo: &ForkRepo) {
    run_ez(
        &repo.path,
        &["create", "feat/base", "--from", "main", "--no-worktree"],
    );
    run(&repo.path, "git", &["checkout", "feat/base"]);
    commit_file(&repo.path, "base.txt", "base\n", "base");
    run_ez(
        &repo.path,
        &["create", "feat/top", "--from", "feat/base", "--no-worktree"],
    );
    run(&repo.path, "git", &["checkout", "feat/top"]);
    commit_file(&repo.path, "top.txt", "top\n", "top");
}

fn advance_upstream(repo: &ForkRepo, prefix: &str) -> (TempPath, String) {
    let writer = TempPath(temp_dir(prefix));
    run(
        &writer.0,
        "git",
        &[
            "clone",
            "--branch",
            "main",
            repo.upstream_remote.to_str().expect("upstream path"),
            ".",
        ],
    );
    run(&writer.0, "git", &["config", "user.name", "Upstream User"]);
    run(
        &writer.0,
        "git",
        &["config", "user.email", "upstream@example.com"],
    );
    commit_file(
        &writer.0,
        "upstream.txt",
        &format!("upstream change from {prefix}\n"),
        "upstream change",
    );
    run(&writer.0, "git", &["push", "origin", "main"]);
    let tip = String::from_utf8_lossy(&run(&writer.0, "git", &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    (writer, tip)
}

#[test]
fn config_accepts_upstream_remote_and_fork_repo() {
    let repo = init_repo("fork-config", "");

    run_ez(
        &repo.path,
        &["config", "set", "upstream_remote", "upstream"],
    );
    run_ez(
        &repo.path,
        &["config", "set", "fork_repo", "fork-owner/project"],
    );

    let state = stack_state(&repo.path);
    assert_eq!(state["upstream_remote"], "upstream");
    assert_eq!(state["fork_repo"], "fork-owner/project");
}

#[test]
fn push_uses_fork_transport_and_explicit_upstream_pr_target() {
    let repo = init_repo(
        "fork-push",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  exit 1
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  printf 'https://github.com/upstream-owner/project/pull/41\n'
  exit 0
fi
"#,
    );
    configure_fork_state(&repo);
    add_feature(&repo);

    let output = run_ez_with_fake_gh(&repo, &repo.path, &["push"]);
    assert!(
        output.status.success(),
        "push failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(&repo.gh_log).expect("read gh log");
    assert!(
        log.lines()
            .any(|line| line.contains("pr view fork-owner:feat/fork")
                && line.contains("--repo upstream-owner/project")),
        "PR lookup must target upstream explicitly:\n{log}"
    );
    assert!(
        log.lines().any(|line| line.contains("pr create")
            && line.contains("--repo upstream-owner/project")
            && line.contains("--head fork-owner:feat/fork")),
        "PR creation must use upstream repo and fork-qualified head:\n{log}"
    );

    let fork_tip = run(
        &repo.fork_remote,
        "git",
        &["rev-parse", "refs/heads/feat/fork"],
    );
    assert!(fork_tip.status.success());
    let upstream_tip = run_raw(
        &repo.upstream_remote,
        "git",
        &["rev-parse", "refs/heads/feat/fork"],
    );
    assert!(!upstream_tip.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(r#""remote":"fork""#), "{stderr}");
    assert!(
        stderr.contains(r#""repo":"upstream-owner/project""#),
        "{stderr}"
    );
    assert!(
        stderr.contains(r#""fork_repo":"fork-owner/project""#),
        "{stderr}"
    );
}

#[test]
fn push_target_overrides_are_reversible_and_do_not_persist() {
    let repo = init_repo(
        "fork-push-overrides",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  exit 1
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  printf 'https://github.com/upstream-owner/project/pull/42\n'
  exit 0
fi
"#,
    );
    add_feature(&repo);
    let before = stack_state(&repo.path);

    let output = run_ez_with_fake_gh(
        &repo,
        &repo.path,
        &[
            "push",
            "--remote",
            "fork",
            "--repo",
            "upstream-owner/project",
            "--fork-repo",
            "fork-owner/project",
        ],
    );
    assert!(
        output.status.success(),
        "push failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let after = stack_state(&repo.path);
    assert_eq!(after["remote"], before["remote"]);
    assert_eq!(after.get("repo"), before.get("repo"));
    assert_eq!(after.get("fork_repo"), before.get("fork_repo"));
    assert_eq!(after.get("upstream_remote"), before.get("upstream_remote"));

    let log = std::fs::read_to_string(&repo.gh_log).expect("read gh log");
    assert!(
        log.contains("--repo upstream-owner/project"),
        "override repo missing:\n{log}"
    );
    assert!(
        log.contains("--head fork-owner:feat/fork"),
        "override fork head missing:\n{log}"
    );
}

#[test]
fn repeated_fork_push_uses_stored_pr_number_and_never_creates_a_duplicate() {
    let repo = init_repo(
        "fork-push-existing",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "41" ]; then
  printf '{"number":41,"url":"https://github.com/upstream-owner/project/pull/41","state":"OPEN","title":"feature","isDraft":false,"mergedAt":null,"baseRefName":"main"}\n'
  exit 0
fi
"#,
    );
    configure_fork_state(&repo);
    add_feature(&repo);
    let mut state = stack_state(&repo.path);
    state["branches"]["feat/fork"]["pr_number"] = Value::from(41_u64);
    write_stack_state(&repo.path, &state);

    let output = run_ez_with_fake_gh(&repo, &repo.path, &["push"]);
    assert!(
        output.status.success(),
        "push failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = std::fs::read_to_string(&repo.gh_log).expect("read gh log");
    assert!(
        log.lines()
            .any(|line| line.contains("pr view 41")
                && line.contains("--repo upstream-owner/project")),
        "existing PR lookup must use its stable number:\n{log}"
    );
    assert!(
        !log.contains("pr create"),
        "repeated push must not create a duplicate fork PR:\n{log}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(r#""created":false"#),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn submit_pushes_fork_stack_and_reports_native_stack_not_applicable() {
    let repo = init_repo(
        "fork-submit",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  exit 1
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  case "$*" in
    *"--head fork-owner:feat/base"*)
      printf 'https://github.com/upstream-owner/project/pull/51\n'
      ;;
    *"--head fork-owner:feat/top"*)
      printf 'https://github.com/upstream-owner/project/pull/52\n'
      ;;
    *)
      exit 1
      ;;
  esac
  exit 0
fi
"#,
    );
    configure_fork_state(&repo);
    add_two_branch_stack(&repo);

    let output = run_ez_with_fake_gh(&repo, &repo.path, &["submit"]);
    assert!(
        output.status.success(),
        "submit failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    run(
        &repo.fork_remote,
        "git",
        &["rev-parse", "refs/heads/feat/base"],
    );
    run(
        &repo.fork_remote,
        "git",
        &["rev-parse", "refs/heads/feat/top"],
    );
    let log = std::fs::read_to_string(&repo.gh_log).expect("read gh log");
    assert!(log.contains("--head fork-owner:feat/base"), "{log}");
    assert!(log.contains("--head fork-owner:feat/top"), "{log}");
    assert!(
        !log.contains("/stacks"),
        "fork submit must not call native stack API:\n{log}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(r#""native_stack_action":"not_applicable""#),
        "{stderr}"
    );
    assert!(stderr.contains("fork"), "{stderr}");
}

#[test]
fn submit_creates_a_github_native_stack_for_same_repo_prs() {
    let repo = init_repo(
        "native-submit-create",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  exit 1
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  case "$*" in
    *"--head feat/base"*) printf 'https://github.com/owner/project/pull/61\n' ;;
    *"--head feat/top"*) printf 'https://github.com/owner/project/pull/62\n' ;;
    *) exit 1 ;;
  esac
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/owner/project/stacks?pull_request=61" ]; then
  printf '[]\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "POST" ] && [ "$4" = "repos/owner/project/stacks" ]; then
  payload=$(cat)
  [ "$payload" = '{"pull_requests":[61,62]}' ] || exit 1
  printf '{"number":91}\n'
  exit 0
fi
"#,
    );
    configure_same_repo_state(&repo);
    add_two_branch_stack(&repo);

    let output = run_ez_with_fake_gh(&repo, &repo.path, &["submit"]);

    assert!(
        output.status.success(),
        "submit failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = std::fs::read_to_string(&repo.gh_log).expect("read gh log");
    assert!(
        log.contains("repos/owner/project/stacks?pull_request=61"),
        "{log}"
    );
    assert!(log.contains("-X POST repos/owner/project/stacks"), "{log}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(r#""native_stack_action":"created""#),
        "{stderr}"
    );
    assert!(stderr.contains(r#""native_stack_number":91"#), "{stderr}");
}

#[test]
fn submit_keeps_created_prs_when_native_stack_reconciliation_fails() {
    let repo = init_repo(
        "native-submit-error",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  exit 1
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  case "$*" in
    *"--head feat/base"*) printf 'https://github.com/owner/project/pull/71\n' ;;
    *"--head feat/top"*) printf 'https://github.com/owner/project/pull/72\n' ;;
    *) exit 1 ;;
  esac
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/owner/project/stacks?pull_request=71" ]; then
  printf 'simulated native stack outage\n' >&2
  exit 1
fi
"#,
    );
    configure_same_repo_state(&repo);
    add_two_branch_stack(&repo);

    let output = run_ez_with_fake_gh(&repo, &repo.path, &["submit"]);

    assert!(
        output.status.success(),
        "submit should preserve its successful PR work:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let state = stack_state(&repo.path);
    assert_eq!(state["branches"]["feat/base"]["pr_number"], 71);
    assert_eq!(state["branches"]["feat/top"]["pr_number"], 72);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("GitHub native stack update skipped"),
        "{stderr}"
    );
    assert!(stderr.contains("simulated native stack outage"), "{stderr}");
    assert!(
        stderr.contains(r#""native_stack_action":"error""#),
        "{stderr}"
    );
}

#[test]
fn sync_fetches_upstream_trunk_and_rebases_the_linked_fork_worktree() {
    let repo = init_repo(
        "fork-sync",
        r#"
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '{"data":{"repository":{"b0":{"nodes":[]}}}}\n'
  exit 0
fi
"#,
    );
    configure_fork_state(&repo);
    run_ez(&repo.path, &["create", "feat/sync", "--from", "main"]);
    let feature_worktree = repo.path.join(".worktrees").join("feat-sync");
    commit_file(
        &feature_worktree,
        "feature-sync.txt",
        "feature\n",
        "feature",
    );

    let (_writer, upstream_tip) = advance_upstream(&repo, "fork-sync-upstream-writer");

    let output = run_ez_with_fake_gh(&repo, &feature_worktree, &["sync"]);
    assert!(
        output.status.success(),
        "sync failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let local_main =
        String::from_utf8_lossy(&run(&repo.path, "git", &["rev-parse", "main"]).stdout)
            .trim()
            .to_string();
    let feature_base = String::from_utf8_lossy(
        &run(&repo.path, "git", &["merge-base", "main", "feat/sync"]).stdout,
    )
    .trim()
    .to_string();
    assert_eq!(local_main, upstream_tip);
    assert_eq!(feature_base, upstream_tip);
    assert_eq!(
        String::from_utf8_lossy(&run(&feature_worktree, "git", &["status", "--porcelain"]).stdout)
            .trim(),
        ""
    );

    let log = std::fs::read_to_string(&repo.gh_log).unwrap_or_default();
    assert!(
        !log.contains("pullRequests(headRefName"),
        "fork sync must not perform ambiguous branch-only PR lookup:\n{log}"
    );
    assert!(!log.contains("/stacks"), "{log}");
}

#[test]
fn restack_fetches_upstream_trunk_and_rebases_the_linked_fork_worktree() {
    let repo = init_repo("fork-restack", "");
    configure_fork_state(&repo);
    run_ez(&repo.path, &["create", "feat/restack", "--from", "main"]);
    let feature_worktree = repo.path.join(".worktrees").join("feat-restack");
    commit_file(
        &feature_worktree,
        "feature-restack.txt",
        "feature\n",
        "feature",
    );
    let (_writer, upstream_tip) = advance_upstream(&repo, "fork-restack-upstream-writer");

    let output = run_ez_with_fake_gh(&repo, &feature_worktree, &["restack"]);
    assert!(
        output.status.success(),
        "restack failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let feature_base = String::from_utf8_lossy(
        &run(
            &repo.path,
            "git",
            &["merge-base", "upstream/main", "feat/restack"],
        )
        .stdout,
    )
    .trim()
    .to_string();
    assert_eq!(feature_base, upstream_tip);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Fetching from `upstream`"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn switch_stale_warning_refreshes_trunk_from_upstream_not_fork() {
    let repo = init_repo("fork-switch-stale", "");
    configure_fork_state(&repo);
    add_feature(&repo);
    run(&repo.path, "git", &["checkout", "main"]);
    let (_writer, upstream_tip) = advance_upstream(&repo, "fork-switch-upstream-writer");

    let output = run_ez_with_fake_gh(
        &repo,
        &repo.path,
        &["switch", "feat/fork", "--no-cd-required"],
    );
    assert!(
        output.status.success(),
        "switch failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not restacked on `main`"),
        "stale switch warning must observe upstream trunk:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let local_main =
        String::from_utf8_lossy(&run(&repo.path, "git", &["rev-parse", "main"]).stdout)
            .trim()
            .to_string();
    assert_eq!(local_main, upstream_tip);
}

#[test]
fn sync_dry_run_reports_native_stack_not_applicable_for_fork_workflow() {
    let repo = init_repo("fork-sync-dry-run", "");
    configure_fork_state(&repo);
    add_two_branch_stack(&repo);
    let mut state = stack_state(&repo.path);
    state["branches"]["feat/base"]["pr_number"] = Value::from(51_u64);
    state["branches"]["feat/top"]["pr_number"] = Value::from(52_u64);
    write_stack_state(&repo.path, &state);

    let output = run_ez_with_fake_gh(&repo, &repo.path, &["sync", "--dry-run"]);
    assert!(
        output.status.success(),
        "dry-run failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Would reconcile GitHub native stack"),
        "{stderr}"
    );
    assert!(
        stderr.contains("not applicable") || stderr.contains("not_applicable"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(&repo.gh_log).unwrap_or_default(),
        ""
    );
}

#[test]
fn sync_uses_stored_pr_number_instead_of_another_forks_same_named_pr() {
    let repo = init_repo(
        "fork-sync-pr-identity",
        r#"
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  case "$*" in
    *"pullRequest(number:41)"*)
      printf '{"data":{"repository":{"b0":{"number":41,"url":"https://github.com/upstream-owner/project/pull/41","state":"OPEN","title":"our fork","baseRefName":"main","headRefName":"feat/fork","isDraft":false,"mergedAt":null}}}}\n'
      ;;
    *)
      printf '{"data":{"repository":{"b0":{"nodes":[{"number":99,"url":"https://github.com/upstream-owner/project/pull/99","state":"MERGED","title":"another fork","baseRefName":"main","isDraft":false,"mergedAt":"2026-07-30T00:00:00Z"}]}}}}\n'
      ;;
  esac
  exit 0
fi
"#,
    );
    configure_fork_state(&repo);
    add_feature(&repo);
    let mut state = stack_state(&repo.path);
    state["branches"]["feat/fork"]["pr_number"] = Value::from(41_u64);
    write_stack_state(&repo.path, &state);

    let output = run_ez_with_fake_gh(&repo, &repo.path, &["sync"]);
    assert!(
        output.status.success(),
        "sync failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        run_raw(
            &repo.path,
            "git",
            &["show-ref", "--verify", "refs/heads/feat/fork"]
        )
        .status
        .success(),
        "another fork's merged PR must not delete our same-named branch"
    );
    assert!(
        stack_state(&repo.path)["branches"]
            .get("feat/fork")
            .is_some(),
        "another fork's merged PR must not remove our stack metadata"
    );
    let log = std::fs::read_to_string(&repo.gh_log).expect("read gh log");
    assert!(
        log.contains("pullRequest(number:41)"),
        "fork sync must query the stored upstream PR number:\n{log}"
    );
    assert!(
        log.contains("owner=upstream-owner") && log.contains("name=project"),
        "stored PR lookup must target the upstream repository:\n{log}"
    );
    assert!(
        !log.contains("b0:pullRequests"),
        "fork sync must not use ambiguous branch-only bulk lookup:\n{log}"
    );
}

#[test]
fn native_stack_inspection_is_not_applicable_without_native_stack_api_calls() {
    let repo = init_repo("fork-native-inspect", "");
    configure_fork_state(&repo);
    add_feature(&repo);
    let mut state = stack_state(&repo.path);
    state["branches"]["feat/fork"]["pr_number"] = Value::from(41_u64);
    write_stack_state(&repo.path, &state);

    let output = run_ez_with_fake_gh(&repo, &repo.path, &["status", "--json", "--native-stack"]);
    assert!(
        output.status.success(),
        "status failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(status["native_stack"]["state"], "not_applicable");
    assert!(
        status["native_stack"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("fork") || reason.contains("cross")),
        "status JSON: {status}"
    );
    assert_eq!(
        status["native_stack"]["local"]["branches"],
        serde_json::json!(["feat/fork"])
    );
    assert_eq!(
        status["native_stack"]["local"]["pull_requests"],
        serde_json::json!([41])
    );
    let log = std::fs::read_to_string(&repo.gh_log).unwrap_or_default();
    assert!(
        !log.contains("/stacks"),
        "fork-native inspection must not call GitHub's same-repository Stack API:\n{log}"
    );
    assert!(
        log.lines()
            .all(|line| line.contains("pr view") && line.contains("--repo upstream-owner/project")),
        "non-stack status lookups must still target upstream explicitly:\n{log}"
    );
}
