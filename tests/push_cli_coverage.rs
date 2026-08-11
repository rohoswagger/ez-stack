use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct PushRepo {
    path: PathBuf,
    remote: PathBuf,
    fake_bin: PathBuf,
    gh_log: PathBuf,
    git_log: PathBuf,
}

impl Drop for PushRepo {
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

fn run_ez_with_fakes(repo: &PushRepo, dir: &Path, args: &[&str]) -> Output {
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
        .env("GIT_LOG", &repo.git_log)
        .output()
        .expect("run ez with fakes")
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    stdout_text(&run(dir, "git", args))
}

fn write_file(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).expect("write file");
}

fn append_file(dir: &Path, name: &str, contents: &str) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join(name))
        .expect("open file");
    file.write_all(contents.as_bytes()).expect("append file");
}

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) {
    write_file(dir, file, contents);
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
}

fn current_branch(dir: &Path) -> String {
    git_output(dir, &["branch", "--show-current"])
}

fn ref_tip(dir: &Path, refspec: &str) -> String {
    git_output(dir, &["rev-parse", refspec])
}

fn remote_tip(repo: &PushRepo, branch: &str) -> String {
    git_output(
        &repo.path,
        &["ls-remote", "origin", &format!("refs/heads/{branch}")],
    )
    .split_whitespace()
    .next()
    .expect("remote sha")
    .to_string()
}

fn git_common_dir(repo: &Path) -> PathBuf {
    let raw = git_output(repo, &["rev-parse", "--git-common-dir"]);
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        repo.join(path)
    }
}

fn stack_path(repo: &Path) -> PathBuf {
    git_common_dir(repo).join("ez").join("stack.json")
}

fn stack_state(repo: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(stack_path(repo)).expect("read stack state"))
        .expect("parse stack state")
}

fn stack_state_bytes(repo: &Path) -> Vec<u8> {
    std::fs::read(stack_path(repo)).expect("read stack state")
}

fn write_stack_state(repo: &Path, state: &Value) {
    std::fs::write(
        stack_path(repo),
        serde_json::to_vec_pretty(state).expect("serialize stack state"),
    )
    .expect("write stack state");
}

fn set_pr_number(repo: &Path, branch: &str, pr_number: Option<u64>) {
    let mut state = stack_state(repo);
    match pr_number {
        Some(number) => state["branches"][branch]["pr_number"] = Value::from(number),
        None => {
            state["branches"][branch]
                .as_object_mut()
                .expect("branch object")
                .remove("pr_number");
        }
    }
    write_stack_state(repo, &state);
}

fn configure_stack(repo: &Path, key: &str, value: Value) {
    let mut state = stack_state(repo);
    state[key] = value;
    write_stack_state(repo, &state);
}

fn set_branch_parent(repo: &Path, branch: &str, parent: &str) {
    let mut state = stack_state(repo);
    state["branches"][branch]["parent"] = Value::from(parent);
    write_stack_state(repo, &state);
}

fn gh_log(repo: &PushRepo) -> String {
    std::fs::read_to_string(&repo.gh_log).unwrap_or_default()
}

