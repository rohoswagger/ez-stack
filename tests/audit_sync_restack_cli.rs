//! End-to-end audit of `ez sync` / `ez restack` against squash-merged stacks, with a fake `gh`
//! that answers PR lookups per branch from files in `$GH_PR_DIR`.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct Repo {
    path: PathBuf,
    remote: PathBuf,
    fake_bin: PathBuf,
    pr_dir: PathBuf,
    extra: Vec<PathBuf>,
}

impl Drop for Repo {
    fn drop(&mut self) {
        for dir in [&self.path, &self.remote, &self.fake_bin]
            .into_iter()
            .chain(self.extra.iter())
        {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ez-audit-{prefix}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).expect("create temp dir");
    // Canonicalize so paths compare equal to what git reports (macOS /var -> /private/var).
    path.canonicalize().expect("canonicalize temp dir")
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

fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&run(dir, "git", args).stdout)
        .trim()
        .to_string()
}

fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(dir.join(file), contents).expect("write file");
    run(dir, "git", &["add", file]);
    run(dir, "git", &["commit", "-m", message]);
}

const FAKE_GH: &str = r#"#!/bin/sh
if [ "$1" = "repo" ] && [ "$2" = "view" ]; then
  printf 'org/repo\n'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  out='{"data":{"repository":{'
  sep=''
  for pair in $(printf '%s' "$*" | grep -o 'b[0-9]*:pullRequests(headRefName:"[^"]*"' | sed 's/:pullRequests(headRefName:"/=/; s/"$//'); do
    alias=${pair%%=*}
    branch=${pair#*=}
    node=''
    if [ -f "$GH_PR_DIR/$branch" ]; then node=$(cat "$GH_PR_DIR/$branch"); fi
    out="$out$sep\"$alias\":{\"nodes\":[$node]}"
    sep=','
  done
  printf '%s}}}\n' "$out"
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ]; then
  exit 0
fi
printf 'unexpected gh invocation: %s\n' "$*" >&2
exit 1
"#;

fn set_pr(repo: &Repo, branch: &str, number: u64, state: &str, base: &str) {
    let merged_at = if state == "MERGED" {
        "\"2026-07-30T00:00:00Z\""
    } else {
        "null"
    };
    std::fs::write(
        repo.pr_dir.join(branch),
        format!(
            r#"{{"number":{number},"url":"https://github.com/org/repo/pull/{number}","state":"{state}","title":"{branch}","baseRefName":"{base}","isDraft":false,"mergedAt":{merged_at}}}"#
        ),
    )
    .expect("write PR fixture");
}

