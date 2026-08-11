use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct PrRepo {
    path: PathBuf,
    remote: PathBuf,
    fake_bin: PathBuf,
    gh_log: PathBuf,
}

struct FakeEditor {
    root: PathBuf,
    path: PathBuf,
}

impl Drop for PrRepo {
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

fn run_ez_with_fake_gh(repo: &PrRepo, args: &[&str]) -> Output {
    run_ez_with_fake_gh_extra(repo, args, &[])
}

fn run_ez_with_fake_gh_extra(repo: &PrRepo, args: &[&str], envs: &[(&str, &Path)]) -> Output {
    let inherited_path = std::env::var("PATH").unwrap_or_default();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ez"));
    cmd.args(args)
        .current_dir(&repo.path)
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

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) {
    write_file(dir, file, contents);
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
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

fn set_configured_repo(repo: &Path, owner_repo: &str) {
    let mut state = stack_state(repo);
    state["repo"] = Value::from(owner_repo);
    write_stack_state(repo, &state);
}

fn gh_log(repo: &PrRepo) -> String {
    std::fs::read_to_string(&repo.gh_log).unwrap_or_default()
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

fn install_fake_editor(prefix: &str, script_body: &str) -> FakeEditor {
    let root = temp_dir(prefix);
    let path = root.join("editor");
    std::fs::write(&path, script_body).expect("write fake editor");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path)
            .expect("fake editor metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("chmod fake editor");
    }
    FakeEditor { root, path }
}

fn init_repo(prefix: &str, script_body: &str, pr_number: u64) -> PrRepo {
    let (fake_bin, gh_log) = install_fake_gh(&format!("{prefix}-gh"), script_body);
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
    run_ez(
        &path,
        &["create", "feat/topic", "--from", "main", "--no-worktree"],
    );
    run(&path, "git", &["checkout", "feat/topic"]);
    commit_file(&path, "topic.txt", "topic\n", "topic");
    set_pr_number(&path, "feat/topic", Some(pr_number));

    PrRepo {
        path,
        remote,
        fake_bin,
        gh_log,
    }
}

fn assert_no_edit_buffers(directory: &Path) {
    let leaked: Vec<_> = std::fs::read_dir(directory)
        .expect("read temporary directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("ez-pr-"))
        .map(|entry| entry.path())
        .collect();
    assert!(
        leaked.is_empty(),
        "temporary edit buffers leaked: {leaked:?}"
    );
}

fn assert_edit_buffer_was_unique_and_cleaned(repo: &PrRepo, path_log: &Path) {
    let path = PathBuf::from(std::fs::read_to_string(path_log).expect("read editor path log"));
    assert_eq!(path.parent(), Some(repo.fake_bin.as_path()));
    assert!(
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("ez-pr-")),
        "unexpected edit buffer path: {}",
        path.display()
    );
    assert!(!path.exists(), "temporary edit buffer should be removed");
    assert_no_edit_buffers(&repo.fake_bin);
}

#[test]
fn pr_edit_explicit_title_body_sends_exact_call_and_reports_status() {
    let repo = init_repo(
        "pr-edit-explicit",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "edit" ] && [ "$3" = "42" ] && [ "$4" = "--title" ] && [ "$5" = "New title" ] && [ "$6" = "--body" ] && [ "$7" = "New body" ]; then
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "42" ] && [ "$4" = "--json" ] && [ "$5" = "number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid" ]; then
  printf '{"number":42,"url":"https://github.com/org/repo/pull/42","state":"OPEN","title":"Topic","isDraft":false,"mergedAt":null,"baseRefName":"main"}\n'
  exit 0
fi
"#,
        42,
    );

    let output = run_ez_with_fake_gh(
        &repo,
        &["pr-edit", "--title", "New title", "--body", "New body"],
    );

    assert_success(&output);
    assert!(stderr_text(&output).contains("https://github.com/org/repo/pull/42"));
    assert_eq!(
        gh_log(&repo),
        "pr edit 42 --title New title --body New body\npr view 42 --json number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid\n"
    );
    assert_clean(&repo.path);
}

