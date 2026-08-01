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

fn user_home_dir() -> Result<PathBuf> {
    for key in ["HOME", "USERPROFILE"] {
        if let Some(value) = std::env::var_os(key) {
            if !value.is_empty() {
                return Ok(PathBuf::from(value));
            }
        }
    }

    anyhow::bail!(
        "could not determine user home directory; set HOME or USERPROFILE before running `ez skill install`"
    )
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

fn try_symlink_or_copy_with(
    link_dir: &Path,
    expected_target: &Path,
    create_symlink: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<AgentInstallStatus> {
    match create_symlink(expected_target, link_dir) {
        Ok(()) => Ok(AgentInstallStatus::Linked),
        Err(_) => {
            write_skill_file(link_dir)?;
            Ok(AgentInstallStatus::Copied)
        }
    }
}

fn try_symlink_or_copy(link_dir: &Path, expected_target: &Path) -> Result<AgentInstallStatus> {
    try_symlink_or_copy_with(link_dir, expected_target, symlink_dir)
}

fn is_ez_workflow_skill(content: &str) -> bool {
    let mut lines = content.lines();
    if lines.next() != Some("---") {
        return false;
    }

    for line in lines {
        if line == "---" {
            return false;
        }
        if line.trim() == "name: ez-workflow" {
            return true;
        }
    }

    false
}

fn managed_fallback_copy_content(dir: &Path) -> Result<Option<String>> {
    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.is_dir() {
        return Ok(None);
    }

    let mut entries = std::fs::read_dir(dir)?;
    let Some(entry) = entries.next().transpose()? else {
        return Ok(None);
    };
    if entries.next().transpose()?.is_some()
        || entry.file_name() != SKILL_FILE
        || !entry.file_type()?.is_file()
    {
        return Ok(None);
    }

    let content = std::fs::read_to_string(entry.path())?;
    Ok(is_ez_workflow_skill(&content).then_some(content))
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

        if let Some(existing) = managed_fallback_copy_content(link_dir)? {
            if existing == SKILL_CONTENT {
                return Ok(AgentInstallStatus::Unchanged);
            }
            write_skill_file(link_dir)?;
            return Ok(AgentInstallStatus::Copied);
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
    let root = user_home_dir()?;
    install_into_root(&root)?;
    Ok(())
}

fn is_managed_agent_skill_target(target_dir: &Path, link_dir: &Path) -> Result<bool> {
    let metadata = std::fs::symlink_metadata(link_dir)?;
    if metadata.file_type().is_symlink() {
        let parent = link_dir
            .parent()
            .context("skill symlink path must have a parent directory")?;
        return Ok(std::fs::read_link(link_dir)? == relative_path(parent, target_dir));
    }

    Ok(managed_fallback_copy_content(link_dir)?.is_some())
}

fn uninstall_from_root(root: &Path) -> Result<()> {
    let canonical_dir = canonical_skill_dir(root);
    let canonical_file = canonical_skill_file(root);

    let mut removed = false;
    for link_dir in agent_skill_dirs(root) {
        if std::fs::symlink_metadata(&link_dir).is_ok()
            && is_managed_agent_skill_target(&canonical_dir, &link_dir)?
        {
            remove_path(&link_dir)?;
            removed = true;
        }
    }

    match std::fs::symlink_metadata(&canonical_file) {
        Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
            std::fs::remove_file(&canonical_file)?;
            removed = true;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    if let Ok(metadata) = std::fs::symlink_metadata(&canonical_dir) {
        if metadata.is_dir() && std::fs::read_dir(&canonical_dir)?.next().is_none() {
            std::fs::remove_dir(&canonical_dir)?;
        }
    }

    if !removed {
        ui::info("ez-workflow skill is not installed for this user");
        return Ok(());
    }

    ui::success("Uninstalled ez-workflow skill");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let root = user_home_dir()?;
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

    #[test]
    fn relative_path_returns_dot_for_identical_paths() {
        assert_eq!(
            relative_path(Path::new("/repo/.agents"), Path::new("/repo/.agents")),
            PathBuf::from(".")
        );
    }

    #[test]
    fn ez_workflow_skill_detection_requires_front_matter_name_before_close() {
        assert!(!is_ez_workflow_skill("name: ez-workflow\n"));
        assert!(!is_ez_workflow_skill("---\n---\nname: ez-workflow\n"));
        assert!(!is_ez_workflow_skill("---\nname: other-workflow\n---\n"));
        assert!(!is_ez_workflow_skill("---\nsummary: missing name\n"));
        assert!(is_ez_workflow_skill("---\nname: ez-workflow\n---\n"));
    }

    #[test]
    fn remove_path_handles_files_and_directories() {
        let root = temp_dir("skill-remove-path");
        let file = root.join("file.txt");
        std::fs::write(&file, "remove me\n").expect("write file");
        remove_path(&file).expect("remove file");
        assert!(!file.exists());

        let dir = root.join("dir");
        std::fs::create_dir_all(dir.join("nested")).expect("create dir");
        std::fs::write(dir.join("nested/file.txt"), "remove me too\n").expect("write nested");
        remove_path(&dir).expect("remove dir");
        assert!(!dir.exists());

        #[cfg(unix)]
        {
            let socket_root = PathBuf::from("/tmp").join(format!(
                "ez-skill-sock-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&socket_root).expect("create short socket root");
            let socket = socket_root.join("agent.sock");
            let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind socket");
            remove_path(&socket).expect("ignore special file");
            assert!(socket.exists());
            drop(listener);
            std::fs::remove_file(socket).expect("remove socket");
            std::fs::remove_dir(socket_root).expect("remove socket root");
        }
    }

    #[test]
    fn managed_fallback_copy_content_rejects_unmanaged_shapes() {
        let root = temp_dir("skill-managed-copy-shapes");

        let plain_file = root.join("plain-file");
        std::fs::write(&plain_file, SKILL_CONTENT).expect("write plain file");
        assert!(
            managed_fallback_copy_content(&plain_file)
                .expect("plain file check")
                .is_none()
        );

        let empty_dir = root.join("empty");
        std::fs::create_dir_all(&empty_dir).expect("create empty dir");
        assert!(
            managed_fallback_copy_content(&empty_dir)
                .expect("empty dir check")
                .is_none()
        );

        let wrong_name = root.join("wrong-name");
        std::fs::create_dir_all(&wrong_name).expect("create wrong-name dir");
        std::fs::write(wrong_name.join("README.md"), SKILL_CONTENT).expect("write wrong-name file");
        assert!(
            managed_fallback_copy_content(&wrong_name)
                .expect("wrong-name check")
                .is_none()
        );

        let extra_entry = root.join("extra-entry");
        std::fs::create_dir_all(&extra_entry).expect("create extra-entry dir");
        std::fs::write(extra_entry.join(SKILL_FILE), SKILL_CONTENT).expect("write skill");
        std::fs::write(extra_entry.join("notes.txt"), "extra\n").expect("write extra");
        assert!(
            managed_fallback_copy_content(&extra_entry)
                .expect("extra-entry check")
                .is_none()
        );
    }

    #[test]
    fn symlink_failure_installs_a_managed_compatibility_copy() {
        let root = temp_dir("skill-symlink-copy-fallback");
        let canonical = canonical_skill_dir(&root);
        write_skill_file(&canonical).expect("write canonical skill");
        let copy_target = root.join(".codex/skills").join(SKILL_NAME);

        let status = try_symlink_or_copy_with(&copy_target, &canonical, |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "simulated symlink denial",
            ))
        })
        .expect("install fallback copy");

        assert_eq!(status, AgentInstallStatus::Copied);
        assert_eq!(
            std::fs::read_to_string(copy_target.join(SKILL_FILE)).expect("read fallback copy"),
            SKILL_CONTENT
        );
    }

    #[test]
    fn uninstall_is_idempotent_when_nothing_is_installed() {
        let root = temp_dir("skill-uninstall-missing");
        uninstall_from_root(&root).expect("uninstall missing skill");
        assert!(!canonical_skill_dir(&root).exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn managed_agent_target_recognizes_relative_symlink_and_fallback_copy() {
        let root = temp_dir("skill-managed-target");
        let canonical = canonical_skill_dir(&root);
        write_skill_file(&canonical).expect("write canonical skill");

        let symlink_target = root.join(".claude/skills").join(SKILL_NAME);
        ensure_agent_skill_target(&canonical, &symlink_target).expect("create symlink target");
        assert!(
            is_managed_agent_skill_target(&canonical, &symlink_target)
                .expect("symlink target check")
        );

        let copy_target = root.join(".codex/skills").join(SKILL_NAME);
        write_skill_file(&copy_target).expect("write fallback copy");
        assert!(
            is_managed_agent_skill_target(&canonical, &copy_target).expect("copy target check")
        );
        assert_eq!(
            ensure_agent_skill_target(&canonical, &copy_target).expect("unchanged copy check"),
            AgentInstallStatus::Unchanged
        );

        std::fs::write(copy_target.join(SKILL_FILE), "---\nname: other\n---\n")
            .expect("replace with unmanaged copy");
        assert!(
            !is_managed_agent_skill_target(&canonical, &copy_target).expect("unmanaged copy check")
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

    #[cfg(any(unix, windows))]
    #[test]
    fn install_repairs_stale_canonical_symlink_and_managed_copy() {
        let root = temp_dir("skill-install-repair");
        let canonical_dir = canonical_skill_dir(&root);
        let canonical_file = canonical_skill_file(&root);
        std::fs::create_dir_all(&canonical_dir).expect("create canonical dir");
        std::fs::write(&canonical_file, "stale canonical\n").expect("write stale canonical");

        let link_dirs = agent_skill_dirs(&root);
        let stale_link = &link_dirs[0];
        std::fs::create_dir_all(stale_link.parent().expect("link parent"))
            .expect("create link parent");
        symlink_dir(Path::new("../wrong-target"), stale_link).expect("create stale symlink");

        let stale_copy = &link_dirs[1];
        std::fs::create_dir_all(stale_copy).expect("create stale copy dir");
        std::fs::write(
            stale_copy.join(SKILL_FILE),
            "---\nname: ez-workflow\n---\nstale\n",
        )
        .expect("write stale managed copy");

        install_into_root(&root).expect("repair installation");

        assert_eq!(
            std::fs::read_to_string(&canonical_file).expect("read canonical"),
            SKILL_CONTENT
        );
        assert_eq!(
            std::fs::read_link(stale_link).expect("read repaired symlink"),
            relative_path(stale_link.parent().expect("link parent"), &canonical_dir)
        );
        assert_eq!(
            std::fs::read_to_string(stale_copy.join(SKILL_FILE)).expect("read repaired copy"),
            SKILL_CONTENT
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn install_recreates_a_missing_link_without_rewriting_canonical_skill() {
        let root = temp_dir("skill-install-relink");
        install_into_root(&root).expect("initial install");
        let canonical_file = canonical_skill_file(&root);
        let original = std::fs::read_to_string(&canonical_file).expect("read canonical");
        let missing_link = agent_skill_dirs(&root).remove(0);
        remove_path(&missing_link).expect("remove compatibility link");

        install_into_root(&root).expect("repair missing link");
        install_into_root(&root).expect("confirm idempotent installation");

        assert_eq!(
            std::fs::read_to_string(&canonical_file).expect("reread canonical"),
            original
        );
        assert!(std::fs::symlink_metadata(&missing_link).is_ok());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn uninstall_removes_managed_targets_and_preserves_custom_skill_directory() {
        let root = temp_dir("skill-uninstall-managed");
        install_into_root(&root).expect("install skill");
        let link_dirs = agent_skill_dirs(&root);
        let managed_link = &link_dirs[0];
        let custom_dir = &link_dirs[1];
        remove_path(custom_dir).expect("remove managed link");
        std::fs::create_dir_all(custom_dir).expect("create custom directory");
        std::fs::write(custom_dir.join("custom.txt"), "preserve\n").expect("write custom skill");

        uninstall_from_root(&root).expect("uninstall managed skill");

        assert!(std::fs::symlink_metadata(managed_link).is_err());
        assert!(!canonical_skill_dir(&root).exists());
        assert_eq!(
            std::fs::read_to_string(custom_dir.join("custom.txt")).expect("read custom skill"),
            "preserve\n"
        );
    }

    #[test]
    fn uninstall_preserves_non_file_canonical_entry() {
        let root = temp_dir("skill-uninstall-canonical-dir");
        let canonical_file = canonical_skill_file(&root);
        std::fs::create_dir_all(&canonical_file).expect("create directory at canonical file path");
        std::fs::write(canonical_file.join("keep.txt"), "preserve\n")
            .expect("write preserved entry");

        uninstall_from_root(&root).expect("ignore non-file canonical entry");

        assert_eq!(
            std::fs::read_to_string(canonical_file.join("keep.txt")).expect("read preserved entry"),
            "preserve\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_propagates_canonical_metadata_permission_errors() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir("skill-uninstall-permission-error");
        let canonical_dir = canonical_skill_dir(&root);
        std::fs::create_dir_all(&canonical_dir).expect("create canonical directory");
        std::fs::set_permissions(&canonical_dir, std::fs::Permissions::from_mode(0o000))
            .expect("deny canonical directory access");

        let error = uninstall_from_root(&root).expect_err("metadata denial should propagate");

        std::fs::set_permissions(&canonical_dir, std::fs::Permissions::from_mode(0o700))
            .expect("restore canonical directory access");
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .expect("io error")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
}
