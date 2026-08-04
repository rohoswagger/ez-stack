use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
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
            "ez-worktree-mutation-cli-{}-{}-{}",
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

    fn create_worktree(&self, branch: &str, parent: &str) -> PathBuf {
        run_ez(
            &self.path,
            &["create", branch, "--from", parent, "--no-worktree"],
        );
        let report = stdout_json(&run_ez(
            &self.path,
            &["worktree", "ensure", branch, "--json"],
        ));
        PathBuf::from(
            report["entries"][0]["path"]
                .as_str()
                .expect("worktree path"),
        )
    }

    fn stack_state(&self) -> Value {
        let common_dir = stdout_text(&run(&self.path, "git", &["rev-parse", "--git-common-dir"]));
        let common_dir = PathBuf::from(common_dir);
        let common_dir = if common_dir.is_absolute() {
            common_dir
        } else {
            self.path.join(common_dir)
        };
        serde_json::from_slice(
            &std::fs::read(common_dir.join("ez/stack.json")).expect("read stack state"),
        )
        .expect("parse stack state")
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct LinearStack {
    repo: TempRepo,
    base_worktree: PathBuf,
    child_worktree: PathBuf,
}

impl LinearStack {
    fn new() -> Self {
        let repo = TempRepo::new();
        let base_worktree = repo.create_worktree("feat/base", "main");
        commit_file(&base_worktree, "base.txt", "base\n", "base");
        let child_worktree = repo.create_worktree("feat/child", "feat/base");
        commit_file(&child_worktree, "child.txt", "child\n", "child");
        Self {
            repo,
            base_worktree,
            child_worktree,
        }
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

fn run_ez_with_fake_dev_tools(
    dir: &Path,
    args: &[&str],
    branch: &str,
    process_cwd: &Path,
    kill_marker: &Path,
    reuse_pid: bool,
) -> Output {
    let fake_bin = dir.join(".fake-dev-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake dev bin");
    let lsof = fake_bin.join("lsof");
    let kill = fake_bin.join("kill");
    let ps = fake_bin.join("ps");
    std::fs::write(
        &lsof,
        "#!/bin/sh\n\
         if [ \"$*\" = \"-nP -iTCP:$EZ_TEST_EXPECTED_PORT -sTCP:LISTEN -t\" ]; then\n\
         \tprintf '4242\\n'\n\
         elif [ \"$*\" = \"-a -p 4242 -d cwd -Fn\" ]; then\n\
         \tprintf 'p4242\\nfcwd\\nn%s\\n' \"$EZ_TEST_PROCESS_CWD\"\n\
         else\n\
         \techo \"unexpected lsof args: $*\" >&2\n\
         \texit 97\n\
         fi\n",
    )
    .expect("write fake lsof");
    std::fs::write(
        &kill,
        "#!/bin/sh\n\
         if [ \"$*\" != \"-TERM 4242\" ]; then\n\
         \techo \"unexpected kill args: $*\" >&2\n\
         \texit 97\n\
         fi\n\
         : > \"$EZ_TEST_KILL_MARKER\"\n",
    )
    .expect("write fake kill");
    std::fs::write(
        &ps,
        "#!/bin/sh\n\
         if [ \"$*\" != \"-o lstart= -p 4242\" ]; then\n\
         \techo \"unexpected ps args: $*\" >&2\n\
         \texit 97\n\
         fi\n\
         if [ \"$EZ_TEST_REUSE_PID\" = \"true\" ] && [ -e \"$EZ_TEST_PS_COUNTER\" ]; then\n\
         \tprintf 'Tue Jan  2 00:00:00 2024\\n'\n\
         else\n\
         \t: > \"$EZ_TEST_PS_COUNTER\"\n\
         \tprintf 'Mon Jan  1 00:00:00 2024\\n'\n\
         fi\n",
    )
    .expect("write fake ps");
    for script in [&lsof, &kill, &ps] {
        let mut permissions = std::fs::metadata(script)
            .expect("stat fake dev tool")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(script, permissions).expect("chmod fake dev tool");
    }
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fake_bin];
    paths.extend(std::env::split_paths(&original_path));
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("PATH", std::env::join_paths(paths).expect("join fake PATH"))
        .env("EZ_TEST_KILL_MARKER", kill_marker)
        .env("EZ_TEST_EXPECTED_PORT", dev_port(branch).to_string())
        .env("EZ_TEST_PROCESS_CWD", process_cwd)
        .env("EZ_TEST_REUSE_PID", reuse_pid.to_string())
        .env("EZ_TEST_PS_COUNTER", dir.join("ps-counter"))
        .output()
        .expect("run ez with fake dev tools")
}

fn run_ez_with_worktree_replacement_race(
    dir: &Path,
    args: &[&str],
    target_path: &Path,
    replacement_branch: &str,
) -> Output {
    let fake_bin = dir.join(".fake-git-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake git bin");
    let wrapper = fake_bin.join("git");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\n\
         if [ \"$1 $2\" = \"worktree prune\" ] && [ ! -e \"$EZ_TEST_RACE_MARKER\" ]; then\n\
         \t: > \"$EZ_TEST_RACE_MARKER\"\n\
         \t\"$EZ_TEST_REAL_GIT\" worktree remove --force --force \"$EZ_TEST_TARGET_WORKTREE\"\n\
         \t\"$EZ_TEST_REAL_GIT\" worktree add \"$EZ_TEST_TARGET_WORKTREE\" \"$EZ_TEST_REPLACEMENT_BRANCH\"\n\
         fi\n\
         exec \"$EZ_TEST_REAL_GIT\" \"$@\"\n",
    )
    .expect("write fake git");
    let mut permissions = std::fs::metadata(&wrapper)
        .expect("stat fake git")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions).expect("chmod fake git");
    let real_git = stdout_text(&run(dir, "which", &["git"]));
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fake_bin];
    paths.extend(std::env::split_paths(&original_path));
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("PATH", std::env::join_paths(paths).expect("join fake PATH"))
        .env("EZ_TEST_REAL_GIT", real_git)
        .env("EZ_TEST_TARGET_WORKTREE", target_path)
        .env("EZ_TEST_REPLACEMENT_BRANCH", replacement_branch)
        .env("EZ_TEST_RACE_MARKER", dir.join("replacement-race"))
        .output()
        .expect("run ez with worktree replacement race")
}

fn run_ez_with_branch_delete_failure(dir: &Path, args: &[&str], branch: &str) -> Output {
    let fake_bin = dir.join(".fake-branch-delete-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake branch delete bin");
    let wrapper = fake_bin.join("git");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\n\
         if [ \"$1 $2 $3\" = \"branch -D $EZ_TEST_DELETE_BRANCH\" ]; then\n\
         \techo 'simulated branch ref lock failure' >&2\n\
         \texit 1\n\
         fi\n\
         exec \"$EZ_TEST_REAL_GIT\" \"$@\"\n",
    )
    .expect("write fake git");
    let mut permissions = std::fs::metadata(&wrapper)
        .expect("stat fake git")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions).expect("chmod fake git");
    let real_git = stdout_text(&run(dir, "which", &["git"]));
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fake_bin];
    paths.extend(std::env::split_paths(&original_path));
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("PATH", std::env::join_paths(paths).expect("join fake PATH"))
        .env("EZ_TEST_REAL_GIT", real_git)
        .env("EZ_TEST_DELETE_BRANCH", branch)
        .output()
        .expect("run ez with branch delete failure")
}

