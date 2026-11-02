use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

const HANG_TIMEOUT: Duration = Duration::from_secs(20);

struct Repo {
    path: PathBuf,
    remote: PathBuf,
    fake_bin: PathBuf,
    gh_log: PathBuf,
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = std::fs::remove_dir_all(&self.remote);
        let _ = std::fs::remove_dir_all(&self.fake_bin);
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ez-audit-{prefix}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).expect("create temp directory");
    std::fs::canonicalize(&path).expect("canonicalize temp dir")
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run command");
    assert!(
        output.status.success(),
        "{program} {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&run(dir, "git", args).stdout)
        .trim()
        .to_string()
}

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(dir.join(file), contents).expect("write file");
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
}

fn ez_command(repo: &Repo, dir: &Path, args: &[&str]) -> Command {
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
    cmd
}

/// Run ez the way an agent harness does: stdin is a pipe that is never written
/// to or closed, and stdout/stderr are pipes read until EOF. A hang (in ez, or
/// in a child that keeps the pipes open) fails the test instead of blocking.
fn run_ez_agent(repo: &Repo, dir: &Path, args: &[&str]) -> Output {
    run_agent_command(repo, ez_command(repo, dir, args), args)
}

fn run_agent_command(repo: &Repo, mut cmd: Command, args: &[&str]) -> Output {
    let mut child: Child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ez");
    let stdin_keepalive = child.stdin.take();
    let mut stdout = child.stdout.take().expect("stdout");
    let mut stderr = child.stderr.take().expect("stderr");
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if start.elapsed() > HANG_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            drop(stdin_keepalive);
            let stderr =
                String::from_utf8_lossy(&err_handle.join().unwrap_or_default()).to_string();
            panic!(
                "ez {args:?} did not exit within {HANG_TIMEOUT:?} with an open stdin pipe\n\
                 stderr:\n{stderr}\ngh log:\n{}",
                gh_log(repo)
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    drop(stdin_keepalive);
    Output {
        status,
        stdout: out_handle.join().unwrap_or_default(),
        stderr: err_handle.join().unwrap_or_default(),
    }
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "unexpected exit\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        stderr_text(output)
    );
}

fn gh_log(repo: &Repo) -> String {
    std::fs::read_to_string(&repo.gh_log).unwrap_or_default()
}

/// Every git/gh child (run by ez itself, not by the test setup) that could
/// wait on a human: ez's stdin pipe attached, gh prompts enabled, or git
/// allowed to prompt on the terminal.
fn stdin_inheritors(repo: &Repo) -> String {
    std::fs::read_to_string(repo.fake_bin.join("gh.log.stdin")).unwrap_or_default()
}

fn stack_state(repo: &Path) -> Value {
    let common = PathBuf::from(git(repo, &["rev-parse", "--git-common-dir"]));
    let common = if common.is_absolute() {
        common
    } else {
        repo.join(common)
    };
    serde_json::from_slice(&std::fs::read(common.join("ez/stack.json")).expect("read stack"))
        .expect("parse stack")
}

/// `gh_body` is shell matched against "$@"; anything it does not handle blocks
/// on stdin (like a gh prompt would) and then fails.
fn init_repo(prefix: &str, gh_body: &str) -> Repo {
    let path = temp_dir(prefix);
    let remote = temp_dir(&format!("{prefix}-remote"));
    let fake_bin = temp_dir(&format!("{prefix}-bin"));
    let gh_log = fake_bin.join("gh.log");
    let gh = fake_bin.join("gh");
    std::fs::write(
        &gh,
        format!(
            r#"#!/bin/sh
echo "$@" >> "$GH_LOG"
if [ -p /dev/stdin ]; then echo "stdin: gh $*" >> "$GH_LOG.stdin"; fi
if [ "$GH_PROMPT_DISABLED" != "1" ]; then echo "prompts: gh $*" >> "$GH_LOG.stdin"; fi
if [ "$GIT_TERMINAL_PROMPT" != "0" ]; then echo "git-prompt: gh $*" >> "$GH_LOG.stdin"; fi
{gh_body}
cat >/dev/null
printf 'unexpected gh invocation: %s\n' "$*" >&2
exit 97
"#
        ),
    )
    .expect("write fake gh");
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .expect("locate git");
    let real_git = String::from_utf8_lossy(&real_git.stdout).trim().to_string();
    let fake_git = fake_bin.join("git");
    std::fs::write(
        &fake_git,
        format!(
            "#!/bin/sh\nif [ -p /dev/stdin ]; then echo \"stdin: git $*\" >> \"$GH_LOG.stdin\"; fi\nif [ \"$GIT_TERMINAL_PROMPT\" != \"0\" ]; then echo \"git-prompt: git $*\" >> \"$GH_LOG.stdin\"; fi\nexec {real_git} \"$@\"\n"
        ),
    )
    .expect("write fake git");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for script in [&gh, &fake_git] {
            std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
    }

    run(&remote, "git", &["init", "--bare", "-b", "main"]);
    run(&path, "git", &["init", "-b", "main"]);
    run(&path, "git", &["config", "user.name", "Test User"]);
    run(&path, "git", &["config", "user.email", "test@example.com"]);
    commit_file(&path, "base.txt", "base\n", "initial");
    run(
        &path,
        "git",
        &["remote", "add", "origin", remote.to_str().expect("remote")],
    );
    run(&path, "git", &["push", "-u", "origin", "main"]);
    run(&path, env!("CARGO_BIN_EXE_ez"), &["init", "--yes"]);
    Repo {
        path,
        remote,
        fake_bin,
        gh_log,
    }
}