fn git_log(repo: &PushRepo) -> String {
    std::fs::read_to_string(&repo.git_log).unwrap_or_default()
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

fn assert_clean(dir: &Path) {
    assert_eq!(
        git_output(dir, &["status", "--porcelain"]),
        "",
        "{} should be clean",
        dir.display()
    );
}

fn receipt(output: &Output) -> Value {
    let stderr = stderr_text(output);
    stderr
        .lines()
        .filter_map(|line| {
            let start = line.find('{')?;
            let end = line.rfind('}')?;
            serde_json::from_str::<Value>(&line[start..=end]).ok()
        })
        .next_back()
        .unwrap_or_else(|| panic!("no JSON receipt in stderr:\n{stderr}"))
}

fn install_fakes(prefix: &str, gh_body: &str) -> (PathBuf, PathBuf, PathBuf) {
    let fake_bin = temp_dir(prefix);
    let gh_log = fake_bin.join("gh.log");
    let git_log = fake_bin.join("git.log");
    let gh = fake_bin.join("gh");
    let gh_contents = format!(
        r#"#!/bin/sh
echo "$@" >> "$GH_LOG"
{gh_body}
printf 'unexpected gh invocation: %s\n' "$*" >&2
exit 97
"#
    );
    std::fs::write(&gh, gh_contents).expect("write fake gh");

    let git = fake_bin.join("git");
    let git_contents = r#"#!/bin/sh
if [ "$1" = "fetch" ] || [ "$1" = "push" ]; then
  echo "$@" >> "$GIT_LOG"
fi
exec /usr/bin/git "$@"
"#;
    std::fs::write(&git, git_contents).expect("write fake git");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for script in [&gh, &git] {
            let mut permissions = std::fs::metadata(script)
                .expect("fake metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(script, permissions).expect("chmod fake");
        }
    }
    (fake_bin, gh_log, git_log)
}

fn init_repo(prefix: &str, gh_body: &str) -> PushRepo {
    let path = temp_dir(prefix);
    let remote = temp_dir(&format!("{prefix}-remote"));
    let (fake_bin, gh_log, git_log) = install_fakes(&format!("{prefix}-bin"), gh_body);

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

    PushRepo {
        path,
        remote,
        fake_bin,
        gh_log,
        git_log,
    }
}

fn add_managed_branch(repo: &PushRepo, branch: &str, parent: &str, file: &str) -> PathBuf {
    run_ez(
        &repo.path,
        &["create", branch, "--from", parent, "--no-worktree"],
    );
    run(&repo.path, "git", &["checkout", branch]);
    commit_file(&repo.path, file, &format!("{branch}\n"), branch);
    run(&repo.path, "git", &["checkout", "main"]);

    let worktree = repo.path.join(".worktrees").join(branch.replace('/', "-"));
    run(
        &repo.path,
        "git",
        &[
            "worktree",
            "add",
            worktree.to_str().expect("worktree"),
            branch,
        ],
    );
    worktree
}

fn init_single_branch_repo(prefix: &str, gh_body: &str) -> (PushRepo, PathBuf) {
    let repo = init_repo(prefix, gh_body);
    let worktree = add_managed_branch(&repo, "feat/topic", "main", "topic.txt");
    assert_eq!(current_branch(&repo.path), "main");
    assert_eq!(current_branch(&worktree), "feat/topic");
    (repo, worktree)
}

#[test]
fn push_fails_on_trunk_before_pushing_or_calling_github() {
    let repo = init_repo("push-trunk-error", "exit 97");
    let before = stack_state_bytes(&repo.path);

    let output = run_ez_with_fakes(&repo, &repo.path, &["push"]);

    assert_failure(&output);
    assert!(stderr_text(&output).contains("on trunk"));
    assert_eq!(stack_state_bytes(&repo.path), before);
    assert_eq!(gh_log(&repo), "");
    assert_eq!(git_log(&repo), "");
}

#[test]
fn push_fails_on_unmanaged_branch_before_pushing_or_calling_github() {
    let (repo, _) = init_single_branch_repo("push-unmanaged-error", "exit 97");
    run(&repo.path, "git", &["checkout", "-b", "scratch"]);
    commit_file(&repo.path, "scratch.txt", "scratch\n", "scratch");
    let before = stack_state_bytes(&repo.path);

    let output = run_ez_with_fakes(&repo, &repo.path, &["push"]);

    assert_failure(&output);
    assert!(stderr_text(&output).contains("not tracked by ez"));
    assert_eq!(stack_state_bytes(&repo.path), before);
    assert_eq!(gh_log(&repo), "");
    assert_eq!(git_log(&repo), "");
}

#[test]
fn push_am_commits_tracked_changes_and_leaves_untracked_files_unstaged() {
    let (repo, worktree) = init_single_branch_repo("push-am-tracked", "exit 97");
    append_file(&worktree, "topic.txt", "tracked change\n");
    write_file(&worktree, "untracked.txt", "new\n");

    let output = run_ez_with_fakes(
        &repo,
        &worktree,
        &["push", "-am", "ship tracked", "--no-pr"],
    );

    assert_success(&output);
    assert_eq!(
        git_output(&worktree, &["log", "-1", "--format=%s"]),
        "ship tracked"
    );
    assert_eq!(
        git_output(&worktree, &["status", "--porcelain"]),
        "?? untracked.txt"
    );
    assert_eq!(remote_tip(&repo, "feat/topic"), ref_tip(&worktree, "HEAD"));
    assert_eq!(
        git_log(&repo).lines().collect::<Vec<_>>(),
        vec![
            "fetch origin feat/topic",
            "push origin feat/topic --force-with-lease"
        ]
    );
    assert_eq!(gh_log(&repo), "");
}

#[test]
fn push_am_all_files_commits_untracked_files() {
    let (repo, worktree) = init_single_branch_repo("push-am-all-files", "exit 97");
    write_file(&worktree, "untracked.txt", "new\n");

    let output = run_ez_with_fakes(&repo, &worktree, &["push", "-Am", "ship all", "--no-pr"]);

    assert_success(&output);
    assert_eq!(
        git_output(&worktree, &["log", "-1", "--format=%s"]),
        "ship all"
    );
    assert_clean(&worktree);
    assert_eq!(
        git_output(&worktree, &["show", "--name-only", "--format=", "HEAD"]),
        "untracked.txt"
    );
}

#[test]
fn push_pr_overrides_no_pr_config_and_creates_pr() {
    let gh = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "feat/topic" ]; then
  printf 'no pull request found\n' >&2
  exit 1
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ] && [ "$3" = "--title" ] && [ "$4" = "feat/topic" ] && [ "$7" = "--base" ] && [ "$8" = "main" ] && [ "$9" = "--head" ] && [ "${10}" = "feat/topic" ]; then
  printf 'https://github.com/org/repo/pull/51\n'
  exit 0
