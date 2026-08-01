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
        "ez-adopt-{prefix}-{}-{}-{}",
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

fn run_ez_with_gh(repo: &TestRepo, args: &[&str]) -> Output {
    let inherited_path = std::env::var("PATH").unwrap_or_default();
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(&repo.path)
        .env("NO_COLOR", "1")
        .env(
            "PATH",
            format!("{}:{inherited_path}", repo.fake_bin.display()),
        )
        .env("GH_LOG", &repo.gh_log)
        .output()
        .expect("run ez with fake gh")
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

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(dir.join(file), contents).expect("write fixture");
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
}

fn install_fake_gh(prefix: &str, script_body: &str) -> (PathBuf, PathBuf) {
    let fake_bin = temp_dir(prefix);
    let gh_log = fake_bin.join("gh.log");
    let script = fake_bin.join("gh");
    let contents = format!(
        r#"#!/bin/sh
echo "$@" >> "$GH_LOG"
{script_body}
printf 'unexpected gh invocation: %s\n' "$*" >&2
exit 97
"#
    );
    std::fs::write(&script, contents).expect("write fake gh");
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

fn init_repo(prefix: &str, gh_script: &str) -> TestRepo {
    let (fake_bin, gh_log) = install_fake_gh(&format!("{prefix}-gh"), gh_script);
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
        &["remote", "add", "origin", remote.to_str().expect("remote")],
    );
    run(&path, "git", &["push", "-u", "origin", "main"]);
    run_ez(&path, &["init", "--yes"]);
    TestRepo {
        path,
        remote,
        fake_bin,
        gh_log,
    }
}

fn create_local_branch(repo: &TestRepo, branch: &str, parent: &str, file: &str) {
    run(&repo.path, "git", &["checkout", "-b", branch, parent]);
    commit_file(&repo.path, file, &format!("{branch}\n"), branch);
    run(&repo.path, "git", &["checkout", "main"]);
}

fn delete_local_branch(repo: &TestRepo, branch: &str) {
    run(&repo.path, "git", &["branch", "-D", branch]);
}

