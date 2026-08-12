use anyhow::{Result, bail};

use crate::error::EzError;
use crate::git;
use crate::stack::StackState;
use crate::ui;

/// One layer of the stack `ez split` will produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SplitLayer {
    /// Branch name for this layer. For the topmost layer this is the original branch.
    pub name: String,
    /// The commit this layer's tip points at.
    pub sha: String,
    pub subject: String,
    pub parent: String,
    /// The commit this layer forks from — its parent's tip, which already holds by construction.
    pub parent_head: String,
    /// True for the original branch, which keeps its name, its tip, and any PR it already has.
    pub is_original: bool,
}

/// Lay out one branch per commit, bottom-up.
///
/// The original branch becomes the *top* layer rather than being replaced or deleted. Its tip is
/// already the last commit, so nothing about it has to move: an existing PR stays valid and simply
/// narrows to that final commit once its base is updated. Only the commits below it need new
/// branches, which is why `commits.len() - 1` layers are created.
///
/// `commits` must be oldest-first and contain at least two entries; `fork_point` is where the
/// branch left its parent.
pub(crate) fn plan_split(
    branch: &str,
    prefix: &str,
    parent: &str,
    fork_point: &str,
    commits: &[(String, String)],
) -> Vec<SplitLayer> {
    let mut layers: Vec<SplitLayer> = Vec::with_capacity(commits.len());
    let last = commits.len().saturating_sub(1);

    for (index, (sha, subject)) in commits.iter().enumerate() {
        let is_original = index == last;
        let name = if is_original {
            branch.to_string()
        } else {
            format!("{prefix}-{}", index + 1)
        };
        let (layer_parent, parent_head) = if index == 0 {
            (parent.to_string(), fork_point.to_string())
        } else {
            (layers[index - 1].name.clone(), commits[index - 1].0.clone())
        };

        layers.push(SplitLayer {
            name,
            sha: sha.clone(),
            subject: subject.clone(),
            parent: layer_parent,
            parent_head,
            is_original,
        });
    }

    layers
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn print_plan(layers: &[SplitLayer]) {
    for layer in layers {
        let marker = if layer.is_original {
            " (this branch, keeps its PR)"
        } else {
            ""
        };
        eprintln!(
            "  {} {} {}{}",
            ui::dim(short(&layer.sha)),
            layer.name,
            layer.subject,
            ui::dim(marker)
        );
    }
}

pub fn run(branch: Option<&str>, prefix: Option<&str>, dry_run: bool) -> Result<()> {
    let mut state = StackState::load()?;
    let current = git::current_branch()?;
    let target = branch.unwrap_or(&current).to_string();

    if state.is_trunk(&target) {
        bail!(EzError::OnTrunk);
    }
    if !state.is_managed(&target) {
        bail!(EzError::BranchNotInStack(target.clone()));
    }

    let meta = state.get_branch(&target)?.clone();
    let parent = meta.parent.clone();

    // Fork point from git, not from the recorded `parent_head`: the cache may be stale, and the
    // bottom layer's base has to be the commit the branch really forked from.
    let fork_point = git::merge_base(&parent, &target)
        .map(|base| base.trim().to_string())
        .unwrap_or_else(|_| meta.parent_head.clone());

    let range = format!("{parent}..{target}");
    let commits = git::log_commits_oldest_first(&range)?;

    if commits.len() < 2 {
        ui::info(&format!(
            "`{target}` has {} commit(s) — nothing to split",
            commits.len()
        ));
        ui::receipt(&serde_json::json!({
            "cmd": "split",
            "branch": target,
            "action": "noop",
            "reason": "fewer_than_two_commits",
            "commits": commits.len(),
        }));
        return Ok(());
    }

    // A merge commit cannot become a single-commit layer without rewriting history, and rewriting
    // is exactly what split promises not to do.
    let merges = git::rev_list_merge_count(&fork_point, &target).unwrap_or(0);
    if merges > 0 {
        bail!(EzError::UserMessage(format!(
            "`{target}` contains {merges} merge commit(s), which cannot be split into single-commit layers\n  → Rebase them away first: `git rebase {parent}`, then rerun `ez split`"
        )));
    }

    let prefix = prefix.unwrap_or(&target).to_string();
    let layers = plan_split(&target, &prefix, &parent, &fork_point, &commits);

    // Validate every new name before creating any of them, so a collision halfway down cannot
    // leave a half-built stack behind.
    let collisions: Vec<&str> = layers
        .iter()
        .filter(|layer| !layer.is_original && git::branch_exists(&layer.name))
        .map(|layer| layer.name.as_str())
        .collect();
    if !collisions.is_empty() {
        bail!(EzError::UserMessage(format!(
            "branch name(s) already taken: {}\n  → Pick a different base name with `ez split --prefix <name>`",
            collisions.join(", ")
        )));
    }

    if dry_run {
        ui::info(&format!(
            "`{target}` would split into {} branches:",
            layers.len()
        ));
        print_plan(&layers);
        ui::receipt(&serde_json::json!({
            "cmd": "split",
            "branch": target,
            "action": "dry_run",
            "parent": parent,
            "layers": layers.iter().map(|layer| serde_json::json!({
                "branch": layer.name,
                "sha": short(&layer.sha),
                "parent": layer.parent,
                "is_original": layer.is_original,
            })).collect::<Vec<_>>(),
        }));
        return Ok(());
    }

    // Nothing here rewrites history. The commits already form a chain, so each layer is a ref
    // pointed at a commit that is already in place plus the metadata describing where it forks.
    for layer in &layers {
        if layer.is_original {
            let meta = state.get_branch_mut(&target)?;
            meta.parent = layer.parent.clone();
            meta.parent_head = layer.parent_head.clone();
            continue;
        }

        git::create_branch_at(&layer.name, &layer.sha)?;
        state.add_branch(&layer.name, &layer.parent, &layer.parent_head, None, None);
    }

    state.save()?;

    ui::success(&format!(
        "Split `{target}` into {} stacked branches",
        layers.len()
    ));
    print_plan(&layers);
    ui::hint("Run `ez submit` to open a PR for each layer");

    ui::receipt(&serde_json::json!({
        "cmd": "split",
        "branch": target,
        "action": "split",
        "parent": parent,
        "created": layers.iter().filter(|l| !l.is_original).count(),
        "layers": layers.iter().map(|layer| serde_json::json!({
            "branch": layer.name,
            "sha": short(&layer.sha),
            "parent": layer.parent,
            "is_original": layer.is_original,
        })).collect::<Vec<_>>(),
    }));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{CwdGuard, init_git_repo, run_cmd, take_env_lock, write_file};

    /// A tracked branch with `n` commits on top of main, checked out.
    fn branch_with_commits(repo: &std::path::Path, branch: &str, n: usize) -> Vec<String> {
        run_cmd(repo, "git", &["checkout", "-b", branch]);
        let mut shas = Vec::new();
        for i in 1..=n {
            let file = format!("f{i}.txt");
            write_file(repo, &file, &format!("c{i}\n"));
            run_cmd(repo, "git", &["add", &file]);
            run_cmd(repo, "git", &["commit", "-m", &format!("part {i}")]);
            shas.push(git::rev_parse(branch).expect("rev-parse"));
        }
        shas
    }

    fn tracked_state(branch: &str) -> StackState {
        let mut state = StackState::new("main".to_string());
        let base = git::merge_base("main", branch).expect("merge base");
        state.add_branch(branch, "main", base.trim(), None, None);
        state.save().expect("save state");
        state
    }

    #[test]
    fn split_builds_one_layer_per_commit_without_moving_any_of_them() {
        let _guard = take_env_lock();
        let repo = init_git_repo("split-basic");
        let _cwd = CwdGuard::enter(&repo);

        let shas = branch_with_commits(&repo, "feat/auth", 3);
        tracked_state("feat/auth");

        run(None, None, false).expect("split should succeed");

        let state = StackState::load().expect("reload state");

        // Every layer points at the commit it was created from — nothing was rebased.
        assert_eq!(git::rev_parse("feat/auth-1").expect("p1"), shas[0]);
        assert_eq!(git::rev_parse("feat/auth-2").expect("p2"), shas[1]);
        assert_eq!(
            git::rev_parse("feat/auth").expect("original"),
            shas[2],
            "the original branch tip must not move"
        );

        // ...and the recorded chain matches.
        assert_eq!(state.get_branch("feat/auth-1").expect("p1").parent, "main");
        assert_eq!(
            state.get_branch("feat/auth-2").expect("p2").parent,
            "feat/auth-1"
        );
        assert_eq!(
            state.get_branch("feat/auth").expect("top").parent,
            "feat/auth-2"
        );

        // Each layer holds exactly one commit, which is the whole point.
        for (branch, parent) in [
            ("feat/auth-1", "main"),
            ("feat/auth-2", "feat/auth-1"),
            ("feat/auth", "feat/auth-2"),
        ] {
            let range = format!("{parent}..{branch}");
            assert_eq!(
                git::log_commits_oldest_first(&range).expect("log").len(),
                1,
                "{branch} should carry exactly one commit"
            );
        }
    }

    #[test]
    fn split_refuses_before_creating_anything_when_a_name_is_taken() {
        let _guard = take_env_lock();
        let repo = init_git_repo("split-collision");
        let _cwd = CwdGuard::enter(&repo);

        branch_with_commits(&repo, "feat/auth", 3);
        tracked_state("feat/auth");
        // Somebody already owns the name the second layer would take.
        git::create_branch_at("feat/auth-2", "main").expect("squatter branch");

        let error = run(None, None, false).expect_err("split must refuse");
        assert!(
            error.to_string().contains("feat/auth-2"),
            "the error names the collision: {error}"
        );

        assert!(
            !git::branch_exists("feat/auth-1"),
            "no layer may be created when the plan cannot complete"
        );
        let state = StackState::load().expect("reload state");
        assert_eq!(
            state.get_branch("feat/auth").expect("original").parent,
            "main",
            "the original branch must be left untouched"
        );
    }

    #[test]
    fn split_refuses_a_branch_containing_a_merge_commit() {
        let _guard = take_env_lock();
        let repo = init_git_repo("split-merge-commit");
        let _cwd = CwdGuard::enter(&repo);

        // Distinct files on each side, so the merge produces a merge commit rather than a conflict.
        run_cmd(&repo, "git", &["checkout", "-b", "feat/side"]);
        write_file(&repo, "side.txt", "side\n");
        run_cmd(&repo, "git", &["add", "side.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "side work"]);

        run_cmd(&repo, "git", &["checkout", "main"]);
        run_cmd(&repo, "git", &["checkout", "-b", "feat/auth"]);
        write_file(&repo, "auth.txt", "auth\n");
        run_cmd(&repo, "git", &["add", "auth.txt"]);
        run_cmd(&repo, "git", &["commit", "-m", "auth work"]);
        run_cmd(
            &repo,
            "git",
            &["merge", "--no-ff", "-m", "merge side", "feat/side"],
        );
        assert_eq!(
            git::rev_list_merge_count("main", "feat/auth").expect("merge count"),
            1,
            "the fixture must actually contain a merge commit"
        );
        tracked_state("feat/auth");

        let error = run(None, None, false).expect_err("split must refuse merge commits");
        assert!(
            error.to_string().contains("merge commit"),
            "the error explains why: {error}"
        );
        assert!(!git::branch_exists("feat/auth-1"));
    }

    #[test]
    fn split_dry_run_creates_nothing() {
        let _guard = take_env_lock();
        let repo = init_git_repo("split-dry-run");
        let _cwd = CwdGuard::enter(&repo);

        branch_with_commits(&repo, "feat/auth", 3);
        tracked_state("feat/auth");

        run(None, None, true).expect("dry run should succeed");

        assert!(!git::branch_exists("feat/auth-1"));
        assert!(!git::branch_exists("feat/auth-2"));
        let state = StackState::load().expect("reload state");
        assert_eq!(
            state.get_branch("feat/auth").expect("original").parent,
            "main"
        );
    }

    fn commits() -> Vec<(String, String)> {
        vec![
            ("aaa1111".to_string(), "first".to_string()),
            ("bbb2222".to_string(), "second".to_string()),
            ("ccc3333".to_string(), "third".to_string()),
        ]
    }

    #[test]
    fn plan_split_chains_each_commit_and_leaves_the_original_on_top() {
        let layers = plan_split("feat/auth", "feat/auth", "main", "base000", &commits());

        assert_eq!(layers.len(), 3);

        // Bottom layer forks from where the branch left its parent.
        assert_eq!(layers[0].name, "feat/auth-1");
        assert_eq!(layers[0].parent, "main");
        assert_eq!(layers[0].parent_head, "base000");
        assert_eq!(layers[0].sha, "aaa1111");

        // Each subsequent layer forks from the commit below it — already true in git, so no
        // rebase is needed to make it so.
        assert_eq!(layers[1].name, "feat/auth-2");
        assert_eq!(layers[1].parent, "feat/auth-1");
        assert_eq!(layers[1].parent_head, "aaa1111");

        // The original branch keeps its name and tip, and becomes the top of the stack.
        assert_eq!(layers[2].name, "feat/auth");
        assert!(layers[2].is_original);
        assert_eq!(layers[2].sha, "ccc3333");
        assert_eq!(layers[2].parent, "feat/auth-2");
        assert_eq!(layers[2].parent_head, "bbb2222");

        assert_eq!(layers.iter().filter(|l| l.is_original).count(), 1);
    }

    #[test]
    fn plan_split_honors_a_custom_prefix_without_renaming_the_original() {
        let layers = plan_split("feat/auth", "part", "main", "base000", &commits());

        assert_eq!(layers[0].name, "part-1");
        assert_eq!(layers[1].name, "part-2");
        assert_eq!(
            layers[2].name, "feat/auth",
            "the original keeps its name so its PR survives"
        );
    }

    #[test]
    fn plan_split_of_two_commits_creates_exactly_one_new_branch() {
        let two = &commits()[..2];
        let layers = plan_split("feat/auth", "feat/auth", "main", "base000", two);

        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].name, "feat/auth-1");
        assert!(!layers[0].is_original);
        assert!(layers[1].is_original);
        assert_eq!(layers.iter().filter(|l| !l.is_original).count(), 1);
    }
}