fi
"#;
    let (repo, worktree) = init_single_branch_repo("push-pr-overrides-no-pr", gh);
    configure_stack(&repo.path, "no_pr", Value::from(true));

    let output = run_ez_with_fakes(&repo, &worktree, &["push", "--pr"]);

    assert_success(&output);
    let receipt = receipt(&output);
    assert_eq!(receipt["created"], Value::from(true));
    assert_eq!(
        stack_state(&repo.path)["branches"]["feat/topic"]["pr_number"],
        Value::from(51)
    );
    assert!(gh_log(&repo).contains("pr create"));
}

#[test]
fn push_create_pr_uses_explicit_title_body_file_draft_repo_and_fork_head() {
    let gh = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "fork-owner:feat/topic" ] && [ "${10}" = "--repo" ] && [ "${11}" = "upstream-owner/project" ]; then
  printf 'no pull request found\n' >&2
  exit 1
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ] && [ "$3" = "--title" ] && [ "$4" = "Explicit title" ] && [ "$5" = "--body" ] && [ "$6" = "Body from file" ] && [ "$7" = "--base" ] && [ "$8" = "main" ] && [ "$9" = "--head" ] && [ "${10}" = "fork-owner:feat/topic" ] && [ "${11}" = "--draft" ] && [ "${12}" = "--repo" ] && [ "${13}" = "upstream-owner/project" ]; then
  printf 'https://github.com/upstream-owner/project/pull/52\n'
  exit 0