fn local_branch_exists(repo: &TestRepo, branch: &str) -> bool {
    run_raw(
        &repo.path,
        "git",
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .status
    .success()
}

fn stack_path(repo: &TestRepo) -> PathBuf {
    repo.path.join(".git/ez/stack.json")
}

fn stack_state(repo: &TestRepo) -> Value {
    serde_json::from_slice(&std::fs::read(stack_path(repo)).expect("read stack state"))
        .expect("parse stack state")
}

fn stack_bytes(repo: &TestRepo) -> Vec<u8> {
    std::fs::read(stack_path(repo)).expect("read stack state")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn gh_log(repo: &TestRepo) -> String {
    std::fs::read_to_string(&repo.gh_log).unwrap_or_default()
}

fn receipt(output: &Output) -> Value {
    stderr(output)
        .lines()
        .filter_map(|line| {
            let start = line.find('{')?;
            let end = line.rfind('}')?;
            serde_json::from_str(&line[start..=end]).ok()
        })
        .next_back()
        .expect("JSON receipt")
}

#[test]
fn adopt_explicit_local_branch_works_without_github_authentication() {
    let repo = init_repo(
        "explicit-offline",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 1
fi
"#,
    );
    create_local_branch(&repo, "feat/local", "main", "local.txt");

    let output = run_ez_with_gh(&repo, &["adopt", "feat/local", "--no-worktrees"]);

    assert_success(&output);
    let state = stack_state(&repo);
    assert_eq!(state["branches"]["feat/local"]["parent"], "main");
    assert!(state["branches"]["feat/local"]["pr_number"].is_null());
    assert!(stderr(&output).contains("Adopted `feat/local`"));
    assert_eq!(
        receipt(&output)["branches"],
        serde_json::json!(["feat/local"])
    );
    assert_eq!(receipt(&output)["worktrees_created"], 0);
    assert_eq!(
        gh_log(&repo).lines().collect::<Vec<_>>(),
        vec!["auth status"]
    );
}

#[test]
fn adopt_explicit_chain_rejects_trunk_and_duplicates_without_mutating_state() {
    let repo = init_repo(
        "explicit-invalid",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 1
fi
"#,
    );
    create_local_branch(&repo, "feat/local", "main", "local.txt");
    let before = stack_bytes(&repo);

    let trunk = run_ez_with_gh(&repo, &["adopt", "main", "--no-worktrees"]);
    assert_failure(&trunk);
    assert!(stderr(&trunk).contains("cannot adopt trunk branch `main`"));
    assert_eq!(stack_bytes(&repo), before);

    let duplicate = run_ez_with_gh(
        &repo,
        &["adopt", "feat/local", "feat/local", "--no-worktrees"],
    );
    assert_failure(&duplicate);
    assert!(stderr(&duplicate).contains("specified more than once"));
    assert_eq!(stack_bytes(&repo), before);
}

#[test]
fn adopt_already_tracked_explicit_branch_reports_a_skip() {
    let repo = init_repo(
        "already-tracked",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 1
fi
"#,
    );
    run_ez(
        &repo.path,
        &["create", "feat/tracked", "--from", "main", "--no-worktree"],
    );

    let output = run_ez_with_gh(&repo, &["adopt", "feat/tracked", "--no-worktrees"]);

    assert_success(&output);
    assert!(stderr(&output).contains("(already tracked)"));
    assert!(stderr(&output).contains("All 1 branch(es) were already tracked"));
    assert_eq!(receipt(&output)["adopted"], 0);
    assert_eq!(receipt(&output)["skipped"], 1);
}

#[test]
fn adopt_default_auto_initializes_and_returns_cleanly_when_no_prs_exist() {
    let repo = init_repo(
        "default-empty",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '{"data":{"repository":{"b0":{"nodes":[]}}}}\n'
  exit 0
fi
"#,
    );
    std::fs::remove_file(stack_path(&repo)).expect("remove stack state");

    let output = run_ez_with_gh(&repo, &["adopt"]);

    assert_success(&output);
    let text = stderr(&output);
    assert!(text.contains("Initialized ez with trunk branch `main`"));
    assert!(text.contains("No open PRs found for local branches"));
    assert_eq!(stack_state(&repo)["trunk"], "main");
    assert!(gh_log(&repo).contains("api graphql"));
}

#[test]
fn adopt_default_requires_github_authentication_before_querying_prs() {
    let repo = init_repo(
        "default-unauthenticated",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 1
fi
"#,
    );
    let before = stack_bytes(&repo);

    let output = run_ez_with_gh(&repo, &["adopt"]);

    assert_failure(&output);
    assert!(stderr(&output).contains("run `gh auth login` first"));
    assert_eq!(stack_bytes(&repo), before);
    assert_eq!(
        gh_log(&repo).lines().collect::<Vec<_>>(),
        vec!["auth status"]
    );
}

#[test]
fn adopt_authenticated_named_branch_without_pr_warns_and_uses_explicit_order() {
    let repo = init_repo(
        "named-no-pr",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '{"data":{"repository":{"b0":{"nodes":[]}}}}\n'
  exit 0
fi
"#,
    );
    create_local_branch(&repo, "feat/no-pr", "main", "no-pr.txt");

    let output = run_ez_with_gh(&repo, &["adopt", "feat/no-pr", "--no-worktrees"]);

    assert_success(&output);
    assert!(
        stderr(&output).contains(
            "Branch `feat/no-pr` has no open PR — treating arguments as an explicit stack"
        )
    );
    assert_eq!(
        stack_state(&repo)["branches"]["feat/no-pr"]["parent"],
        "main"
    );
}

#[test]
fn adopt_default_warns_when_local_pr_chain_is_orphaned() {
    let repo = init_repo(
        "default-orphan",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '{"data":{"repository":{"b0":{"nodes":[{"number":52,"url":"https://github.com/org/repo/pull/52","state":"OPEN","title":"Orphan PR","baseRefName":"feat/remote-parent","headRefName":"feat/orphan","isDraft":false,"mergedAt":null}]},"b1":{"nodes":[]}}}}\n'
  exit 0
fi
"#,
    );
    create_local_branch(&repo, "feat/orphan", "main", "orphan.txt");
    let before = stack_bytes(&repo);

    let output = run_ez_with_gh(&repo, &["adopt"]);

    assert_success(&output);
    let text = stderr(&output);
    assert!(text.contains("`feat/orphan` (#52) bases on `feat/remote-parent`"));
    assert!(text.contains("Run `ez adopt --pr 52`"));
    assert!(text.contains("No open PRs found for local branches that root on trunk"));
    assert_eq!(stack_bytes(&repo), before);
}

#[test]
fn adopt_named_branch_uses_open_pr_metadata_and_real_pull_ref() {
    let repo = init_repo(
        "named-pr",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '{"data":{"repository":{"b0":{"nodes":[{"number":42,"url":"https://github.com/org/repo/pull/42","state":"OPEN","title":"Named PR","baseRefName":"main","headRefName":"feat/named","isDraft":true,"mergedAt":null}]}}}}\n'
  exit 0
fi
"#,
    );
    create_local_branch(&repo, "feat/named", "main", "named.txt");
    run(
        &repo.path,
        "git",
        &["push", "origin", "feat/named:refs/pull/42/head"],
    );

    let output = run_ez_with_gh(&repo, &["adopt", "feat/named", "--no-worktrees"]);

    assert_success(&output);
    let state = stack_state(&repo);
    assert_eq!(state["branches"]["feat/named"]["parent"], "main");
    assert_eq!(state["branches"]["feat/named"]["pr_number"], 42);
    assert!(stderr(&output).contains("#42, base: `main`) [draft]"));
    assert_eq!(receipt(&output)["pr_numbers"], serde_json::json!([42]));
}

#[test]
fn adopt_by_pr_uses_github_native_stack_order_and_records_stack_identity() {
    let repo = init_repo(
        "native-pr",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=42" ]; then
  printf '[{"number":7,"base":{"ref":"main"},"open":true,"pull_requests":[{"number":41},{"number":42}]}]\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  case "$*" in
    *num=41*)
      printf '{"data":{"repository":{"pullRequest":{"number":41,"url":"https://github.com/org/repo/pull/41","state":"OPEN","title":"Native base","baseRefName":"main","headRefName":"feat/base","isDraft":false,"mergedAt":null}}}}\n'
      exit 0
      ;;
    *num=42*)
      printf '{"data":{"repository":{"pullRequest":{"number":42,"url":"https://github.com/org/repo/pull/42","state":"OPEN","title":"Native top","baseRefName":"feat/base","headRefName":"feat/top","isDraft":false,"mergedAt":null}}}}\n'
      exit 0
      ;;
  esac
fi
"#,
    );
    create_local_branch(&repo, "feat/base", "main", "base.txt");
    create_local_branch(&repo, "feat/top", "feat/base", "top.txt");
    run(
        &repo.path,
        "git",
        &[
            "push",
            "origin",
            "feat/base:refs/pull/41/head",
            "feat/top:refs/pull/42/head",
        ],
    );

    let output = run_ez_with_gh(&repo, &["adopt", "--pr", "42", "--no-worktrees"]);

    assert_success(&output);
    let state = stack_state(&repo);
    assert_eq!(state["branches"]["feat/base"]["parent"], "main");
    assert_eq!(state["branches"]["feat/base"]["pr_number"], 41);
    assert_eq!(state["branches"]["feat/top"]["parent"], "feat/base");
    assert_eq!(state["branches"]["feat/top"]["pr_number"], 42);
    let receipt = receipt(&output);
    assert_eq!(receipt["native_stack_number"], 7);
    assert_eq!(
        receipt["branches"],
        serde_json::json!(["feat/base", "feat/top"])
    );
    assert!(
        gh_log(&repo).contains(
            "api repos/org/repo/stacks?pull_request=42 -H X-GitHub-Api-Version: 2026-03-10"
        )
    );
}

