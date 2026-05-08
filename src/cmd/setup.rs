use anyhow::{Result, bail};
use std::path::PathBuf;

use crate::ui;

/// Returns the user-level ez config directory (~/.ez/).
fn ez_home() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("could not determine home directory ($HOME not set)"))?;
    Ok(PathBuf::from(home).join(".ez"))
}

/// Returns true if `ez setup` has already been run.
pub fn is_setup_done() -> bool {
    ez_home()
        .map(|p| p.join(".setup-done").exists())
        .unwrap_or(false)
}

fn mark_setup_done() -> Result<()> {
    let dir = ez_home()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(".setup-done"), "")?;
    Ok(())
}

fn detect_shell() -> Option<String> {
    std::env::var("SHELL")
        .ok()
        .and_then(|s| detect_shell_name(&s).map(str::to_string))
}

fn detect_shell_name(shell: &str) -> Option<&str> {
    let name = shell.rsplit('/').next().unwrap_or(shell);
    match name {
        "bash" | "zsh" | "fish" => Some(name),
        _ => None,
    }
}

fn rc_file_for(shell: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    rc_file_for_home(shell, PathBuf::from(home), None)
}

fn rc_file_for_home(shell: &str, home: PathBuf, bashrc_exists: Option<bool>) -> Option<PathBuf> {
    match shell {
        "zsh" => Some(home.join(".zshrc")),
        "bash" => {
            let bashrc = home.join(".bashrc");
            let exists = bashrc_exists.unwrap_or_else(|| bashrc.exists());
            if exists {
                Some(bashrc)
            } else {
                Some(home.join(".bash_profile"))
            }
        }
        "fish" => Some(home.join(".config/fish/config.fish")),
        _ => None,
    }
}

fn shell_init_line(shell: &str) -> String {
    match shell {
        "fish" => "ez shell-init | source".to_string(),
        _ => r#"eval "$(ez shell-init)""#.to_string(),
    }
}

fn path_export_line(shell: &str, install_dir: &str) -> String {
    match shell {
        "fish" => format!("fish_add_path {install_dir}"),
        _ => format!(r#"export PATH="{install_dir}:$PATH""#),
    }
}

fn planned_setup_lines(
    shell: &str,
    install_dir: Option<&str>,
    path_env: &str,
    rc_content: &str,
) -> Vec<String> {
    let mut lines_to_add: Vec<String> = Vec::new();

    if let Some(install_dir) = install_dir {
        let in_path = path_env.split(':').any(|p| p == install_dir);
        let already_in_rc = rc_content.contains(install_dir);
        if !in_path && !already_in_rc {
            lines_to_add.push(path_export_line(shell, install_dir));
        }
    }

    let init_line = shell_init_line(shell);
    let already_has_init = rc_content.contains("ez shell-init");
    if !already_has_init {
        lines_to_add.push(init_line);
    }

    lines_to_add
}

pub fn run(yes: bool, interactive: bool) -> Result<()> {
    let shell = detect_shell();

    let Some(shell) = shell else {
        bail!(
            "could not detect shell from $SHELL\n  → Set $SHELL or manually add `eval \"$(ez shell-init)\"` to your shell config"
        );
    };

    let Some(rc_path) = rc_file_for(&shell) else {
        bail!(
            "could not determine rc file for shell `{shell}`\n  → Manually add `eval \"$(ez shell-init)\"` to your shell config"
        );
    };

    let rc_display = rc_path
        .to_str()
        .unwrap_or("shell config")
        .replace(&std::env::var("HOME").unwrap_or_default(), "~");

    // Read existing rc file content (or empty if it doesn't exist).
    let rc_content = std::fs::read_to_string(&rc_path).unwrap_or_default();

    let install_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().and_then(|p| p.to_str().map(str::to_string)));
    let lines_to_add = planned_setup_lines(
        &shell,
        install_dir.as_deref(),
        &std::env::var("PATH").unwrap_or_default(),
        &rc_content,
    );

    if lines_to_add.is_empty() {
        ui::success(&format!("Shell already configured in {rc_display}"));
        mark_setup_done()?;
        return Ok(());
    }

    // Show what we'll add.
    ui::info(&format!("Will add to {rc_display}:"));
    for line in &lines_to_add {
        eprintln!("  {line}");
    }

    if !yes && interactive && !ui::confirm("Add these lines?") {
        ui::info("Cancelled — add manually if needed");
        return Ok(());
    }

    // Append to rc file.
    let mut append = String::new();
    append.push_str("\n# ez-stack shell integration\n");
    for line in &lines_to_add {
        append.push_str(line);
        append.push('\n');
    }

    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&rc_path)?;
    std::fs::write(&rc_path, format!("{rc_content}{append}"))?;

    mark_setup_done()?;

    ui::success(&format!("Updated {rc_display}"));
    ui::hint(&format!("Restart your shell or run: source {rc_display}"));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_shell_name_accepts_supported_shells_only() {
        assert_eq!(detect_shell_name("/bin/zsh"), Some("zsh"));
        assert_eq!(detect_shell_name("/usr/local/bin/bash"), Some("bash"));
        assert_eq!(detect_shell_name("fish"), Some("fish"));
        assert_eq!(detect_shell_name("/bin/tcsh"), None);
    }

    #[test]
    fn rc_file_for_home_picks_expected_shell_config() {
        let home = PathBuf::from("/tmp/home");
        assert_eq!(
            rc_file_for_home("zsh", home.clone(), None),
            Some(PathBuf::from("/tmp/home/.zshrc"))
        );
        assert_eq!(
            rc_file_for_home("fish", home.clone(), None),
            Some(PathBuf::from("/tmp/home/.config/fish/config.fish"))
        );
        assert_eq!(
            rc_file_for_home("bash", home.clone(), Some(true)),
            Some(PathBuf::from("/tmp/home/.bashrc"))
        );
        assert_eq!(
            rc_file_for_home("bash", home, Some(false)),
            Some(PathBuf::from("/tmp/home/.bash_profile"))
        );
    }

    #[test]
    fn planned_setup_lines_adds_only_missing_lines() {
        assert_eq!(
            planned_setup_lines("zsh", Some("/bin/ez"), "", ""),
            vec![
                r#"export PATH="/bin/ez:$PATH""#.to_string(),
                r#"eval "$(ez shell-init)""#.to_string()
            ]
        );
        assert_eq!(
            planned_setup_lines("fish", Some("/bin/ez"), "/bin/ez:/usr/bin", "ez shell-init"),
            Vec::<String>::new()
        );
    }
}