fi
"#;
    let (repo, worktree) = init_single_branch_repo("push-create-metadata", gh);
    run(&repo.path, "git", &["remote", "rename", "origin", "fork"]);
    configure_stack(&repo.path, "remote", Value::from("fork"));
    configure_stack(&repo.path, "repo", Value::from("upstream-owner/project"));
    configure_stack(&repo.path, "fork_repo", Value::from("fork-owner/project"));
    let body_file = worktree.join("body.md");
    std::fs::write(&body_file, "Body from file").expect("write body file");

    let output = run_ez_with_fakes(
        &repo,
        &worktree,
        &[
            "push",
            "--title",
            "Explicit title",
            "--body-file",
            body_file.to_str().expect("body path"),
            "--draft",
        ],
    );

    assert_success(&output);
    assert_eq!(
        gh_log(&repo).lines().collect::<Vec<_>>(),
        vec![
            "pr view fork-owner:feat/topic --json number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid --repo upstream-owner/project",
            "pr create --title Explicit title --body Body from file --base main --head fork-owner:feat/topic --draft --repo upstream-owner/project",
        ]
    );
    let receipt = receipt(&output);
    assert_eq!(receipt["repo"], Value::from("upstream-owner/project"));
    assert_eq!(receipt["fork_repo"], Value::from("fork-owner/project"));
    assert_eq!(
        stack_state(&repo.path)["branches"]["feat/topic"]["pr_number"],
        Value::from(52)
    );
}

#[test]
fn push_update_pr_repairs_base_when_parent_is_an_ancestor() {
    let gh = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "41" ]; then
  printf '{"number":41,"url":"https://github.com/org/repo/pull/41","state":"OPEN","title":"old","isDraft":false,"mergedAt":null,"baseRefName":"old-base"}\n'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ] && [ "$3" = "41" ] && [ "$4" = "--base" ] && [ "$5" = "main" ]; then
  exit 0
fi
"#;
    let (repo, worktree) = init_single_branch_repo("push-update-base", gh);
    set_pr_number(&repo.path, "feat/topic", Some(41));

    let output = run_ez_with_fakes(&repo, &worktree, &["push"]);

    assert_success(&output);
    assert_eq!(
        gh_log(&repo).lines().collect::<Vec<_>>(),
        vec![
            "repo view --json nameWithOwner -q .nameWithOwner",
            "pr view 41 --json number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid",
            "pr edit 41 --base main",
        ]
    );
    assert_eq!(receipt(&output)["created"], Value::from(false));
}

#[test]
fn push_update_pr_edits_title_without_body_when_only_title_is_explicit() {
    let gh = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "41" ]; then
  printf '{"number":41,"url":"https://github.com/org/repo/pull/41","state":"OPEN","title":"old","isDraft":false,"mergedAt":null,"baseRefName":"main"}\n'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ] && [ "$3" = "41" ] && [ "$4" = "--title" ] && [ "$5" = "New title" ] && [ -z "$6" ]; then
  exit 0
fi
"#;
    let (repo, worktree) = init_single_branch_repo("push-update-title", gh);
    set_pr_number(&repo.path, "feat/topic", Some(41));

    let output = run_ez_with_fakes(&repo, &worktree, &["push", "--title", "New title"]);

    assert_success(&output);
    assert_eq!(
        gh_log(&repo).lines().collect::<Vec<_>>(),
        vec![
            "repo view --json nameWithOwner -q .nameWithOwner",
            "pr view 41 --json number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid",
            "pr edit 41 --title New title",
        ]
    );
}

#[test]
fn push_update_pr_edits_body_when_body_is_explicit() {
    let gh = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "41" ]; then
  printf '{"number":41,"url":"https://github.com/org/repo/pull/41","state":"OPEN","title":"old","isDraft":false,"mergedAt":null,"baseRefName":"main"}\n'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ] && [ "$3" = "41" ] && [ "$4" = "--body" ] && [ "$5" = "Updated body" ] && [ -z "$6" ]; then
  exit 0
fi
"#;
    let (repo, worktree) = init_single_branch_repo("push-update-body", gh);
    set_pr_number(&repo.path, "feat/topic", Some(41));

    let output = run_ez_with_fakes(&repo, &worktree, &["push", "--body", "Updated body"]);

    assert_success(&output);
    assert_eq!(
        gh_log(&repo).lines().collect::<Vec<_>>(),
        vec![
            "repo view --json nameWithOwner -q .nameWithOwner",
            "pr view 41 --json number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid",
            "pr edit 41 --body Updated body",
        ]
    );
}