#[test]
fn adopt_by_pr_falls_back_to_legacy_pr_chain_when_native_lookup_is_empty() {
    let repo = init_repo(
        "legacy-pr",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=62" ]; then
  printf '[]\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '{"data":{"repository":{"pullRequest":{"number":62,"url":"https://github.com/org/repo/pull/62","state":"OPEN","title":"Legacy PR","baseRefName":"main","headRefName":"feat/legacy","isDraft":false,"mergedAt":null}}}}\n'
  exit 0
fi
"#,
    );
    create_local_branch(&repo, "feat/legacy", "main", "legacy.txt");
    run(
        &repo.path,
        "git",
        &["push", "origin", "feat/legacy:refs/pull/62/head"],
    );

    let output = run_ez_with_gh(&repo, &["adopt", "--pr", "62", "--no-worktrees"]);

    assert_success(&output);
    let state = stack_state(&repo);
    assert_eq!(state["branches"]["feat/legacy"]["parent"], "main");
    assert_eq!(state["branches"]["feat/legacy"]["pr_number"], 62);
    assert!(receipt(&output)["native_stack_number"].is_null());
}

#[test]
fn adopt_named_remote_chain_expands_parent_fetches_pr_refs_and_creates_worktrees() {
    let repo = init_repo(
        "named-remote-chain",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  case "$*" in
    *feat/top*)
      printf '{"data":{"repository":{"b0":{"nodes":[{"number":42,"url":"https://github.com/org/repo/pull/42","state":"OPEN","title":"Top PR","baseRefName":"feat/base","headRefName":"feat/top","isDraft":false,"mergedAt":null}]}}}}\n'
      exit 0
      ;;
    *feat/base*)
      printf '{"data":{"repository":{"b0":{"nodes":[{"number":41,"url":"https://github.com/org/repo/pull/41","state":"OPEN","title":"Base PR","baseRefName":"main","headRefName":"feat/base","isDraft":true,"mergedAt":null}]}}}}\n'
      exit 0
      ;;
  esac
