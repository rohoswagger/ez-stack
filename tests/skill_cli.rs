use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SKILL_CONTENT: &str = include_str!("../SKILL.md");

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ez-skill-cli-{prefix}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn run_ez_with_home(dir: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .output()
        .expect("run ez")
}

fn run_ez_with_userprofile(dir: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("HOME", "")
        .env("USERPROFILE", home)
        .output()
        .expect("run ez")
}

fn run_ez_without_home(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .output()
        .expect("run ez")
}

fn assert_success(output: &Output, args: &[&str]) {
    assert!(
        output.status.success(),
        "ez {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_no_agent_roots(dir: &Path) {
    for name in [".agents", ".claude", ".codex"] {
        assert!(
            !dir.join(name).exists(),
            "invocation directory should not contain {name}"
        );
    }
}

fn assert_compat_target(home: &Path, relative_path: &str) {
    let dir = home.join(relative_path);
    let metadata = std::fs::symlink_metadata(&dir).expect("compat target metadata");
    assert!(
        metadata.file_type().is_symlink() || metadata.is_dir(),
        "compat target should be a symlink or fallback copy"
    );

    let skill_file = dir.join("SKILL.md");
    assert!(
        skill_file.exists() || std::fs::read_link(&dir).is_ok(),
        "compat target should resolve as a symlink or contain a fallback skill copy"
    );

    assert_eq!(
        std::fs::read_to_string(skill_file).expect("read compat skill"),
        SKILL_CONTENT
    );
}

#[test]
fn skill_install_and_uninstall_are_global_and_do_not_require_git() {
    let home = TempDir::new("home");
    let cwd = TempDir::new("cwd");
    std::fs::write(home.path().join("unrelated.txt"), "keep\n").expect("write unrelated file");

    let install = run_ez_with_home(cwd.path(), home.path(), &["skill", "install"]);
    assert_success(&install, &["skill", "install"]);

    let canonical = home.path().join(".agents/skills/ez-workflow/SKILL.md");
    assert_eq!(
        std::fs::read_to_string(&canonical).expect("read canonical skill"),
        SKILL_CONTENT
    );
    assert_compat_target(home.path(), ".claude/skills/ez-workflow");
    assert_compat_target(home.path(), ".codex/skills/ez-workflow");
    assert_no_agent_roots(cwd.path());

    let uninstall = run_ez_with_home(cwd.path(), home.path(), &["skill", "uninstall"]);
    assert_success(&uninstall, &["skill", "uninstall"]);

    assert!(!home.path().join(".agents/skills/ez-workflow").exists());
    assert!(!home.path().join(".claude/skills/ez-workflow").exists());
    assert!(!home.path().join(".codex/skills/ez-workflow").exists());
    assert_eq!(
        std::fs::read_to_string(home.path().join("unrelated.txt")).expect("read unrelated file"),
        "keep\n"
    );
    assert_no_agent_roots(cwd.path());
}

#[test]
fn skill_install_falls_back_to_userprofile_when_home_is_empty() {
    let home = TempDir::new("userprofile");
    let cwd = TempDir::new("userprofile-cwd");

    let output = run_ez_with_userprofile(cwd.path(), home.path(), &["skill", "install"]);
    assert_success(&output, &["skill", "install"]);

    assert_eq!(
        std::fs::read_to_string(home.path().join(".agents/skills/ez-workflow/SKILL.md"))
            .expect("read canonical skill"),
        SKILL_CONTENT
    );
    assert_no_agent_roots(cwd.path());
}

#[test]
fn skill_install_explains_how_to_supply_a_missing_home() {
    let cwd = TempDir::new("missing-home-cwd");

    let output = run_ez_without_home(cwd.path(), &["skill", "install"]);

    assert!(
        !output.status.success(),
        "skill install should fail without a home"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("set HOME or USERPROFILE before running `ez skill install`"),
        "unexpected stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_no_agent_roots(cwd.path());
}

#[test]
fn skill_help_describes_the_user_global_contract() {
    let cwd = TempDir::new("help-cwd");

    let output = run_ez_without_home(cwd.path(), &["skill", "--help"]);
    assert_success(&output, &["skill", "--help"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Install the ez-workflow skill for the current user"),
        "unexpected stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Remove the ez-workflow skill for the current user"),
        "unexpected stdout:\n{stdout}"
    );
}

#[test]
fn skill_uninstall_preserves_preexisting_agent_skill_directories() {
    let home = TempDir::new("preserved-skill-home");
    let cwd = TempDir::new("preserved-skill-cwd");
    let custom_skill = home.path().join(".claude/skills/ez-workflow");
    std::fs::create_dir_all(&custom_skill).expect("create custom skill directory");
    std::fs::write(custom_skill.join("custom.txt"), "keep custom skill\n")
        .expect("write custom skill");
    let unrelated_skill = home.path().join(".codex/skills/ez-workflow");
    std::fs::create_dir_all(&unrelated_skill).expect("create unrelated skill directory");
    std::fs::write(
        unrelated_skill.join("SKILL.md"),
        "---\nname: custom-workflow\n---\nkeep unrelated skill\n",
    )
    .expect("write unrelated skill");

    let install = run_ez_with_home(cwd.path(), home.path(), &["skill", "install"]);
    assert_success(&install, &["skill", "install"]);
    assert_eq!(
        std::fs::read_to_string(custom_skill.join("custom.txt")).expect("read custom skill"),
        "keep custom skill\n"
    );
    assert!(
        std::fs::read_to_string(unrelated_skill.join("SKILL.md"))
            .expect("read unrelated skill")
            .contains("name: custom-workflow")
    );

    let uninstall = run_ez_with_home(cwd.path(), home.path(), &["skill", "uninstall"]);
    assert_success(&uninstall, &["skill", "uninstall"]);
    assert_eq!(
        std::fs::read_to_string(custom_skill.join("custom.txt")).expect("read custom skill"),
        "keep custom skill\n"
    );
    assert!(
        std::fs::read_to_string(unrelated_skill.join("SKILL.md"))
            .expect("read unrelated skill")
            .contains("name: custom-workflow")
    );
}

#[test]
fn skill_uninstall_preserves_extra_files_in_the_canonical_skill_directory() {
    let home = TempDir::new("canonical-extra-home");
    let cwd = TempDir::new("canonical-extra-cwd");
    let canonical_skill = home.path().join(".agents/skills/ez-workflow");

    let install = run_ez_with_home(cwd.path(), home.path(), &["skill", "install"]);
    assert_success(&install, &["skill", "install"]);
    std::fs::write(canonical_skill.join("notes.txt"), "keep canonical notes\n")
        .expect("write canonical notes");

    let uninstall = run_ez_with_home(cwd.path(), home.path(), &["skill", "uninstall"]);
    assert_success(&uninstall, &["skill", "uninstall"]);

    assert_eq!(
        std::fs::read_to_string(canonical_skill.join("notes.txt")).expect("read canonical notes"),
        "keep canonical notes\n"
    );
    assert!(!canonical_skill.join("SKILL.md").exists());
}

#[test]
fn skill_install_updates_and_uninstall_removes_stale_managed_compatibility_copies() {
    let home = TempDir::new("stale-compat-home");
    let cwd = TempDir::new("stale-compat-cwd");
    let compatibility_skill = home.path().join(".claude/skills/ez-workflow");
    std::fs::create_dir_all(&compatibility_skill).expect("create compatibility skill");
    std::fs::write(
        compatibility_skill.join("SKILL.md"),
        "---\nname: ez-workflow\n---\nold managed content\n",
    )
    .expect("write stale compatibility copy");

    let install = run_ez_with_home(cwd.path(), home.path(), &["skill", "install"]);
    assert_success(&install, &["skill", "install"]);
    assert_eq!(
        std::fs::read_to_string(compatibility_skill.join("SKILL.md"))
            .expect("read updated compatibility copy"),
        SKILL_CONTENT
    );

    std::fs::write(
        compatibility_skill.join("SKILL.md"),
        "---\nname: ez-workflow\n---\nolder managed content\n",
    )
    .expect("restore stale compatibility copy");
    let uninstall = run_ez_with_home(cwd.path(), home.path(), &["skill", "uninstall"]);
    assert_success(&uninstall, &["skill", "uninstall"]);
    assert!(!compatibility_skill.exists());
}