fn run_ez(repo: &Repo, dir: &Path, args: &[&str]) -> Output {
    let inherited_path = std::env::var("PATH").unwrap_or_default();
    Command::new(env!("CARGO_BIN_EXE_ez"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(
            "PATH",
            format!("{}:{inherited_path}", repo.fake_bin.display()),
        )
        .env("GH_PR_DIR", &repo.pr_dir)
        .output()
        .expect("run ez")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn state(repo: &Repo) -> Value {
    serde_json::from_slice(&std::fs::read(repo.path.join(".git/ez/stack.json")).expect("state"))
        .expect("state JSON")
}

fn branch_exists(repo: &Repo, branch: &str) -> bool {
    run_raw(
        &repo.path,
        "git",
        &["show-ref", "--verify", &format!("refs/heads/{branch}")],
    )
    .status
    .success()
}

fn is_ancestor(repo: &Repo, ancestor: &str, descendant: &str) -> bool {
    run_raw(
        &repo.path,
        "git",
        &["merge-base", "--is-ancestor", ancestor, descendant],
    )
    .status
    .success()
}

fn commit_count(repo: &Repo, range: &str) -> usize {
    git(&repo.path, &["rev-list", "--count", range])
        .parse()
        .expect("count")
}

/// main -> A -> B -> C -> D, each branch adding one file, all pushed with PRs 1-4. On the remote,
/// A and B are then squash-merged into main and main moves on with a change to A's file, so
/// replaying A's (or B's) own commit onto main conflicts exactly like it does on GitHub.
fn squash_merged_stack(prefix: &str) -> Repo {
    let path = temp_dir(prefix);
    let remote = temp_dir(&format!("{prefix}-remote")).join("origin.git");
    std::fs::create_dir_all(&remote).expect("remote dir");
    run(&remote, "git", &["init", "--bare", "-b", "main"]);
    run(&path, "git", &["init", "-b", "main"]);
    run(&path, "git", &["config", "user.name", "Test"]);
    run(&path, "git", &["config", "user.email", "t@example.com"]);
    commit_file(&path, "tracked.txt", "initial\n", "initial");
    run(
        &path,
        "git",
        &["remote", "add", "origin", remote.to_str().expect("remote")],
    );
    run(&path, "git", &["push", "-u", "origin", "main"]);

    let fake_bin = temp_dir(&format!("{prefix}-bin"));
    let script = fake_bin.join("gh");
    std::fs::write(&script, FAKE_GH).expect("fake gh");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake gh");
    }
    let pr_dir = fake_bin.join("prs");
    std::fs::create_dir_all(&pr_dir).expect("pr dir");

    let mut branches = serde_json::Map::new();
    let mut parent = "main".to_string();
    for (number, name) in ["A", "B", "C", "D"].into_iter().enumerate() {
        let parent_head = git(&path, &["rev-parse", &parent]);
        run(&path, "git", &["checkout", "-q", "-b", name, &parent]);
        commit_file(
            &path,
            &format!("{}.txt", name.to_lowercase()),
            &format!("{name}\n"),
            name,
        );
        run(&path, "git", &["push", "-q", "-u", "origin", name]);
        branches.insert(
            name.to_string(),
            serde_json::json!({
                "name": name,
                "parent": parent,
                "parent_head": parent_head,
                "pr_number": number as u64 + 1,
            }),
        );
        parent = name.to_string();
    }
    run(&path, "git", &["checkout", "-q", "main"]);
    std::fs::create_dir_all(path.join(".git/ez")).expect("ez dir");
    std::fs::write(
        path.join(".git/ez/stack.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "trunk": "main",
            "remote": "origin",
            "branches": branches,
        }))
        .expect("serialize"),
    )
    .expect("write state");

    let writer = temp_dir(&format!("{prefix}-writer"));
    run(
        &writer,
        "git",
        &["clone", "-q", remote.to_str().expect("remote"), "."],
    );
    run(&writer, "git", &["config", "user.name", "Remote"]);
    run(&writer, "git", &["config", "user.email", "r@example.com"]);
    for (branch, number) in [("A", 1), ("B", 2)] {
        run(
            &writer,
            "git",
            &["merge", "--squash", &format!("origin/{branch}")],
        );
        run(
            &writer,
            "git",
            &["commit", "-q", "-m", &format!("{branch} (#{number})")],
        );
    }
    commit_file(&writer, "a.txt", "A, revised on main\n", "main moves on");
    run(&writer, "git", &["push", "-q", "origin", "main"]);
    std::fs::remove_dir_all(&writer).expect("remove writer");

    let repo = Repo {
        path,
        remote,
        fake_bin,
        pr_dir,
        extra: Vec::new(),
    };
    set_pr(&repo, "A", 1, "MERGED", "main");
    set_pr(&repo, "B", 2, "MERGED", "A");
    set_pr(&repo, "C", 3, "OPEN", "B");
    set_pr(&repo, "D", 4, "OPEN", "C");
    repo
}

fn assert_c_and_d_restacked_onto_main(repo: &Repo) {
    let state = state(repo);
    assert_eq!(state["branches"]["C"]["parent"], "main", "{state:#}");
    assert_eq!(state["branches"]["D"]["parent"], "C", "{state:#}");
    assert!(is_ancestor(repo, "origin/main", "C"), "C must sit on main");
    assert_eq!(
        commit_count(repo, "origin/main..C"),
        1,
        "only C's own commit"
    );
    assert!(is_ancestor(repo, "C", "D"), "D must sit on C");
    assert_eq!(commit_count(repo, "C..D"), 1, "only D's own commit");
}

