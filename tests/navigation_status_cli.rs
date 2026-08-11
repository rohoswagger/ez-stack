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
        "ez-navigation-status-{prefix}-{}-{}-{}",
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

fn run_ez_raw(dir: &Path, args: &[&str]) -> Output {
    run_raw(dir, env!("CARGO_BIN_EXE_ez"), args)
}

fn run_ez_with_fake_gh(repo: &TestRepo, dir: &Path, args: &[&str]) -> Output {
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

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "parse stdout JSON: {err}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    stdout_text(&run(dir, "git", args))
}

fn current_branch(dir: &Path) -> String {
    git_output(dir, &["branch", "--show-current"])
}

fn ref_tip(repo: &Path, refspec: &str) -> String {
    git_output(repo, &["rev-parse", refspec])
}

fn canonical_stdout_path(output: &Output) -> PathBuf {
    PathBuf::from(stdout_text(output))
        .canonicalize()
        .expect("canonical stdout path")
}

fn canonical_status_path(status: &Value, key: &str) -> PathBuf {
    PathBuf::from(status[key].as_str().expect(key))
        .canonicalize()
        .expect("canonical status path")
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

fn write_stack_state(repo: &Path, state: &Value) {
    std::fs::write(
        stack_path(repo),
        serde_json::to_vec_pretty(state).expect("serialize stack state"),
    )
    .expect("write stack state");
}

fn set_pr_number(repo: &Path, branch: &str, pr_number: u64) {
    let mut state = stack_state(repo);
    state["branches"][branch]["pr_number"] = Value::from(pr_number);
    write_stack_state(repo, &state);
}

fn set_scope(repo: &Path, branch: &str, scope: &[&str], mode: &str) {
    let mut state = stack_state(repo);
    state["branches"][branch]["scope"] = Value::from(scope.to_vec());
    state["branches"][branch]["scope_mode"] = Value::from(mode);
    write_stack_state(repo, &state);
}

fn make_parent_head_stale(repo: &Path, branch: &str) {
    let mut state = stack_state(repo);
    state["branches"][branch]["parent_head"] =
        Value::from("0000000000000000000000000000000000000000");
    write_stack_state(repo, &state);
}

fn gh_log(repo: &TestRepo) -> String {
    std::fs::read_to_string(&repo.gh_log).unwrap_or_default()
}

fn native_stack_request_lines(repo: &TestRepo) -> Vec<String> {
    gh_log(repo)
        .lines()
        .filter(|line| line.contains("/stacks?pull_request="))
        .map(str::to_string)
        .collect()
}

fn clear_gh_log(repo: &TestRepo) {
    std::fs::write(&repo.gh_log, "").expect("clear gh log");
}

fn install_fake_gh(prefix: &str, body: &str) -> (PathBuf, PathBuf) {
    let fake_bin = temp_dir(prefix);
    let gh_log = fake_bin.join("gh.log");
    let script = fake_bin.join("gh");
    let script_contents = format!(
        r#"#!/bin/sh
echo "$@" >> "$GH_LOG"
{body}
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

fn init_repo(prefix: &str, fake_gh_body: &str) -> TestRepo {
    let (fake_bin, gh_log) = install_fake_gh(&format!("{prefix}-gh"), fake_gh_body);
    let path = temp_dir(prefix);
    let remote = temp_dir(&format!("{prefix}-remote"));
    run(&remote, "git", &["init", "--bare"]);
    run(&path, "git", &["init", "-b", "main"]);
    run(&path, "git", &["config", "user.name", "Test User"]);
    run(&path, "git", &["config", "user.email", "test@example.com"]);
    std::fs::write(path.join("tracked.txt"), "initial\n").expect("write tracked file");
    run(&path, "git", &["add", "tracked.txt"]);
    run(&path, "git", &["commit", "-m", "initial"]);
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
    TestRepo {
        path,
        remote,
        fake_bin,
        gh_log,
    }
}

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(dir.join(file), contents).expect("write file");
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
}

fn add_managed_branch(repo: &TestRepo, branch: &str, parent: &str) -> PathBuf {
    run_ez(
        &repo.path,
        &["create", branch, "--from", parent, "--no-worktree"],
    );
    run(&repo.path, "git", &["checkout", branch]);
    let file = format!("{}.txt", branch.replace('/', "-"));
    commit_file(&repo.path, &file, &format!("{branch}\n"), branch);
    run(&repo.path, "git", &["push", "-u", "origin", branch]);
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

fn init_linear_stack(prefix: &str) -> (TestRepo, PathBuf, PathBuf, PathBuf) {
    let repo = init_repo(prefix, "exit 97");
    let base = add_managed_branch(&repo, "feat/base", "main");
    let middle = add_managed_branch(&repo, "feat/middle", "feat/base");
    let top = add_managed_branch(&repo, "feat/top", "feat/middle");
    assert_eq!(current_branch(&repo.path), "main");
    assert_eq!(current_branch(&base), "feat/base");
    assert_eq!(current_branch(&middle), "feat/middle");
    assert_eq!(current_branch(&top), "feat/top");
    (repo, base, middle, top)
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_exit_code(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected exit status\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn up_from_trunk_prints_the_child_worktree_path() {
    let (repo, base, _middle, _top) = init_linear_stack("up-from-trunk");

    let output = run_ez(&repo.path, &["up"]);

    assert_eq!(
        canonical_stdout_path(&output),
        base.canonicalize().expect("base path")
    );
    assert_eq!(current_branch(&repo.path), "main");
}

#[test]
fn down_from_linked_child_prints_the_parent_worktree_path() {
    let (_repo, base, middle, _top) = init_linear_stack("down-from-linked-child");

    let output = run_ez(&middle, &["down"]);

    assert_eq!(
        canonical_stdout_path(&output),
        base.canonicalize().expect("base path")
    );
    assert_eq!(current_branch(&middle), "feat/middle");
}

#[test]
fn top_from_middle_prints_the_top_worktree_path() {
    let (_repo, _base, middle, top) = init_linear_stack("top-from-middle");

    let output = run_ez(&middle, &["top"]);

    assert_eq!(
        canonical_stdout_path(&output),
        top.canonicalize().expect("top path")
    );
}

#[test]
fn bottom_from_top_prints_the_bottom_branch_worktree_path() {
    let (_repo, base, _middle, top) = init_linear_stack("bottom-from-top");

    let output = run_ez(&top, &["bottom"]);

    assert_eq!(
        canonical_stdout_path(&output),
        base.canonicalize().expect("base path")
    );
}

#[test]
fn switch_by_pr_number_prints_the_matching_worktree_path() {
    let (repo, _base, _middle, top) = init_linear_stack("switch-by-pr");
    set_pr_number(&repo.path, "feat/top", 303);

    let output = run_ez_raw(&repo.path, &["switch", "303", "--no-cd-required"]);

    assert_exit_code(&output, 0);
    assert_eq!(
        canonical_stdout_path(&output),
        top.canonicalize().expect("top path")
    );
}

#[test]
fn up_without_tty_rejects_ambiguous_children() {
    let repo = init_repo("up-ambiguous", "exit 97");
    add_managed_branch(&repo, "feat/alpha", "main");
    add_managed_branch(&repo, "feat/beta", "main");

    let output = run_ez_raw(&repo.path, &["up"]);

    assert_exit_code(&output, 5);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("multiple child branches"), "{stderr}");
    assert!(stderr.contains("feat/alpha, feat/beta"), "{stderr}");
}

#[test]
fn up_rejects_explicit_branch_that_is_not_a_child() {
    let (repo, _base, _middle, _top) = init_linear_stack("up-wrong-child");

    let output = run_ez_raw(&repo.path, &["up", "feat/top"]);

    assert_exit_code(&output, 5);
    assert!(stderr_text(&output).contains("is not a child branch of `main`"));
}

#[test]
fn up_rejects_pr_number_that_is_not_a_child() {
    let (repo, _base, _middle, _top) = init_linear_stack("up-wrong-pr");
    set_pr_number(&repo.path, "feat/top", 303);

    let output = run_ez_raw(&repo.path, &["up", "303"]);

    assert_exit_code(&output, 5);
    assert!(stderr_text(&output).contains("no child of `main` has PR #303"));
}

#[test]
fn down_rejects_explicit_branch_that_is_not_the_parent() {
    let (_repo, _base, middle, _top) = init_linear_stack("down-wrong-parent");

    let output = run_ez_raw(&middle, &["down", "main"]);

    assert_exit_code(&output, 5);
    assert!(stderr_text(&output).contains("is not the stack parent of `feat/middle`"));
}

#[test]
fn top_rejects_current_top_branch() {
    let (_repo, _base, _middle, top) = init_linear_stack("top-already-top");

    let output = run_ez_raw(&top, &["top"]);

    assert_exit_code(&output, 5);
    assert!(stderr_text(&output).contains("already at the top"));
}

#[test]
fn bottom_rejects_trunk_without_children() {
    let repo = init_repo("bottom-no-children", "exit 97");

    let output = run_ez_raw(&repo.path, &["bottom"]);

    assert_exit_code(&output, 5);
    assert!(stderr_text(&output).contains("already at the bottom"));
}

#[test]
fn status_json_on_trunk_reports_children_and_worktree_counts() {
    let (repo, _base, _middle, _top) = init_linear_stack("status-trunk");
    std::fs::write(repo.path.join("untracked.txt"), "untracked\n").expect("write untracked");

    let status = stdout_json(&run_ez(&repo.path, &["status", "--json"]));

    assert_eq!(status["branch"], "main");
    assert!(status["parent"].is_null());
    assert_eq!(status["children"], serde_json::json!(["feat/base"]));
    assert_eq!(status["depth"], 0);
    assert_eq!(status["untracked_files"], 2);
    assert_eq!(status["in_linked_worktree"], false);
    assert_eq!(
        canonical_status_path(&status, "active_edit_root"),
        repo.path.canonicalize().expect("repo path")
    );
}

#[test]
fn status_json_on_managed_branch_reports_scope_pr_and_dirty_state() {
    let pr_body = r#"
if [ "$1" = "repo" ] && [ "$2" = "view" ] && [ "$3" = "--json" ] && [ "$4" = "nameWithOwner" ] && [ "$5" = "-q" ] && [ "$6" = ".nameWithOwner" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "42" ]; then
  printf '{"number":42,"url":"https://github.com/org/repo/pull/42","state":"CLOSED","title":"finished topic","isDraft":true,"mergedAt":null,"baseRefName":"main"}\n'
  exit 0
fi
"#;
    let repo = init_repo("status-managed", pr_body);
    let worktree = add_managed_branch(&repo, "feat/topic", "main");
    set_pr_number(&repo.path, "feat/topic", 42);
    set_scope(&repo.path, "feat/topic", &["src/**", "tests/**"], "strict");
    std::fs::write(worktree.join("staged.txt"), "staged\n").expect("write staged");
    run(&worktree, "git", &["add", "staged.txt"]);
    std::fs::write(worktree.join("feat-topic.txt"), "modified\n").expect("modify tracked");
    std::fs::write(worktree.join("untracked.txt"), "untracked\n").expect("write untracked");

    let status = stdout_json(&run_ez_with_fake_gh(
        &repo,
        &worktree,
        &["status", "--json"],
    ));

    assert_eq!(status["branch"], "feat/topic");
    assert_eq!(status["parent"], "main");
    assert_eq!(status["pr_number"], 42);
    assert_eq!(status["pr_url"], "https://github.com/org/repo/pull/42");
    assert_eq!(status["pr_state"], "CLOSED");
    assert_eq!(status["is_draft"], true);
    assert_eq!(status["scope"], serde_json::json!(["src/**", "tests/**"]));
    assert_eq!(status["scope_mode"], "strict");
    assert_eq!(status["scope_defined"], true);
    assert_eq!(status["staged_files"], 1);
    assert_eq!(status["modified_files"], 1);
    assert_eq!(status["untracked_files"], 1);
    assert_eq!(status["in_linked_worktree"], true);
    assert_eq!(
        canonical_status_path(&status, "active_edit_root"),
        worktree.canonicalize().expect("worktree path")
    );
    assert_eq!(
        gh_log(&repo).lines().collect::<Vec<_>>(),
        vec![
            "pr view 42 --json number,url,state,title,isDraft,mergedAt,baseRefName,headRefOid",
            "repo view --json nameWithOwner -q .nameWithOwner",
        ]
    );
}

#[test]
fn status_json_defaults_open_pr_state_when_github_lookup_fails() {
    let repo = init_repo("status-pr-fallback", "exit 1");
    let worktree = add_managed_branch(&repo, "feat/topic", "main");
    set_pr_number(&repo.path, "feat/topic", 42);

    let status = stdout_json(&run_ez_with_fake_gh(
        &repo,
        &worktree,
        &["status", "--json"],
    ));

    assert_eq!(status["pr_number"], 42);
    assert_eq!(status["pr_state"], "OPEN");
    assert_eq!(status["is_draft"], false);
}

#[test]
fn status_json_reports_warn_scope_mode() {
    let (repo, base, _middle, _top) = init_linear_stack("status-json-warn-scope");
    set_scope(&repo.path, "feat/base", &["src/**"], "warn");

    let status = stdout_json(&run_ez(&base, &["status", "--json"]));

    assert_eq!(status["scope_mode"], "warn");
}

#[test]
fn status_json_marks_branch_as_needing_restack_when_the_parent_moved() {
    let (repo, base, _middle, _top) = init_linear_stack("status-needs-restack");
    commit_file(&repo.path, "trunk.txt", "trunk\n", "advance trunk");

    let status = stdout_json(&run_ez(&base, &["status", "--json"]));

    assert_eq!(status["branch"], "feat/base");
    assert_eq!(status["needs_restack"], true);
}

#[test]
fn status_json_ignores_a_stale_parent_head_when_git_says_the_branch_is_restacked() {
    let (repo, base, _middle, _top) = init_linear_stack("status-stale-metadata-only");
    // Metadata is a cache of git, not the source of truth: `parent_head` goes stale whenever
    // history moves outside ez (a hand-rolled `git rebase`), and that alone is not a restack.
    make_parent_head_stale(&repo.path, "feat/base");

    let status = stdout_json(&run_ez(&base, &["status", "--json"]));

    assert_eq!(status["branch"], "feat/base");
    assert_eq!(status["needs_restack"], false);
}

#[test]
fn status_human_reports_scope_pr_fallback_commits_and_dirty_state() {
    let repo = init_repo("status-human", "exit 1");
    let worktree = add_managed_branch(&repo, "feat/topic", "main");
    set_pr_number(&repo.path, "feat/topic", 42);
    set_scope(&repo.path, "feat/topic", &["src/**"], "warn");
    std::fs::write(worktree.join("untracked.txt"), "untracked\n").expect("write untracked");

    let output = run_ez_with_fake_gh(&repo, &worktree, &["status"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("Branch:"), "{stderr}");
    assert!(stderr.contains("feat/topic"), "{stderr}");
    assert!(stderr.contains("Parent:"), "{stderr}");
    assert!(stderr.contains("main"), "{stderr}");
    assert!(stderr.contains("Scope: warn (1 pattern(s))"), "{stderr}");
    assert!(stderr.contains("src/**"), "{stderr}");
    assert!(stderr.contains("PR:"), "{stderr}");
    assert!(stderr.contains("#42"), "{stderr}");
    assert!(stderr.contains("Stack position: 1 deep"), "{stderr}");
    assert!(stderr.contains("Commits: 1 commit"), "{stderr}");
    assert!(stderr.contains("Working tree: 1 untracked"), "{stderr}");
}

#[test]
fn status_human_reports_successful_pr_lookup() {
    let pr_body = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ] && [ "$3" = "42" ]; then
  printf '{"number":42,"url":"https://github.com/org/repo/pull/42","state":"OPEN","title":"ready topic","isDraft":false,"mergedAt":null,"baseRefName":"main"}\n'
  exit 0
fi
"#;
    let repo = init_repo("status-human-pr-success", pr_body);
    let worktree = add_managed_branch(&repo, "feat/topic", "main");
    set_pr_number(&repo.path, "feat/topic", 42);

    let output = run_ez_with_fake_gh(&repo, &worktree, &["status"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("PR:"), "{stderr}");
    assert!(stderr.contains("#42"), "{stderr}");
    assert!(stderr.contains("OPEN"), "{stderr}");
    assert!(stderr.contains("ready topic"), "{stderr}");
    assert!(
        stderr.contains("https://github.com/org/repo/pull/42"),
        "{stderr}"
    );
}

#[test]
fn status_human_reports_clean_prless_branch_without_commits() {
    let repo = init_repo("status-human-prless-clean", "exit 97");
    run_ez(
        &repo.path,
        &["create", "feat/empty", "--from", "main", "--no-worktree"],
    );
    let worktree = expected_worktree_path(&repo.path, "feat/empty");
    run(
        &repo.path,
        "git",
        &[
            "worktree",
            "add",
            worktree.to_str().expect("worktree path"),
            "feat/empty",
        ],
    );

    let output = run_ez(&worktree, &["status"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("PR: not yet created"), "{stderr}");
    assert!(stderr.contains("Run `ez submit`"), "{stderr}");
    assert!(stderr.contains("Commits: none"), "{stderr}");
    assert!(stderr.contains("Working tree: clean"), "{stderr}");
}

#[test]
fn status_human_reports_plural_commits_and_all_dirty_categories() {
    let repo = init_repo("status-human-plural-dirty", "exit 97");
    let worktree = add_managed_branch(&repo, "feat/topic", "main");
    commit_file(&worktree, "second.txt", "second\n", "second");
    std::fs::write(worktree.join("staged.txt"), "staged\n").expect("write staged");
    run(&worktree, "git", &["add", "staged.txt"]);
    std::fs::write(worktree.join("feat-topic.txt"), "modified\n").expect("modify tracked");
    std::fs::write(worktree.join("untracked.txt"), "untracked\n").expect("write untracked");

    let output = run_ez(&worktree, &["status"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("Commits: 2 commits"), "{stderr}");
    assert!(
        stderr.contains("Working tree: 1 staged, 1 modified, 1 untracked"),
        "{stderr}"
    );
}

#[test]
fn status_human_warns_when_the_parent_moved() {
    let (repo, base, _middle, _top) = init_linear_stack("status-human-stale");
    commit_file(&repo.path, "trunk.txt", "trunk\n", "advance trunk");

    let output = run_ez(&base, &["status"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("Branch may need restacking"), "{stderr}");
    assert!(stderr.contains("Run `ez restack`"), "{stderr}");
}

#[test]
fn status_human_stays_quiet_when_only_the_parent_head_cache_is_stale() {
    let (repo, base, _middle, _top) = init_linear_stack("status-human-stale-cache-only");
    make_parent_head_stale(&repo.path, "feat/base");

    let output = run_ez(&base, &["status"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(!stderr.contains("Branch may need restacking"), "{stderr}");
}

#[test]
fn status_human_on_trunk_reports_direct_children_and_dirty_state() {
    let (repo, _base, _middle, _top) = init_linear_stack("status-human-trunk");
    std::fs::write(repo.path.join("untracked.txt"), "untracked\n").expect("write untracked");

    let output = run_ez(&repo.path, &["status"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("On trunk branch:"), "{stderr}");
    assert!(stderr.contains("main"), "{stderr}");
    assert!(stderr.contains("1 stacked branch(es):"), "{stderr}");
    assert!(stderr.contains("feat/base"), "{stderr}");
    assert!(stderr.contains("Working tree: 2 untracked"), "{stderr}");
}

#[test]
fn status_human_on_empty_trunk_reports_create_hint() {
    let repo = init_repo("status-human-empty-trunk", "exit 97");

    let output = run_ez(&repo.path, &["status"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("No stacked branches yet."), "{stderr}");
    assert!(stderr.contains("Run `ez create <name>`"), "{stderr}");
}

#[test]
fn status_human_on_trunk_reports_staged_and_modified_counts() {
    let repo = init_repo("status-human-trunk-dirty", "exit 97");
    std::fs::write(repo.path.join("staged.txt"), "staged\n").expect("write staged");
    run(&repo.path, "git", &["add", "staged.txt"]);
    std::fs::write(repo.path.join("tracked.txt"), "modified\n").expect("modify tracked");

    let output = run_ez(&repo.path, &["status"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("Working tree: 1 staged, 1 modified"),
        "{stderr}"
    );
}

#[test]
fn status_human_on_unmanaged_branch_exits_successfully_with_hint() {
    let repo = init_repo("status-unmanaged", "exit 97");
    run(&repo.path, "git", &["checkout", "-b", "scratch"]);

    let output = run_ez_raw(&repo.path, &["status"]);

    assert_exit_code(&output, 0);
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("Branch `scratch` is not tracked by ez."),
        "{stderr}"
    );
    assert!(stderr.contains("Run `ez create <name>`"), "{stderr}");
}

#[test]
fn status_json_on_unmanaged_branch_fails() {
    let repo = init_repo("status-json-unmanaged", "exit 97");
    run(&repo.path, "git", &["checkout", "-b", "scratch"]);

    let output = run_ez_raw(&repo.path, &["status", "--json"]);

    assert_exit_code(&output, 5);
    let stderr = stderr_text(&output);
    assert!(stderr.contains("scratch"), "{stderr}");
    assert!(stderr.contains("not tracked by ez"), "{stderr}");
}

#[test]
fn status_native_stack_on_trunk_does_not_call_github() {
    let (repo, _base, _middle, _top) = init_linear_stack("status-native-trunk-zero-gh");
    clear_gh_log(&repo);

    let status = stdout_json(&run_ez_with_fake_gh(
        &repo,
        &repo.path,
        &["status", "--json", "--native-stack"],
    ));

    assert_eq!(status["native_stack"]["state"], "not_applicable");
    assert_eq!(native_stack_request_lines(&repo), Vec::<String>::new());
}

#[test]
fn status_native_stack_without_pr_does_not_call_github() {
    let (repo, base, _middle, _top) = init_linear_stack("status-native-no-pr-zero-gh");
    clear_gh_log(&repo);

    let status = stdout_json(&run_ez_with_fake_gh(
        &repo,
        &base,
        &["status", "--json", "--native-stack"],
    ));

    assert_eq!(status["native_stack"]["state"], "not_applicable");
    assert_eq!(
        status["native_stack"]["local"]["branches"],
        serde_json::json!(["feat/base"])
    );
    assert_eq!(
        status["native_stack"]["local"]["pull_requests"],
        serde_json::json!([])
    );
    assert_eq!(native_stack_request_lines(&repo), Vec::<String>::new());
}

#[test]
fn status_native_stack_in_fork_mode_does_not_call_github() {
    let (repo, base, _middle, _top) = init_linear_stack("status-native-fork-zero-gh");
    set_pr_number(&repo.path, "feat/base", 101);
    let mut state = stack_state(&repo.path);
    state["repo"] = Value::from("upstream/repo");
    state["fork_repo"] = Value::from("fork/repo");
    write_stack_state(&repo.path, &state);
    clear_gh_log(&repo);

    let status = stdout_json(&run_ez_with_fake_gh(
        &repo,
        &base,
        &["status", "--json", "--native-stack"],
    ));

    assert_eq!(status["native_stack"]["state"], "not_applicable");
    assert_eq!(
        status["native_stack"]["local"]["pull_requests"],
        serde_json::json!([101])
    );
    assert_eq!(native_stack_request_lines(&repo), Vec::<String>::new());
}

#[test]
fn status_human_native_stack_reports_not_applicable_without_github() {
    let (repo, base, _middle, _top) = init_linear_stack("status-human-native-zero-gh");
    clear_gh_log(&repo);

    let output = run_ez_with_fake_gh(&repo, &base, &["status", "--native-stack"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("Native stack: GitHub native stack does not apply to this branch"),
        "{stderr}"
    );
    assert_eq!(gh_log(&repo), "");
}

#[test]
fn status_json_preserves_scope_fields_when_scope_is_absent() {
    let (_repo, base, _middle, _top) = init_linear_stack("status-no-scope");

    let status = stdout_json(&run_ez(&base, &["status", "--json"]));

    assert!(status["scope"].is_null());
    assert!(status["scope_mode"].is_null());
    assert_eq!(status["scope_defined"], false);
}

#[test]
fn status_json_reports_commit_count_for_managed_branch() {
    let (repo, base, _middle, _top) = init_linear_stack("status-commit-count");

    let status = stdout_json(&run_ez(&base, &["status", "--json"]));

    assert_eq!(status["commits"], 1);
    assert_eq!(ref_tip(&repo.path, "feat/base"), ref_tip(&base, "HEAD"));
}
