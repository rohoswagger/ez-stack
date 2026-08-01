use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TestRepo {
    path: PathBuf,
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ez-small-shared-{prefix}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).expect("create temporary directory");
    path
}

fn output(dir: &Path, program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .output()
        .expect("run command")
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    let result = output(dir, program, args);
    assert!(
        result.status.success(),
        "{program} {args:?} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    result
}

fn ez(dir: &Path, args: &[&str]) -> Output {
    output(dir, env!("CARGO_BIN_EXE_ez"), args)
}

fn run_ez(dir: &Path, args: &[&str]) -> Output {
    run(dir, env!("CARGO_BIN_EXE_ez"), args)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

fn init_repo(prefix: &str) -> TestRepo {
    let path = temp_dir(prefix);
    run(&path, "git", &["init", "-b", "main"]);
    run(&path, "git", &["config", "user.email", "test@example.com"]);
    run(&path, "git", &["config", "user.name", "Test User"]);
    std::fs::write(path.join("README.md"), "base\n").expect("write fixture");
    run(&path, "git", &["add", "README.md"]);
    run(&path, "git", &["commit", "-m", "base"]);
    TestRepo { path }
}

fn init_ez_and_track(repo: &TestRepo, branch: &str) {
    run_ez(&repo.path, &["init", "--yes"]);
    run(&repo.path, "git", &["switch", "-c", branch]);
    run_ez(&repo.path, &["track", branch, "--parent", "main"]);
}

fn stack_state(repo: &TestRepo) -> Value {
    let common_dir = text(&run(&repo.path, "git", &["rev-parse", "--git-common-dir"]).stdout)
        .trim()
        .to_string();
    let common_dir = PathBuf::from(common_dir);
    let common_dir = if common_dir.is_absolute() {
        common_dir
    } else {
        repo.path.join(common_dir)
    };
    serde_json::from_slice(
        &std::fs::read(common_dir.join("ez").join("stack.json")).expect("read stack state"),
    )
    .expect("parse stack state")
}

#[test]
fn shell_init_prints_a_complete_wrapper_without_a_repository() {
    let cwd = temp_dir("shell-init");
    let result = run_ez(&cwd, &["shell-init"]);
    let stdout = text(&result.stdout);
    let stderr = text(&result.stderr);

    assert!(stdout.starts_with("# ez shell integration"));
    assert!(stdout.contains("create|delete|switch|checkout|co|up|down|top|bottom"));
    assert!(stdout.contains("merge|sync|fold)"));
    assert!(stdout.contains("EZ_SHELL_INTEGRATION=1 command ez \"$@\""));
    assert!(stderr.contains("[ok |"));

    std::fs::remove_dir_all(cwd).expect("remove temporary directory");
}

#[test]
fn init_accepts_an_explicit_trunk_and_enables_rerere_noninteractively() {
    let repo = init_repo("init-explicit");
    run(&repo.path, "git", &["branch", "trunk"]);

    run_ez(&repo.path, &["init", "--trunk", "trunk", "--yes"]);

    let state = stack_state(&repo);
    assert_eq!(state["trunk"], "trunk");
    assert_eq!(state["rerere"], true);
    assert_eq!(
        text(&run(&repo.path, "git", &["config", "rerere.enabled"]).stdout).trim(),
        "true"
    );
    assert_eq!(
        text(&run(&repo.path, "git", &["config", "rerere.autoupdate"]).stdout).trim(),
        "true"
    );
}

#[test]
fn diff_refuses_trunk_and_unmanaged_branches_then_reports_managed_changes() {
    let repo = init_repo("diff");
    run_ez(&repo.path, &["init", "--yes"]);

    let trunk = ez(&repo.path, &["diff"]);
    assert!(!trunk.status.success());
    assert!(text(&trunk.stderr).contains("currently on trunk branch"));

    run(&repo.path, "git", &["switch", "-c", "feat/diff"]);
    let unmanaged = ez(&repo.path, &["diff"]);
    assert!(!unmanaged.status.success());
    assert!(text(&unmanaged.stderr).contains("not tracked by ez"));

    run_ez(&repo.path, &["track", "feat/diff", "--parent", "main"]);
    std::fs::write(repo.path.join("feature.txt"), "feature\n").expect("write feature");
    run(&repo.path, "git", &["add", "feature.txt"]);
    run(&repo.path, "git", &["commit", "-m", "feature"]);

    let names = run_ez(&repo.path, &["diff", "--name-only"]);
    assert_eq!(text(&names.stdout), "feature.txt");
    let stat = run_ez(&repo.path, &["diff", "--stat"]);
    assert!(text(&stat.stdout).contains("feature.txt"));
    let patch = run_ez(&repo.path, &["diff"]);
    assert!(text(&patch.stdout).contains("+feature"));
}

#[test]
fn scope_commands_validate_normalize_persist_and_clear_real_stack_state() {
    let repo = init_repo("scope");
    init_ez_and_track(&repo, "feat/scope");

    let empty = run_ez(&repo.path, &["scope", "show"]);
    assert!(text(&empty.stderr).contains("No scope configured"));

    let invalid_add = ez(&repo.path, &["scope", "add", "   "]);
    assert!(!invalid_add.status.success());
    assert!(text(&invalid_add.stderr).contains("at least one non-empty pattern"));
    let invalid_set = ez(&repo.path, &["scope", "set", ""]);
    assert!(!invalid_set.status.success());
    assert!(text(&invalid_set.stderr).contains("at least one non-empty pattern"));

    run_ez(
        &repo.path,
        &["scope", "set", "--mode", "strict", " src/** ", "src/**"],
    );
    run_ez(&repo.path, &["scope", "add", " tests/** ", "src/**"]);
    let shown = run_ez(&repo.path, &["scope", "show"]);
    let shown_stderr = text(&shown.stderr);
    assert!(shown_stderr.contains("Mode: strict"));
    assert!(shown_stderr.contains("src/**"));
    assert!(shown_stderr.contains("tests/**"));

    let state = stack_state(&repo);
    assert_eq!(state["branches"]["feat/scope"]["scope_mode"], "strict");
    assert_eq!(
        state["branches"]["feat/scope"]["scope"],
        serde_json::json!(["src/**", "tests/**"])
    );

    run_ez(&repo.path, &["scope", "clear"]);
    let cleared = stack_state(&repo);
    assert!(cleared["branches"]["feat/scope"]["scope"].is_null());
    assert!(cleared["branches"]["feat/scope"]["scope_mode"].is_null());
}

#[test]
fn scope_and_pr_view_refuse_invalid_branch_contexts_without_calling_github() {
    let repo = init_repo("context-refusals");
    run_ez(&repo.path, &["init", "--yes"]);

    let trunk_scope = ez(&repo.path, &["scope", "show"]);
    assert!(!trunk_scope.status.success());
    assert!(text(&trunk_scope.stderr).contains("currently on trunk branch"));

    run(&repo.path, "git", &["switch", "-c", "feat/unmanaged"]);
    let unmanaged_scope = ez(&repo.path, &["scope", "show"]);
    assert!(!unmanaged_scope.status.success());
    assert!(text(&unmanaged_scope.stderr).contains("not tracked by ez"));

    let unmanaged_pr = ez(&repo.path, &["pr"]);
    assert!(!unmanaged_pr.status.success());
    assert!(text(&unmanaged_pr.stderr).contains("not tracked by ez"));

    run_ez(&repo.path, &["track", "feat/unmanaged", "--parent", "main"]);
    let missing_pr = ez(&repo.path, &["pr"]);
    assert!(!missing_pr.status.success());
    assert!(text(&missing_pr.stderr).contains("No PR found"));
}