#[test]
fn sync_dry_run_in_external_worktree_of_open_top_branch_does_not_plan_its_removal() {
    let mut repo = squash_merged_stack("dry-ext");
    let external = temp_dir("dry-ext-wt").join("wt");
    repo.extra
        .push(external.parent().expect("parent").to_path_buf());
    run(
        &repo.path,
        "git",
        &["worktree", "add", "-q", external.to_str().expect("wt"), "D"],
    );
    run(&external, "git", &["fetch", "-q", "origin"]);

    let output = run_ez(&repo, &external, &["sync", "--dry-run"]);
    let err = stderr(&output);
    assert!(output.status.success(), "{err}");
    assert!(
        !err.contains("Would remove worktree"),
        "no worktree belongs to a cleaned branch:\n{err}"
    );
    for expected in [
        "Would clean up `A` (merged)",
        "Would clean up `B` (merged)",
        "Would reparent `C` onto `main`",
        "Would restack `C` onto `main`",
        "Would restack `D` onto `C`",
    ] {
        assert!(err.contains(expected), "missing `{expected}`:\n{err}");
    }
    assert!(!err.contains("Would clean up `C`"), "{err}");
    assert!(!err.contains("Would clean up `D`"), "{err}");
    assert!(branch_exists(&repo, "A"), "dry run must not mutate");
}

#[test]
fn sync_in_external_worktree_cleans_merged_bottom_and_keeps_its_own_worktree() {
    let mut repo = squash_merged_stack("real-ext");
    let external = temp_dir("real-ext-wt").join("wt");
    repo.extra
        .push(external.parent().expect("parent").to_path_buf());
    run(
        &repo.path,
        "git",
        &["worktree", "add", "-q", external.to_str().expect("wt"), "D"],
    );

    let output = run_ez(&repo, &external, &["sync"]);
    let err = stderr(&output);
    assert!(output.status.success(), "{err}");
    assert!(!err.contains("Removed worktree"), "{err}");
    assert!(external.exists(), "sync removed the worktree it runs in");
    assert_eq!(git(&external, &["branch", "--show-current"]), "D");
    assert_eq!(git(&external, &["status", "--porcelain"]), "");
    assert!(!branch_exists(&repo, "A") && !branch_exists(&repo, "B"));
    assert!(state(&repo)["branches"].get("A").is_none());
    assert_c_and_d_restacked_onto_main(&repo);
}

#[test]
fn sync_dry_run_and_real_run_agree_on_removing_a_merged_branch_worktree() {
    let repo = squash_merged_stack("wt-merged");
    let a_worktree = repo.path.join(".worktrees").join("A");
    run(
        &repo.path,
        "git",
        &[
            "worktree",
            "add",
            "-q",
            a_worktree.to_str().expect("wt"),
            "A",
        ],
    );
    run(&repo.path, "git", &["fetch", "-q", "origin"]);

    let dry = run_ez(&repo, &repo.path, &["sync", "--dry-run"]);
    let dry_err = stderr(&dry);
    assert!(dry.status.success(), "{dry_err}");
    assert_eq!(
        dry_err.matches("Would remove worktree").count(),
        1,
        "{dry_err}"
    );
    assert!(
        dry_err.contains(&format!(
            "Would remove worktree at `{}`",
            a_worktree.display()
        )),
        "{dry_err}"
    );

    let real = run_ez(&repo, &repo.path, &["sync"]);
    let real_err = stderr(&real);
    assert!(real.status.success(), "{real_err}");
    assert!(!a_worktree.exists(), "{real_err}");
    assert!(!branch_exists(&repo, "A"));
    assert_c_and_d_restacked_onto_main(&repo);
}

#[test]
fn restack_skips_squash_merged_branches_and_moves_open_children_onto_main() {
    let repo = squash_merged_stack("restack-squash");
    run(&repo.path, "git", &["checkout", "-q", "D"]);
    let a_before = git(&repo.path, &["rev-parse", "A"]);
    let b_before = git(&repo.path, &["rev-parse", "B"]);

    let output = run_ez(&repo, &repo.path, &["restack"]);
    let err = stderr(&output);
    assert!(output.status.success(), "{err}");
    assert!(!err.contains("could not be restacked"), "{err}");
    assert_eq!(
        git(&repo.path, &["rev-parse", "A"]),
        a_before,
        "A untouched"
    );
    assert_eq!(
        git(&repo.path, &["rev-parse", "B"]),
        b_before,
        "B untouched"
    );
    assert_eq!(git(&repo.path, &["branch", "--show-current"]), "D");
    assert_eq!(git(&repo.path, &["status", "--porcelain"]), "");
    assert_c_and_d_restacked_onto_main(&repo);

    // A second restack is a no-op rather than retrying the merged branches.
    let again = run_ez(&repo, &repo.path, &["restack"]);
    assert!(again.status.success(), "{}", stderr(&again));
    assert_c_and_d_restacked_onto_main(&repo);
}
