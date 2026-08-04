use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_REPO_ID: AtomicU64 = AtomicU64::new(1);

struct TempRepo {
    path: PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ez-switch-worktree-cli-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            NEXT_TEMP_REPO_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create unique temp repo");
        run(&path, "git", &["init", "-b", "main"]);
        run(&path, "git", &["config", "user.name", "Test User"]);
        run(&path, "git", &["config", "user.email", "test@example.com"]);
        std::fs::write(path.join("tracked.txt"), "initial\n").expect("write tracked file");
        run(&path, "git", &["add", "tracked.txt"]);
        run(&path, "git", &["commit", "-m", "initial"]);
        run_ez(&path, &["init", "--yes"]);
        Self { path }
    }

    fn create_managed_branch_without_worktree(&self, branch: &str) {
        run_ez(
            &self.path,
            &["create", branch, "--from", "main", "--no-worktree"],
        );
    }

    fn ensure_worktree(&self, branch: &str) -> PathBuf {
        self.create_managed_branch_without_worktree(branch);
        run_ez(&self.path, &["worktree", "ensure", branch]);
        canonicalize_existing(&expected_worktree_path(&self.path, branch))
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
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

fn run_raw(dir: &Path, program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .output()
        .expect("run command")
}

fn run_ez(dir: &Path, args: &[&str]) -> Output {
    run(dir, env!("CARGO_BIN_EXE_ez"), args)
}

fn run_ez_raw(dir: &Path, args: &[&str]) -> Output {
    run_raw(dir, env!("CARGO_BIN_EXE_ez"), args)
}

fn run_ez_with_shell_integration(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("EZ_SHELL_INTEGRATION", "1")
        .output()
        .expect("run ez with shell integration marker")
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

fn current_branch(worktree: &Path) -> String {
    git_output(worktree, &["branch", "--show-current"])
}

fn canonical_stdout_path(output: &Output) -> PathBuf {
    canonicalize_existing(&PathBuf::from(stdout_text(output)))
}

fn canonicalize_existing(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|err| panic!("canonicalize {}: {err}", path.display()))
}

fn expected_worktree_path(repo: &Path, branch: &str) -> PathBuf {
    repo.join(".worktrees").join(branch.replace('/', "-"))
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
fn direct_switch_to_existing_worktree_refuses_without_shell_integration() {
    let repo = TempRepo::new();
    let worktree = repo.ensure_worktree("feat/existing");
    assert_eq!(current_branch(&repo.path), "main");
    assert_eq!(current_branch(&worktree), "feat/existing");

    let output = run_ez_raw(&repo.path, &["switch", "feat/existing"]);

    assert_exit_code(&output, 5);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("could not switch"),
        "stderr should explain the failed switch:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("cd {}", worktree.display())),
        "stderr should give an exact manual cd command:\n{stderr}"
    );
    assert!(
        stderr.contains("ez setup") || stderr.contains("shell integration"),
        "stderr should mention setup or shell integration:\n{stderr}"
    );
    assert_eq!(current_branch(&repo.path), "main");
    assert_eq!(current_branch(&worktree), "feat/existing");
}

#[test]
fn no_cd_required_switch_to_existing_worktree_prints_only_the_path() {
    let repo = TempRepo::new();
    let worktree = repo.ensure_worktree("feat/existing");

    let output = run_ez_raw(&repo.path, &["switch", "feat/existing", "--no-cd-required"]);

    assert_exit_code(&output, 0);
    assert_eq!(canonical_stdout_path(&output), worktree);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{}\n", worktree.display())
    );
    assert_eq!(current_branch(&repo.path), "main");
    assert_eq!(current_branch(&worktree), "feat/existing");
}

#[test]
fn shell_integration_switch_to_existing_worktree_prints_path_and_preserves_git_state() {
    let repo = TempRepo::new();
    let worktree = repo.ensure_worktree("feat/existing");

    let output = run_ez_with_shell_integration(&repo.path, &["switch", "feat/existing"]);

    assert_exit_code(&output, 0);
    assert_eq!(canonical_stdout_path(&output), worktree);
    assert_eq!(current_branch(&repo.path), "main");
    assert_eq!(current_branch(&worktree), "feat/existing");
}

#[test]
fn direct_switch_to_managed_branch_without_worktree_refuses_before_creating_it() {
    let repo = TempRepo::new();
    repo.create_managed_branch_without_worktree("feat/new");
    let expected_path = expected_worktree_path(&repo.path, "feat/new");

    let direct = run_ez_raw(&repo.path, &["switch", "feat/new"]);

    assert_exit_code(&direct, 5);
    assert!(!expected_path.exists());
    assert_eq!(current_branch(&repo.path), "main");

    let allowed = run_ez_raw(&repo.path, &["switch", "feat/new", "--no-cd-required"]);

    assert_exit_code(&allowed, 0);
    let printed_path = canonical_stdout_path(&allowed);
    assert_eq!(printed_path, canonicalize_existing(&expected_path));
    assert!(printed_path.is_dir());
    assert_eq!(current_branch(&repo.path), "main");
    assert_eq!(current_branch(&printed_path), "feat/new");
}

#[test]
fn switch_by_missing_pr_number_fails_without_changing_branches() {
    let repo = TempRepo::new();
    repo.create_managed_branch_without_worktree("feat/topic");

    let output = run_ez_raw(&repo.path, &["switch", "404"]);

    assert_exit_code(&output, 5);
    assert_eq!(current_branch(&repo.path), "main");
    assert!(stdout_text(&output).is_empty());
    assert!(
        stderr_text(&output).contains("No branch found with PR #404"),
        "stderr should identify the missing PR mapping:\n{}",
        stderr_text(&output)
    );
}

#[test]
fn switch_to_current_branch_is_a_noop_without_worktree_handoff() {
    let repo = TempRepo::new();

    let output = run_ez_raw(&repo.path, &["switch", "main"]);

    assert_exit_code(&output, 0);
    assert_eq!(current_branch(&repo.path), "main");
    assert!(stdout_text(&output).is_empty());
    assert!(
        stderr_text(&output).contains("Already on `main`"),
        "stderr should report the noop:\n{}",
        stderr_text(&output)
    );
}

#[test]
fn switch_to_unknown_branch_fails_before_plain_git_checkout() {
    let repo = TempRepo::new();

    let output = run_ez_raw(&repo.path, &["checkout", "missing/branch"]);

    assert_exit_code(&output, 5);
    assert_eq!(current_branch(&repo.path), "main");
    assert!(
        stderr_text(&output).contains("branch `missing/branch` is not tracked by ez"),
        "stderr should explain the unknown branch:\n{}",
        stderr_text(&output)
    );
}
