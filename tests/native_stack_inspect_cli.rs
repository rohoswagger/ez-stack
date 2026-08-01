use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct NativeStackRepo {
    path: PathBuf,
    remote: PathBuf,
    fake_bin: PathBuf,
    gh_log: PathBuf,
}

impl Drop for NativeStackRepo {
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

fn run_ez_with_fake_gh(repo: &NativeStackRepo, dir: &Path, args: &[&str]) -> Output {
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

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout JSON")
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

fn expected_worktree_path(repo: &Path, branch: &str) -> PathBuf {
    repo.join(".worktrees").join(branch.replace('/', "-"))
}

fn stack_path(repo: &Path) -> PathBuf {
    repo.join(".git").join("ez").join("stack.json")
}

fn stack_state_bytes(repo: &Path) -> Vec<u8> {
    std::fs::read(stack_path(repo)).expect("read stack state")
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

fn ref_tip(repo: &Path, refspec: &str) -> String {
    git_output(repo, &["rev-parse", refspec])
}

fn gh_log(repo: &NativeStackRepo) -> String {
    std::fs::read_to_string(&repo.gh_log).unwrap_or_default()
}

fn stack_request_lines(repo: &NativeStackRepo) -> Vec<String> {
    gh_log(repo)
        .lines()
        .filter(|line| line.contains("/stacks?pull_request="))
        .map(str::to_string)
        .collect()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn install_fake_gh(
    prefix: &str,
    pr_statuses: &[(&str, u64, &str)],
    stack_response: Option<(u64, Result<Vec<u64>, &'static str>)>,
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
    let stack_case = match stack_response {
        Some((lookup_pr, Ok(prs))) => {
            let pull_requests = prs
                .iter()
                .map(|pr| format!(r#"{{"number":{pr}}}"#))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "if [ \"$1\" = \"api\" ] && [ \"$2\" = \"repos/org/repo/stacks?pull_request={lookup_pr}\" ]; then\n  printf '[{{\"number\":88,\"base\":{{\"ref\":\"main\"}},\"open\":true,\"pull_requests\":[{pull_requests}]}}]\\n'\n  exit 0\nfi\n"
            )
        }
        Some((lookup_pr, Err(message))) => {
            format!(
                "if [ \"$1\" = \"api\" ] && [ \"$2\" = \"repos/org/repo/stacks?pull_request={lookup_pr}\" ]; then\n  printf '{message}\\n' >&2\n  exit 1\nfi\n"
            )
        }
        None => String::new(),
    };
    let script_contents = format!(
        r#"#!/bin/sh
echo "$@" >> "$GH_LOG"
if [ "$1" = "repo" ] && [ "$2" = "view" ] && [ "$3" = "--json" ] && [ "$4" = "nameWithOwner" ] && [ "$5" = "-q" ] && [ "$6" = ".nameWithOwner" ]; then
  printf 'org/repo\n'
  exit 0
fi
{pr_cases}{stack_case}printf 'unexpected gh invocation: %s\n' "$*" >&2
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

fn init_repo(prefix: &str, fake_bin: PathBuf, gh_log: PathBuf) -> NativeStackRepo {
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

    NativeStackRepo {
        path,
        remote,
        fake_bin,
        gh_log,
    }
}

fn add_managed_branch(
    repo: &NativeStackRepo,
    branch: &str,
    parent: &str,
    pr_number: u64,
) -> PathBuf {
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

fn init_two_pr_stack(
    prefix: &str,
    stack_response: Option<(u64, Result<Vec<u64>, &'static str>)>,
) -> (NativeStackRepo, PathBuf, PathBuf) {
    let (fake_bin, gh_log) = install_fake_gh(
        &format!("{prefix}-gh"),
        &[("feat/a", 101, "main"), ("feat/b", 102, "feat/a")],
        stack_response,
    );
    let repo = init_repo(prefix, fake_bin, gh_log);
    let first_worktree = add_managed_branch(&repo, "feat/a", "main", 101);
    let second_worktree = add_managed_branch(&repo, "feat/b", "feat/a", 102);
    assert_eq!(current_branch(&repo.path), "main");
    assert_eq!(current_branch(&first_worktree), "feat/a");
    assert_eq!(current_branch(&second_worktree), "feat/b");
    (repo, first_worktree, second_worktree)
}

#[test]
fn status_default_omits_native_stack_and_never_calls_stack_endpoint() {
    let (fake_bin, gh_log) = install_fake_gh(
        "native-stack-status-default-gh",
        &[("feat/a", 101, "main")],
        None,
    );
    let repo = init_repo("native-stack-status-default", fake_bin, gh_log);
    let worktree = add_managed_branch(&repo, "feat/a", "main", 101);
    assert_eq!(current_branch(&worktree), "feat/a");

    let output = run_ez_with_fake_gh(&repo, &worktree, &["status", "--json"]);

    assert_success(&output);
    let status = stdout_json(&output);
    assert!(status.get("native_stack").is_none());
    assert!(stack_request_lines(&repo).is_empty());
}

#[test]
fn status_native_stack_reports_in_sync_remote_and_local_chain() {
    let (repo, _first_worktree, second_worktree) = init_two_pr_stack(
        "native-stack-status-in-sync",
        Some((102, Ok(vec![101, 102]))),
    );

    let output = run_ez_with_fake_gh(
        &repo,
        &second_worktree,
        &["status", "--json", "--native-stack"],
    );

    assert_success(&output);
    let status = stdout_json(&output);
    let native_stack = &status["native_stack"];
    assert_eq!(native_stack["provider"], "github");
    assert_eq!(native_stack["preview"], true);
    assert_eq!(native_stack["state"], "in_sync");
    assert_eq!(
        native_stack["local"]["branches"],
        serde_json::json!(["feat/a", "feat/b"])
    );
    assert_eq!(
        native_stack["local"]["pull_requests"],
        serde_json::json!([101, 102])
    );
    assert_eq!(
        native_stack["github"],
        serde_json::json!({
            "number": 88,
            "base_ref": "main",
            "open": true,
            "position": 2,
            "size": 2,
            "pull_requests": [101, 102],
        })
    );
    assert_eq!(
        stack_request_lines(&repo),
        vec![
            "api repos/org/repo/stacks?pull_request=102 -H X-GitHub-Api-Version: 2026-03-10 -H Accept: application/vnd.github+json"
        ]
    );
}

#[test]
fn status_native_stack_reports_divergence_without_mutation() {
    let (repo, first_worktree, second_worktree) = init_two_pr_stack(
        "native-stack-status-diverged",
        Some((102, Ok(vec![101, 999]))),
    );
    let stack_before = stack_state_bytes(&repo.path);
    let first_tip_before = ref_tip(&repo.path, "feat/a");
    let second_tip_before = ref_tip(&repo.path, "feat/b");
    let first_porcelain_before = status_porcelain(&first_worktree);
    let second_porcelain_before = status_porcelain(&second_worktree);

    let output = run_ez_with_fake_gh(
        &repo,
        &second_worktree,
        &["status", "--json", "--native-stack"],
    );

    assert_success(&output);
    let status = stdout_json(&output);
    let native_stack = &status["native_stack"];
    assert_eq!(native_stack["state"], "diverged");
    assert_eq!(
        native_stack["local"]["pull_requests"],
        serde_json::json!([101, 102])
    );
    assert_eq!(
        native_stack["github"]["pull_requests"],
        serde_json::json!([101, 999])
    );
    assert!(native_stack["github"]["position"].is_null());
    assert_eq!(stack_state_bytes(&repo.path), stack_before);
    assert_eq!(ref_tip(&repo.path, "feat/a"), first_tip_before);
    assert_eq!(ref_tip(&repo.path, "feat/b"), second_tip_before);
    assert_eq!(status_porcelain(&first_worktree), first_porcelain_before);
    assert_eq!(status_porcelain(&second_worktree), second_porcelain_before);
}

#[test]
fn status_native_stack_reports_preview_unavailable_on_404() {
    let (repo, _first_worktree, second_worktree) = init_two_pr_stack(
        "native-stack-status-unavailable",
        Some((102, Err("HTTP 404: Not Found"))),
    );

    let output = run_ez_with_fake_gh(
        &repo,
        &second_worktree,
        &["status", "--json", "--native-stack"],
    );

    assert_success(&output);
    let status = stdout_json(&output);
    let native_stack = &status["native_stack"];
    assert_eq!(native_stack["state"], "unavailable");
    assert!(native_stack["github"].is_null());
    assert_eq!(
        native_stack["local"]["branches"],
        serde_json::json!(["feat/a", "feat/b"])
    );
    assert_eq!(
        native_stack["local"]["pull_requests"],
        serde_json::json!([101, 102])
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("error:"),
        "stderr should not claim ordinary command failure:\n{stderr}"
    );
}

#[test]
fn log_native_stack_queries_once_per_local_chain_and_reports_each_position() {
    let (repo, _first_worktree, second_worktree) =
        init_two_pr_stack("native-stack-log-in-sync", Some((102, Ok(vec![101, 102]))));

    let output = run_ez_with_fake_gh(
        &repo,
        &second_worktree,
        &["log", "--json", "--native-stack"],
    );

    assert_success(&output);
    let log = stdout_json(&output);
    let entries = log.as_array().expect("log JSON array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["branch"], "feat/a");
    assert_eq!(entries[0]["native_stack"]["state"], "in_sync");
    assert_eq!(entries[0]["native_stack"]["github"]["position"], 1);
    assert_eq!(entries[1]["branch"], "feat/b");
    assert_eq!(entries[1]["native_stack"]["state"], "in_sync");
    assert_eq!(entries[1]["native_stack"]["github"]["position"], 2);
    assert_eq!(
        stack_request_lines(&repo),
        vec![
            "api repos/org/repo/stacks?pull_request=102 -H X-GitHub-Api-Version: 2026-03-10 -H Accept: application/vnd.github+json"
        ]
    );
}