fn run_ez_with_post_unlock_replacement(
    dir: &Path,
    args: &[&str],
    original_path: &Path,
    replacement_branch: &str,
) -> Output {
    let fake_bin = dir.join(".fake-post-unlock-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake post-unlock bin");
    let wrapper = fake_bin.join("git");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\n\
         if [ \"$1 $2\" = \"worktree unlock\" ] && [ ! -e \"$EZ_TEST_RACE_MARKER\" ]; then\n\
         \t\"$EZ_TEST_REAL_GIT\" \"$@\"\n\
         \tstatus=$?\n\
         \t: > \"$EZ_TEST_RACE_MARKER\"\n\
         \t\"$EZ_TEST_REAL_GIT\" worktree add \"$EZ_TEST_ORIGINAL_WORKTREE\" \"$EZ_TEST_REPLACEMENT_BRANCH\"\n\
         \texit $status\n\
         fi\n\
         exec \"$EZ_TEST_REAL_GIT\" \"$@\"\n",
    )
    .expect("write fake git");
    let mut permissions = std::fs::metadata(&wrapper)
        .expect("stat fake git")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions).expect("chmod fake git");
    let real_git = stdout_text(&run(dir, "which", &["git"]));
    let original_path_env = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fake_bin];
    paths.extend(std::env::split_paths(&original_path_env));
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("PATH", std::env::join_paths(paths).expect("join fake PATH"))
        .env("EZ_TEST_REAL_GIT", real_git)
        .env("EZ_TEST_ORIGINAL_WORKTREE", original_path)
        .env("EZ_TEST_REPLACEMENT_BRANCH", replacement_branch)
        .env("EZ_TEST_RACE_MARKER", dir.join("post-unlock-race"))
        .output()
        .expect("run ez with post-unlock replacement")
}

fn run_ez_with_quarantine_unlock_failure(dir: &Path, args: &[&str]) -> Output {
    let fake_bin = dir.join(".fake-quarantine-unlock-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake quarantine unlock bin");
    let wrapper = fake_bin.join("git");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\n\
         case \"$1 $2 $3\" in\n\
         \"worktree unlock \"*.ez-delete-*)\n\
         \tif [ ! -e \"$EZ_TEST_UNLOCK_MARKER\" ]; then\n\
         \t\t: > \"$EZ_TEST_UNLOCK_MARKER\"\n\
         \t\techo 'simulated quarantine unlock failure' >&2\n\
         \t\texit 1\n\
         \tfi\n\
         \t;;\n\
         esac\n\
         exec \"$EZ_TEST_REAL_GIT\" \"$@\"\n",
    )
    .expect("write fake git");
    let mut permissions = std::fs::metadata(&wrapper)
        .expect("stat fake git")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions).expect("chmod fake git");
    let real_git = stdout_text(&run(dir, "which", &["git"]));
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fake_bin];
    paths.extend(std::env::split_paths(&original_path));
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("PATH", std::env::join_paths(paths).expect("join fake PATH"))
        .env("EZ_TEST_REAL_GIT", real_git)
        .env(
            "EZ_TEST_UNLOCK_MARKER",
            dir.join("quarantine-unlock-failed"),
        )
        .output()
        .expect("run ez with quarantine unlock failure")
}

fn run_ez_with_prune_failure(dir: &Path, args: &[&str]) -> Output {
    let fake_bin = dir.join(".fake-prune-failure-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake prune failure bin");
    let wrapper = fake_bin.join("git");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\n\
         if [ \"$1 $2\" = \"worktree prune\" ]; then\n\
         \techo 'simulated worktree prune failure' >&2\n\
         \texit 1\n\
         fi\n\
         exec \"$EZ_TEST_REAL_GIT\" \"$@\"\n",
    )
    .expect("write fake git");
    let mut permissions = std::fs::metadata(&wrapper)
        .expect("stat fake git")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions).expect("chmod fake git");
    let real_git = stdout_text(&run(dir, "which", &["git"]));
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fake_bin];
    paths.extend(std::env::split_paths(&original_path));
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("PATH", std::env::join_paths(paths).expect("join fake PATH"))
        .env("EZ_TEST_REAL_GIT", real_git)
        .output()
        .expect("run ez with prune failure")
}

fn run_ez_with_listener_lookup_failure(dir: &Path, args: &[&str], branch: &str) -> Output {
    let fake_bin = dir.join(".fake-listener-lookup-failure-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake listener failure bin");
    let lsof = fake_bin.join("lsof");
    let kill = fake_bin.join("kill");
    std::fs::write(
        &lsof,
        "#!/bin/sh\n\
         if [ \"$*\" = \"-nP -iTCP:$EZ_TEST_EXPECTED_PORT -sTCP:LISTEN -t\" ]; then\n\
         \techo 'simulated lsof ownership failure' >&2\n\
         \texit 2\n\
         fi\n\
         echo \"unexpected lsof args: $*\" >&2\n\
         exit 97\n",
    )
    .expect("write fake lsof");
    std::fs::write(
        &kill,
        "#!/bin/sh\n\
         echo 'kill should not be called after listener lookup failure' >&2\n\
         exit 97\n",
    )
    .expect("write fake kill");
    for script in [&lsof, &kill] {
        let mut permissions = std::fs::metadata(script)
            .expect("stat fake listener tool")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(script, permissions).expect("chmod fake listener tool");
    }
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fake_bin];
    paths.extend(std::env::split_paths(&original_path));
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("PATH", std::env::join_paths(paths).expect("join fake PATH"))
        .env("EZ_TEST_EXPECTED_PORT", dev_port(branch).to_string())
        .output()
        .expect("run ez with listener lookup failure")
}