#[test]
fn pr_edit_body_file_reads_content_and_sends_exact_body() {
    let repo = init_repo(
        "pr-edit-body-file",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "edit" ] && [ "$3" = "42" ] && [ "$4" = "--body" ]; then
  printf '%s' "$5" > "$CAPTURED_BODY"
  cmp -s "$CAPTURED_BODY" "$EXPECTED_BODY"
  exit $?
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "42" ] && [ "$4" = "--json" ] && [ "$5" = "number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid" ]; then
  printf '{"number":42,"url":"https://github.com/org/repo/pull/42","state":"OPEN","title":"Topic","isDraft":false,"mergedAt":null,"baseRefName":"main"}\n'
  exit 0
fi
"#,
        42,
    );
    let body_file = repo.path.join("body.md");
    let captured = repo.path.join("captured-body.md");
    std::fs::write(&body_file, "Body from file\nSecond line\n").expect("write body");

    let output = run_ez_with_fake_gh_extra(
        &repo,
        &[
            "pr-edit",
            "--body-file",
            body_file.to_str().expect("body file"),
        ],
        &[("EXPECTED_BODY", &body_file), ("CAPTURED_BODY", &captured)],
    );

    assert_success(&output);
    assert_eq!(
        std::fs::read_to_string(captured).expect("captured body"),
        "Body from file\nSecond line\n"
    );
    assert_eq!(
        gh_log(&repo),
        "pr edit 42 --body Body from file\nSecond line\n\npr view 42 --json number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid\n"
    );
}

#[test]
fn pr_edit_editor_unchanged_makes_no_edit_and_cleans_temp_file() {
    let pr_number = 43;
    let repo = init_repo(
        "pr-edit-editor-unchanged",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "43" ] && [ "$4" = "--json" ] && [ "$5" = "body" ] && [ "$6" = "-q" ] && [ "$7" = ".body" ]; then
  printf 'Existing body\n'
  exit 0
fi
"#,
        pr_number,
    );
    let edit_path_log = repo.fake_bin.join("editor-path.log");
    let editor = install_fake_editor(
        "pr-edit-editor-unchanged-editor",
        r#"#!/bin/sh
printf '%s' "$1" > "$EDIT_PATH_LOG"
exit 0
"#,
    );

    let output = run_ez_with_fake_gh_extra(
        &repo,
        &["pr-edit"],
        &[
            ("EDITOR", &editor.path),
            ("TMPDIR", &repo.fake_bin),
            ("EDIT_PATH_LOG", &edit_path_log),
        ],
    );

    assert_success(&output);
    assert!(stderr_text(&output).contains("No changes made"));
    assert_eq!(
        gh_log(&repo),
        "pr view 43 --json body -q .body\n",
        "unchanged editor must not edit PR"
    );
    assert_edit_buffer_was_unique_and_cleaned(&repo, &edit_path_log);
    assert_clean(&repo.path);
}

#[test]
fn pr_edit_editor_changed_updates_body_and_reports_status() {
    let pr_number = 44;
    let repo = init_repo(
        "pr-edit-editor-changed",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "44" ] && [ "$4" = "--json" ] && [ "$5" = "body" ] && [ "$6" = "-q" ] && [ "$7" = ".body" ]; then
  printf 'Existing body\n'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ] && [ "$3" = "44" ] && [ "$4" = "--body" ] && [ "$5" = "Edited body" ]; then
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "44" ] && [ "$4" = "--json" ] && [ "$5" = "number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid" ]; then
  printf '{"number":44,"url":"https://github.com/org/repo/pull/44","state":"OPEN","title":"Topic","isDraft":false,"mergedAt":null,"baseRefName":"main"}\n'
  exit 0
fi
"#,
        pr_number,
    );
    let edit_path_log = repo.fake_bin.join("editor-path.log");
    let editor = install_fake_editor(
        "pr-edit-editor-changed-editor",
        r#"#!/bin/sh
printf '%s' "$1" > "$EDIT_PATH_LOG"
printf 'Edited body' > "$1"
"#,
    );

    let output = run_ez_with_fake_gh_extra(
        &repo,
        &["pr-edit"],
        &[
            ("EDITOR", &editor.path),
            ("TMPDIR", &repo.fake_bin),
            ("EDIT_PATH_LOG", &edit_path_log),
        ],
    );

    assert_success(&output);
    assert!(stderr_text(&output).contains("https://github.com/org/repo/pull/44"));
    assert_eq!(
        gh_log(&repo),
        "pr view 44 --json body -q .body\npr edit 44 --body Edited body\npr view 44 --json number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid\n"
    );
    assert_edit_buffer_was_unique_and_cleaned(&repo, &edit_path_log);
}

