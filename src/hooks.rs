use std::path::{Path, PathBuf};

use crate::git;
use crate::ui;

/// Hook files live in `.ez/hooks/<event>/` in the main worktree root.
/// They are markdown files with instructions for agents, NOT executable scripts.
///
/// Directory structure:
///   .ez/hooks/
///     post-create/
///       default.md       ← runs unless --hook overrides
///       setup-node.md    ← ez create --hook setup-node
///       setup-python.md  ← ez create --hook setup-python
///     pre-push/
///       default.md
///
/// ez prints the hook contents to stderr. The agent reads and follows them.
fn hooks_dir() -> Option<PathBuf> {
    let root = git::main_worktree_root().ok()?;
    Some(hooks_dir_from_root(&root))
}

fn hooks_dir_from_root(root: &str) -> PathBuf {
    Path::new(root).join(".ez/hooks")
}

fn hook_path(root: &str, event: &str, hook_name: Option<&str>) -> PathBuf {
    let name = hook_name.unwrap_or("default");
    hooks_dir_from_root(root)
        .join(event)
        .join(format!("{name}.md"))
}

fn list_hook_names(dir: &Path) -> Vec<String> {
    if !dir.exists() {
        return vec![];
    }

    std::fs::read_dir(dir)
        .ok()
        .map(|entries| {
            let mut hooks: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.strip_suffix(".md").map(|n| n.to_string())
                })
                .collect();
            hooks.sort();
            hooks
        })
        .unwrap_or_default()
}

/// Get hook content for a specific event and optional hook name.
/// If hook_name is None, looks for "default.md".
/// If hook_name is Some, looks for "<name>.md".
pub fn get_hook(event: &str, hook_name: Option<&str>) -> Option<String> {
    let root = git::main_worktree_root().ok()?;
    let hook_path = hook_path(&root, event, hook_name);

    if !hook_path.exists() {
        return None;
    }

    std::fs::read_to_string(&hook_path).ok()
}

/// Print hook instructions to stderr if the hook file exists.
/// Returns true if a hook was found and printed.
pub fn emit_hook(event: &str, hook_name: Option<&str>) -> bool {
    let name = hook_name.unwrap_or("default");
    if let Some(content) = get_hook(event, hook_name) {
        let content = content.trim();
        if content.is_empty() {
            return false;
        }
        if hook_name.is_some() {
            ui::info(&format!("Hook: {event}/{name}"));
        } else {
            ui::info(&format!("Hook: {event}"));
        }
        eprintln!("{content}");
        true
    } else {
        false
    }
}

