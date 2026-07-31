use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static NEXT_TEMP_REPO_ID: AtomicU64 = AtomicU64::new(1);

struct TempRepo {
    path: PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ez-worktree-leases-cli-{}-{}-{}",
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
        run(&path, "git", &["remote", "add", "origin", "."]);
        run_ez(&path, &["init", "--yes"]);
        Self { path }
    }

    fn create_worktree(&self, branch: &str) -> PathBuf {
        let output = run_ez(
            &self.path,
            &["create", branch, "--from", "main", "--no-worktree"],
        );
        assert!(output.status.success());
        let ensured = stdout_json(&run_ez(
            &self.path,
            &["worktree", "ensure", branch, "--json"],
        ));
        PathBuf::from(
            ensured["entries"][0]["path"]
                .as_str()
                .expect("worktree path"),
        )
    }

    fn stack_state_bytes(&self) -> Vec<u8> {
        let common_dir = stdout_text(&run(&self.path, "git", &["rev-parse", "--git-common-dir"]));
        let common_dir = PathBuf::from(common_dir);
        let common_dir = if common_dir.is_absolute() {
            common_dir
        } else {
            self.path.join(common_dir)
        };
        std::fs::read(common_dir.join("ez/stack.json")).expect("read stack state")
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
        "{program} {args:?} failed:\n{}",
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

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout JSON")
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn worktree_porcelain(repo: &Path) -> String {
    String::from_utf8_lossy(&run(repo, "git", &["worktree", "list", "--porcelain"]).stdout)
        .into_owned()
}

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(dir.join(file), contents).expect("write committed file");
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
}

#[test]
fn claim_and_release_are_visible_without_mutating_stack_state() {
    let repo = TempRepo::new();
    let branch = "feat/owned";
    let worktree = repo.create_worktree(branch);
    let state_before = repo.stack_state_bytes();

    let claim = stdout_json(&run_ez(
        &worktree,
        &[
            "worktree", "claim", "--owner", "agent-a", "--ttl", "2h", "--json",
        ],
    ));

    assert_eq!(claim["cmd"], "worktree.claim");
    assert_eq!(claim["branch"], branch);
    assert_eq!(claim["path"], worktree.to_string_lossy().as_ref());
    assert_eq!(claim["lease"]["owner"], "agent-a");
    assert_eq!(claim["lease"]["stale"], false);
    assert!(
        claim["lease"]["expires_at"].as_u64().expect("expiry")
            > claim["lease"]["created_at"].as_u64().expect("creation")
    );
    let porcelain = worktree_porcelain(&repo.path);
    assert!(porcelain.contains("locked "));
    assert!(porcelain.contains("ez-lease:"));

    let leases = stdout_json(&run_ez(&repo.path, &["worktree", "leases", "--json"]));
    let entry = leases["entries"]
        .as_array()
        .expect("lease entries")
        .iter()
        .find(|entry| entry["branch"] == branch)
        .expect("claimed branch");
    assert_eq!(entry["lease"]["owner"], "agent-a");
    assert_eq!(entry["lease"]["stale"], false);
    assert!(entry["foreign_lock_reason"].is_null());

    let list = stdout_json(&run_ez(&repo.path, &["list", "--json"]));
    let list_entry = list
        .as_array()
        .expect("list entries")
        .iter()
        .find(|entry| entry["branch"] == branch)
        .expect("listed branch");
    assert_eq!(list_entry["worktree_lock"]["kind"], "lease");
    assert_eq!(list_entry["worktree_lock"]["owner"], "agent-a");
    assert_eq!(repo.stack_state_bytes(), state_before);

    let release = stdout_json(&run_ez(
        &repo.path,
        &[
            "worktree", "release", branch, "--owner", "agent-a", "--json",
        ],
    ));
    assert_eq!(release["cmd"], "worktree.release");
    assert_eq!(release["released"], true);
    assert!(!worktree_porcelain(&repo.path).contains("ez-lease:"));
    assert_eq!(repo.stack_state_bytes(), state_before);
}

