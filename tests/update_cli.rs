use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct UpdateEnv {
    root: PathBuf,
    home: PathBuf,
    fake_bin: PathBuf,
    log: PathBuf,
    exe: PathBuf,
}

impl UpdateEnv {
    fn new(prefix: &str) -> Self {
        Self::with_exe(prefix, PathBuf::from(env!("CARGO_BIN_EXE_ez")))
    }

    fn cargo_install(prefix: &str) -> Self {
        let root = temp_dir(prefix);
        let home = root.join("home");
        let fake_bin = root.join("fake-bin");
        let cargo_bin = home.join(".cargo/bin");
        std::fs::create_dir_all(&fake_bin).expect("create fake bin");
        std::fs::create_dir_all(&cargo_bin).expect("create cargo bin");
        let exe = cargo_bin.join("ez");
        std::fs::copy(env!("CARGO_BIN_EXE_ez"), &exe).expect("copy ez test binary");
        make_executable(&exe);
        let env = Self {
            log: root.join("commands.log"),
            root,
            home,
            fake_bin,
            exe,
        };
        env.install_fake_commands();
        env
    }

    fn with_exe(prefix: &str, exe: PathBuf) -> Self {
        let root = temp_dir(prefix);
        let home = root.join("home");
        let fake_bin = root.join("fake-bin");
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::create_dir_all(&fake_bin).expect("create fake bin");
        let env = Self {
            log: root.join("commands.log"),
            root,
            home,
            fake_bin,
            exe,
        };
        env.install_fake_commands();
        env
    }

    fn run(&self, args: &[&str], curl_body: &str) -> Output {
        self.run_with_env(args, curl_body, &[])
    }

    fn run_with_env(&self, args: &[&str], curl_body: &str, extra: &[(&str, &str)]) -> Output {
        let inherited_path = std::env::var("PATH").unwrap_or_default();
        let mut cmd = Command::new(&self.exe);
        cmd.args(args)
            .current_dir(&self.root)
            .env("NO_COLOR", "1")
            .env("HOME", &self.home)
            .env(
                "PATH",
                format!("{}:{inherited_path}", self.fake_bin.display()),
            )
            .env("EZ_FAKE_COMMAND_LOG", &self.log)
            .env("EZ_FAKE_CURL_BODY", curl_body);
        for (key, value) in extra {
            cmd.env(key, value);
        }
        cmd.output().expect("run ez update")
    }

    fn command_log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn install_fake_commands(&self) {
        write_executable(
            &self.fake_bin.join("curl"),
            r#"#!/bin/sh
echo "curl $@" >> "$EZ_FAKE_COMMAND_LOG"
if [ "${EZ_FAKE_CURL_FAIL:-0}" = "1" ]; then
  printf 'fake curl failure\n' >&2
  exit 22
fi
printf '%s\n' "$EZ_FAKE_CURL_BODY"
"#,
        );
        write_executable(
            &self.fake_bin.join("cargo"),
            r#"#!/bin/sh
echo "cargo $@" >> "$EZ_FAKE_COMMAND_LOG"
if [ "${EZ_FAKE_CARGO_FAIL:-0}" = "1" ]; then
  printf 'fake cargo failure\n' >&2
  exit 44
fi
exit 0
"#,
        );
        write_executable(
            &self.fake_bin.join("bash"),
            r#"#!/bin/sh
echo "bash $@" >> "$EZ_FAKE_COMMAND_LOG"
if [ "${EZ_FAKE_BASH_FAIL:-0}" = "1" ]; then
  printf 'fake bash failure\n' >&2
  exit 45
fi
exit 0
"#,
        );
        write_executable(
            &self.fake_bin.join("gh"),
            r#"#!/bin/sh
echo "gh $@" >> "$EZ_FAKE_COMMAND_LOG"
printf 'gh should not be called by ez update\n' >&2
exit 46
"#,
        );
        write_executable(
            &self.fake_bin.join("brew"),
            r#"#!/bin/sh
echo "brew $@" >> "$EZ_FAKE_COMMAND_LOG"
printf 'brew should not be called by ez update\n' >&2
exit 47
"#,
        );
    }
}

impl Drop for UpdateEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ez-update-cli-{prefix}-{}-{}-{}",
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

fn write_executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write executable");
    make_executable(path);
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod executable");
    }
}

fn current_tag() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

fn release_body(tag: &str) -> String {
    format!(
        r#"{{
  "tag_name": "{tag}"
}}"#
    )
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
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

#[test]
fn update_check_reports_already_current_without_installing() {
    let env = UpdateEnv::new("check-current");

    let output = env.run(&["update", "--check"], &release_body(&current_tag()));

    assert_success(&output);
    assert_eq!(stdout_text(&output), current_tag());
    assert!(stderr_text(&output).contains("Already on the latest version"));
    assert!(
        env.command_log()
            .starts_with("curl -fsSL https://api.github.com/")
    );
    assert!(!env.command_log().contains("cargo "));
    assert!(!env.command_log().contains("bash "));
    assert!(!env.command_log().contains("brew "));
    assert!(!env.command_log().contains("gh "));
}