#[test]
fn pr_edit_editor_nonzero_is_actionable_cleans_temp_file_and_preserves_state() {
    let pr_number = 45;
    let repo = init_repo(
        "pr-edit-editor-nonzero",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "45" ] && [ "$4" = "--json" ] && [ "$5" = "body" ] && [ "$6" = "-q" ] && [ "$7" = ".body" ]; then
  printf 'Existing body\n'
  exit 0
fi
"#,
        pr_number,
    );
    let before_state = stack_state_bytes(&repo.path);
    let edit_path_log = repo.fake_bin.join("editor-path.log");
    let editor = install_fake_editor(
        "pr-edit-editor-nonzero-editor",
        r#"#!/bin/sh
printf '%s' "$1" > "$EDIT_PATH_LOG"
printf 'partial edit' > "$1"
exit 12
"#,
    );

    let output = run_ez_with_fake_gh_extra(
        &repo,
        &["pr-edit"],
        &[
            ("EDITOR", &editor.path),
            ("TMPDIR", &repo.fake_bin),
            ("EDIT_PATH_LOG", &edit_path_log),
        ],
    );

    assert_failure(&output);
    assert!(
        stderr_text(&output).contains("Editor exited with non-zero status"),
        "error should be actionable:\n{}",
        stderr_text(&output)
    );
    assert_eq!(stack_state_bytes(&repo.path), before_state);
    assert_eq!(gh_log(&repo), "pr view 45 --json body -q .body\n");
    assert_edit_buffer_was_unique_and_cleaned(&repo, &edit_path_log);
    assert_clean(&repo.path);
}

#[test]
fn pr_edit_editor_launch_failure_cleans_temp_file() {
    let pr_number = 46;
    let repo = init_repo(
        "pr-edit-editor-missing",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "46" ] && [ "$4" = "--json" ] && [ "$5" = "body" ] && [ "$6" = "-q" ] && [ "$7" = ".body" ]; then
  printf 'Existing body\n'
  exit 0
fi
"#,
        pr_number,
    );
    let missing_editor = repo.path.join("missing-editor");

    let output = run_ez_with_fake_gh_extra(
        &repo,
        &["pr-edit"],
        &[("EDITOR", &missing_editor), ("TMPDIR", &repo.fake_bin)],
    );

    assert_failure(&output);
    assert!(
        stderr_text(&output).contains("failed to launch editor"),
        "error should explain how to configure the editor:\n{}",
        stderr_text(&output)
    );
    assert_eq!(gh_log(&repo), "pr view 46 --json body -q .body\n");
    assert_no_edit_buffers(&repo.fake_bin);
    assert_clean(&repo.path);
}

#[test]
fn pr_edit_editor_change_falls_back_when_status_lookup_fails() {
    let pr_number = 47;
    let repo = init_repo(
        "pr-edit-editor-status-fallback",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "47" ] && [ "$4" = "--json" ] && [ "$5" = "body" ] && [ "$6" = "-q" ] && [ "$7" = ".body" ]; then
  printf 'Existing body\n'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ] && [ "$3" = "47" ] && [ "$4" = "--body" ] && [ "$5" = "Edited body" ]; then
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "47" ] && [ "$4" = "--json" ] && [ "$5" = "number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid" ]; then
  exit 1
fi
"#,
        pr_number,
    );
    let edit_path_log = repo.fake_bin.join("editor-path.log");
    let editor = install_fake_editor(
        "pr-edit-editor-status-fallback-editor",
        r#"#!/bin/sh
printf '%s' "$1" > "$EDIT_PATH_LOG"
printf 'Edited body' > "$1"
"#,
    );

    let output = run_ez_with_fake_gh_extra(
        &repo,
        &["pr-edit"],
        &[
            ("EDITOR", &editor.path),
            ("TMPDIR", &repo.fake_bin),
            ("EDIT_PATH_LOG", &edit_path_log),
        ],
    );

    assert_success(&output);
    assert!(stderr_text(&output).contains("Updated PR #47 body"));
    assert_edit_buffer_was_unique_and_cleaned(&repo, &edit_path_log);
}