fn run_ez_with_kill_failure(dir: &Path, args: &[&str], branch: &str, process_cwd: &Path) -> Output {
    let fake_bin = dir.join(".fake-kill-failure-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake kill failure bin");
    let lsof = fake_bin.join("lsof");
    let kill = fake_bin.join("kill");
    let ps = fake_bin.join("ps");
    std::fs::write(
        &lsof,
        "#!/bin/sh\n\
         if [ \"$*\" = \"-nP -iTCP:$EZ_TEST_EXPECTED_PORT -sTCP:LISTEN -t\" ]; then\n\
         \tprintf '4242\\n'\n\
         elif [ \"$*\" = \"-a -p 4242 -d cwd -Fn\" ]; then\n\
         \tprintf 'p4242\\nfcwd\\nn%s\\n' \"$EZ_TEST_PROCESS_CWD\"\n\
         else\n\
         \techo \"unexpected lsof args: $*\" >&2\n\
         \texit 97\n\
         fi\n",
    )
    .expect("write fake lsof");
    std::fs::write(
        &ps,
        "#!/bin/sh\n\
         if [ \"$*\" != \"-o lstart= -p 4242\" ]; then\n\
         \techo \"unexpected ps args: $*\" >&2\n\
         \texit 97\n\
         fi\n\
         printf 'Mon Jan  1 00:00:00 2024\\n'\n",
    )
    .expect("write fake ps");
    std::fs::write(
        &kill,
        "#!/bin/sh\n\
         if [ \"$*\" != \"-TERM 4242\" ]; then\n\
         \techo \"unexpected kill args: $*\" >&2\n\
         \texit 97\n\
         fi\n\
         echo 'simulated kill failure' >&2\n\
         exit 1\n",
    )
    .expect("write fake kill");
    for script in [&lsof, &ps, &kill] {
        let mut permissions = std::fs::metadata(script)
            .expect("stat fake kill tool")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(script, permissions).expect("chmod fake kill tool");
    }
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fake_bin];
    paths.extend(std::env::split_paths(&original_path));
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("PATH", std::env::join_paths(paths).expect("join fake PATH"))
        .env("EZ_TEST_EXPECTED_PORT", dev_port(branch).to_string())
        .env("EZ_TEST_PROCESS_CWD", process_cwd)
        .output()
        .expect("run ez with kill failure")
}

fn run_ez_with_remove_failure_and_blocked_rollback(
    dir: &Path,
    args: &[&str],
    original_worktree: &Path,
) -> Output {
    let fake_bin = dir.join(".fake-remove-rollback-failure-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake remove rollback failure bin");
    let wrapper = fake_bin.join("git");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\n\
         case \"$1 $2 $3\" in\n\
         \"worktree remove \"*.ez-delete-*|\"worktree remove --force\")\n\
         \tmkdir -p \"$EZ_TEST_ORIGINAL_WORKTREE\"\n\
         \techo 'simulated worktree remove failure' >&2\n\
         \texit 1\n\
         \t;;\n\
         esac\n\
         exec \"$EZ_TEST_REAL_GIT\" \"$@\"\n",
    )
    .expect("write fake git");
    let mut permissions = std::fs::metadata(&wrapper)
        .expect("stat fake git")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions).expect("chmod fake git");
    let real_git = stdout_text(&run(dir, "which", &["git"]));
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fake_bin];
    paths.extend(std::env::split_paths(&original_path));
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("PATH", std::env::join_paths(paths).expect("join fake PATH"))
        .env("EZ_TEST_REAL_GIT", real_git)
        .env("EZ_TEST_ORIGINAL_WORKTREE", original_worktree)
        .output()
        .expect("run ez with remove rollback failure")
}

fn run_ez_with_branch_delete_and_worktree_add_failure(
    dir: &Path,
    args: &[&str],
    branch: &str,
) -> Output {
    let fake_bin = dir.join(".fake-branch-delete-add-failure-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake branch delete add failure bin");
    let wrapper = fake_bin.join("git");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\n\
         if [ \"$1 $2 $3\" = \"branch -D $EZ_TEST_DELETE_BRANCH\" ]; then\n\
         \techo 'simulated branch ref lock failure' >&2\n\
         \texit 1\n\
         fi\n\
         if [ \"$1 $2\" = \"worktree add\" ]; then\n\
         \techo 'simulated worktree restore failure' >&2\n\
         \texit 1\n\
         fi\n\
         exec \"$EZ_TEST_REAL_GIT\" \"$@\"\n",
    )
    .expect("write fake git");
    let mut permissions = std::fs::metadata(&wrapper)
        .expect("stat fake git")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions).expect("chmod fake git");
    let real_git = stdout_text(&run(dir, "which", &["git"]));
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fake_bin];
    paths.extend(std::env::split_paths(&original_path));
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("PATH", std::env::join_paths(paths).expect("join fake PATH"))
        .env("EZ_TEST_REAL_GIT", real_git)
        .env("EZ_TEST_DELETE_BRANCH", branch)
        .output()
        .expect("run ez with branch delete and worktree add failure")
}

fn run_ez_with_fake_delete_integrations(
    dir: &Path,
    args: &[&str],
    remote_log: &Path,
    gh_log: &Path,
    fail_remote_delete: bool,
    fail_pr_edit: bool,
) -> Output {
    let fake_bin = dir.join(".fake-delete-integrations-bin");
    std::fs::create_dir_all(&fake_bin).expect("create fake integration bin");
    let git = fake_bin.join("git");
    let gh = fake_bin.join("gh");
    std::fs::write(
        &git,
        "#!/bin/sh\n\
         if [ \"$1 $3\" = \"push --delete\" ]; then\n\
         \tprintf '%s\\n' \"$*\" >> \"$EZ_TEST_REMOTE_LOG\"\n\
         \tif [ \"$EZ_TEST_FAIL_REMOTE_DELETE\" = \"true\" ]; then\n\
         \t\techo 'simulated remote delete failure' >&2\n\
         \t\texit 1\n\
         \tfi\n\
         \texit 0\n\
         fi\n\
         exec \"$EZ_TEST_REAL_GIT\" \"$@\"\n",
    )
    .expect("write fake git");
    std::fs::write(
        &gh,
        "#!/bin/sh\n\
         printf '%s\\n' \"$*\" >> \"$GH_LOG\"\n\
         if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"edit\" ]; then\n\
         \tif [ \"$EZ_TEST_FAIL_PR_EDIT\" = \"true\" ]; then\n\
         \t\techo 'simulated PR edit failure' >&2\n\
         \t\texit 1\n\
         \tfi\n\
         \texit 0\n\
         fi\n\
         printf 'unexpected gh invocation: %s\\n' \"$*\" >&2\n\
         exit 97\n",
    )
    .expect("write fake gh");
    for script in [&git, &gh] {
        let mut permissions = std::fs::metadata(script)
            .expect("stat fake integration tool")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(script, permissions).expect("chmod fake integration tool");
    }
    let real_git = stdout_text(&run(dir, "which", &["git"]));
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fake_bin];
    paths.extend(std::env::split_paths(&original_path));
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("PATH", std::env::join_paths(paths).expect("join fake PATH"))
        .env("EZ_TEST_REAL_GIT", real_git)
        .env("EZ_TEST_REMOTE_LOG", remote_log)
        .env("GH_LOG", gh_log)
        .env("EZ_TEST_FAIL_REMOTE_DELETE", fail_remote_delete.to_string())
        .env("EZ_TEST_FAIL_PR_EDIT", fail_pr_edit.to_string())
        .output()
        .expect("run ez with fake delete integrations")
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout JSON")
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(dir.join(file), contents).expect("write commit file");
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
}

fn branch_tip(repo: &Path, branch: &str) -> String {
    stdout_text(&run(repo, "git", &["rev-parse", branch]))
}

fn current_branch(worktree: &Path) -> String {
    stdout_text(&run(worktree, "git", &["branch", "--show-current"]))
}

fn status_porcelain(worktree: &Path) -> String {
    stdout_text(&run(worktree, "git", &["status", "--porcelain"]))
}