fi
"#,
    );
    create_local_branch(&repo, "feat/base", "main", "base.txt");
    create_local_branch(&repo, "feat/top", "feat/base", "top.txt");
    run(
        &repo.path,
        "git",
        &[
            "push",
            "origin",
            "feat/base:refs/pull/41/head",
            "feat/top:refs/pull/42/head",
        ],
    );
    delete_local_branch(&repo, "feat/top");
    delete_local_branch(&repo, "feat/base");

    let output = run_ez_with_gh(&repo, &["adopt", "feat/top"]);

    assert_success(&output);
    let state = stack_state(&repo);
    assert_eq!(state["branches"]["feat/base"]["parent"], "main");
    assert_eq!(state["branches"]["feat/base"]["pr_number"], 41);
    assert_eq!(state["branches"]["feat/top"]["parent"], "feat/base");
    assert_eq!(state["branches"]["feat/top"]["pr_number"], 42);
    assert!(local_branch_exists(&repo, "feat/base"));
    assert!(local_branch_exists(&repo, "feat/top"));

    let receipt = receipt(&output);
    assert_eq!(
        receipt["branches"],
        serde_json::json!(["feat/base", "feat/top"])
    );
    assert_eq!(receipt["worktrees_created"], 2);
    let paths = receipt["worktree_paths"]
        .as_array()
        .expect("worktree paths");
    assert_eq!(paths.len(), 2);
    for path in paths {
        assert!(
            Path::new(path.as_str().expect("path string")).exists(),
            "worktree path should exist: {path}"
        );
    }
    assert!(stderr(&output).contains("Worktrees are ready"));
    let log = gh_log(&repo);
    assert!(log.contains("feat/top"));
    assert!(log.contains("feat/base"));
}

#[test]
fn adopt_named_branch_rejects_explicit_pr_base_mismatch_without_mutation() {
    let repo = init_repo(
        "named-base-mismatch",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '{"data":{"repository":{"b0":{"nodes":[]},"b1":{"nodes":[{"number":42,"url":"https://github.com/org/repo/pull/42","state":"OPEN","title":"Child PR","baseRefName":"main","headRefName":"feat/child","isDraft":false,"mergedAt":null}]}}}}\n'
  exit 0
fi
"#,
    );
    create_local_branch(&repo, "feat/base", "main", "base.txt");
    create_local_branch(&repo, "feat/child", "feat/base", "child.txt");
    let before = stack_bytes(&repo);

    let output = run_ez_with_gh(
        &repo,
        &["adopt", "feat/base", "feat/child", "--no-worktrees"],
    );

    assert_failure(&output);
    let text = stderr(&output);
    assert!(text.contains("feat/child"));
    assert!(text.contains("reported base `main`"));
    assert!(text.contains("explicit parent is `feat/base`"));
    assert_eq!(stack_bytes(&repo), before);
}

#[test]
fn adopt_by_pr_reports_missing_legacy_and_native_stack_members_without_mutation() {
    let legacy = init_repo(
        "missing-legacy-pr",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=404" ]; then
  printf '[]\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '{"data":{"repository":{"pullRequest":null}}}\n'
  exit 0
fi
"#,
    );
    let legacy_before = stack_bytes(&legacy);

    let missing_legacy = run_ez_with_gh(&legacy, &["adopt", "--pr", "404", "--no-worktrees"]);

    assert_failure(&missing_legacy);
    assert!(stderr(&missing_legacy).contains("PR #404 not found"));
    assert_eq!(stack_bytes(&legacy), legacy_before);

    let native = init_repo(
        "missing-native-member",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=42" ]; then
  printf '[{"number":7,"base":{"ref":"main"},"open":true,"pull_requests":[{"number":41},{"number":42}]}]\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  case "$*" in
    *num=41*)
      printf '{"data":{"repository":{"pullRequest":null}}}\n'
      exit 0
      ;;
    *num=42*)
      printf '{"data":{"repository":{"pullRequest":{"number":42,"url":"https://github.com/org/repo/pull/42","state":"OPEN","title":"Top","baseRefName":"feat/base","headRefName":"feat/top","isDraft":false,"mergedAt":null}}}}\n'
      exit 0
      ;;
  esac
