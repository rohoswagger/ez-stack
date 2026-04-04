use anyhow::{Context, Result};
use std::path::{Component, Path, PathBuf};

use crate::ui;

const SKILL_CONTENT: &str = include_str!("../../SKILL.md");
const SKILL_NAME: &str = "ez-workflow";
const SKILL_FILE: &str = "SKILL.md";
const AGENT_LINK_DIRS: &[&str] = &[".claude/skills", ".codex/skills"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentInstallStatus {
    Unchanged,
    Linked,
    Copied,
    PreservedExisting,
}

fn repo_root() -> Result<PathBuf> {
    Ok(PathBuf::from(crate::git::repo_root()?))
}

fn canonical_skill_dir(root: &Path) -> PathBuf {
    root.join(".agents/skills").join(SKILL_NAME)
}

fn canonical_skill_file(root: &Path) -> PathBuf {
    canonical_skill_dir(root).join(SKILL_FILE)
}

fn agent_skill_file(dir: &Path) -> PathBuf {
    dir.join(SKILL_FILE)
}

fn agent_skill_dirs(root: &Path) -> Vec<PathBuf> {
    AGENT_LINK_DIRS
        .iter()
        .map(|dir| root.join(dir).join(SKILL_NAME))
        .collect()
}

fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let from_components: Vec<Component<'_>> = from.components().collect();
    let to_components: Vec<Component<'_>> = to.components().collect();

    let common_len = from_components
        .iter()
        .zip(to_components.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let mut result = PathBuf::new();
    for _ in common_len..from_components.len() {
        result.push("..");
    }
    for component in &to_components[common_len..] {
        result.push(component.as_os_str());
    }

    if result.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        result
    }
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

fn remove_path(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        std::fs::remove_file(path)?;
    } else if metadata.is_dir() {
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn write_skill_file(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(agent_skill_file(dir), SKILL_CONTENT)?;
    Ok(())
}

fn try_symlink_or_copy(link_dir: &Path, expected_target: &Path) -> Result<AgentInstallStatus> {
    match symlink_dir(expected_target, link_dir) {
        Ok(()) => Ok(AgentInstallStatus::Linked),
        Err(_) => {
            write_skill_file(link_dir)?;
            Ok(AgentInstallStatus::Copied)
        }
    }
}

fn ensure_agent_skill_target(target_dir: &Path, link_dir: &Path) -> Result<AgentInstallStatus> {
    let parent = link_dir
        .parent()
        .context("skill symlink path must have a parent directory")?;
    std::fs::create_dir_all(parent)?;

    let expected_target = relative_path(parent, target_dir);

    if let Ok(metadata) = std::fs::symlink_metadata(link_dir) {
        if metadata.file_type().is_symlink() {
            if std::fs::read_link(link_dir)? == expected_target {
                return Ok(AgentInstallStatus::Unchanged);
            }
            remove_path(link_dir)?;
            return try_symlink_or_copy(link_dir, &expected_target);
        }
        return Ok(AgentInstallStatus::PreservedExisting);
    }

    try_symlink_or_copy(link_dir, &expected_target)
}

fn install_into_root(root: &Path) -> Result<PathBuf> {
    let canonical_dir = canonical_skill_dir(root);
    let skill_path = canonical_skill_file(root);

    let mut changed = false;
    if skill_path.exists() {
        let existing = std::fs::read_to_string(&skill_path)?;
        if existing != SKILL_CONTENT {
            std::fs::write(&skill_path, SKILL_CONTENT)?;
            changed = true;
        }
    } else {
        std::fs::create_dir_all(&canonical_dir)?;
        std::fs::write(&skill_path, SKILL_CONTENT)?;
        changed = true;
    }

    let mut linked_any = false;
    let mut copied_any = false;
    let mut preserved_existing = Vec::new();
    for link_dir in agent_skill_dirs(root) {
        match ensure_agent_skill_target(&canonical_dir, &link_dir)? {
            AgentInstallStatus::Unchanged => {}
            AgentInstallStatus::Linked => linked_any = true,
            AgentInstallStatus::Copied => copied_any = true,
            AgentInstallStatus::PreservedExisting => {
                preserved_existing.push(link_dir);
            }
        }
    }

    if changed {
        ui::success("Installed ez-workflow skill");
    } else if linked_any || copied_any {
        ui::success("Updated ez-workflow skill links");
    } else {
        ui::info("ez-workflow skill is already up to date");
    }

    if copied_any {
        ui::hint(
            "Symlinks were not available for some agent roots, installed compatibility copies instead",
        );
    }
    for preserved in &preserved_existing {
        ui::warn(&format!(
            "Preserved existing skill directory at `{}` instead of replacing it with a symlink",
            preserved.display()
        ));
    }

    println!("{}", skill_path.display());
    Ok(skill_path)
}

pub fn install() -> Result<()> {
    let root = repo_root()?;
    install_into_root(&root)?;
    Ok(())
}

fn uninstall_from_root(root: &Path) -> Result<()> {
    let canonical_dir = canonical_skill_dir(root);

    let mut removed = false;
    for link_dir in agent_skill_dirs(root) {
        if std::fs::symlink_metadata(&link_dir).is_ok() {
            remove_path(&link_dir)?;
            removed = true;
        }
    }

    if canonical_dir.exists() {
        std::fs::remove_dir_all(&canonical_dir)?;
        removed = true;
    }

    if !removed {
        ui::info("ez-workflow skill is not installed in this repo");
        return Ok(());
    }

    ui::success("Uninstalled ez-workflow skill");
    ui::hint("Remove .agents/skills/ez-workflow/ from version control if committed");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let root = repo_root()?;
    uninstall_from_root(&root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_dir;

    #[test]
    fn canonical_skill_dir_points_to_agents_skills() {
        assert_eq!(
            canonical_skill_dir(Path::new("/repo")),
            PathBuf::from("/repo/.agents/skills/ez-workflow")
        );
    }

    #[test]
    fn agent_skill_dirs_include_claude_and_codex() {
        assert_eq!(
            agent_skill_dirs(Path::new("/repo")),
            vec![
                PathBuf::from("/repo/.claude/skills/ez-workflow"),
                PathBuf::from("/repo/.codex/skills/ez-workflow"),
            ]
        );
    }

    #[test]
    fn relative_path_walks_between_agent_dirs() {
        assert_eq!(
            relative_path(
                Path::new("/repo/.claude/skills"),
                Path::new("/repo/.agents/skills/ez-workflow")
            ),
            PathBuf::from("../../.agents/skills/ez-workflow")
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn install_creates_canonical_skill_and_symlinks() {
        let root = temp_dir("skill-install");
        install_into_root(&root).expect("install skill");

        let canonical_file = canonical_skill_file(&root);
        assert_eq!(
            std::fs::read_to_string(&canonical_file).expect("read skill"),
            SKILL_CONTENT
        );

        for link_dir in agent_skill_dirs(&root) {
            let metadata = std::fs::symlink_metadata(&link_dir).expect("link metadata");
            assert!(metadata.file_type().is_symlink() || metadata.is_dir());
        }
    }

    #[test]
    fn install_preserves_existing_non_symlink_agent_skill_dir() {
        let root = temp_dir("skill-install-preserve");
        let existing_dir = root.join(".claude/skills").join(SKILL_NAME);
        std::fs::create_dir_all(&existing_dir).expect("create existing dir");
        std::fs::write(existing_dir.join("custom.txt"), "keep me\n").expect("write custom file");

        install_into_root(&root).expect("install skill");

        assert_eq!(
            std::fs::read_to_string(existing_dir.join("custom.txt")).expect("read custom file"),
            "keep me\n"
        );
        assert!(
            std::fs::symlink_metadata(&existing_dir)
                .expect("metadata")
                .file_type()
                .is_dir()
        );
    }

    #[test]
    fn install_falls_back_to_copy_when_symlink_dir_exists_as_plain_dir() {
        let root = temp_dir("skill-install-copy");
        let codex_dir = root.join(".codex/skills");
        std::fs::create_dir_all(&codex_dir).expect("create parent");

        let copied_dir = codex_dir.join(SKILL_NAME);
        write_skill_file(&copied_dir).expect("write compatibility copy");

        assert_eq!(
            std::fs::read_to_string(copied_dir.join(SKILL_FILE)).expect("read copied skill"),
            SKILL_CONTENT
        );
    }
}