/// Create `branch` on `parent` in its own linked worktree with one commit.
fn add_branch(repo: &Repo, branch: &str, parent: &str) -> PathBuf {
    run(
        &repo.path,
        env!("CARGO_BIN_EXE_ez"),
        &["create", branch, "--from", parent, "--no-worktree"],
    );
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
    let file = format!("{}.txt", branch.replace('/', "-"));
    commit_file(
        &worktree,
        &file,
        &format!("{branch}\n"),
        &format!("add {branch}"),
    );
    worktree
}

const PR_VIEW_NONE: &str = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  printf 'no pull requests found for branch\n' >&2
  exit 1
fi
"#;

#[test]
fn push_exits_with_open_stdin_pipe_inside_worktree() {
    let gh = format!(
        r#"{PR_VIEW_NONE}
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  printf 'https://github.com/org/repo/pull/11\n'
  exit 0
fi
"#
    );
    let repo = init_repo("push-open-stdin", &gh);
    let worktree = add_branch(&repo, "feat/a", "main");

    let output = run_ez_agent(&repo, &worktree, &["push"]);

    assert_exit(&output, 0);
    assert_eq!(stdin_inheritors(&repo), "", "children inherited ez's stdin");
    assert!(stderr_text(&output).contains("PR: https://github.com/org/repo/pull/11"));
    assert_eq!(
        stack_state(&repo.path)["branches"]["feat/a"]["pr_number"],
        Value::from(11)
    );
}

fn write_stack(repo: &Path, state: &Value) {
    std::fs::write(
        repo.join(".git/ez/stack.json"),
        serde_json::to_vec_pretty(state).expect("serialize"),
    )
    .expect("write stack");
}

fn set_pr_number(repo: &Path, branch: &str, number: u64) {
    let mut state = stack_state(repo);
    state["branches"][branch]["pr_number"] = Value::from(number);
    write_stack(repo, &state);
}

fn write_executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

#[test]
fn submit_three_deep_stack_from_worktree_exits_and_links_ancestors() {
    let gh = format!(
        r#"{PR_VIEW_NONE}
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  case "$*" in
    *"--head feat/one"*) printf 'https://github.com/org/repo/pull/21\n'; exit 0 ;;
    *"--head feat/two"*) printf 'https://github.com/org/repo/pull/22\n'; exit 0 ;;
    *"--head feat/three"*) printf 'https://github.com/org/repo/pull/23\n'; exit 0 ;;
  esac
fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ]; then exit 0; fi
if [ "$1" = "api" ]; then exit 1; fi
"#
    );
    let repo = init_repo("submit-three-deep", &gh);
    add_branch(&repo, "feat/one", "main");
    add_branch(&repo, "feat/two", "feat/one");
    let top = add_branch(&repo, "feat/three", "feat/two");
    let mut state = stack_state(&repo.path);
    state["repo"] = Value::from("org/repo");
    write_stack(&repo.path, &state);

    let output = run_ez_agent(&repo, &top, &["submit"]);

    assert_exit(&output, 0);
    assert_eq!(stdin_inheritors(&repo), "", "children inherited ez's stdin");
    let state = stack_state(&repo.path);
    for (branch, number) in [("feat/one", 21), ("feat/two", 22), ("feat/three", 23)] {
        assert_eq!(state["branches"][branch]["pr_number"], Value::from(number));
    }
    let log = gh_log(&repo);
    let create_three = log
        .split("pr create")
        .find(|chunk| chunk.contains("--head feat/three"))
        .expect("feat/three PR created");
    assert!(
        create_three.contains("1. [feat/one #21](https://github.com/org/repo/pull/21)")
            && create_three.contains("2. [feat/two #22](https://github.com/org/repo/pull/22)"),
        "top PR body must list both ancestors trunk-first:\n{log}"
    );
}