fi
"#,
    );
    let native_before = stack_bytes(&native);

    let missing_native = run_ez_with_gh(&native, &["adopt", "--pr", "42", "--no-worktrees"]);

    assert_failure(&missing_native);
    let text = stderr(&missing_native);
    assert!(text.contains("PR #41 from native stack #7 not found"));
    assert_eq!(stack_bytes(&native), native_before);
}

#[test]
fn adopt_rejects_existing_local_branch_that_does_not_contain_pr_head() {
    let repo = init_repo(
        "stale-local-pr-head",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '{"data":{"repository":{"b0":{"nodes":[{"number":71,"url":"https://github.com/org/repo/pull/71","state":"OPEN","title":"Stale PR","baseRefName":"main","headRefName":"feat/stale","isDraft":false,"mergedAt":null}]}}}}\n'
  exit 0
fi
"#,
    );
    create_local_branch(&repo, "feat/stale", "main", "stale-local.txt");
    create_local_branch(&repo, "feat/pr-head", "main", "stale-remote.txt");
    run(
        &repo.path,
        "git",
        &["push", "origin", "feat/pr-head:refs/pull/71/head"],
    );
    let before = stack_bytes(&repo);

    let output = run_ez_with_gh(&repo, &["adopt", "feat/stale", "--no-worktrees"]);

    assert_failure(&output);
    let text = stderr(&output);
    assert!(text.contains("local branch `feat/stale` does not contain PR #71 head"));
    assert_eq!(stack_bytes(&repo), before);
    assert!(stack_state(&repo)["branches"]["feat/stale"].is_null());
}

#[test]
fn adopt_rejects_local_metadata_conflicts_before_fetching_refs() {
    let repo = init_repo(
        "metadata-conflict",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '{"data":{"repository":{"b0":{"nodes":[{"number":71,"url":"https://github.com/org/repo/pull/71","state":"OPEN","title":"Conflict PR","baseRefName":"main","headRefName":"feat/conflict","isDraft":false,"mergedAt":null}]}}}}\n'
  exit 0
fi
"#,
    );
    run_ez(
        &repo.path,
        &["create", "feat/conflict", "--from", "main", "--no-worktree"],
    );
    let mut state = stack_state(&repo);
    state["branches"]["feat/conflict"]["pr_number"] = serde_json::json!(99);
    std::fs::write(
        stack_path(&repo),
        serde_json::to_vec_pretty(&state).expect("state json"),
    )
    .expect("rewrite state");
    let before = stack_bytes(&repo);

    let output = run_ez_with_gh(&repo, &["adopt", "feat/conflict", "--no-worktrees"]);

    assert_failure(&output);
    let text = stderr(&output);
    assert!(text.contains("conflicts with local ez metadata for `feat/conflict`"));
    assert!(text.contains("local PR=#99"));
    assert!(text.contains("remote PR=#71"));
    assert_eq!(stack_bytes(&repo), before);
    assert!(!gh_log(&repo).contains("refs/pull/71/head"));
}

#[test]
fn adopt_explicit_missing_branch_ref_fails_without_creating_metadata_or_branch() {
    let repo = init_repo(
        "missing-explicit-ref",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 1
fi
"#,
    );
    let before = stack_bytes(&repo);

    let output = run_ez_with_gh(&repo, &["adopt", "feat/missing", "--no-worktrees"]);

    assert_failure(&output);
    assert!(stderr(&output).contains("branch `feat/missing` was not found locally or on remote"));
    assert_eq!(stack_bytes(&repo), before);
    assert!(!local_branch_exists(&repo, "feat/missing"));
}