#[test]
fn push_update_pr_warns_and_skips_base_edit_when_parent_is_not_an_ancestor() {
    let gh = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "41" ]; then
  printf '{"number":41,"url":"https://github.com/org/repo/pull/41","state":"OPEN","title":"old","isDraft":false,"mergedAt":null,"baseRefName":"other-base"}\n'
  exit 0
fi
"#;
    let (repo, worktree) = init_single_branch_repo("push-stale-metadata-parent", gh);
    set_pr_number(&repo.path, "feat/topic", Some(41));
    set_branch_parent(&repo.path, "feat/topic", "missing-parent");

    let output = run_ez_with_fakes(&repo, &worktree, &["push"]);

    assert_success(&output);
    assert!(stderr_text(&output).contains("base not updated"));
    assert_eq!(
        gh_log(&repo).lines().collect::<Vec<_>>(),
        vec![
            "repo view --json nameWithOwner -q .nameWithOwner",
            "pr view 41 --json number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid",
        ]
    );
}

#[test]
fn push_does_not_persist_pr_number_when_github_create_fails() {
    let gh = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "feat/topic" ]; then
  exit 1
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  printf 'create failed\n' >&2
  exit 2
fi
"#;
    let (repo, worktree) = init_single_branch_repo("push-create-fails", gh);
    let before = stack_state_bytes(&repo.path);

    let output = run_ez_with_fakes(&repo, &worktree, &["push"]);

    assert_failure(&output);
    assert!(stderr_text(&output).contains("gh CLI error: create failed"));
    assert_eq!(stack_state_bytes(&repo.path), before);
    assert_eq!(remote_tip(&repo, "feat/topic"), ref_tip(&worktree, "HEAD"));
}

#[test]
fn push_stale_force_with_lease_failure_does_not_call_github_or_persist_state() {
    let (repo, worktree) = init_single_branch_repo("push-stale-lease", "exit 97");
    run(&worktree, "git", &["push", "origin", "feat/topic"]);
    commit_file(&worktree, "local.txt", "local\n", "local advance");
    let hook = git_common_dir(&worktree).join("hooks/pre-push");
    let hook_body = format!(
        r#"#!/bin/sh
git --git-dir="{}" update-ref refs/heads/feat/topic refs/heads/main
"#,
        repo.remote.display()
    );
    std::fs::write(&hook, hook_body).expect("write pre-push hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&hook)
            .expect("hook metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).expect("chmod hook");
    }
    let before = stack_state_bytes(&repo.path);

    let output = run_ez_with_fakes(&repo, &worktree, &["push"]);

    assert_failure(&output);
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("remote ref for `feat/topic` is stale"),
        "{stderr}"
    );
    assert_eq!(stack_state_bytes(&repo.path), before);
    assert_eq!(gh_log(&repo), "");
}

#[test]
fn push_stack_delegates_to_submit_and_pushes_atomically() {
    let gh = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  exit 1
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  case "$*" in
    *"--head feat/base"*) printf 'https://github.com/org/repo/pull/61\n'; exit 0 ;;
    *"--head feat/top"*) printf 'https://github.com/org/repo/pull/62\n'; exit 0 ;;
  esac
fi
"#;
    let repo = init_repo("push-stack-delegates", gh);
    let base = add_managed_branch(&repo, "feat/base", "main", "base.txt");
    let top = add_managed_branch(&repo, "feat/top", "feat/base", "top.txt");

    let output = run_ez_with_fakes(&repo, &top, &["push", "--stack"]);

    assert_success(&output);
    assert!(
        git_log(&repo)
            .lines()
            .any(|line| line == "push --atomic --force-with-lease origin feat/base feat/top"),
        "submit should use one atomic force-with-lease push:\n{}",
        git_log(&repo)
    );
    assert_eq!(
        stack_state(&repo.path)["branches"]["feat/base"]["pr_number"],
        Value::from(61)
    );
    assert_eq!(
        stack_state(&repo.path)["branches"]["feat/top"]["pr_number"],
        Value::from(62)
    );
    assert_eq!(current_branch(&base), "feat/base");
}
