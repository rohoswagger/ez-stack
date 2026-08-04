use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};

pub(crate) struct EnvVarGuard {
    name: &'static str,
    old_value: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    pub(crate) fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let old_value = std::env::var_os(name);
        // SAFETY: callers must hold the shared test environment mutex while
        // mutating process-global environment variables.
        unsafe {
            std::env::set_var(name, value);
        }
        Self { name, old_value }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: paired with EnvVarGuard::set under the same test environment
        // mutex held by the caller for the lifetime of this guard.
        unsafe {
            if let Some(value) = &self.old_value {
                std::env::set_var(self.name, value);
            } else {
                std::env::remove_var(self.name);
            }
        }
    }
}

pub(crate) fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub(crate) fn take_env_lock() -> MutexGuard<'static, ()> {
    env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn temp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ez-tests-{}-{}-{}",
        prefix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

pub(crate) fn write_file(dir: &Path, relative_path: &str, contents: &str) {
    let path = dir.join(relative_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(path, contents).expect("write file");
}

pub(crate) fn run_cmd(dir: &Path, bin: &str, args: &[&str]) {
    let status = Command::new(bin)
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run command");
    assert!(status.success(), "{bin} {:?} failed", args);
}

pub(crate) fn cmd_output(dir: &Path, bin: &str, args: &[&str]) -> String {
    let output = Command::new(bin)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run command");
    assert!(output.status.success(), "{bin} {:?} failed", args);
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub(crate) fn init_git_repo(name: &str) -> PathBuf {
    let dir = temp_dir(name);
    run_cmd(&dir, "git", &["init", "-b", "main"]);
    run_cmd(&dir, "git", &["config", "user.name", "Test User"]);
    run_cmd(&dir, "git", &["config", "user.email", "test@example.com"]);
    write_file(&dir, "tracked.txt", "hello\n");
    run_cmd(&dir, "git", &["add", "tracked.txt"]);
    run_cmd(&dir, "git", &["commit", "-m", "initial"]);
    dir
}

pub(crate) struct CwdGuard {
    original: PathBuf,
}

impl CwdGuard {
    pub(crate) fn enter(dir: &Path) -> Self {
        let original = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(dir).expect("set cwd");
        Self { original }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

pub(crate) struct PathGuard {
    old_path: String,
}

impl PathGuard {
    pub(crate) fn install(dir: &Path) -> Self {
        let old_path = std::env::var("PATH").unwrap_or_default();
        // SAFETY: test code holds a global mutex around PATH mutation, so no
        // concurrent environment access occurs while this guard is active.
        unsafe {
            std::env::set_var("PATH", format!("{}:{}", dir.display(), old_path));
        }
        Self { old_path }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        // SAFETY: paired with the install() mutation above under the same global lock.
        unsafe {
            std::env::set_var("PATH", &self.old_path);
        }
    }
}

pub(crate) fn install_fake_bin(prefix: &str, name: &str, script: &str) -> PathBuf {
    let dir = temp_dir(prefix);
    let path = dir.join(name);
    std::fs::write(&path, script).expect("write fake binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
    dir
}
