use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);
static EDITOR_TEST_LOCK: Mutex<()> = Mutex::new(());

struct CoreRepo {
    path: PathBuf,
    remote: PathBuf,
    fake_bin: PathBuf,
    gh_log: PathBuf,
}

struct FakeEditor {
    root: PathBuf,
    path: PathBuf,
}

struct TempFileGuard {
    path: PathBuf,
}

impl Drop for CoreRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = std::fs::remove_dir_all(&self.remote);
        let _ = std::fs::remove_dir_all(&self.fake_bin);
    }
}

impl Drop for FakeEditor {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
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

fn run_ez_with_fake_gh(repo: &CoreRepo, dir: &Path, args: &[&str]) -> Output {
    run_ez_with_fake_gh_extra(repo, dir, args, &[])
}

fn run_ez_with_fake_gh_extra(
    repo: &CoreRepo,
    dir: &Path,
    args: &[&str],
    envs: &[(&str, &Path)],
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
        .env("GH_LOG", &repo.gh_log);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().expect("run ez with fake gh")
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

fn status_porcelain(dir: &Path) -> String {
    git_output(dir, &["status", "--porcelain"])
}

fn ref_tip(repo: &Path, refspec: &str) -> String {
    git_output(repo, &["rev-parse", refspec])
}

fn merge_base(repo: &Path, first: &str, second: &str) -> String {
    git_output(repo, &["merge-base", first, second])
}

fn expected_worktree_path(repo: &Path, branch: &str) -> PathBuf {
    repo.join(".worktrees").join(branch.replace('/', "-"))
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

fn gh_log(repo: &CoreRepo) -> String {
    std::fs::read_to_string(&repo.gh_log).unwrap_or_default()
}

fn clear_gh_log(repo: &CoreRepo) {
    std::fs::write(&repo.gh_log, "").expect("clear gh log");
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
        status_porcelain(dir),
        "",
        "{} should be clean",
        dir.display()
    );
}

fn assert_receipt_field(stderr: &str, field: &str, expected: Value) {
    let receipt = stderr
        .lines()
        .filter_map(|line| {
            let start = line.find('{')?;
            let end = line.rfind('}')?;
            serde_json::from_str::<Value>(&line[start..=end]).ok()
        })
        .next_back()
        .unwrap_or_else(|| panic!("no JSON receipt in stderr:\n{stderr}"));
    assert_eq!(receipt[field], expected, "receipt:\n{receipt}");
}

fn install_fake_gh(prefix: &str, script_body: &str) -> (PathBuf, PathBuf) {
    let fake_bin = temp_dir(prefix);
    let gh_log = fake_bin.join("gh.log");
    let script = fake_bin.join("gh");
    let script_contents = format!(
        r#"#!/bin/sh
echo "$@" >> "$GH_LOG"
{script_body}
printf 'unexpected gh invocation: %s\n' "$*" >&2
exit 97
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

fn install_fake_editor(prefix: &str) -> FakeEditor {
    let fake_bin = temp_dir(prefix);
    let script = fake_bin.join("editor");
    std::fs::write(
        &script,
        r#"#!/bin/sh
exit 0
"#,
    )
    .expect("write fake editor");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script)
            .expect("fake editor metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod fake editor");
    }
    FakeEditor {
        root: fake_bin,
        path: script,
    }
}

fn init_repo(prefix: &str, fake_bin: PathBuf, gh_log: PathBuf) -> CoreRepo {
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

    CoreRepo {
        path,
        remote,
        fake_bin,
        gh_log,
    }
}

fn add_managed_branch(repo: &CoreRepo, branch: &str, parent: &str, file: &str) -> PathBuf {
    run_ez(
        &repo.path,
        &["create", branch, "--from", parent, "--no-worktree"],
    );
    run(&repo.path, "git", &["checkout", branch]);
    commit_file(&repo.path, file, &format!("{branch}\n"), branch);
    run(&repo.path, "git", &["checkout", "main"]);

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

fn init_two_branch_stack(prefix: &str) -> (CoreRepo, PathBuf, PathBuf) {
    let (fake_bin, gh_log) = install_fake_gh(&format!("{prefix}-gh"), "");
    let repo = init_repo(prefix, fake_bin, gh_log);
    let base = add_managed_branch(&repo, "feat/base", "main", "base.txt");
    let child = add_managed_branch(&repo, "feat/child", "feat/base", "child.txt");
    assert_eq!(current_branch(&repo.path), "main");
    assert_eq!(current_branch(&base), "feat/base");
    assert_eq!(current_branch(&child), "feat/child");
    (repo, base, child)
}

fn init_single_branch_repo(prefix: &str, script_body: &str) -> (CoreRepo, PathBuf) {
    let (fake_bin, gh_log) = install_fake_gh(&format!("{prefix}-gh"), script_body);
    let repo = init_repo(prefix, fake_bin, gh_log);
    let worktree = add_managed_branch(&repo, "feat/topic", "main", "topic.txt");
    assert_eq!(current_branch(&repo.path), "main");
    assert_eq!(current_branch(&worktree), "feat/topic");
    (repo, worktree)
}

#[test]
fn commit_from_linked_worktree_restacks_linked_descendant() {
    let (repo, base_worktree, child_worktree) = init_two_branch_stack("core-commit-restack");
    let base_before = ref_tip(&repo.path, "feat/base");
    let child_before = ref_tip(&repo.path, "feat/child");

    write_file(&base_worktree, "base-follow-up.txt", "base follow-up\n");
    let output = run_ez(&base_worktree, &["commit", "-Am", "base follow-up"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert_receipt_field(&stderr, "cmd", Value::from("commit"));
    assert_ne!(ref_tip(&repo.path, "feat/base"), base_before);
    assert_ne!(ref_tip(&repo.path, "feat/child"), child_before);
    let base_after = ref_tip(&repo.path, "feat/base");
    assert_eq!(
        stack_state(&repo.path)["branches"]["feat/child"]["parent_head"],
        Value::from(base_after.clone())
    );
    assert_eq!(
        merge_base(&repo.path, "feat/base", "feat/child"),
        base_after
    );
    assert_eq!(current_branch(&child_worktree), "feat/child");
    assert!(base_worktree.join("base-follow-up.txt").exists());
    assert!(child_worktree.join("base-follow-up.txt").exists());
    assert!(child_worktree.join("child.txt").exists());
    assert_clean(&base_worktree);
    assert_clean(&child_worktree);
}

#[test]
fn amend_from_linked_worktree_restacks_linked_descendant() {
    let (repo, base_worktree, child_worktree) = init_two_branch_stack("core-amend-restack");
    let base_before = ref_tip(&repo.path, "feat/base");
    let child_before = ref_tip(&repo.path, "feat/child");

    append_file(&base_worktree, "base.txt", "amended\n");
    let output = run_ez(&base_worktree, &["amend", "-a", "-m", "amended base"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert_receipt_field(&stderr, "cmd", Value::from("amend"));
    assert_ne!(ref_tip(&repo.path, "feat/base"), base_before);
    assert_ne!(ref_tip(&repo.path, "feat/child"), child_before);
    let base_after = ref_tip(&repo.path, "feat/base");
    assert_eq!(
        stack_state(&repo.path)["branches"]["feat/child"]["parent_head"],
        Value::from(base_after.clone())
    );
    assert_eq!(
        merge_base(&repo.path, "feat/base", "feat/child"),
        base_after
    );
    assert_eq!(current_branch(&child_worktree), "feat/child");
    let child_base = std::fs::read_to_string(child_worktree.join("base.txt")).expect("base file");
    assert!(child_base.contains("amended"));
    assert!(child_worktree.join("child.txt").exists());
    assert_clean(&base_worktree);
    assert_clean(&child_worktree);
}

#[test]
fn log_and_parent_report_the_real_nested_worktree_stack() {
    let (repo, base_worktree, child_worktree) = init_two_branch_stack("core-log-parent");

    let parent = run_ez(&child_worktree, &["parent"]);
    assert_eq!(stdout_text(&parent), "feat/base");

    let json_output = run_ez(&child_worktree, &["log", "--json"]);
    let entries: Value = serde_json::from_slice(&json_output.stdout).expect("parse log json");
    assert_eq!(entries[0]["branch"], "feat/base");
    assert_eq!(entries[0]["parent"], "main");
    assert_eq!(entries[0]["depth"], 1);
    assert_eq!(entries[0]["children"], serde_json::json!(["feat/child"]));
    assert_eq!(entries[1]["branch"], "feat/child");
    assert_eq!(entries[1]["parent"], "feat/base");
    assert_eq!(entries[1]["depth"], 2);

    let human = run_ez(&child_worktree, &["log"]);
    let stderr = stderr_text(&human);
    assert!(stderr.contains("feat/base"), "{stderr}");
    assert!(stderr.contains("feat/child"), "{stderr}");
    assert!(stderr.contains("[wt: feat-base]"), "{stderr}");
    assert!(stderr.contains("[wt: feat-child]"), "{stderr}");
    assert!(stderr.contains("← current"), "{stderr}");
    assert!(base_worktree.is_dir());
    assert!(repo.path.is_dir());
}

#[test]
fn log_renders_pr_draft_ci_and_url_from_github_responses() {
    let script = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  printf '%s\n' '{"number":7,"url":"https://github.com/owner/repo/pull/7","state":"OPEN","title":"Topic","isDraft":true,"mergedAt":null,"baseRefName":"main"}'
  exit 0
fi
if [ "$1" = "run" ] && [ "$2" = "list" ]; then
  printf '%s\n' '{"status":"completed","conclusion":"success"}'
  exit 0
fi
"#;
    let (repo, worktree) = init_single_branch_repo("core-log-pr", script);
    set_pr_number(&repo.path, "feat/topic", Some(7));
    let mut state = stack_state(&repo.path);
    state["repo"] = Value::from("owner/repo");
    write_stack_state(&repo.path, &state);

    let human = run_ez_with_fake_gh(&repo, &worktree, &["log"]);
    let stderr = stderr_text(&human);
    assert!(stderr.contains("#7"), "{stderr}");
    assert!(stderr.contains("draft"), "{stderr}");
    assert!(stderr.contains('✓'), "{stderr}");
    assert!(stderr.contains("[wt: feat-topic]"), "{stderr}");

    let json_output = run_ez_with_fake_gh(&repo, &worktree, &["log", "--json"]);
    assert_success(&json_output);
    let entries: Value = serde_json::from_slice(&json_output.stdout).expect("parse log json");
    assert_eq!(entries[0]["branch"], "feat/topic");
    assert_eq!(entries[0]["pr_number"], 7);
    assert_eq!(entries[0]["pr_url"], "https://github.com/owner/repo/pull/7");
    assert_eq!(entries[0]["pr_state"], "OPEN");
    assert_eq!(entries[0]["is_draft"], true);

    let calls = gh_log(&repo);
    assert!(calls.contains("pr view 7"), "{calls}");
    assert!(calls.contains("run list --branch feat/topic"), "{calls}");
}

#[test]
fn push_no_pr_updates_real_bare_remote_without_github_pr_calls() {
    let (repo, worktree) = init_single_branch_repo("core-push-no-pr", "exit 97");

    let output = run_ez_with_fake_gh(&repo, &worktree, &["push", "--no-pr"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert_receipt_field(&stderr, "cmd", Value::from("push"));
    assert_receipt_field(&stderr, "no_pr", Value::from(true));
    assert_eq!(
        git_output(
            &repo.path,
            &["ls-remote", "origin", "refs/heads/feat/topic"]
        )
        .split_whitespace()
        .next()
        .expect("remote sha"),
        ref_tip(&repo.path, "feat/topic")
    );
    assert!(stack_state(&repo.path)["branches"]["feat/topic"]["pr_number"].is_null());
    assert_eq!(gh_log(&repo), "");
}

#[test]
fn push_draft_creates_pr_and_persists_number() {
    let script_body = r#"
if [ "$1" = "repo" ] && [ "$2" = "view" ] && [ "$3" = "--json" ] && [ "$4" = "nameWithOwner" ] && [ "$5" = "-q" ] && [ "$6" = ".nameWithOwner" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "feat/topic" ] && [ "$4" = "--json" ] && [ "$5" = "number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid" ]; then
  printf 'no pull request found\n' >&2
  exit 1
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ] && [ "$3" = "--title" ] && [ "$4" = "feat/topic" ] && [ "$5" = "--body" ] && [ "$6" = 'Part of a stack managed by `ez`.' ] && [ "$7" = "--base" ] && [ "$8" = "main" ] && [ "$9" = "--head" ] && [ "${10}" = "feat/topic" ] && [ "${11}" = "--draft" ]; then
  printf 'https://github.com/org/repo/pull/42\n'
  exit 0
fi
"#;
    let (repo, worktree) = init_single_branch_repo("core-push-draft", script_body);

    let output = run_ez_with_fake_gh(&repo, &worktree, &["push", "--draft"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert_receipt_field(&stderr, "cmd", Value::from("push"));
    assert_receipt_field(&stderr, "created", Value::from(true));
    assert_eq!(
        git_output(
            &repo.path,
            &["ls-remote", "origin", "refs/heads/feat/topic"]
        )
        .split_whitespace()
        .next()
        .expect("remote sha"),
        ref_tip(&repo.path, "feat/topic")
    );
    assert_eq!(
        stack_state(&repo.path)["branches"]["feat/topic"]["pr_number"],
        Value::from(42)
    );
    let log = gh_log(&repo);
    assert!(log.contains("repo view --json nameWithOwner -q .nameWithOwner"));
    assert!(log.contains(
        "pr view feat/topic --json number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid"
    ));
    assert!(
        log.contains("pr create --title feat/topic --body "),
        "expected pr create with derived title:\n{log}"
    );
    assert!(log.contains("--base main --head feat/topic --draft"));
}

#[test]
fn push_prefills_body_with_repo_pr_template() {
    let script_body = r#"
if [ "$1" = "repo" ] && [ "$2" = "view" ] && [ "$3" = "--json" ] && [ "$4" = "nameWithOwner" ] && [ "$5" = "-q" ] && [ "$6" = ".nameWithOwner" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "feat/topic" ] && [ "$4" = "--json" ] && [ "$5" = "number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid" ]; then
  printf 'no pull request found\n' >&2
  exit 1
fi
if [ "$1" = "pr" ] && [ "$2" = "create" ] && [ "$3" = "--title" ] && [ "$4" = "feat/topic" ] && [ "$5" = "--body" ] && [ "$6" = "$(printf '## Description\n\nFill me in.')" ] && [ "$7" = "--base" ] && [ "$8" = "main" ] && [ "$9" = "--head" ] && [ "${10}" = "feat/topic" ] && [ "${11}" = "--draft" ]; then
  printf 'https://github.com/org/repo/pull/42\n'
  exit 0
fi
"#;
    let (repo, worktree) = init_single_branch_repo("core-push-template", script_body);
    std::fs::create_dir_all(worktree.join(".github")).expect("create .github dir");
    write_file(
        &worktree,
        ".github/pull_request_template.md",
        "## Description\n\nFill me in.\n",
    );

    let output = run_ez_with_fake_gh(&repo, &worktree, &["push", "--draft"]);

    assert_success(&output);
    assert_eq!(
        stack_state(&repo.path)["branches"]["feat/topic"]["pr_number"],
        Value::from(42)
    );
    let log = gh_log(&repo);
    assert!(
        log.contains("## Description"),
        "expected pr create to pre-fill the repo PR template:\n{log}"
    );
}

#[test]
fn draft_and_ready_emit_exact_github_calls() {
    let script_body = r#"
if [ "$1" = "pr" ] && [ "$2" = "ready" ] && [ "$3" = "--undo" ] && [ "$4" = "42" ]; then
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "ready" ] && [ "$3" = "42" ]; then
  exit 0
fi
"#;
    let (repo, worktree) = init_single_branch_repo("core-draft-ready", script_body);
    set_pr_number(&repo.path, "feat/topic", Some(42));
    let stack_before = stack_state_bytes(&repo.path);

    let draft = run_ez_with_fake_gh(&repo, &worktree, &["draft"]);
    let ready = run_ez_with_fake_gh(&repo, &worktree, &["ready"]);

    assert_success(&draft);
    assert_success(&ready);
    assert!(stderr_text(&draft).contains("marked as draft"));
    assert!(stderr_text(&ready).contains("marked as ready"));
    assert_eq!(
        gh_log(&repo).lines().collect::<Vec<_>>(),
        vec!["pr ready --undo 42", "pr ready 42"]
    );
    assert_eq!(stack_state_bytes(&repo.path), stack_before);
}

#[test]
fn pr_link_prefers_repo_name_and_falls_back_to_pr_status() {
    let repo_view_ok = r#"
if [ "$1" = "repo" ] && [ "$2" = "view" ] && [ "$3" = "--json" ] && [ "$4" = "nameWithOwner" ] && [ "$5" = "-q" ] && [ "$6" = ".nameWithOwner" ]; then
  printf 'org/repo\n'
  exit 0
fi
"#;
    let (repo, worktree) = init_single_branch_repo("core-pr-link-fast", repo_view_ok);
    set_pr_number(&repo.path, "feat/topic", Some(42));
    let stack_before = stack_state_bytes(&repo.path);

    let output = run_ez_with_fake_gh(&repo, &worktree, &["pr-link"]);

    assert_success(&output);
    assert_eq!(stdout_text(&output), "https://github.com/org/repo/pull/42");
    assert_eq!(
        gh_log(&repo).lines().collect::<Vec<_>>(),
        vec!["repo view --json nameWithOwner -q .nameWithOwner"]
    );
    assert_eq!(stack_state_bytes(&repo.path), stack_before);

    let fallback = r#"
if [ "$1" = "repo" ] && [ "$2" = "view" ] && [ "$3" = "--json" ] && [ "$4" = "nameWithOwner" ] && [ "$5" = "-q" ] && [ "$6" = ".nameWithOwner" ]; then
  printf 'repo unavailable\n' >&2
  exit 1
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "42" ] && [ "$4" = "--json" ] && [ "$5" = "number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid" ]; then
  printf '{"number":42,"url":"https://github.com/org/repo/pull/42","state":"OPEN","title":"feat/topic","isDraft":false,"mergedAt":null,"baseRefName":"main"}\n'
  exit 0
fi
"#;
    let (repo, worktree) = init_single_branch_repo("core-pr-link-fallback", fallback);
    set_pr_number(&repo.path, "feat/topic", Some(42));
    let stack_before = stack_state_bytes(&repo.path);

    let output = run_ez_with_fake_gh(&repo, &worktree, &["pr-link"]);

    assert_success(&output);
    assert_eq!(stdout_text(&output), "https://github.com/org/repo/pull/42");
    assert_eq!(
        gh_log(&repo).lines().collect::<Vec<_>>(),
        vec![
            "repo view --json nameWithOwner -q .nameWithOwner",
            "pr view 42 --json number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid",
        ]
    );
    assert_eq!(stack_state_bytes(&repo.path), stack_before);
}

#[test]
fn pr_view_opens_browser_and_missing_pr_short_circuits() {
    let script_body = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "--web" ] && [ "$4" = "42" ]; then
  exit 0
fi
"#;
    let (repo, worktree) = init_single_branch_repo("core-pr-view", script_body);
    set_pr_number(&repo.path, "feat/topic", Some(42));

    let output = run_ez_with_fake_gh(&repo, &worktree, &["pr"]);

    assert_success(&output);
    assert_eq!(
        gh_log(&repo).lines().collect::<Vec<_>>(),
        vec!["pr view --web 42"]
    );
    assert!(stderr_text(&output).contains("Opened PR for `feat/topic`"));

    set_pr_number(&repo.path, "feat/topic", None);
    clear_gh_log(&repo);
    let missing = run_ez_with_fake_gh(&repo, &worktree, &["pr"]);

    assert_failure(&missing);
    assert!(stderr_text(&missing).contains("run `ez push`"));
    assert_eq!(gh_log(&repo), "");
}

#[test]
fn pr_edit_explicit_fields_calls_github_and_preserves_local_state() {
    let script_body = r#"
if [ "$1" = "pr" ] && [ "$2" = "edit" ] && [ "$3" = "42" ] && [ "$4" = "--title" ] && [ "$5" = "New title" ] && [ "$6" = "--body" ] && [ "$7" = "New body" ]; then
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "42" ] && [ "$4" = "--json" ] && [ "$5" = "number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid" ]; then
  printf '{"number":42,"url":"https://github.com/org/repo/pull/42","state":"OPEN","title":"New title","isDraft":false,"mergedAt":null,"baseRefName":"main"}\n'
  exit 0
fi
"#;
    let (repo, worktree) = init_single_branch_repo("core-pr-edit-explicit", script_body);
    set_pr_number(&repo.path, "feat/topic", Some(42));
    let stack_before = stack_state_bytes(&repo.path);

    let output = run_ez_with_fake_gh(
        &repo,
        &worktree,
        &["pr-edit", "--title", "New title", "--body", "New body"],
    );

    assert_success(&output);
    assert!(stderr_text(&output).contains("https://github.com/org/repo/pull/42"));
    assert_eq!(
        gh_log(&repo).lines().collect::<Vec<_>>(),
        vec![
            "pr edit 42 --title New title --body New body",
            "pr view 42 --json number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid",
        ]
    );
    assert_eq!(stack_state_bytes(&repo.path), stack_before);
}

#[test]
fn pr_edit_unchanged_editor_body_skips_update() {
    let _guard = lock_editor_test();
    let pr_number = 1_000_000 + u64::from(std::process::id());
    let tmp_path = PathBuf::from(format!("/tmp/ez-pr-{pr_number}.md"));
    let tmp_guard = TempFileGuard {
        path: tmp_path.clone(),
    };
    let _ = std::fs::remove_file(&tmp_path);

    let script_body = format!(
        r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "{pr_number}" ] && [ "$4" = "--json" ] && [ "$5" = "body" ] && [ "$6" = "-q" ] && [ "$7" = ".body" ]; then
  printf 'Existing body\n'
  exit 0
fi
"#
    );
    let (repo, worktree) = init_single_branch_repo("core-pr-edit-editor", &script_body);
    set_pr_number(&repo.path, "feat/topic", Some(pr_number));
    let fake_editor = install_fake_editor("core-pr-edit-editor-bin");
    let stack_before = stack_state_bytes(&repo.path);

    let output = run_ez_with_fake_gh_extra(
        &repo,
        &worktree,
        &["pr-edit"],
        &[("VISUAL", &fake_editor.path)],
    );

    assert_success(&output);
    assert!(stderr_text(&output).contains("No changes made"));
    assert_eq!(
        gh_log(&repo).lines().collect::<Vec<_>>(),
        vec![format!("pr view {pr_number} --json body -q .body")]
    );
    assert_eq!(stack_state_bytes(&repo.path), stack_before);
    assert!(
        !tmp_path.exists(),
        "temporary editor file should be removed"
    );
    drop(tmp_guard);
}

fn lock_editor_test() -> MutexGuard<'static, ()> {
    EDITOR_TEST_LOCK.lock().expect("editor test lock")
}