#[test]
fn pr_edit_failure_is_propagated_without_status_lookup_or_state_change() {
    let repo = init_repo(
        "pr-edit-failure",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "edit" ] && [ "$3" = "42" ] && [ "$4" = "--title" ] && [ "$5" = "Rejected title" ]; then
  printf 'edit rejected\n' >&2
  exit 1
fi
"#,
        42,
    );
    let before_state = stack_state_bytes(&repo.path);

    let output = run_ez_with_fake_gh(&repo, &["pr-edit", "--title", "Rejected title"]);

    assert_failure(&output);
    assert!(stderr_text(&output).contains("edit rejected"));
    assert_eq!(stack_state_bytes(&repo.path), before_state);
    assert_eq!(gh_log(&repo), "pr edit 42 --title Rejected title\n");
    assert_clean(&repo.path);
}

#[test]
fn pr_edit_rejects_trunk_unmanaged_and_managed_branch_without_pr() {
    let repo = init_repo("pr-edit-rejections", "", 42);

    run(&repo.path, "git", &["checkout", "main"]);
    let trunk = run_ez_with_fake_gh(&repo, &["pr-edit", "--title", "Nope"]);
    assert_failure(&trunk);
    assert!(stderr_text(&trunk).contains("on trunk"));

    run(&repo.path, "git", &["checkout", "-b", "loose"]);
    let unmanaged = run_ez_with_fake_gh(&repo, &["pr-edit", "--title", "Nope"]);
    assert_failure(&unmanaged);
    assert!(stderr_text(&unmanaged).contains("not tracked by ez"));

    run(&repo.path, "git", &["checkout", "feat/topic"]);
    set_pr_number(&repo.path, "feat/topic", None);
    let no_pr = run_ez_with_fake_gh(&repo, &["pr-edit", "--title", "Nope"]);
    assert_failure(&no_pr);
    assert!(stderr_text(&no_pr).contains("run `ez push`"));

    assert_eq!(gh_log(&repo), "", "rejections must not invoke gh");
}

#[test]
fn pr_edit_success_falls_back_when_post_edit_status_lookup_fails() {
    let repo = init_repo(
        "pr-edit-status-fallback",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "edit" ] && [ "$3" = "42" ] && [ "$4" = "--title" ] && [ "$5" = "Fallback title" ]; then
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "42" ] && [ "$4" = "--json" ] && [ "$5" = "number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid" ]; then
  printf 'lookup failed\n' >&2
  exit 1
fi
"#,
        42,
    );

    let output = run_ez_with_fake_gh(&repo, &["pr-edit", "--title", "Fallback title"]);

    assert_success(&output);
    assert!(stderr_text(&output).contains("Updated PR #42"));
    assert_eq!(
        gh_log(&repo),
        "pr edit 42 --title Fallback title\npr view 42 --json number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid\n"
    );
}

#[test]
fn draft_and_ready_send_exact_gh_calls() {
    let repo = init_repo(
        "draft-ready",
        r#"
if [ "$1" = "pr" ] && [ "$2" = "ready" ] && [ "$3" = "--undo" ] && [ "$4" = "42" ]; then
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "ready" ] && [ "$3" = "42" ]; then
  exit 0
fi
"#,
        42,
    );

    let draft = run_ez_with_fake_gh(&repo, &["draft"]);
    let ready = run_ez_with_fake_gh(&repo, &["ready"]);

    assert_success(&draft);
    assert_success(&ready);
    assert!(stderr_text(&draft).contains("marked as draft"));
    assert!(stderr_text(&ready).contains("marked as ready for review"));
    assert_eq!(gh_log(&repo), "pr ready --undo 42\npr ready 42\n");
}