#[test]
fn release_requires_the_owner_unless_force_is_explicit() {
    let repo = TempRepo::new();
    let branch = "feat/owner-check";
    repo.create_worktree(branch);
    run_ez(
        &repo.path,
        &["worktree", "claim", branch, "--owner", "agent-a"],
    );

    let rejected = run_ez_raw(
        &repo.path,
        &["worktree", "release", branch, "--owner", "agent-b"],
    );
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(stderr.contains("agent-a"), "unexpected stderr: {stderr}");
    assert!(stderr.contains("--force"), "unexpected stderr: {stderr}");
    assert!(worktree_porcelain(&repo.path).contains("ez-lease:"));

    run_ez(&repo.path, &["worktree", "release", branch, "--force"]);
    assert!(!worktree_porcelain(&repo.path).contains("ez-lease:"));
}

#[test]
fn foreign_git_locks_are_visible_and_never_overwritten_or_released() {
    let repo = TempRepo::new();
    let branch = "feat/foreign-lock";
    let worktree = repo.create_worktree(branch);
    let path = worktree.to_string_lossy();
    run(
        &repo.path,
        "git",
        &["worktree", "lock", "--reason", "maintenance window", &path],
    );

    let claim = run_ez_raw(
        &repo.path,
        &["worktree", "claim", branch, "--owner", "agent-a"],
    );
    assert!(!claim.status.success());
    let claim_stderr = String::from_utf8_lossy(&claim.stderr);
    assert!(
        claim_stderr.contains("maintenance window"),
        "unexpected stderr: {claim_stderr}"
    );

    let release = run_ez_raw(&repo.path, &["worktree", "release", branch, "--force"]);
    assert!(!release.status.success());
    let release_stderr = String::from_utf8_lossy(&release.stderr);
    assert!(
        release_stderr.contains("maintenance window"),
        "unexpected stderr: {release_stderr}"
    );

    let leases = stdout_json(&run_ez(&repo.path, &["worktree", "leases", "--json"]));
    let entry = leases["entries"]
        .as_array()
        .expect("lease entries")
        .iter()
        .find(|entry| entry["branch"] == branch)
        .expect("foreign locked branch");
    assert!(entry["lease"].is_null());
    assert_eq!(entry["foreign_lock_reason"], "maintenance window");
    assert!(worktree_porcelain(&repo.path).contains("locked maintenance window"));
}

