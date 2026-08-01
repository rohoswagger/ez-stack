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
