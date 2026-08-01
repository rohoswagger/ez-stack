use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TempMergeRepo {
    path: PathBuf,
    remote: PathBuf,
    fake_bin: PathBuf,
    gh_log: PathBuf,
}

impl Drop for TempMergeRepo {
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

fn run_ez_with_fake_gh(repo: &TempMergeRepo, dir: &Path, args: &[&str]) -> Output {
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

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    stdout_text(&run(dir, "git", args))
}

fn write_file(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).expect("write file");
}

fn current_branch(dir: &Path) -> String {
    git_output(dir, &["branch", "--show-current"])
}

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) {
    write_file(dir, file, contents);
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
}

fn expected_worktree_path(repo: &Path, branch: &str) -> PathBuf {
    repo.join(".worktrees").join(branch.replace('/', "-"))
}

fn stack_path(repo: &Path) -> PathBuf {
    repo.join(".git").join("ez").join("stack.json")
}

fn stack_state(repo: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(stack_path(repo)).expect("read stack state"))
        .expect("parse stack state")
}

fn set_pr_number(repo: &Path, branch: &str, pr_number: u64) {
    let path = stack_path(repo);
    let mut state: Value = serde_json::from_slice(&std::fs::read(&path).expect("read stack state"))
        .expect("parse stack state");
    state["branches"][branch]["pr_number"] = Value::from(pr_number);
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&state).expect("serialize stack state"),
    )
    .expect("write stack state");
}

fn canonicalize_existing(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    path.canonicalize()
        .unwrap_or_else(|err| panic!("canonicalize {}: {err}", path.display()))
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_stdout_canonicalizes_to(output: &Output, expected: &Path) {
    let stdout = stdout_text(output);
    assert!(!stdout.is_empty(), "stdout should include cd target");
    assert_eq!(
        canonicalize_existing(stdout),
        canonicalize_existing(expected)
    );
}

fn assert_stderr_lacks_worktree_checkout_errors(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    for unexpected in ["already used by worktree", "already checked out", "fatal:"] {
        assert!(
            !stderr.contains(unexpected),
            "stderr should not contain `{unexpected}`:\n{stderr}"
        );
    }
}

fn local_branch_exists(repo: &Path, branch: &str) -> bool {
    !git_output(repo, &["branch", "--list", branch]).is_empty()
}

fn remote_branch_exists(repo: &Path, branch: &str) -> bool {
    !git_output(repo, &["ls-remote", "--heads", "origin", branch]).is_empty()
}

fn merge_async_put_lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .expect("read gh log")
        .lines()
        .filter(|line| line.contains("api -X PUT") && line.contains("/merge-async"))
        .map(str::to_string)
        .collect()
}