/// List available hooks for an event.
pub fn list_hooks(event: &str) -> Vec<String> {
    let dir = match hooks_dir() {
        Some(d) => d.join(event),
        None => return vec![],
    };
    list_hook_names(&dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{CwdGuard, init_git_repo, take_env_lock, temp_dir, write_file};

    fn create_linked_worktree(repo: &Path, branch: &str) -> PathBuf {
        let worktree = temp_dir("linked-worktree");
        let status = std::process::Command::new("git")
            .args(["worktree", "add", "-b", branch])
            .arg(&worktree)
            .arg("HEAD")
            .current_dir(repo)
            .status()
            .expect("create linked worktree");
        assert!(status.success(), "git worktree add failed");
        worktree
    }

    #[test]
    fn hooks_dir_from_root_appends_expected_path() {
        assert_eq!(
            hooks_dir_from_root("/repo"),
            PathBuf::from("/repo/.ez/hooks")
        );
    }

    #[test]
    fn hook_path_uses_default_when_name_missing() {
        assert_eq!(
            hook_path("/repo", "post-create", None),
            PathBuf::from("/repo/.ez/hooks/post-create/default.md")
        );
        assert_eq!(
            hook_path("/repo", "post-create", Some("setup-node")),
            PathBuf::from("/repo/.ez/hooks/post-create/setup-node.md")
        );
    }

    #[test]
    fn list_hook_names_returns_sorted_markdown_stems_only() {
        let dir = temp_dir("list");
        std::fs::write(dir.join("b.md"), "").expect("write b");
        std::fs::write(dir.join("a.md"), "").expect("write a");
        std::fs::write(dir.join("notes.txt"), "").expect("write notes");

        assert_eq!(
            list_hook_names(&dir),
            vec!["a".to_string(), "b".to_string()]
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn list_hook_names_returns_empty_when_directory_is_missing() {
        let dir = temp_dir("missing-list").join("does-not-exist");

        assert_eq!(list_hook_names(&dir), Vec::<String>::new());
    }

    #[test]
    fn get_hook_returns_default_hook_from_main_worktree() {
        let _guard = take_env_lock();
        let repo = init_git_repo("hooks-main-default");
        write_file(&repo, ".ez/hooks/post-create/default.md", "main default\n");
        let _cwd = CwdGuard::enter(&repo);

        assert_eq!(
            get_hook("post-create", None).as_deref(),
            Some("main default\n")
        );
    }

    #[test]
    fn get_hook_returns_named_hook_from_linked_worktree_main_root() {
        let _guard = take_env_lock();
        let repo = init_git_repo("hooks-linked-named");
        write_file(
            &repo,
            ".ez/hooks/post-create/setup-node.md",
            "linked named\n",
        );
        let worktree = create_linked_worktree(&repo, "feat/hooks");
        let _cwd = CwdGuard::enter(&worktree);

        assert_eq!(
            get_hook("post-create", Some("setup-node")).as_deref(),
            Some("linked named\n")
        );
    }

    #[test]
    fn get_hook_returns_none_when_hook_file_is_missing() {
        let _guard = take_env_lock();
        let repo = init_git_repo("hooks-missing-file");
        let _cwd = CwdGuard::enter(&repo);

        assert_eq!(get_hook("post-create", None), None);
    }

    #[test]
    fn get_hook_returns_empty_content_when_hook_file_is_empty() {
        let _guard = take_env_lock();
        let repo = init_git_repo("hooks-empty-file");
        write_file(&repo, ".ez/hooks/post-create/default.md", "");
        let _cwd = CwdGuard::enter(&repo);

        assert_eq!(get_hook("post-create", None).as_deref(), Some(""));
    }

    #[cfg(unix)]
    #[test]
    fn get_hook_returns_none_when_hook_file_is_unreadable() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = take_env_lock();
        let repo = init_git_repo("hooks-unreadable-file");
        write_file(&repo, ".ez/hooks/post-create/default.md", "secret\n");
        let hook = repo.join(".ez/hooks/post-create/default.md");
        let mut perms = std::fs::metadata(&hook).expect("metadata").permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&hook, perms).expect("chmod unreadable");
        let _cwd = CwdGuard::enter(&repo);

        assert_eq!(get_hook("post-create", None), None);
    }

    #[test]
    fn emit_hook_returns_false_when_hook_file_is_missing() {
        let _guard = take_env_lock();
        let repo = init_git_repo("hooks-emit-missing");
        let _cwd = CwdGuard::enter(&repo);

        assert!(!emit_hook("post-create", None));
    }

    #[test]
    fn emit_hook_returns_false_when_hook_file_is_empty() {
        let _guard = take_env_lock();
        let repo = init_git_repo("hooks-emit-empty");
        write_file(&repo, ".ez/hooks/post-create/default.md", "\n\t\n");
        let _cwd = CwdGuard::enter(&repo);

        assert!(!emit_hook("post-create", None));
    }

    #[test]
    fn emit_hook_returns_true_for_default_hook() {
        let _guard = take_env_lock();
        let repo = init_git_repo("hooks-emit-default");
        write_file(&repo, ".ez/hooks/post-create/default.md", "run setup\n");
        let _cwd = CwdGuard::enter(&repo);

        assert!(emit_hook("post-create", None));
    }

    #[test]
    fn emit_hook_returns_true_for_named_hook() {
        let _guard = take_env_lock();
        let repo = init_git_repo("hooks-emit-named");
        write_file(
            &repo,
            ".ez/hooks/post-create/setup-python.md",
            "run python\n",
        );
        let _cwd = CwdGuard::enter(&repo);

        assert!(emit_hook("post-create", Some("setup-python")));
    }

    #[test]
    fn list_hooks_returns_sorted_markdown_stems_from_main_worktree() {
        let _guard = take_env_lock();
        let repo = init_git_repo("hooks-list-real");
        write_file(&repo, ".ez/hooks/post-create/z-last.md", "");
        write_file(&repo, ".ez/hooks/post-create/a-first.md", "");
        write_file(&repo, ".ez/hooks/post-create/readme.txt", "");
        let worktree = create_linked_worktree(&repo, "feat/list-hooks");
        let _cwd = CwdGuard::enter(&worktree);

        assert_eq!(
            list_hooks("post-create"),
            vec!["a-first".to_string(), "z-last".to_string()]
        );
    }
}