fn set_pr_number(repo: &TempRepo, branch: &str, pr_number: u64) {
    let mut state = repo.stack_state();
    state["branches"][branch]["pr_number"] = Value::from(pr_number);
    save_stack_state(repo, &state);
}

fn set_stack_repo(repo: &TempRepo, github_repo: &str) {
    let mut state = repo.stack_state();
    state["repo"] = Value::from(github_repo);
    save_stack_state(repo, &state);
}

fn save_stack_state(repo: &TempRepo, state: &Value) {
    let common_dir = stdout_text(&run(&repo.path, "git", &["rev-parse", "--git-common-dir"]));
    let common_dir = PathBuf::from(common_dir);
    let common_dir = if common_dir.is_absolute() {
        common_dir
    } else {
        repo.path.join(common_dir)
    };
    std::fs::write(
        common_dir.join("ez/stack.json"),
        serde_json::to_vec_pretty(state).expect("serialize stack state"),
    )
    .expect("write stack state");
}

fn dev_port(branch: &str) -> u16 {
    let mut hash: u32 = 5381;
    for byte in branch.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(byte as u32);
    }
    10000 + (hash % 10000) as u16
}

fn assert_ancestor(repo: &Path, ancestor: &str, descendant: &str) {
    run(
        repo,
        "git",
        &["merge-base", "--is-ancestor", ancestor, descendant],
    );
}

fn assert_no_rebase_state(worktree: &Path) {
    assert!(
        !rebase_state_exists(worktree),
        "rebase state should be cleaned up"
    );
}

fn rebase_state_exists(worktree: &Path) -> bool {
    for state_dir in ["rebase-merge", "rebase-apply"] {
        let raw = stdout_text(&run(
            worktree,
            "git",
            &["rev-parse", "--git-path", state_dir],
        ));
        let path = PathBuf::from(raw);
        let path = if path.is_absolute() {
            path
        } else {
            worktree.join(path)
        };
        if path.exists() {
            return true;
        }
    }
    false
}

#[test]
fn restack_rebases_branches_inside_their_linked_worktrees() {
    let stack = LinearStack::new();
    let child_before = branch_tip(&stack.repo.path, "feat/child");
    commit_file(
        &stack.base_worktree,
        "base-advance.txt",
        "advance\n",
        "advance base",
    );

    run_ez(&stack.repo.path, &["restack"]);

    let child_after = branch_tip(&stack.repo.path, "feat/child");
    assert_ne!(child_after, child_before);
    assert_ancestor(&stack.repo.path, "feat/base", "feat/child");
    assert_eq!(current_branch(&stack.base_worktree), "feat/base");
    assert_eq!(current_branch(&stack.child_worktree), "feat/child");
    assert_eq!(status_porcelain(&stack.base_worktree), "");
    assert_eq!(status_porcelain(&stack.child_worktree), "");
    assert_eq!(
        stack.repo.stack_state()["branches"]["feat/child"]["parent_head"],
        branch_tip(&stack.repo.path, "feat/base")
    );
}

#[test]
fn restack_preserves_dirty_child_worktree_and_leaves_it_retryable() {
    let stack = LinearStack::new();
    run(
        &stack.repo.path,
        "git",
        &["config", "rebase.autoStash", "true"],
    );
    commit_file(
        &stack.base_worktree,
        "tracked.txt",
        "base changed\n",
        "advance base",
    );
    let child_before = branch_tip(&stack.repo.path, "feat/child");
    let parent_head_before =
        stack.repo.stack_state()["branches"]["feat/child"]["parent_head"].clone();
    std::fs::write(
        stack.child_worktree.join("tracked.txt"),
        "dirty child edit\n",
    )
    .expect("write dirty edit");

    let output = run_ez_raw(&stack.repo.path, &["restack"]);

    assert!(!output.status.success());
    assert_eq!(branch_tip(&stack.repo.path, "feat/child"), child_before);
    assert_eq!(
        std::fs::read_to_string(stack.child_worktree.join("tracked.txt")).expect("read dirty edit"),
        "dirty child edit\n"
    );
    assert_eq!(current_branch(&stack.child_worktree), "feat/child");
    assert_eq!(
        stack.repo.stack_state()["branches"]["feat/child"]["parent_head"],
        parent_head_before
    );
    assert_ne!(
        stack.repo.stack_state()["branches"]["feat/child"]["parent_head"],
        branch_tip(&stack.repo.path, "feat/base")
    );
    assert_no_rebase_state(&stack.child_worktree);
}

#[test]
fn restack_cleans_up_a_conflicting_worktree_and_continues_to_a_sibling() {
    let repo = TempRepo::new();
    let bad_worktree = repo.create_worktree("feat/bad", "main");
    commit_file(
        &bad_worktree,
        "tracked.txt",
        "bad branch\n",
        "conflicting branch",
    );
    let good_worktree = repo.create_worktree("feat/good", "main");
    commit_file(&good_worktree, "good.txt", "good\n", "good branch");
    let bad_before = branch_tip(&repo.path, "feat/bad");
    let good_before = branch_tip(&repo.path, "feat/good");
    commit_file(
        &repo.path,
        "tracked.txt",
        "main branch\n",
        "conflicting trunk",
    );

    let output = run_ez_raw(&repo.path, &["restack"]);

    assert!(!output.status.success());
    assert_eq!(branch_tip(&repo.path, "feat/bad"), bad_before);
    assert_ne!(branch_tip(&repo.path, "feat/good"), good_before);
    assert_ancestor(&repo.path, "main", "feat/good");
    assert_eq!(current_branch(&bad_worktree), "feat/bad");
    assert_eq!(current_branch(&good_worktree), "feat/good");
    assert_eq!(status_porcelain(&bad_worktree), "");
    assert_eq!(status_porcelain(&good_worktree), "");
    assert_no_rebase_state(&bad_worktree);
    let state = repo.stack_state();
    assert_ne!(
        state["branches"]["feat/bad"]["parent_head"],
        branch_tip(&repo.path, "main")
    );
    assert_eq!(
        state["branches"]["feat/good"]["parent_head"],
        branch_tip(&repo.path, "main")
    );
}

#[test]
fn restack_does_not_abort_a_rebase_started_in_another_worktree() {
    let repo = TempRepo::new();
    let base_worktree = repo.create_worktree("feat/base", "main");
    commit_file(
        &base_worktree,
        "tracked.txt",
        "base baseline\n",
        "base baseline",
    );
    let child_worktree = repo.create_worktree("feat/child", "feat/base");
    commit_file(
        &child_worktree,
        "tracked.txt",
        "child change\n",
        "child change",
    );
    commit_file(
        &base_worktree,
        "tracked.txt",
        "base changed\n",
        "base changed",
    );
    let external_rebase = run_raw(&child_worktree, "git", &["rebase", "feat/base"]);
    assert!(!external_rebase.status.success());
    assert!(rebase_state_exists(&child_worktree));
    let status_before = status_porcelain(&child_worktree);
    let child_tip_before = branch_tip(&repo.path, "feat/child");
    let state_before = repo.stack_state();

    let output = run_ez_raw(&repo.path, &["restack"]);

    assert!(!output.status.success());
    assert!(rebase_state_exists(&child_worktree));
    assert_eq!(status_porcelain(&child_worktree), status_before);
    assert_eq!(branch_tip(&repo.path, "feat/child"), child_tip_before);
    assert_eq!(repo.stack_state(), state_before);
}

