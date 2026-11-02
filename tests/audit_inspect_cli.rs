//! Regression tests from the inspect/metadata command audit (status, log,
//! list, scope, adopt, create --hook discovery) driven against real repos.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(prefix: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "ez-audit-inspect-{prefix}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("bin")).expect("bin dir");
        std::fs::create_dir_all(root.join("home")).expect("home dir");
        let gh = root.join("bin/gh");
        std::fs::write(
            &gh,
            "#!/bin/sh\ncase \"$1 $2\" in \"auth status\") exit 0;; esac\nexit 1\n",
        )
        .expect("fake gh");
        run(&root, "chmod", &["+x", gh.to_str().unwrap()]);
        Self { root }
    }

    /// A repo with an `origin` bare remote and one commit on `main`.
    fn repo(&self) -> PathBuf {
        run(&self.root, "git", &["init", "-q", "--bare", "remote.git"]);
        run(&self.root, "git", &["init", "-q", "-b", "main", "repo"]);
        let repo = self.root.join("repo");
        run(&repo, "git", &["config", "user.email", "audit@example.com"]);
        run(&repo, "git", &["config", "user.name", "Audit"]);
        commit(&repo, "README.md", "init");
        run(&repo, "git", &["remote", "add", "origin", "../remote.git"]);
        run(&repo, "git", &["push", "-q", "origin", "main"]);
        repo
    }

    fn ez(&self, dir: &Path, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_ez"))
            .args(args)
            .current_dir(dir)
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            .env("NO_COLOR", "1")
            .output()
            .expect("run ez")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn run(dir: &Path, program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn");
    assert!(
        output.status.success(),
        "{program} {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit(repo: &Path, file: &str, message: &str) {
    std::fs::write(repo.join(file), message).expect("write file");
    run(repo, "git", &["add", file]);
    run(repo, "git", &["commit", "-q", "-m", message]);
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

fn ok(o: &Output) {
    assert!(
        o.status.success(),
        "expected success\nstdout:\n{}\nstderr:\n{}",
        stdout(o),
        stderr(o)
    );
}

fn json(o: &Output) -> serde_json::Value {
    ok(o);
    serde_json::from_str(&stdout(o))
        .unwrap_or_else(|e| panic!("invalid JSON ({e}):\n{}", stdout(o)))
}

/// Initialized repo with `a -> b -> c` stacked on main, currently on `c`.
fn three_deep(sb: &Sandbox) -> PathBuf {
    let repo = sb.repo();
    ok(&sb.ez(&repo, &["init", "--yes"]));
    for name in ["a", "b", "c"] {
        run(&repo, "git", &["checkout", "-q", "-b", name]);
        commit(&repo, &format!("{name}.txt"), name);
        ok(&sb.ez(&repo, &["track", name]));
    }
    repo
}

fn rewrite_stack(repo: &Path, f: impl FnOnce(&mut serde_json::Value)) {
    let path = repo.join(".git/ez/stack.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
    f(&mut value);
    std::fs::write(&path, serde_json::to_string(&value).unwrap()).expect("write");
}

#[test]
fn log_json_and_list_survive_a_recorded_parent_that_no_longer_exists() {
    let sb = Sandbox::new("missing-parent");
    let repo = three_deep(&sb);
    rewrite_stack(&repo, |v| v["branches"]["c"]["parent"] = "gone".into());

    // Before: `ez log --json` panicked (exit 101) on `branches.get("gone").unwrap()`.
    let log = json(&sb.ez(&repo, &["log", "--json"]));
    let names: Vec<&str> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["branch"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"c"), "{names:?}");
    assert!(!names.contains(&"gone"), "{names:?}");

    // Before: `ez list --json` reported a phantom branch `gone` that exists nowhere.
    let list = json(&sb.ez(&repo, &["list", "--json"]));
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .all(|e| e["branch"] != "gone"),
        "{list:#}"
    );
}

#[test]
fn status_counts_commits_when_recorded_parent_no_longer_exists() {
    let sb = Sandbox::new("status-missing-parent");
    let repo = three_deep(&sb);
    rewrite_stack(&repo, |v| v["branches"]["c"]["parent"] = "gone".into());

    // Before: exit 1 with a raw `ambiguous argument 'gone..c'` git error.
    let out = sb.ez(&repo, &["status", "--json"]);
    let value = json(&out);
    assert_eq!(value["commits"], 1, "{value:#}");
    assert!(stderr(&out).contains("ez restack"), "{}", stderr(&out));

    ok(&sb.ez(&repo, &["status"]));
}

#[test]
fn log_json_order_is_deterministic() {
    let sb = Sandbox::new("order");
    let repo = sb.repo();
    ok(&sb.ez(&repo, &["init", "--yes"]));
    let names = ["e", "b", "d", "a", "c", "f"];
    for name in names {
        run(&repo, "git", &["branch", name, "main"]);
        ok(&sb.ez(&repo, &["track", name, "--parent", "main"]));
    }
    let mut sorted = names.to_vec();
    sorted.sort();

    for _ in 0..3 {
        let log = json(&sb.ez(&repo, &["log", "--json"]));
        let order: Vec<&str> = log
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["branch"].as_str().unwrap())
            .collect();
        assert_eq!(order, sorted);
    }
}

#[test]
fn detached_head_is_reported_as_detached_not_as_an_untracked_branch() {
    let sb = Sandbox::new("detached");
    let repo = three_deep(&sb);
    run(&repo, "git", &["checkout", "-q", "--detach", "HEAD~1"]);

    // Before: list --json emitted a branch named "(HEAD detached at <sha>)".
    let list = json(&sb.ez(&repo, &["list", "--json"]));
    for entry in list.as_array().unwrap() {
        let name = entry["branch"].as_str().unwrap();
        assert!(!name.contains("detached"), "{list:#}");
    }

    // Before: "branch `HEAD` is not tracked by ez → Run `ez track`".
    for args in [&["status", "--json"][..], &["status"], &["scope", "show"]] {
        let out = sb.ez(&repo, args);
        assert_eq!(out.status.code(), Some(5), "{args:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("HEAD is detached"), "{args:?}: {err}");
        assert!(!err.contains("ez track"), "{args:?}: {err}");
        assert!(stdout(&out).is_empty(), "{args:?}");
    }
}

#[test]
fn untracked_branch_status_points_at_ez_track() {
    let sb = Sandbox::new("untracked");
    let repo = sb.repo();
    ok(&sb.ez(&repo, &["init", "--yes"]));
    run(&repo, "git", &["checkout", "-q", "-b", "outside"]);

    let out = sb.ez(&repo, &["status"]);
    ok(&out);
    assert!(
        stderr(&out).contains("ez track outside"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn read_commands_outside_a_repo_say_so_with_a_hint() {
    let sb = Sandbox::new("not-repo");
    let dir = sb.root.join("plain");
    std::fs::create_dir_all(&dir).unwrap();

    for args in [
        &["status", "--json"][..],
        &["log", "--json"],
        &["list", "--json"],
        &["scope", "show"],
        &["config", "list"],
    ] {
        let out = sb.ez(&dir, args);
        assert!(!out.status.success(), "{args:?}");
        let err = stderr(&out);
        // Before: "git command failed: fatal: not a git repository" with no hint.
        assert!(err.contains("not a git repository"), "{args:?}: {err}");
        assert!(err.contains("Run `git init`"), "{args:?}: {err}");
        assert!(stdout(&out).is_empty(), "{args:?}");
    }
}

#[test]
fn corrupt_stack_json_is_reported_and_adopt_does_not_overwrite_it() {
    let sb = Sandbox::new("corrupt");
    let repo = three_deep(&sb);
    let path = repo.join(".git/ez/stack.json");
    std::fs::write(&path, "{not json").unwrap();

    let out = sb.ez(&repo, &["status"]);
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    assert!(stderr(&out).contains("stack.json"), "{}", stderr(&out));

    // Before: adopt treated any load error as "not initialized" and replaced
    // the user's stack metadata with an empty state.
    let out = sb.ez(&repo, &["adopt", "a"]);
    assert!(!out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
}

#[test]
fn create_hook_alone_lists_hooks_without_a_branch_name() {
    let sb = Sandbox::new("hook");
    let repo = sb.repo();
    ok(&sb.ez(&repo, &["init", "--yes"]));
    let hooks = repo.join(".ez/hooks/post-create");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(hooks.join("setup-node.md"), "npm install\n").unwrap();

    // Before: clap rejected it with "required arguments were not provided: <NAME>".
    let out = sb.ez(&repo, &["create", "--hook"]);
    ok(&out);
    assert!(stdout(&out).contains("setup-node"), "{}", stdout(&out));
    assert_eq!(run(&repo, "git", &["branch", "--show-current"]), "main");

    let out = sb.ez(&repo, &["create", "--hook", "setup-node"]);
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("missing branch name"),
        "{}",
        stderr(&out)
    );
}