#[test]
fn adopt_existing_managed_branch_adds_missing_pr_number_and_reports_skip() {
    let repo = init_repo(
        "managed-add-pr-number",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '{"data":{"repository":{"b0":{"nodes":[{"number":80,"url":"https://github.com/org/repo/pull/80","state":"OPEN","title":"Tracked PR","baseRefName":"main","headRefName":"feat/tracked","isDraft":false,"mergedAt":null}]}}}}\n'
  exit 0
fi
"#,
    );
    run_ez(
        &repo.path,
        &["create", "feat/tracked", "--from", "main", "--no-worktree"],
    );
    run(
        &repo.path,
        "git",
        &["push", "origin", "feat/tracked:refs/pull/80/head"],
    );

    let output = run_ez_with_gh(&repo, &["adopt", "feat/tracked", "--no-worktrees"]);

    assert_success(&output);
    assert!(stderr(&output).contains("Updated PR number for `feat/tracked` → #80"));
    assert!(stderr(&output).contains("All 1 branch(es) were already tracked"));
    let state = stack_state(&repo);
    assert_eq!(state["branches"]["feat/tracked"]["pr_number"], 80);
    assert_eq!(receipt(&output)["adopted"], 0);
    assert_eq!(receipt(&output)["skipped"], 1);
}

#[test]
fn adopt_default_local_pr_success_uses_branch_list_scope() {
    let repo = init_repo(
        "default-local-success",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '{"data":{"repository":{"b0":{"nodes":[{"number":91,"url":"https://github.com/org/repo/pull/91","state":"OPEN","title":"Local PR","baseRefName":"main","headRefName":"feat/local","isDraft":false,"mergedAt":null}]},"b1":{"nodes":[{"number":91,"url":"https://github.com/org/repo/pull/91","state":"OPEN","title":"Local PR","baseRefName":"main","headRefName":"feat/local","isDraft":false,"mergedAt":null}]}}}}\n'
  exit 0
fi
"#,
    );
    create_local_branch(&repo, "feat/local", "main", "local.txt");
    run(
        &repo.path,
        "git",
        &["push", "origin", "feat/local:refs/pull/91/head"],
    );

    let output = run_ez_with_gh(&repo, &["adopt", "--no-worktrees"]);

    assert_success(&output);
    let state = stack_state(&repo);
    assert_eq!(state["branches"]["feat/local"]["parent"], "main");
    assert_eq!(state["branches"]["feat/local"]["pr_number"], 91);
    assert!(state["branches"]["main"].is_null());
    assert!(stderr(&output).contains("Run `ez log` to see the adopted stack"));
    assert_eq!(receipt(&output)["worktrees_created"], 0);
}

#[test]
fn adopt_explicit_remote_prless_branch_fetches_remote_ref_without_github_authentication() {
    let repo = init_repo(
        "remote-prless-explicit",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 1
fi
"#,
    );
    create_local_branch(&repo, "feat/remote", "main", "remote.txt");
    run(&repo.path, "git", &["push", "origin", "feat/remote"]);
    delete_local_branch(&repo, "feat/remote");

    let output = run_ez_with_gh(&repo, &["adopt", "feat/remote", "--no-worktrees"]);

    assert_success(&output);
    assert!(local_branch_exists(&repo, "feat/remote"));
    let state = stack_state(&repo);
    assert_eq!(state["branches"]["feat/remote"]["parent"], "main");
    assert!(state["branches"]["feat/remote"]["pr_number"].is_null());
    assert_eq!(
        receipt(&output)["branches"],
        serde_json::json!(["feat/remote"])
    );
}

#[test]
fn adopt_explicit_remote_chain_rolls_back_created_refs_when_worktree_provisioning_fails() {
    let repo = init_repo(
        "remote-chain-worktree-rollback",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 1
fi
"#,
    );
    create_local_branch(&repo, "feat/base", "main", "base.txt");
    create_local_branch(&repo, "feat/child", "feat/base", "child.txt");
    run(
        &repo.path,
        "git",
        &["push", "origin", "feat/base", "feat/child"],
    );
    delete_local_branch(&repo, "feat/child");
    delete_local_branch(&repo, "feat/base");
    let child_collision = repo.path.join(".worktrees").join("feat-child");
    std::fs::create_dir_all(&child_collision).expect("create child worktree collision");
    std::fs::write(child_collision.join("collision.txt"), "collision\n")
        .expect("write collision marker");
    let before = stack_bytes(&repo);

    let output = run_ez_with_gh(&repo, &["adopt", "feat/base", "feat/child"]);

    assert_failure(&output);
    let text = stderr(&output);
    assert!(
        text.contains("already exists") || text.contains("exists") || text.contains("not empty"),
        "unexpected stderr: {text}"
    );
    assert_eq!(stack_bytes(&repo), before);
    assert!(!local_branch_exists(&repo, "feat/base"));
    assert!(!local_branch_exists(&repo, "feat/child"));
    assert!(
        !repo.path.join(".worktrees").join("feat-base").exists(),
        "created base worktree should be rolled back"
    );
    assert!(
        child_collision.join("collision.txt").exists(),
        "pre-existing collision directory should be preserved"
    );
}