#[test]
fn restack_aborts_rebase_state_created_by_a_generic_git_failure() {
    let stack = LinearStack::new();
    commit_file(
        &stack.base_worktree,
        "base-advance.txt",
        "advance\n",
        "advance base",
    );
    let child_tip_before = branch_tip(&stack.repo.path, "feat/child");
    let state_before = stack.repo.stack_state();
    run(
        &stack.repo.path,
        "git",
        &["config", "commit.gpgSign", "true"],
    );
    run(&stack.repo.path, "git", &["config", "gpg.program", "false"]);

    let output = run_ez_raw(&stack.repo.path, &["restack"]);

    assert!(!output.status.success());
    assert_eq!(branch_tip(&stack.repo.path, "feat/child"), child_tip_before);
    assert_eq!(stack.repo.stack_state(), state_before);
    assert_eq!(current_branch(&stack.child_worktree), "feat/child");
    assert_eq!(status_porcelain(&stack.child_worktree), "");
    assert_no_rebase_state(&stack.child_worktree);
}

#[test]
fn move_from_a_linked_worktree_restacks_descendant_worktrees() {
    let repo = TempRepo::new();
    let a_worktree = repo.create_worktree("feat/a", "main");
    commit_file(&a_worktree, "a.txt", "a\n", "a");
    let b_worktree = repo.create_worktree("feat/b", "feat/a");
    commit_file(&b_worktree, "b.txt", "b\n", "b");
    let c_worktree = repo.create_worktree("feat/c", "feat/b");
    commit_file(&c_worktree, "c.txt", "c\n", "c");
    let c_before = branch_tip(&repo.path, "feat/c");

    run_ez(&b_worktree, &["move", "--onto", "main"]);

    let state = repo.stack_state();
    assert_eq!(state["branches"]["feat/b"]["parent"], "main");
    assert_eq!(state["branches"]["feat/c"]["parent"], "feat/b");
    assert_eq!(
        state["branches"]["feat/c"]["parent_head"],
        branch_tip(&repo.path, "feat/b")
    );
    assert_ne!(branch_tip(&repo.path, "feat/c"), c_before);
    assert_ancestor(&repo.path, "feat/b", "feat/c");
    assert_eq!(current_branch(&b_worktree), "feat/b");
    assert_eq!(current_branch(&c_worktree), "feat/c");
    assert_eq!(status_porcelain(&b_worktree), "");
    assert_eq!(status_porcelain(&c_worktree), "");
}

#[test]
fn move_derives_the_replay_range_after_external_worktree_history_changes() {
    let repo = TempRepo::new();
    let a_worktree = repo.create_worktree("feat/a", "main");
    commit_file(&a_worktree, "a.txt", "a\n", "a");
    let b_worktree = repo.create_worktree("feat/b", "feat/a");
    commit_file(&b_worktree, "b.txt", "b\n", "b");
    commit_file(&a_worktree, "a-advance.txt", "parent only\n", "advance a");
    run(&b_worktree, "git", &["rebase", "feat/a"]);
    assert!(b_worktree.join("a-advance.txt").exists());

    run_ez(&b_worktree, &["move", "--onto", "main"]);

    assert!(!b_worktree.join("a.txt").exists());
    assert!(
        !b_worktree.join("a-advance.txt").exists(),
        "move must not replay a former parent's commits when parent_head metadata is stale"
    );
    assert!(b_worktree.join("b.txt").exists());
    assert_eq!(repo.stack_state()["branches"]["feat/b"]["parent"], "main");
}

#[test]
fn move_conflict_keeps_the_linked_worktree_attached_and_state_unchanged() {
    let repo = TempRepo::new();
    let a_worktree = repo.create_worktree("feat/a", "main");
    commit_file(&a_worktree, "tracked.txt", "a changed\n", "conflicting a");
    let target_worktree = repo.create_worktree("feat/target", "main");
    commit_file(
        &target_worktree,
        "tracked.txt",
        "target changed\n",
        "conflicting target",
    );
    let branch_before = branch_tip(&repo.path, "feat/a");
    let state_before = repo.stack_state();

    let output = run_ez_raw(&a_worktree, &["move", "--onto", "feat/target"]);

    assert!(!output.status.success());
    assert_eq!(branch_tip(&repo.path, "feat/a"), branch_before);
    assert_eq!(repo.stack_state(), state_before);
    assert_eq!(current_branch(&a_worktree), "feat/a");
    assert_eq!(status_porcelain(&a_worktree), "");
    assert_no_rebase_state(&a_worktree);
}

#[test]
fn move_rejects_a_dirty_linked_worktree_without_losing_edits_or_state() {
    let repo = TempRepo::new();
    run(&repo.path, "git", &["config", "rebase.autoStash", "true"]);
    let a_worktree = repo.create_worktree("feat/a", "main");
    commit_file(&a_worktree, "a.txt", "a\n", "a");
    let target_worktree = repo.create_worktree("feat/target", "main");
    commit_file(
        &target_worktree,
        "tracked.txt",
        "target changed\n",
        "target",
    );
    let branch_before = branch_tip(&repo.path, "feat/a");
    let state_before = repo.stack_state();
    std::fs::write(a_worktree.join("tracked.txt"), "dirty a edit\n").expect("write dirty edit");

    let output = run_ez_raw(&a_worktree, &["move", "--onto", "feat/target"]);

    assert!(!output.status.success());
    assert_eq!(branch_tip(&repo.path, "feat/a"), branch_before);
    assert_eq!(repo.stack_state(), state_before);
    assert_eq!(current_branch(&a_worktree), "feat/a");
    assert_eq!(
        std::fs::read_to_string(a_worktree.join("tracked.txt")).expect("read dirty edit"),
        "dirty a edit\n"
    );
    assert_no_rebase_state(&a_worktree);
}

#[test]
fn delete_dirty_linked_worktree_does_not_stop_its_process_or_change_user_state() {
    let repo = TempRepo::new();
    let branch = "feat/live-dirty";
    let live_worktree = repo.create_worktree(branch, "main");
    commit_file(&live_worktree, "live.txt", "live\n", "live");
    let kill_marker = repo.path.join("kill-attempted");
    let branch_before = branch_tip(&repo.path, branch);
    let state_before = repo.stack_state();
    std::fs::write(live_worktree.join("tracked.txt"), "dirty live edit\n")
        .expect("write dirty edit");

    let output = run_ez_with_fake_dev_tools(
        &repo.path,
        &["delete", branch],
        branch,
        &live_worktree,
        &kill_marker,
        false,
    );

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Could not remove worktree"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !kill_marker.exists(),
        "a failed delete must not stop the worktree's live process"
    );
    assert_eq!(branch_tip(&repo.path, branch), branch_before);
    assert_eq!(repo.stack_state(), state_before);
    assert!(live_worktree.exists());
    assert_eq!(current_branch(&live_worktree), branch);
    assert_eq!(
        std::fs::read_to_string(live_worktree.join("tracked.txt")).expect("read dirty edit"),
        "dirty live edit\n"
    );
    assert_ne!(status_porcelain(&live_worktree), "");
}

