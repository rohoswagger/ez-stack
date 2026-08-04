use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct SetupEnv {
    root: PathBuf,
    home: PathBuf,
}

impl SetupEnv {
    fn new(prefix: &str) -> Self {
        let root = temp_dir(prefix);
        let home = root.join("home");
        let fake_bin = root.join("fake-bin");
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::create_dir_all(&fake_bin).expect("create fake bin");
        Self { root, home }
    }

    fn run(&self, shell: Option<&str>, path_env: &str) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ez"));
        cmd.args(["setup", "--yes"])
            .current_dir(&self.root)
            .env("NO_COLOR", "1")
            .env("HOME", &self.home)
            .env("PATH", path_env);
        if let Some(shell) = shell {
            cmd.env("SHELL", shell);
        } else {
            cmd.env_remove("SHELL");
        }
        cmd.output().expect("run ez setup")
    }

    fn run_without_home(&self, shell: &str, path_env: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ez"))
            .args(["setup", "--yes"])
            .current_dir(&self.root)
            .env("NO_COLOR", "1")
            .env("SHELL", shell)
            .env("PATH", path_env)
            .env_remove("HOME")
            .output()
            .expect("run ez setup without home")
    }
}

impl Drop for SetupEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ez-setup-cli-{prefix}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&path).expect("create temp dir");
    path
}

fn ez_install_dir() -> String {
    Path::new(env!("CARGO_BIN_EXE_ez"))
        .parent()
        .expect("ez binary parent")
        .to_str()
        .expect("utf8 binary parent")
        .to_string()
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

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[test]
fn zsh_setup_yes_creates_rc_marker_and_is_idempotent() {
    let env = SetupEnv::new("zsh-idempotent");
    let path = "/usr/bin:/bin";

    let first = env.run(Some("/bin/zsh"), path);
    let second = env.run(Some("/bin/zsh"), path);

    assert_success(&first);
    assert_success(&second);
    let rc = env.home.join(".zshrc");
    let content = std::fs::read_to_string(&rc).expect("zshrc should exist");
    assert_eq!(
        count_occurrences(&content, "# ez-stack shell integration"),
        1
    );
    assert_eq!(
        count_occurrences(
            &content,
            &format!(r#"export PATH="{}:$PATH""#, ez_install_dir())
        ),
        1
    );
    assert_eq!(count_occurrences(&content, r#"eval "$(ez shell-init)""#), 1);
    assert!(env.home.join(".ez/.setup-done").exists());
    assert!(stderr_text(&second).contains("Shell already configured"));
}

#[test]
fn bash_setup_selects_existing_bashrc() {
    let env = SetupEnv::new("bashrc");
    let bashrc = env.home.join(".bashrc");
    std::fs::write(&bashrc, "existing bashrc\n").expect("write bashrc");

    let output = env.run(Some("/bin/bash"), "/usr/bin:/bin");

    assert_success(&output);
    let bashrc_content = std::fs::read_to_string(&bashrc).expect("read bashrc");
    assert!(bashrc_content.starts_with("existing bashrc\n"));
    assert!(bashrc_content.contains(r#"eval "$(ez shell-init)""#));
    assert!(!env.home.join(".bash_profile").exists());
}

#[test]
fn bash_setup_uses_bash_profile_when_bashrc_is_absent() {
    let env = SetupEnv::new("bash-profile");

    let output = env.run(Some("/usr/local/bin/bash"), "/usr/bin:/bin");

    assert_success(&output);
    assert!(!env.home.join(".bashrc").exists());
    let profile = env.home.join(".bash_profile");
    let content = std::fs::read_to_string(&profile).expect("read bash_profile");
    assert!(content.contains(r#"eval "$(ez shell-init)""#));
}

#[test]
fn setup_suppresses_only_path_line_when_install_dir_is_already_in_path() {
    let env = SetupEnv::new("path-present");
    let path = format!("{}:/usr/bin:/bin", ez_install_dir());

    let output = env.run(Some("/bin/zsh"), &path);

    assert_success(&output);
    let content = std::fs::read_to_string(env.home.join(".zshrc")).expect("read zshrc");
    assert!(!content.contains("export PATH="));
    assert!(content.contains(r#"eval "$(ez shell-init)""#));
    assert!(env.home.join(".ez/.setup-done").exists());
}

#[test]
fn setup_writes_only_marker_when_rc_is_preconfigured() {
    let env = SetupEnv::new("preconfigured");
    let rc = env.home.join(".zshrc");
    let original = format!(
        "before\n{}\n{}\nafter\n",
        format_args!(r#"export PATH="{}:$PATH""#, ez_install_dir()),
        r#"eval "$(ez shell-init)""#
    );
    std::fs::write(&rc, &original).expect("write preconfigured rc");

    let output = env.run(Some("/bin/zsh"), "/usr/bin:/bin");

    assert_success(&output);
    assert_eq!(std::fs::read_to_string(&rc).expect("read zshrc"), original);
    assert!(env.home.join(".ez/.setup-done").exists());
    assert!(stderr_text(&output).contains("Shell already configured"));
}

#[test]
fn setup_rejects_unsupported_shell_without_mutation() {
    let env = SetupEnv::new("unsupported");

    let output = env.run(Some("/bin/tcsh"), "/usr/bin:/bin");

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_text(&output).contains("could not detect shell"));
    assert!(!env.home.join(".ez/.setup-done").exists());
    assert!(!env.home.join(".zshrc").exists());
    assert!(!env.home.join(".bashrc").exists());
    assert!(!env.home.join(".bash_profile").exists());
    assert!(!env.home.join(".config").exists());
}

#[test]
fn setup_rejects_missing_shell_without_mutation() {
    let env = SetupEnv::new("missing-shell");

    let output = env.run(None, "/usr/bin:/bin");

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_text(&output).contains("could not detect shell"));
    assert!(!env.home.join(".ez/.setup-done").exists());
    assert!(!env.home.join(".zshrc").exists());
    assert!(!env.home.join(".bashrc").exists());
    assert!(!env.home.join(".bash_profile").exists());
    assert!(!env.home.join(".config").exists());
}

#[test]
fn setup_rejects_missing_home_without_mutation() {
    let env = SetupEnv::new("missing-home");

    let output = env.run_without_home("/bin/zsh", "/usr/bin:/bin");

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_text(&output).contains("could not determine rc file"));
    assert!(!env.home.join(".ez/.setup-done").exists());
    assert!(!env.home.join(".zshrc").exists());
}

#[test]
fn fish_setup_creates_missing_config_parent_and_marker() {
    let env = SetupEnv::new("fish-parent");
    let path = "/usr/bin:/bin";

    let output = env.run(Some("/opt/homebrew/bin/fish"), path);

    assert_success(&output);
    let rc = env.home.join(".config/fish/config.fish");
    let content = std::fs::read_to_string(&rc).expect("fish rc should exist");
    assert!(content.contains(&format!("fish_add_path {}", ez_install_dir())));
    assert!(content.contains("ez shell-init | source"));
    assert!(env.home.join(".ez/.setup-done").exists());
}