#[test]
fn adopt_named_pr_chain_skips_child_with_unrelated_history_without_rolling_back_parent() {
    let repo = init_repo(
        "named-unrelated-child-skip",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  case "$*" in
    *feat/child*)
      printf '{"data":{"repository":{"b0":{"nodes":[{"number":42,"url":"https://github.com/org/repo/pull/42","state":"OPEN","title":"Child PR","baseRefName":"feat/base","headRefName":"feat/child","isDraft":false,"mergedAt":null}]}}}}\n'
      exit 0
      ;;
    *feat/base*)
      printf '{"data":{"repository":{"b0":{"nodes":[{"number":41,"url":"https://github.com/org/repo/pull/41","state":"OPEN","title":"Base PR","baseRefName":"main","headRefName":"feat/base","isDraft":false,"mergedAt":null}]}}}}\n'
      exit 0
      ;;
  esac
fi
"#,
    );
    create_local_branch(&repo, "feat/base", "main", "base.txt");
    run(
        &repo.path,
        "git",
        &["push", "origin", "feat/base:refs/pull/41/head"],
    );
    delete_local_branch(&repo, "feat/base");

    let other = temp_dir("named-unrelated-child-other");
    run(&other, "git", &["init", "-b", "main"]);
    run(&other, "git", &["config", "user.name", "Test User"]);
    run(&other, "git", &["config", "user.email", "test@example.com"]);
    commit_file(&other, "other.txt", "other\n", "other root");
    run(
        &other,
        "git",
        &[
            "push",
            repo.remote.to_str().expect("remote"),
            "main:refs/pull/42/head",
        ],
    );

    let output = run_ez_with_gh(&repo, &["adopt", "feat/child", "--no-worktrees"]);

    assert_success(&output);
    let text = stderr(&output);
    assert!(text.contains("Could not resolve parent `feat/base` for `feat/child` — skipping"));
    assert_eq!(receipt(&output)["adopted"], 1);
    assert_eq!(receipt(&output)["skipped"], 1);
    let state = stack_state(&repo);
    assert_eq!(state["branches"]["feat/base"]["parent"], "main");
    assert!(state["branches"]["feat/child"].is_null());
    assert!(local_branch_exists(&repo, "feat/base"));
    assert!(!local_branch_exists(&repo, "feat/child"));

    let _ = std::fs::remove_dir_all(other);
}

#[test]
fn adopt_explicit_chain_enriches_matching_pr_child_when_parent_is_prless() {
    let repo = init_repo(
        "explicit-mixed-pr-child",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '{"data":{"repository":{"b0":{"nodes":[]},"b1":{"nodes":[{"number":42,"url":"https://github.com/org/repo/pull/42","state":"OPEN","title":"Child PR","baseRefName":"feat/base","headRefName":"feat/child","isDraft":true,"mergedAt":null}]}}}}\n'
  exit 0
fi
"#,
    );
    create_local_branch(&repo, "feat/base", "main", "base.txt");
    create_local_branch(&repo, "feat/child", "feat/base", "child.txt");
    run(
        &repo.path,
        "git",
        &["push", "origin", "feat/child:refs/pull/42/head"],
    );

    let output = run_ez_with_gh(
        &repo,
        &["adopt", "feat/base", "feat/child", "--no-worktrees"],
    );

    assert_success(&output);
    let state = stack_state(&repo);
    assert!(state["branches"]["feat/base"]["pr_number"].is_null());
    assert_eq!(state["branches"]["feat/child"]["pr_number"], 42);
    assert!(stderr(&output).contains("#42, base: `feat/base`) [draft]"));
    assert_eq!(
        receipt(&output)["pr_numbers"],
        serde_json::json!([null, 42])
    );
}

#[test]
fn adopt_by_pr_rejects_legacy_pr_that_does_not_root_on_trunk() {
    let repo = init_repo(
        "legacy-pr-not-rooted",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=72" ]; then
  printf '[]\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  case "$*" in
    *num=72*)
      printf '{"data":{"repository":{"pullRequest":{"number":72,"url":"https://github.com/org/repo/pull/72","state":"OPEN","title":"Orphan","baseRefName":"develop","headRefName":"feat/orphan","isDraft":false,"mergedAt":null}}}}\n'
      exit 0
      ;;
    *develop*)
      printf '{"data":{"repository":{"b0":{"nodes":[]}}}}\n'
      exit 0
      ;;
  esac