#[test]
fn draft_and_ready_reject_trunk_unmanaged_and_managed_branch_without_pr() {
    let repo = init_repo("draft-ready-rejections", "", 42);

    run(&repo.path, "git", &["checkout", "main"]);
    let trunk = run_ez_with_fake_gh(&repo, &["draft"]);
    assert_failure(&trunk);
    assert!(stderr_text(&trunk).contains("on trunk"));

    run(&repo.path, "git", &["checkout", "-b", "loose"]);
    let unmanaged = run_ez_with_fake_gh(&repo, &["ready"]);
    assert_failure(&unmanaged);
    assert!(stderr_text(&unmanaged).contains("not tracked by ez"));

    run(&repo.path, "git", &["checkout", "feat/topic"]);
    set_pr_number(&repo.path, "feat/topic", None);
    let no_pr = run_ez_with_fake_gh(&repo, &["draft"]);
    assert_failure(&no_pr);
    assert!(stderr_text(&no_pr).contains("run `ez push`"));

    assert_eq!(gh_log(&repo), "", "rejections must not invoke gh");
}

#[test]
fn pr_link_uses_configured_repo_with_zero_gh_calls() {
    let repo = init_repo("pr-link-configured", "", 42);
    set_configured_repo(&repo.path, "configured/repo");

    let output = run_ez_with_fake_gh(&repo, &["pr-link"]);

    assert_success(&output);
    assert_eq!(
        stdout_text(&output),
        "https://github.com/configured/repo/pull/42"
    );
    assert_eq!(gh_log(&repo), "");
}

#[test]
fn pr_link_discovers_repository_name() {
    let repo = init_repo(
        "pr-link-discovery",
        r#"
if [ "$1" = "repo" ] && [ "$2" = "view" ] && [ "$3" = "--json" ] && [ "$4" = "nameWithOwner" ] && [ "$5" = "-q" ] && [ "$6" = ".nameWithOwner" ]; then
  printf 'org/repo\n'
  exit 0
fi
"#,
        42,
    );

    let output = run_ez_with_fake_gh(&repo, &["pr-link"]);

    assert_success(&output);
    assert_eq!(stdout_text(&output), "https://github.com/org/repo/pull/42");
    assert_eq!(
        gh_log(&repo),
        "repo view --json nameWithOwner -q .nameWithOwner\n"
    );
}

#[test]
fn pr_link_falls_back_to_pr_status_url_when_repository_discovery_fails() {
    let repo = init_repo(
        "pr-link-fallback",
        r#"
if [ "$1" = "repo" ] && [ "$2" = "view" ] && [ "$3" = "--json" ] && [ "$4" = "nameWithOwner" ] && [ "$5" = "-q" ] && [ "$6" = ".nameWithOwner" ]; then
  printf 'repo unavailable\n' >&2
  exit 1
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "42" ] && [ "$4" = "--json" ] && [ "$5" = "number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid" ]; then
  printf '{"number":42,"url":"https://github.com/from/status/pull/42","state":"OPEN","title":"Topic","isDraft":false,"mergedAt":null,"baseRefName":"main"}\n'
  exit 0
fi
"#,
        42,
    );

    let output = run_ez_with_fake_gh(&repo, &["pr-link"]);

    assert_success(&output);
    assert_eq!(
        stdout_text(&output),
        "https://github.com/from/status/pull/42"
    );
    assert_eq!(
        gh_log(&repo),
        "repo view --json nameWithOwner -q .nameWithOwner\npr view 42 --json number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid\n"
    );
}

#[test]
fn pr_link_rejects_missing_pr_without_gh_calls() {
    let repo = init_repo("pr-link-missing-pr", "", 42);
    set_pr_number(&repo.path, "feat/topic", None);

    let output = run_ez_with_fake_gh(&repo, &["pr-link"]);

    assert_failure(&output);
    assert!(stderr_text(&output).contains("run `ez push`"));
    assert_eq!(gh_log(&repo), "");
}

#[test]
fn pr_link_rejects_unmanaged_branch_without_gh_calls() {
    let repo = init_repo("pr-link-unmanaged", "", 42);
    run(&repo.path, "git", &["checkout", "-b", "loose"]);

    let output = run_ez_with_fake_gh(&repo, &["pr-link"]);

    assert_failure(&output);
    assert!(stderr_text(&output).contains("not tracked by ez"));
    assert_eq!(gh_log(&repo), "");
}
