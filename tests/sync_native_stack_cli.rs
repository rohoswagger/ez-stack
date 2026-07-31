use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_REPO_ID: AtomicU64 = AtomicU64::new(1);

struct SyncRepo {
    path: PathBuf,
    remote: PathBuf,
    fake_bin: PathBuf,
    gh_log: PathBuf,
    payload: PathBuf,
}

impl Drop for SyncRepo {
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
        NEXT_REPO_ID.fetch_add(1, Ordering::Relaxed)
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

fn git_output(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&run(dir, "git", args).stdout)
        .trim()
        .to_string()
}

fn write_file(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).expect("write file");
}

fn init_linear_sync_repo() -> SyncRepo {
    let path = temp_dir("sync-native-repo");
    let remote = temp_dir("sync-native-remote").join("origin.git");
    std::fs::create_dir_all(&remote).expect("create remote");
    run(&remote, "git", &["init", "--bare"]);

    run(&path, "git", &["init", "-b", "main"]);
    run(&path, "git", &["config", "user.name", "Test User"]);
    run(&path, "git", &["config", "user.email", "test@example.com"]);
    write_file(&path, "tracked.txt", "initial\n");
    run(&path, "git", &["add", "tracked.txt"]);
    run(&path, "git", &["commit", "-m", "initial"]);
    let main_head = git_output(&path, &["rev-parse", "HEAD"]);
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

    run(&path, "git", &["checkout", "-b", "feat/a"]);
    write_file(&path, "a.txt", "a\n");
    run(&path, "git", &["add", "a.txt"]);
    run(&path, "git", &["commit", "-m", "a"]);
    let a_head = git_output(&path, &["rev-parse", "HEAD"]);
    run(&path, "git", &["push", "-u", "origin", "feat/a"]);

    run(&path, "git", &["checkout", "-b", "feat/b"]);
    write_file(&path, "b.txt", "b\n");
    run(&path, "git", &["add", "b.txt"]);
    run(&path, "git", &["commit", "-m", "b"]);
    run(&path, "git", &["push", "-u", "origin", "feat/b"]);
    run(&path, "git", &["checkout", "main"]);

    let state = serde_json::json!({
        "trunk": "main",
        "remote": "origin",
        "branches": {
            "feat/a": {
                "name": "feat/a",
                "parent": "main",
                "parent_head": main_head,
                "pr_number": 101
            },
            "feat/b": {
                "name": "feat/b",
                "parent": "feat/a",
                "parent_head": a_head,
                "pr_number": 102
            }
        }
    });
    let ez_dir = path.join(".git/ez");
    std::fs::create_dir_all(&ez_dir).expect("create ez metadata");
    std::fs::write(
        ez_dir.join("stack.json"),
        serde_json::to_vec_pretty(&state).expect("serialize state"),
    )
    .expect("write state");

    let fake_bin = temp_dir("sync-native-bin");
    let gh_log = fake_bin.join("gh.log");
    let payload = fake_bin.join("payload.json");
    let script = fake_bin.join("gh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
echo "$@" >> "$GH_LOG"
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  printf '{"data":{"repository":{}}}\n'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ]; then
  if [ "$GH_MODE" = "base_failure" ]; then
    printf 'simulated PR base update failure\n' >&2
    exit 1
  fi
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=101" ]; then
  case "$GH_MODE" in
    create) printf '[]\n'; exit 0 ;;
    extend) printf '[{"number":88,"pull_requests":[{"number":101},{"number":102}]}]\n'; exit 0 ;;
    unavailable) printf 'HTTP 404: Not Found\n' >&2; exit 1 ;;
    divergence) printf '[{"number":88,"pull_requests":[{"number":101},{"number":999}]}]\n'; exit 0 ;;
    stale_longer) printf '[{"number":88,"pull_requests":[{"number":101},{"number":102},{"number":999}]}]\n'; exit 0 ;;
  esac
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=102" ] && { [ "$GH_MODE" = "cleanup" ] || [ "$GH_MODE" = "base_failure" ]; }; then
  printf '[]\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "POST" ] && [ "$4" = "repos/org/repo/stacks" ]; then
  cat > "$GH_PAYLOAD"
  printf '{"number":88,"pull_requests":[{"number":101},{"number":102}]}\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "POST" ] && [ "$4" = "repos/org/repo/stacks/88/add" ]; then
  cat > "$GH_PAYLOAD"
  printf '{"number":88}\n'
  exit 0