#[test]
fn update_check_reports_newer_version_without_installing() {
    let env = UpdateEnv::new("check-newer");

    let output = env.run(&["update", "--check"], &release_body("v9.9.9"));

    assert_success(&output);
    assert_eq!(stdout_text(&output), current_tag());
    assert!(stderr_text(&output).contains("Update available"));
    assert!(stderr_text(&output).contains("v9.9.9"));
    assert!(
        env.command_log()
            .starts_with("curl -fsSL https://api.github.com/")
    );
    assert!(!env.command_log().contains("cargo "));
    assert!(!env.command_log().contains("bash "));
}

#[test]
fn update_script_install_delegates_to_install_script_for_newer_release() {
    let env = UpdateEnv::new("script-install");

    let output = env.run(&["update"], &release_body("v9.9.9"));

    assert_success(&output);
    assert_eq!(stdout_text(&output), current_tag());
    let log = env.command_log();
    assert!(log.contains("curl -fsSL https://api.github.com/"));
    assert!(log.contains("bash -c curl -fsSL 'https://raw.githubusercontent.com/rohoswagger/ez-stack/main/install.sh' | bash -s -- v9.9.9"), "{log}");
    assert!(!log.contains("cargo "));
    assert!(stderr_text(&output).contains("Updated to v9.9.9"));
}

#[test]
fn update_script_install_uses_explicit_target_version() {
    let env = UpdateEnv::new("script-explicit");

    let output = env.run(
        &["update", "--version", "v8.8.8"],
        &release_body(&current_tag()),
    );

    assert_success(&output);
    let log = env.command_log();
    assert!(log.contains("bash -c curl -fsSL 'https://raw.githubusercontent.com/rohoswagger/ez-stack/main/install.sh' | bash -s -- v8.8.8"), "{log}");
    assert!(stderr_text(&output).contains("Updated to v8.8.8"));
}

#[test]
fn update_cargo_install_delegates_to_cargo_when_binary_lives_under_cargo_bin() {
    let env = UpdateEnv::cargo_install("cargo-install");

    let output = env.run(&["update"], &release_body("v9.9.9"));

    assert_success(&output);
    let log = env.command_log();
    assert!(log.contains("curl -fsSL https://api.github.com/"));
    assert!(log.contains("cargo install ez-stack --force"), "{log}");
    assert!(!log.contains("bash "));
    assert!(stderr_text(&output).contains("Detected cargo install"));
}

#[test]
fn update_cargo_install_strips_v_prefix_for_explicit_version() {
    let env = UpdateEnv::cargo_install("cargo-version");

    let output = env.run(
        &["update", "--version", "v8.8.8"],
        &release_body(&current_tag()),
    );

    assert_success(&output);
    let log = env.command_log();
    assert!(
        log.contains("cargo install ez-stack --force --version 8.8.8"),
        "{log}"
    );
    assert!(!log.contains("bash "));
}

#[test]
fn update_fails_actionably_when_latest_version_lookup_fails() {
    let env = UpdateEnv::new("curl-fail");

    let output = env.run_with_env(&["update", "--check"], "", &[("EZ_FAKE_CURL_FAIL", "1")]);

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_text(&output).contains("failed to fetch latest version from GitHub"));
    assert!(stderr_text(&output).contains("Check your internet connection"));
    assert!(!env.command_log().contains("cargo "));
    assert!(!env.command_log().contains("bash "));
}

#[test]
fn update_fails_actionably_when_latest_version_response_is_malformed() {
    let env = UpdateEnv::new("malformed");

    let output = env.run(&["update", "--check"], r#"{"name":"missing tag"}"#);

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_text(&output).contains("could not parse latest version"));
    assert!(stderr_text(&output).contains("releases manually"));
    assert!(!env.command_log().contains("cargo "));
    assert!(!env.command_log().contains("bash "));
}

#[test]
fn update_reports_actionable_cargo_install_failure() {
    let env = UpdateEnv::cargo_install("cargo-fail");

    let output = env.run_with_env(
        &["update"],
        &release_body("v9.9.9"),
        &[("EZ_FAKE_CARGO_FAIL", "1")],
    );

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_text(&output).contains("cargo install failed"));
    assert!(stderr_text(&output).contains("cargo install ez-stack --force"));
}

#[test]
fn update_reports_actionable_install_script_failure() {
    let env = UpdateEnv::new("script-fail");

    let output = env.run_with_env(
        &["update"],
        &release_body("v9.9.9"),
        &[("EZ_FAKE_BASH_FAIL", "1")],
    );

    assert_failure(&output);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_text(&output).contains("install script failed"));
    assert!(stderr_text(&output).contains("Try manually: curl -fsSL"));
}