fn install_fake_gh(
    prefix: &str,
    pr_statuses: &[(&str, u64, &str)],
    native_stack_for_top: Option<(u64, &[u64])>,
) -> (PathBuf, PathBuf) {
    let fake_bin = temp_dir(prefix);
    let gh_log = fake_bin.join("gh.log");
    let script = fake_bin.join("gh");
    let pr_cases = pr_statuses
        .iter()
        .map(|(branch, pr, base)| {
            format!(
                "if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ] && [ \"$3\" = \"{branch}\" ]; then\n  printf '{{\"number\":{pr},\"url\":\"https://github.com/org/repo/pull/{pr}\",\"state\":\"OPEN\",\"title\":\"{branch}\",\"isDraft\":false,\"mergedAt\":null,\"baseRefName\":\"{base}\"}}\\n'\n  exit 0\nfi\n"
            )
        })
        .collect::<String>();
    let stack_case = native_stack_for_top
        .map(|(lookup_pr, prs)| {
            let pull_requests = prs
                .iter()
                .map(|pr| format!(r#"{{"number":{pr}}}"#))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "if [ \"$1\" = \"api\" ] && [ \"$2\" = \"repos/org/repo/stacks?pull_request={lookup_pr}\" ]; then\n  printf '[{{\"number\":88,\"pull_requests\":[{pull_requests}]}}]\\n'\n  exit 0\nfi\n"
            )
        })
        .unwrap_or_default();
    let script_contents = format!(
        r#"#!/bin/sh
echo "$@" >> "$GH_LOG"
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
{pr_cases}{stack_case}if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "PUT" ]; then
  cat >/dev/null
  printf '{{"status":"merged"}}\n'
  exit 0
fi
if [ "$1" = "api" ]; then
  printf '[]\n'
  exit 0
fi
printf 'unexpected gh invocation: %s\n' "$*" >&2
exit 1
"#
    );
    std::fs::write(&script, script_contents).expect("write fake gh");
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

fn init_repo_with_origin(prefix: &str, fake_bin: PathBuf, gh_log: PathBuf) -> TempMergeRepo {
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

    TempMergeRepo {
        path,
        remote,
        fake_bin,
        gh_log,
    }
}

fn add_managed_branch(repo: &TempMergeRepo, branch: &str, parent: &str, pr_number: u64) -> PathBuf {
    run_ez(
        &repo.path,
        &["create", branch, "--from", parent, "--no-worktree"],
    );
    run(&repo.path, "git", &["checkout", branch]);
    let filename = format!("{}.txt", branch.replace('/', "-"));
    commit_file(&repo.path, &filename, branch, branch);
    run(&repo.path, "git", &["push", "-u", "origin", branch]);
    run(&repo.path, "git", &["checkout", "main"]);
    set_pr_number(&repo.path, branch, pr_number);

    let worktree = expected_worktree_path(&repo.path, branch);
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

fn assert_branch_removed(repo: &TempMergeRepo, branch: &str) {
    assert!(
        !local_branch_exists(&repo.path, branch),
        "local branch {branch} should be removed"
    );
    assert!(
        !remote_branch_exists(&repo.path, branch),
        "remote branch {branch} should be removed"
    );
    assert!(
        stack_state(&repo.path)["branches"][branch].is_null(),
        "stack state should remove {branch}"
    );
}

#[test]
fn merge_yes_from_target_linked_worktree_removes_branch_without_primary_checkout_conflict() {
    let branch = "feat/target";
    let (fake_bin, gh_log) = install_fake_gh("merge-cli-gh-single", &[(branch, 42, "main")], None);
    let repo = init_repo_with_origin("merge-cli-single", fake_bin, gh_log);
    let worktree = add_managed_branch(&repo, branch, "main", 42);
    assert_eq!(current_branch(&repo.path), "main");
    assert_eq!(current_branch(&worktree), branch);

    let output = run_ez_with_fake_gh(&repo, &worktree, &["merge", "--yes"]);

    assert_success(&output);
    assert_stdout_canonicalizes_to(&output, &repo.path);
    assert_stderr_lacks_worktree_checkout_errors(&output);
    assert_eq!(current_branch(&repo.path), "main");
    assert!(
        !worktree.exists(),
        "target linked worktree should be removed"
    );
    assert_branch_removed(&repo, branch);
    let puts = merge_async_put_lines(&repo.gh_log);
    assert_eq!(
        puts,
        vec![
            "api -X PUT repos/org/repo/pulls/42/merge-async --input - -H X-GitHub-Api-Version: 2026-03-10"
        ]
    );
}

#[test]
fn merge_stack_yes_uses_native_top_pr_once_and_removes_all_target_worktrees() {
    let first = "feat/a";
    let second = "feat/b";
    let (fake_bin, gh_log) = install_fake_gh(
        "merge-cli-gh-native",
        &[(first, 101, "main"), (second, 102, first)],
        Some((102, &[101, 102])),
    );
    let repo = init_repo_with_origin("merge-cli-native", fake_bin, gh_log);
    let first_worktree = add_managed_branch(&repo, first, "main", 101);
    let second_worktree = add_managed_branch(&repo, second, first, 102);
    assert_eq!(current_branch(&repo.path), "main");
    assert_eq!(current_branch(&first_worktree), first);
    assert_eq!(current_branch(&second_worktree), second);

    let output = run_ez_with_fake_gh(&repo, &second_worktree, &["merge", "--stack", "--yes"]);

    assert_success(&output);
    assert_stdout_canonicalizes_to(&output, &repo.path);
    assert_stderr_lacks_worktree_checkout_errors(&output);
    assert_eq!(current_branch(&repo.path), "main");
    assert!(
        !first_worktree.exists(),
        "first linked worktree should be removed"
    );
    assert!(
        !second_worktree.exists(),
        "second linked worktree should be removed"
    );
    assert_branch_removed(&repo, first);
    assert_branch_removed(&repo, second);
    let puts = merge_async_put_lines(&repo.gh_log);
    assert_eq!(
        puts,
        vec![
            "api -X PUT repos/org/repo/pulls/102/merge-async --input - -H X-GitHub-Api-Version: 2026-03-10"
        ]
    );
    let log = std::fs::read_to_string(&repo.gh_log).expect("read gh log");
    assert!(
        !log.contains("pulls/101/merge-async"),
        "bottom PR should not be merged separately: {log}"
    );
    assert!(
        !log.contains("pulls/101/merge-async --input - -H X-GitHub-Api-Version: 2026-03-10"),
        "bottom PR should not be merged separately with API version header: {log}"
    );
}