#[test]
fn claim_rejects_main_worktrees_and_missing_linked_worktrees() {
    let repo = TempRepo::new();
    let branch = "feat/main-checkout";
    run_ez(
        &repo.path,
        &["create", branch, "--from", "main", "--no-worktree"],
    );

    let missing = run_ez_raw(
        &repo.path,
        &["worktree", "claim", branch, "--owner", "agent-a"],
    );
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("linked worktree"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&missing.stderr)
    );

    run(&repo.path, "git", &["switch", branch]);
    let main = run_ez_raw(
        &repo.path,
        &["worktree", "claim", branch, "--owner", "agent-a"],
    );
    assert!(!main.status.success());
    let stderr = String::from_utf8_lossy(&main.stderr);
    assert!(
        stderr.contains("main worktree"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn stale_leases_require_an_explicit_takeover() {
    let repo = TempRepo::new();
    let branch = "feat/stale-lease";
    let worktree = repo.create_worktree(branch);
    let reason = format!(
        "ez-lease:{}",
        serde_json::json!({
            "version": 1,
            "owner": "gone-agent",
            "branch": branch,
            "created_at": 1,
            "expires_at": 2,
        })
    );
    run(
        &repo.path,
        "git",
        &[
            "worktree",
            "lock",
            "--reason",
            &reason,
            &worktree.to_string_lossy(),
        ],
    );

    let leases = stdout_json(&run_ez(&repo.path, &["worktree", "leases", "--json"]));
    let entry = leases["entries"]
        .as_array()
        .expect("lease entries")
        .iter()
        .find(|entry| entry["branch"] == branch)
        .expect("stale lease");
    assert_eq!(entry["lease"]["owner"], "gone-agent");
    assert_eq!(entry["lease"]["stale"], true);

    let rejected = run_ez_raw(
        &repo.path,
        &["worktree", "claim", branch, "--owner", "agent-b"],
    );
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(stderr.contains("stale"), "unexpected stderr: {stderr}");
    assert!(
        stderr.contains("--break-stale"),
        "unexpected stderr: {stderr}"
    );

    let takeover = stdout_json(&run_ez(
        &repo.path,
        &[
            "worktree",
            "claim",
            branch,
            "--owner",
            "agent-b",
            "--break-stale",
            "--json",
        ],
    ));
    assert_eq!(takeover["lease"]["owner"], "agent-b");
    assert_eq!(takeover["lease"]["stale"], false);
}

#[test]
fn delete_refuses_to_remove_an_actively_leased_worktree() {
    let repo = TempRepo::new();
    let branch = "feat/live-agent";
    let worktree = repo.create_worktree(branch);
    let state_before = repo.stack_state_bytes();
    let tip_before = stdout_text(&run(&repo.path, "git", &["rev-parse", branch]));
    run_ez(
        &repo.path,
        &["worktree", "claim", branch, "--owner", "agent-a"],
    );

    let output = run_ez_raw(&repo.path, &["delete", branch, "--force", "--yes"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("agent-a"), "unexpected stderr: {stderr}");
    assert!(stderr.contains("release"), "unexpected stderr: {stderr}");
    assert!(worktree.exists());
    assert_eq!(
        stdout_text(&run(&repo.path, "git", &["rev-parse", branch])),
        tip_before
    );
    assert_eq!(repo.stack_state_bytes(), state_before);
}

#[test]
fn claim_is_idempotent_and_validates_owner_ttl_and_active_takeover() {
    let repo = TempRepo::new();
    let branch = "feat/idempotent-claim";
    repo.create_worktree(branch);

    for args in [
        vec!["worktree", "claim", branch, "--owner", "", "--json"],
        vec![
            "worktree", "claim", branch, "--owner", "agent-a", "--ttl", "forever",
        ],
    ] {
        let output = run_ez_raw(&repo.path, &args);
        assert!(!output.status.success());
    }

    let first = stdout_json(&run_ez(
        &repo.path,
        &["worktree", "claim", branch, "--owner", "agent-a", "--json"],
    ));
    assert_eq!(first["claimed"], true);

    let second = stdout_json(&run_ez(
        &repo.path,
        &["worktree", "claim", branch, "--owner", "agent-a", "--json"],
    ));
    assert_eq!(second["claimed"], false);
    assert_eq!(second["lease"], first["lease"]);

    let takeover = run_ez_raw(
        &repo.path,
        &[
            "worktree",
            "claim",
            branch,
            "--owner",
            "agent-b",
            "--break-stale",
        ],
    );
    assert!(!takeover.status.success());
    let stderr = String::from_utf8_lossy(&takeover.stderr);
    assert!(
        stderr.contains("actively claimed"),
        "unexpected stderr: {stderr}"
    );

    let no_owner = run_ez_raw(&repo.path, &["worktree", "release", branch]);
    assert!(!no_owner.status.success());
    assert!(
        String::from_utf8_lossy(&no_owner.stderr).contains("--owner"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&no_owner.stderr)
    );

    run_ez(
        &repo.path,
        &["worktree", "release", branch, "--owner", "agent-a"],
    );
    let already_released = stdout_json(&run_ez(
        &repo.path,
        &[
            "worktree", "release", branch, "--owner", "agent-a", "--json",
        ],
    ));
    assert_eq!(already_released["released"], false);
}

#[test]
fn fold_refuses_to_advance_a_leased_parent_worktree() {
    let repo = TempRepo::new();
    let parent = "feat/leased-parent";
    let child = "feat/fold-child";
    let parent_worktree = repo.create_worktree(parent);
    commit_file(&parent_worktree, "parent.txt", "parent\n", "parent");
    run_ez(
        &repo.path,
        &["create", child, "--from", parent, "--no-worktree"],
    );
    let child_ensure = stdout_json(&run_ez(
        &repo.path,
        &["worktree", "ensure", child, "--json"],
    ));
    let child_worktree = PathBuf::from(
        child_ensure["entries"][0]["path"]
            .as_str()
            .expect("child worktree path"),
    );
    commit_file(&child_worktree, "child.txt", "child\n", "child");
    run_ez(
        &repo.path,
        &["worktree", "claim", parent, "--owner", "parent-agent"],
    );
    let state_before = repo.stack_state_bytes();
    let parent_tip = stdout_text(&run(&repo.path, "git", &["rev-parse", parent]));
    let child_tip = stdout_text(&run(&repo.path, "git", &["rev-parse", child]));

    let output = run_ez_raw(&repo.path, &["fold", child, "--yes"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("parent-agent"),
        "unexpected stderr: {stderr}"
    );
    assert_eq!(
        stdout_text(&run(&repo.path, "git", &["rev-parse", parent])),
        parent_tip
    );
    assert_eq!(
        stdout_text(&run(&repo.path, "git", &["rev-parse", child])),
        child_tip
    );
    assert!(parent_worktree.exists());
    assert!(child_worktree.exists());
    assert_eq!(repo.stack_state_bytes(), state_before);
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_crashed_coordination_process_does_not_wedge_real_lease_operations() {
    let repo = TempRepo::new();
    let branch = "feat/crash-recovery";
    repo.create_worktree(branch);
    let lock_path = repo.path.join(".git/ez/worktree-lease.lock");
    let ready_path = repo.path.join(".git/ez/worktree-lease.ready");
    let child = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--ignored",
            "--exact",
            "kernel_coordination_lock_holder_process",
        ])
        .env("EZ_TEST_COORDINATION_LOCK_PATH", &lock_path)
        .env("EZ_TEST_COORDINATION_READY_PATH", &ready_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn coordination lock holder");
    let mut child = ChildGuard(child);

    for _ in 0..100 {
        if ready_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        ready_path.exists(),
        "coordination holder did not become ready"
    );

    let blocked = run_ez_raw(
        &repo.path,
        &["worktree", "claim", branch, "--owner", "agent-a"],
    );
    assert!(!blocked.status.success());
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("operation is in progress"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    assert!(!worktree_porcelain(&repo.path).contains("ez-lease:"));

    child.0.kill().expect("simulate holder crash");
    child.0.wait().expect("reap crashed holder");

    run_ez(
        &repo.path,
        &["worktree", "claim", branch, "--owner", "agent-a"],
    );
    assert!(worktree_porcelain(&repo.path).contains("ez-lease:"));
}

#[test]
#[ignore = "subprocess helper invoked explicitly by the crash-recovery test"]
fn kernel_coordination_lock_holder_process() {
    let Some(lock_path) = std::env::var_os("EZ_TEST_COORDINATION_LOCK_PATH").map(PathBuf::from)
    else {
        return;
    };
    let ready_path =
        PathBuf::from(std::env::var_os("EZ_TEST_COORDINATION_READY_PATH").expect("ready path"));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .expect("open coordination lock");
    lock_exclusive(&file).expect("lock coordination file");
    file.set_len(0).expect("truncate holder metadata");
    file.write_all(br#"{"pid":999999,"operation":"test crash holder"}"#)
        .expect("write holder metadata");
    file.flush().expect("flush holder metadata");
    std::fs::write(ready_path, b"ready").expect("signal ready");
    loop {
        std::thread::park();
    }
}

#[cfg(unix)]
fn lock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    unsafe extern "C" {
        fn flock(fd: std::ffi::c_int, operation: std::ffi::c_int) -> std::ffi::c_int;
    }

    const LOCK_EX: std::ffi::c_int = 2;
    let result = unsafe { flock(file.as_raw_fd(), LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &std::fs::File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "test requires Unix file locking",
    ))
}