#[test]
fn delete_defaults_to_current_branch_reparents_child_updates_pr_base_and_deletes_remote() {
    let repo = TempRepo::new();
    let target = "feat/default-delete";
    let child = "feat/default-child";
    run_ez(
        &repo.path,
        &["create", target, "--from", "main", "--no-worktree"],
    );
    run(&repo.path, "git", &["switch", target]);
    commit_file(&repo.path, "target.txt", "target\n", "target");
    run_ez(
        &repo.path,
        &["create", child, "--from", target, "--no-worktree"],
    );
    run(&repo.path, "git", &["switch", child]);
    commit_file(&repo.path, "child.txt", "child\n", "child");
    run(&repo.path, "git", &["switch", target]);
    let child_parent_head_before = repo.stack_state()["branches"][child]["parent_head"].clone();
    set_stack_repo(&repo, "org/repo");
    set_pr_number(&repo, target, 101);
    set_pr_number(&repo, child, 202);
    let remote_log = repo.path.join("remote-delete.log");
    let gh_log = repo.path.join("gh.log");

    let output = run_ez_with_fake_delete_integrations(
        &repo.path,
        &["delete", "--force", "--yes"],
        &remote_log,
        &gh_log,
        false,
        false,
    );

    assert!(
        output.status.success(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(current_branch(&repo.path), "main");
    assert!(
        !run_raw(&repo.path, "git", &["rev-parse", target])
            .status
            .success()
    );
    let state = repo.stack_state();
    assert!(state["branches"][target].is_null());
    assert_eq!(state["branches"][child]["parent"], "main");
    assert_eq!(
        state["branches"][child]["parent_head"],
        child_parent_head_before
    );
    assert_eq!(
        std::fs::read_to_string(&gh_log).expect("read gh log"),
        "pr edit 202 --base main --repo org/repo\n"
    );
    assert_eq!(
        std::fs::read_to_string(&remote_log).expect("read remote log"),
        "push origin --delete feat/default-delete\n"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("\"branch\":\"feat/default-delete\""));
    assert!(stderr.contains("\"reparented_children\":1"));
}

#[test]
fn delete_remote_failure_is_ignored_after_local_branch_delete_succeeds() {
    let repo = TempRepo::new();
    let branch = "feat/remote-failure";
    run_ez(
        &repo.path,
        &["create", branch, "--from", "main", "--no-worktree"],
    );
    run(&repo.path, "git", &["switch", branch]);
    commit_file(&repo.path, "remote-failure.txt", "remote\n", "remote");
    run(&repo.path, "git", &["switch", "main"]);
    let remote_log = repo.path.join("remote-delete.log");
    let gh_log = repo.path.join("gh.log");

    let output = run_ez_with_fake_delete_integrations(
        &repo.path,
        &["delete", branch, "--force", "--yes"],
        &remote_log,
        &gh_log,
        true,
        false,
    );

    assert!(
        output.status.success(),
        "remote branch deletion is best-effort:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !run_raw(&repo.path, "git", &["rev-parse", branch])
            .status
            .success()
    );
    assert!(repo.stack_state()["branches"][branch].is_null());
    assert_eq!(
        std::fs::read_to_string(&remote_log).expect("read remote log"),
        "push origin --delete feat/remote-failure\n"
    );
    assert!(
        !gh_log.exists()
            || std::fs::read_to_string(&gh_log)
                .expect("read gh log")
                .is_empty()
    );
}

#[test]
fn delete_pr_base_update_failure_warns_but_preserves_completed_delete() {
    let repo = TempRepo::new();
    let target = "feat/pr-update-fails";
    let child = "feat/pr-child";
    run_ez(
        &repo.path,
        &["create", target, "--from", "main", "--no-worktree"],
    );
    run(&repo.path, "git", &["switch", target]);
    commit_file(&repo.path, "target.txt", "target\n", "target");
    run_ez(
        &repo.path,
        &["create", child, "--from", target, "--no-worktree"],
    );
    run(&repo.path, "git", &["switch", child]);
    commit_file(&repo.path, "child.txt", "child\n", "child");
    run(&repo.path, "git", &["switch", "main"]);
    set_stack_repo(&repo, "org/repo");
    set_pr_number(&repo, target, 301);
    set_pr_number(&repo, child, 302);
    let remote_log = repo.path.join("remote-delete.log");
    let gh_log = repo.path.join("gh.log");

    let output = run_ez_with_fake_delete_integrations(
        &repo.path,
        &["delete", target, "--force", "--yes"],
        &remote_log,
        &gh_log,
        false,
        true,
    );

    assert!(
        output.status.success(),
        "PR repair warning should not roll back delete:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Failed to update PR base for `feat/pr-child`"));
    assert!(repo.stack_state()["branches"][target].is_null());
    assert_eq!(repo.stack_state()["branches"][child]["parent"], "main");
    assert_eq!(
        std::fs::read_to_string(&gh_log).expect("read gh log"),
        "pr edit 302 --base main --repo org/repo\n"
    );
}

#[test]
fn delete_rejects_trunk_without_changing_repository_or_stack_state() {
    let repo = TempRepo::new();
    let state_before = repo.stack_state();
    let branch_before = current_branch(&repo.path);

    let output = run_ez_raw(&repo.path, &["delete", "main", "--yes"]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("on trunk"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(current_branch(&repo.path), branch_before);
    assert_eq!(repo.stack_state(), state_before);
    assert!(
        run_raw(&repo.path, "git", &["rev-parse", "main"])
            .status
            .success()
    );
}

#[test]
fn delete_rejects_unmanaged_branch_without_changing_repository_or_stack_state() {
    let repo = TempRepo::new();
    run(&repo.path, "git", &["branch", "unmanaged"]);
    let state_before = repo.stack_state();

    let output = run_ez_raw(&repo.path, &["delete", "unmanaged", "--yes"]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not tracked by ez"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(repo.stack_state(), state_before);
    assert!(
        run_raw(&repo.path, "git", &["rev-parse", "unmanaged"])
            .status
            .success()
    );
}

#[test]
fn delete_inside_linked_worktree_without_yes_cancel_preserves_everything() {
    let repo = TempRepo::new();
    let branch = "feat/confirm-cancel";
    let worktree = repo.create_worktree(branch, "main");
    commit_file(&worktree, "confirm.txt", "confirm\n", "confirm");
    let branch_before = branch_tip(&repo.path, branch);
    let state_before = repo.stack_state();

    let output = run_ez_raw(&worktree, &["delete"]);

    assert!(
        output.status.success(),
        "prompt cancellation is a no-op success:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("You are inside the worktree"));
    assert!(stderr.contains("Cancelled"));
    assert!(worktree.exists());
    assert_eq!(current_branch(&worktree), branch);
    assert_eq!(branch_tip(&repo.path, branch), branch_before);
    assert_eq!(repo.stack_state(), state_before);
}

#[test]
fn delete_clean_linked_worktree_stops_its_process_and_removes_the_workspace() {
    let repo = TempRepo::new();
    let branch = "feat/live-clean";
    let live_worktree = repo.create_worktree(branch, "main");
    commit_file(&live_worktree, "live.txt", "live\n", "live");
    let kill_marker = repo.path.join("kill-attempted");

    let output = run_ez_with_fake_dev_tools(
        &repo.path,
        &["delete", branch, "--yes"],
        branch,
        &live_worktree,
        &kill_marker,
        false,
    );

    assert!(output.status.success());
    assert!(kill_marker.exists());
    assert!(!live_worktree.exists());
    assert!(
        !run_raw(&repo.path, "git", &["rev-parse", branch])
            .status
            .success()
    );
    assert!(repo.stack_state()["branches"][branch].is_null());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("\"cmd\":\"delete\""));
    assert!(stderr.contains("\"killed_pids\":[4242]"));
    assert!(stderr.contains(live_worktree.to_string_lossy().as_ref()));
}

#[test]
fn delete_stale_worktree_registration_preserves_unknown_directory_contents() {
    let repo = TempRepo::new();
    let stale_worktree = repo.create_worktree("feat/stale", "main");
    commit_file(&stale_worktree, "stale.txt", "stale\n", "stale");
    std::fs::write(stale_worktree.join("keep-me.txt"), "user data\n").expect("write user data");
    std::fs::remove_file(stale_worktree.join(".git")).expect("remove worktree git file");
    let branch_before = branch_tip(&repo.path, "feat/stale");
    let state_before = repo.stack_state();

    let output = run_ez_raw(&repo.path, &["delete", "feat/stale"]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("refuse to remove"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(stale_worktree.join("keep-me.txt")).expect("read user data"),
        "user data\n"
    );
    assert_eq!(branch_tip(&repo.path, "feat/stale"), branch_before);
    assert_eq!(repo.stack_state(), state_before);
}

#[test]
fn delete_missing_registered_worktree_path_is_refused_without_mutation() {
    let repo = TempRepo::new();
    let branch = "feat/missing-path";
    let worktree = repo.create_worktree(branch, "main");
    commit_file(&worktree, "missing.txt", "missing\n", "missing");
    std::fs::remove_dir_all(&worktree).expect("remove registered worktree path");
    let branch_before = branch_tip(&repo.path, branch);
    let state_before = repo.stack_state();

    let output = run_ez_raw(&repo.path, &["delete", branch, "--yes"]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("registered worktree path is missing"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(branch_tip(&repo.path, branch), branch_before);
    assert_eq!(repo.stack_state(), state_before);
}

#[test]
fn delete_managed_branch_checked_out_in_main_worktree_keeps_the_repository() {
    let repo = TempRepo::new();
    run_ez(
        &repo.path,
        &[
            "create",
            "feat/main-worktree",
            "--from",
            "main",
            "--no-worktree",
        ],
    );
    run(&repo.path, "git", &["switch", "feat/main-worktree"]);
    commit_file(&repo.path, "main-worktree.txt", "feature\n", "feature");

    run_ez(
        &repo.path,
        &["delete", "feat/main-worktree", "--force", "--yes"],
    );

    assert!(repo.path.join(".git").is_dir());
    assert_eq!(current_branch(&repo.path), "main");
    assert!(
        !run_raw(&repo.path, "git", &["rev-parse", "feat/main-worktree"])
            .status
            .success()
    );
    assert!(repo.stack_state()["branches"]["feat/main-worktree"].is_null());
}

#[test]
fn delete_listener_lookup_failure_warns_and_continues_without_killing_processes() {
    let repo = TempRepo::new();
    let branch = "feat/listener-lookup-fails";
    let worktree = repo.create_worktree(branch, "main");
    commit_file(&worktree, "listener.txt", "listener\n", "listener");

    let output =
        run_ez_with_listener_lookup_failure(&repo.path, &["delete", branch, "--yes"], branch);

    assert!(
        output.status.success(),
        "listener lookup failure is warning-only:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Could not verify process ownership on dev port"));
    assert!(stderr.contains("\"killed_pids\":[]"));
    assert!(!worktree.exists());
    assert!(repo.stack_state()["branches"][branch].is_null());
}

#[test]
fn delete_worktree_prune_failure_releases_claim_and_preserves_everything() {
    let repo = TempRepo::new();
    let branch = "feat/prune-fails";
    let worktree = repo.create_worktree(branch, "main");
    commit_file(&worktree, "prune.txt", "prune\n", "prune");
    let state_before = repo.stack_state();
    let tip_before = branch_tip(&repo.path, branch);

    let output = run_ez_with_prune_failure(&repo.path, &["delete", branch, "--yes"]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("simulated worktree prune failure"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(worktree.exists());
    assert_eq!(current_branch(&worktree), branch);
    assert_eq!(branch_tip(&repo.path, branch), tip_before);
    assert_eq!(repo.stack_state(), state_before);
    assert!(
        !String::from_utf8_lossy(
            &run(&repo.path, "git", &["worktree", "list", "--porcelain"]).stdout
        )
        .contains("ez-delete:"),
        "delete claim should be released after prune failure"
    );
}

#[test]
fn delete_rejects_a_worktree_path_replaced_by_another_branch() {
    let repo = TempRepo::new();
    let target = "feat/delete-target";
    let replacement = "feat/replacement";
    let target_worktree = repo.create_worktree(target, "main");
    commit_file(&target_worktree, "target.txt", "target\n", "target");
    run_ez(
        &repo.path,
        &["create", replacement, "--from", "main", "--no-worktree"],
    );
    let target_tip = branch_tip(&repo.path, target);
    let state_before = repo.stack_state();

    let output = run_ez_with_worktree_replacement_race(
        &repo.path,
        &["delete", target, "--yes"],
        &target_worktree,
        replacement,
    );

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("ownership changed"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(current_branch(&target_worktree), replacement);
    assert_eq!(branch_tip(&repo.path, target), target_tip);
    assert_eq!(repo.stack_state(), state_before);
}

#[test]
fn delete_restores_a_removed_worktree_when_local_branch_deletion_fails() {
    let repo = TempRepo::new();
    let branch = "feat/ref-lock";
    let worktree = repo.create_worktree(branch, "main");
    commit_file(&worktree, "locked.txt", "locked\n", "locked");
    let state_before = repo.stack_state();

    let output =
        run_ez_with_branch_delete_failure(&repo.path, &["delete", branch, "--yes"], branch);

    assert!(!output.status.success());
    assert!(worktree.exists());
    assert_eq!(current_branch(&worktree), branch);
    assert_eq!(status_porcelain(&worktree), "");
    assert_eq!(repo.stack_state(), state_before);
}

#[test]
fn delete_reports_when_branch_delete_failure_cannot_restore_worktree() {
    let repo = TempRepo::new();
    let branch = "feat/ref-and-restore-fail";
    let worktree = repo.create_worktree(branch, "main");
    commit_file(&worktree, "restore.txt", "restore\n", "restore");
    let state_before = repo.stack_state();
    let tip_before = branch_tip(&repo.path, branch);

    let output = run_ez_with_branch_delete_and_worktree_add_failure(
        &repo.path,
        &["delete", branch, "--yes"],
        branch,
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Worktree rollback also failed"),
        "unexpected stderr: {stderr}"
    );
    assert!(!worktree.exists());
    assert_eq!(branch_tip(&repo.path, branch), tip_before);
    assert_eq!(repo.stack_state(), state_before);
}

#[test]
fn delete_does_not_kill_an_unrelated_process_on_the_same_dev_port() {
    let repo = TempRepo::new();
    let branch = "feat/port-collision";
    let worktree = repo.create_worktree(branch, "main");
    commit_file(&worktree, "collision.txt", "collision\n", "collision");
    let kill_marker = repo.path.join("kill-attempted");

    let output = run_ez_with_fake_dev_tools(
        &repo.path,
        &["delete", branch, "--yes"],
        branch,
        &repo.path,
        &kill_marker,
        false,
    );

    assert!(output.status.success());
    assert!(!kill_marker.exists());
    assert!(!worktree.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("\"killed_pids\":[]"));
}

#[test]
fn delete_does_not_signal_a_reused_listener_pid() {
    let repo = TempRepo::new();
    let branch = "feat/reused-pid";
    let worktree = repo.create_worktree(branch, "main");
    commit_file(&worktree, "listener.txt", "listener\n", "listener");
    let kill_marker = repo.path.join("kill-attempted");

    let output = run_ez_with_fake_dev_tools(
        &repo.path,
        &["delete", branch, "--yes"],
        branch,
        &worktree,
        &kill_marker,
        true,
    );

    assert!(output.status.success());
    assert!(!kill_marker.exists());
    assert!(!worktree.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("\"killed_pids\":[]"));
}

#[test]
fn delete_kill_failure_warns_after_successful_branch_and_worktree_removal() {
    let repo = TempRepo::new();
    let branch = "feat/kill-fails";
    let worktree = repo.create_worktree(branch, "main");
    commit_file(&worktree, "kill.txt", "kill\n", "kill");

    let output =
        run_ez_with_kill_failure(&repo.path, &["delete", branch, "--yes"], branch, &worktree);

    assert!(
        output.status.success(),
        "kill failure should not roll back completed deletion:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Failed to stop process(es) on dev port"));
    assert!(stderr.contains("\"killed_pids\":[]"));
    assert!(!worktree.exists());
    assert!(
        !run_raw(&repo.path, "git", &["rev-parse", branch])
            .status
            .success()
    );
    assert!(repo.stack_state()["branches"][branch].is_null());
}

#[test]
fn delete_branch_only_failure_restores_checkout_and_preserves_stack_state() {
    let repo = TempRepo::new();
    let branch = "feat/main-ref-failure";
    run_ez(
        &repo.path,
        &["create", branch, "--from", "main", "--no-worktree"],
    );
    run(&repo.path, "git", &["switch", branch]);
    commit_file(&repo.path, "failure.txt", "failure\n", "failure");
    let state_before = repo.stack_state();

    let output = run_ez_with_branch_delete_failure(
        &repo.path,
        &["delete", branch, "--force", "--yes"],
        branch,
    );

    assert!(!output.status.success());
    assert_eq!(current_branch(&repo.path), branch);
    assert_eq!(repo.stack_state(), state_before);
    assert!(
        run_raw(&repo.path, "git", &["rev-parse", branch])
            .status
            .success()
    );
}

#[test]
fn delete_quarantine_keeps_a_post_unlock_replacement_worktree_safe() {
    let repo = TempRepo::new();
    let target = "feat/quarantine-target";
    let replacement = "feat/post-unlock-replacement";
    let target_worktree = repo.create_worktree(target, "main");
    commit_file(&target_worktree, "target.txt", "target\n", "target");
    run_ez(
        &repo.path,
        &["create", replacement, "--from", "main", "--no-worktree"],
    );

    let output = run_ez_with_post_unlock_replacement(
        &repo.path,
        &["delete", target, "--yes"],
        &target_worktree,
        replacement,
    );

    assert!(
        output.status.success(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(target_worktree.exists());
    assert_eq!(current_branch(&target_worktree), replacement);
    assert!(
        run_raw(&repo.path, "git", &["rev-parse", replacement])
            .status
            .success()
    );
    assert!(
        !run_raw(&repo.path, "git", &["rev-parse", target])
            .status
            .success()
    );
}

#[test]
fn delete_quarantine_unlock_failure_restores_the_original_worktree() {
    let repo = TempRepo::new();
    let target = "feat/quarantine-unlock-failure";
    let target_worktree = repo.create_worktree(target, "main");
    commit_file(&target_worktree, "target.txt", "committed\n", "target");
    std::fs::write(target_worktree.join("target.txt"), "dirty\n").expect("dirty target");
    let state_before = repo.stack_state();
    let tip_before = stdout_text(&run(&repo.path, "git", &["rev-parse", target]));

    let output =
        run_ez_with_quarantine_unlock_failure(&repo.path, &["delete", target, "--force", "--yes"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unlock"), "unexpected stderr: {stderr}");
    assert!(
        stderr.contains("Restored worktree"),
        "unexpected stderr: {stderr}"
    );
    assert!(target_worktree.exists());
    assert_eq!(current_branch(&target_worktree), target);
    assert_eq!(
        std::fs::read_to_string(target_worktree.join("target.txt")).expect("read dirty target"),
        "dirty\n"
    );
    assert_eq!(repo.stack_state(), state_before);
    assert_eq!(
        stdout_text(&run(&repo.path, "git", &["rev-parse", target])),
        tip_before
    );
    let parent = target_worktree.parent().expect("worktree parent");
    assert!(
        std::fs::read_dir(parent)
            .expect("list worktree parent")
            .all(|entry| !entry
                .expect("worktree parent entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".ez-delete-"))
    );
}

#[test]
fn delete_remove_failure_reports_when_rollback_path_is_occupied() {
    let repo = TempRepo::new();
    let branch = "feat/remove-rollback-fails";
    let worktree = repo.create_worktree(branch, "main");
    commit_file(&worktree, "remove.txt", "remove\n", "remove");
    let state_before = repo.stack_state();
    let tip_before = branch_tip(&repo.path, branch);

    let output = run_ez_with_remove_failure_and_blocked_rollback(
        &repo.path,
        &["delete", branch, "--yes"],
        &worktree,
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Worktree-path rollback also failed"),
        "unexpected stderr: {stderr}"
    );
    assert!(worktree.exists());
    assert!(
        !worktree.join(".git").exists(),
        "the occupied original path must not be mistaken for a restored worktree"
    );
    assert_eq!(branch_tip(&repo.path, branch), tip_before);
    assert_eq!(repo.stack_state(), state_before);
    let quarantine_parent = worktree.parent().expect("worktree parent");
    assert!(
        std::fs::read_dir(quarantine_parent)
            .expect("list worktree parent")
            .any(|entry| entry
                .expect("worktree parent entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".ez-delete-")),
        "failed rollback should leave the quarantined worktree for inspection"
    );
}