fi
"#,
    );
    let before = stack_bytes(&repo);

    let output = run_ez_with_gh(&repo, &["adopt", "--pr", "72", "--no-worktrees"]);

    assert_failure(&output);
    assert!(stderr(&output).contains("does not lead back to trunk `main`"));
    assert_eq!(stack_bytes(&repo), before);
}

#[test]
fn adopt_named_open_pr_not_rooted_on_trunk_fails_before_fetching_refs() {
    let repo = init_repo(
        "named-pr-not-rooted",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  case "$*" in
    *feat/orphan*)
      printf '{"data":{"repository":{"b0":{"nodes":[{"number":73,"url":"https://github.com/org/repo/pull/73","state":"OPEN","title":"Orphan","baseRefName":"develop","headRefName":"feat/orphan","isDraft":false,"mergedAt":null}]}}}}\n'
      exit 0
      ;;
    *develop*)
      printf '{"data":{"repository":{"b0":{"nodes":[]}}}}\n'
      exit 0
      ;;
  esac
fi
"#,
    );
    create_local_branch(&repo, "feat/orphan", "main", "orphan.txt");
    let before = stack_bytes(&repo);

    let output = run_ez_with_gh(&repo, &["adopt", "feat/orphan", "--no-worktrees"]);

    assert_failure(&output);
    assert!(
        stderr(&output).contains("None of the specified branches have open PRs rooted on `main`")
    );
    assert_eq!(stack_bytes(&repo), before);
}

#[test]
fn adopt_native_stack_skips_inactive_member_and_adopts_remaining_pr_on_trunk() {
    let repo = init_repo(
        "native-skips-inactive",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 0
fi
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=42" ]; then
  printf '[{"number":7,"base":{"ref":"main"},"open":true,"pull_requests":[{"number":41},{"number":42}]}]\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  case "$*" in
    *num=41*)
      printf '{"data":{"repository":{"pullRequest":{"number":41,"url":"https://github.com/org/repo/pull/41","state":"CLOSED","title":"Closed","baseRefName":"main","headRefName":"feat/closed","isDraft":false,"mergedAt":null}}}}\n'
      exit 0
      ;;
    *num=42*)
      printf '{"data":{"repository":{"pullRequest":{"number":42,"url":"https://github.com/org/repo/pull/42","state":"OPEN","title":"Top","baseRefName":"feat/closed","headRefName":"feat/top","isDraft":false,"mergedAt":null}}}}\n'
      exit 0
      ;;
  esac
fi
"#,
    );
    create_local_branch(&repo, "feat/top", "main", "top.txt");
    run(
        &repo.path,
        "git",
        &["push", "origin", "feat/top:refs/pull/42/head"],
    );

    let output = run_ez_with_gh(&repo, &["adopt", "--pr", "42", "--no-worktrees"]);

    assert_success(&output);
    let state = stack_state(&repo);
    assert!(state["branches"]["feat/closed"].is_null());
    assert_eq!(state["branches"]["feat/top"]["parent"], "main");
    assert_eq!(
        receipt(&output)["branches"],
        serde_json::json!(["feat/top"])
    );
}

#[test]
fn adopt_explicit_unrelated_existing_child_aborts_and_preserves_state() {
    let repo = init_repo(
        "explicit-unrelated-existing-child",
        r#"
if [ "$1" = "auth" ] && [ "$2" = "status" ]; then
  exit 1
fi
"#,
    );
    create_local_branch(&repo, "feat/base", "main", "base.txt");

    let other = temp_dir("explicit-unrelated-existing-child-other");
    run(&other, "git", &["init", "-b", "main"]);
    run(&other, "git", &["config", "user.name", "Test User"]);
    run(&other, "git", &["config", "user.email", "test@example.com"]);
    commit_file(&other, "other.txt", "other\n", "other root");
    run(
        &repo.path,
        "git",
        &[
            "fetch",
            other.to_str().expect("other"),
            "main:refs/heads/feat/child",
        ],
    );
    let before = stack_bytes(&repo);

    let output = run_ez_with_gh(
        &repo,
        &["adopt", "feat/base", "feat/child", "--no-worktrees"],
    );

    assert_failure(&output);
    assert!(stderr(&output).contains("could not resolve parent `feat/base` for `feat/child`"));
    assert_eq!(stack_bytes(&repo), before);
    assert!(local_branch_exists(&repo, "feat/base"));
    assert!(local_branch_exists(&repo, "feat/child"));

    let _ = std::fs::remove_dir_all(other);
}