fi
printf 'unexpected gh invocation: %s\n' "$*" >&2
exit 1
"#,
    )
    .expect("write fake gh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("make fake gh executable");
    }

    SyncRepo {
        path,
        remote,
        fake_bin,
        gh_log,
        payload,
    }
}

fn run_ez_sync(repo: &SyncRepo, mode: &str) -> Output {
    run_ez(repo, &["sync"], mode)
}

fn run_ez(repo: &SyncRepo, args: &[&str], mode: &str) -> Output {
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
        .env("GH_PAYLOAD", &repo.payload)
        .env("GH_MODE", mode)
        .output()
        .expect("run ez sync")
}

fn stack_state(repo: &SyncRepo) -> Value {
    serde_json::from_slice(
        &std::fs::read(repo.path.join(".git/ez/stack.json")).expect("read state"),
    )
    .expect("state JSON")
}

fn save_stack_state(repo: &SyncRepo, state: &Value) {
    std::fs::write(
        repo.path.join(".git/ez/stack.json"),
        serde_json::to_vec_pretty(state).expect("serialize state"),
    )
    .expect("write state");
}

fn add_branch(repo: &SyncRepo, name: &str, parent: &str, file: &str, pr_number: u64) {
    run(&repo.path, "git", &["checkout", parent]);
    let parent_head = git_output(&repo.path, &["rev-parse", "HEAD"]);
    run(&repo.path, "git", &["checkout", "-b", name]);
    write_file(&repo.path, file, &format!("{name}\n"));
    run(&repo.path, "git", &["add", file]);
    run(&repo.path, "git", &["commit", "-m", name]);
    run(&repo.path, "git", &["push", "-u", "origin", name]);
    run(&repo.path, "git", &["checkout", "main"]);

    let mut state = stack_state(repo);
    state["branches"][name] = serde_json::json!({
        "name": name,
        "parent": parent,
        "parent_head": parent_head,
        "pr_number": pr_number,
    });
    save_stack_state(repo, &state);
}

fn push_remote_main_change(repo: &SyncRepo, file: &str, contents: &str, message: &str) -> String {
    let clone = temp_dir("sync-native-upstream");
    run(
        &clone,
        "git",
        &["clone", repo.remote.to_str().expect("remote path"), "."],
    );
    run(&clone, "git", &["config", "user.name", "Upstream User"]);
    run(
        &clone,
        "git",
        &["config", "user.email", "upstream@example.com"],
    );
    write_file(&clone, file, contents);
    run(&clone, "git", &["add", file]);
    run(&clone, "git", &["commit", "-m", message]);
    let head = git_output(&clone, &["rev-parse", "HEAD"]);
    run(&clone, "git", &["push", "origin", "main"]);
    std::fs::remove_dir_all(clone).expect("remove upstream clone");
    head
}

fn advance_remote_main(repo: &SyncRepo) -> String {
    push_remote_main_change(repo, "upstream.txt", "upstream\n", "advance upstream main")
}