#[test]
fn sync_fetch_does_not_hand_ez_stdin_to_git() {
    let repo = init_repo("sync-open-stdin", "exit 1");
    let worktree = add_branch(&repo, "feat/a", "main");

    let _ = run_ez_agent(&repo, &worktree, &["sync"]);

    let git_inheritors: Vec<_> = stdin_inheritors(&repo)
        .lines()
        .filter(|line| line.contains(": git "))
        .map(str::to_string)
        .collect();
    assert!(git_inheritors.is_empty(), "{git_inheritors:#?}");
}

#[test]
fn push_reports_gh_failure_as_exit_2() {
    let gh = r#"
if [ "$1" = "pr" ]; then
  printf 'HTTP 401: Bad credentials\n' >&2
  exit 1
fi
"#;
    let repo = init_repo("push-gh-failure", gh);
    let worktree = add_branch(&repo, "feat/a", "main");

    let output = run_ez_agent(&repo, &worktree, &["push"]);

    assert_exit(&output, 2);
}

#[test]
fn push_retargets_existing_pr_base_to_stack_parent() {
    let gh = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  printf '{"number":31,"url":"https://github.com/org/repo/pull/31","state":"OPEN","title":"t","isDraft":false,"mergedAt":null,"baseRefName":"main","headRefOid":""}\n'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ] && [ "$3" = "31" ] && [ "$4" = "--base" ] && [ "$5" = "feat/one" ]; then exit 0; fi
"#;
    let repo = init_repo("push-base-mismatch", gh);
    add_branch(&repo, "feat/one", "main");
    let two = add_branch(&repo, "feat/two", "feat/one");
    set_pr_number(&repo.path, "feat/two", 31);

    let output = run_ez_agent(&repo, &two, &["push"]);

    assert_exit(&output, 0);
    assert!(gh_log(&repo).contains("pr edit 31 --base feat/one"));
    assert!(stderr_text(&output).contains("Updated PR #31 base to `feat/one`"));
}

#[test]
fn pr_edit_without_editor_or_terminal_fails_fast_instead_of_hanging() {
    let gh = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then printf 'old body\n'; exit 0; fi
"#;
    let repo = init_repo("pr-edit-no-tty", gh);
    let worktree = add_branch(&repo, "feat/a", "main");
    set_pr_number(&repo.path, "feat/a", 41);
    // Stands in for `vi` reading a stdin that is not a terminal.
    write_executable(&repo.fake_bin.join("vi"), "#!/bin/sh\ncat >/dev/null\n");

    let mut cmd = ez_command(&repo, &worktree, &["pr-edit"]);
    cmd.env_remove("EDITOR").env_remove("VISUAL");
    let output = run_agent_command(&repo, cmd, &["pr-edit"]);

    assert_exit(&output, 5);
    assert!(stderr_text(&output).contains("--body"));
    assert!(!gh_log(&repo).contains("pr edit"));
}

#[test]
fn pr_edit_without_pr_is_a_usage_error() {
    let repo = init_repo("pr-edit-no-pr", "");
    let worktree = add_branch(&repo, "feat/a", "main");

    let output = run_ez_agent(&repo, &worktree, &["pr-edit", "--title", "x"]);

    assert_exit(&output, 5);
    assert!(stderr_text(&output).contains("run `ez push`"));
}

#[test]
fn pr_view_does_not_claim_success_when_gh_fails() {
    let gh = r#"
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then printf 'HTTP 404\n' >&2; exit 1; fi
"#;
    let repo = init_repo("pr-view-gh-fail", gh);
    let worktree = add_branch(&repo, "feat/a", "main");
    set_pr_number(&repo.path, "feat/a", 51);

    let output = run_ez_agent(&repo, &worktree, &["pr"]);

    assert_exit(&output, 2);
    assert!(!stderr_text(&output).contains("Opened PR"));
}

#[test]
fn draft_ready_and_pr_link_work_from_worktree() {
    let gh = r#"
if [ "$1" = "pr" ] && [ "$2" = "ready" ]; then exit 0; fi
"#;
    let repo = init_repo("draft-ready-link", gh);
    let worktree = add_branch(&repo, "feat/a", "main");
    set_pr_number(&repo.path, "feat/a", 61);
    let mut state = stack_state(&repo.path);
    state["repo"] = Value::from("org/repo");
    write_stack(&repo.path, &state);

    assert_exit(&run_ez_agent(&repo, &worktree, &["draft"]), 0);
    assert_exit(&run_ez_agent(&repo, &worktree, &["ready"]), 0);
    let link = run_ez_agent(&repo, &worktree, &["pr-link"]);
    assert_exit(&link, 0);
    assert_eq!(
        String::from_utf8_lossy(&link.stdout).trim(),
        "https://github.com/org/repo/pull/61"
    );
    let log = gh_log(&repo);
    assert!(
        log.contains("pr ready --undo 61 --repo org/repo")
            && log.contains("pr ready 61 --repo org/repo"),
        "{log}"
    );
}