fn receipt(output: &Output, action: &str) -> Value {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let needle = format!(r#""native_stack_action":"{action}""#);
    let line = stderr
        .lines()
        .find(|line| line.contains(&needle))
        .unwrap_or_else(|| panic!("missing {action} receipt in:\n{stderr}"));
    let start = line.find('{').expect("receipt start");
    let end = line.rfind('}').expect("receipt end");
    serde_json::from_str(&line[start..=end]).expect("receipt JSON")
}

#[test]
fn sync_reconciles_linear_real_git_stack_with_github_native_stack() {
    let repo = init_linear_sync_repo();
    let old_a = git_output(&repo.path, &["rev-parse", "feat/a"]);
    let old_b = git_output(&repo.path, &["rev-parse", "feat/b"]);
    let upstream_main = advance_remote_main(&repo);

    let output = run_ez_sync(&repo, "create");

    assert!(
        output.status.success(),
        "sync failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&repo.payload).expect("native payload"))
            .expect("payload JSON"),
        serde_json::json!({"pull_requests": [101, 102]})
    );
    let native_receipt = receipt(&output, "created");
    assert_eq!(native_receipt["cmd"], "sync");
    assert_eq!(
        native_receipt["branches"],
        serde_json::json!(["feat/a", "feat/b"])
    );
    assert_eq!(
        native_receipt["pull_requests"],
        serde_json::json!([101, 102])
    );
    assert_eq!(native_receipt["native_stack_number"], 88);

    let log = std::fs::read_to_string(&repo.gh_log).expect("gh log");
    let lookup = log.find("stacks?pull_request=101").expect("stack lookup");
    let create = log
        .find("-X POST repos/org/repo/stacks")
        .expect("stack create");
    assert!(lookup < create, "lookup must happen before create:\n{log}");

    assert_eq!(
        git_output(&repo.path, &["rev-parse", "main"]),
        upstream_main
    );
    assert_ne!(git_output(&repo.path, &["rev-parse", "feat/a"]), old_a);
    assert_ne!(git_output(&repo.path, &["rev-parse", "feat/b"]), old_b);
    assert!(
        run_raw(
            &repo.path,
            "git",
            &["merge-base", "--is-ancestor", "main", "feat/a"]
        )
        .status
        .success()
    );
    assert!(
        run_raw(
            &repo.path,
            "git",
            &["merge-base", "--is-ancestor", "feat/a", "feat/b"]
        )
        .status
        .success()
    );
    let saved = stack_state(&repo);
    assert_eq!(saved["branches"]["feat/a"]["parent_head"], upstream_main);
    assert_eq!(
        saved["branches"]["feat/b"]["parent_head"],
        git_output(&repo.path, &["rev-parse", "feat/a"])
    );
}

#[test]
fn sync_extends_native_stack_with_only_the_new_top_pr() {
    let repo = init_linear_sync_repo();
    add_branch(&repo, "feat/c", "feat/b", "c.txt", 103);

    let output = run_ez_sync(&repo, "extend");

    assert!(
        output.status.success(),
        "sync failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&repo.payload).expect("add payload"))
            .expect("payload JSON"),
        serde_json::json!({"pull_requests": [103]})
    );
    let native_receipt = receipt(&output, "extended");
    assert_eq!(native_receipt["native_stack_number"], 88);
    assert_eq!(native_receipt["native_stack_added"], 1);
    assert_eq!(
        native_receipt["pull_requests"],
        serde_json::json!([101, 102, 103])
    );
    assert!(
        std::fs::read_to_string(&repo.gh_log)
            .expect("gh log")
            .contains("-X POST repos/org/repo/stacks/88/add --input -")
    );
}

#[test]
fn sync_keeps_working_when_native_stack_preview_endpoint_is_unavailable() {
    let repo = init_linear_sync_repo();

    let output = run_ez_sync(&repo, "unavailable");

    assert!(
        output.status.success(),
        "404 fallback should not fail sync:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(receipt(&output, "unavailable")["cmd"], "sync");
    assert!(
        !repo.payload.exists(),
        "404 fallback must not mutate GitHub"
    );
}

#[test]
fn sync_reports_divergence_without_overwriting_github_stack() {
    let repo = init_linear_sync_repo();

    let output = run_ez_sync(&repo, "divergence");

    assert!(
        output.status.success(),
        "native divergence should not discard successful local sync:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let error_receipt = receipt(&output, "error");
    assert_eq!(error_receipt["cmd"], "sync");
    assert!(
        error_receipt["native_stack_error"]
            .as_str()
            .expect("error text")
            .contains("retry `ez sync`")
    );
    assert!(!repo.payload.exists(), "divergence must not mutate GitHub");
}

#[test]
fn sync_reports_stale_remote_superset_instead_of_claiming_it_is_current() {
    let repo = init_linear_sync_repo();

    let output = run_ez_sync(&repo, "stale_longer");

    assert!(
        output.status.success(),
        "strict native reconciliation is best-effort:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let error_receipt = receipt(&output, "error");
    assert!(
        error_receipt["native_stack_error"]
            .as_str()
            .expect("error text")
            .contains("existing pull_requests=[101, 102, 999]")
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(r#""native_stack_action":"unchanged""#)
    );
    assert!(
        !repo.payload.exists(),
        "strict divergence must not mutate GitHub"
    );
}

#[test]
fn sync_does_not_flatten_branching_worktree_graph_into_github_stack() {
    let repo = init_linear_sync_repo();
    add_branch(&repo, "feat/c", "feat/a", "c.txt", 103);

    let output = run_ez_sync(&repo, "create");

    assert!(
        output.status.success(),
        "branch-aware sync failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let skipped = receipt(&output, "skipped");
    assert_eq!(skipped["native_stack_reason"], "branching_component");
    assert_eq!(
        skipped["branches"],
        serde_json::json!(["feat/a", "feat/b", "feat/c"])
    );
    let log = std::fs::read_to_string(&repo.gh_log).expect("gh log");
    assert!(
        !log.contains("stacks?pull_request="),
        "branching graph must not reach native stack API:\n{log}"
    );
    assert!(!repo.payload.exists());
}

#[test]
fn sync_cleans_merged_bottom_branch_then_links_remaining_real_git_chain() {
    let repo = init_linear_sync_repo();
    add_branch(&repo, "feat/c", "feat/b", "c.txt", 103);
    run(&repo.path, "git", &["push", "origin", "feat/a:main"]);

    let output = run_ez_sync(&repo, "cleanup");

    assert!(
        output.status.success(),
        "cleanup sync failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !run_raw(
            &repo.path,
            "git",
            &["show-ref", "--verify", "refs/heads/feat/a"]
        )
        .status
        .success(),
        "merged bottom branch should be removed"
    );
    let state = stack_state(&repo);
    assert!(state["branches"].get("feat/a").is_none());
    assert_eq!(state["branches"]["feat/b"]["parent"], "main");
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&repo.payload).expect("native payload"))
            .expect("payload JSON"),
        serde_json::json!({"pull_requests": [102, 103]})
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let cleaned = stderr
        .find(r#""action":"cleaned","branch":"feat/a""#)
        .expect("cleanup receipt");
    let created = stderr
        .find(r#""native_stack_action":"created""#)
        .expect("native stack receipt");
    assert!(
        cleaned < created,
        "cleanup must finish before GitHub reconciliation"
    );
    assert_eq!(
        receipt(&output, "created")["pull_requests"],
        serde_json::json!([102, 103])
    );
    let log = std::fs::read_to_string(&repo.gh_log).expect("gh log");
    let rebase_pr = log
        .find("pr edit 102 --base main")
        .expect("remaining bottom PR must be retargeted to trunk");
    let native_lookup = log
        .find("stacks?pull_request=102")
        .expect("remaining native stack lookup");
    assert!(
        rebase_pr < native_lookup,
        "PR bases must match local parents before native linking:\n{log}"
    );
}

#[test]
fn sync_never_mutates_github_when_real_git_restack_is_incomplete() {
    let repo = init_linear_sync_repo();
    run(&repo.path, "git", &["checkout", "feat/a"]);
    write_file(&repo.path, "tracked.txt", "feature version\n");
    run(&repo.path, "git", &["add", "tracked.txt"]);
    run(
        &repo.path,
        "git",
        &["commit", "-m", "feature edits tracked file"],
    );
    run(&repo.path, "git", &["checkout", "main"]);
    push_remote_main_change(
        &repo,
        "tracked.txt",
        "upstream version\n",
        "upstream edits tracked file",
    );

    let output = run_ez_sync(&repo, "create");

    assert_eq!(output.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(r#""action":"restack_incomplete""#),
        "expected restack error:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = std::fs::read_to_string(&repo.gh_log).expect("gh log");
    assert!(
        !log.contains("stacks?pull_request="),
        "incomplete local restack must not mutate native stack:\n{log}"
    );
    assert!(!repo.payload.exists());
}

#[test]
fn sync_skips_native_linking_when_survivor_pr_base_cannot_be_repaired() {
    let repo = init_linear_sync_repo();
    add_branch(&repo, "feat/c", "feat/b", "c.txt", 103);
    run(&repo.path, "git", &["push", "origin", "feat/a:main"]);

    let output = run_ez_sync(&repo, "base_failure");

    assert!(
        output.status.success(),
        "PR base repair is best-effort for local sync:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(r#""action":"pr_base_update_error""#));
    assert!(stderr.contains(r#""native_stack_reason":"pr_base_update_failed""#));
    let log = std::fs::read_to_string(&repo.gh_log).expect("gh log");
    assert!(log.contains("pr edit 102 --base main"));
    assert!(
        !log.contains("stacks?pull_request="),
        "native linking must wait for valid PR bases:\n{log}"
    );
    assert!(!repo.payload.exists());
}

#[test]
fn sync_dry_run_previews_native_chain_without_fetching_or_calling_github() {
    let repo = init_linear_sync_repo();

    let output = run_ez(&repo, &["sync", "--dry-run"], "create");

    assert!(
        output.status.success(),
        "dry-run failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Would reconcile GitHub native stack for PRs [101, 102]")
    );
    assert!(!repo.gh_log.exists(), "dry-run must not invoke gh");
    assert!(!repo.payload.exists(), "dry-run must not mutate GitHub");
}
