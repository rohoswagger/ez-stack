use console::Term;
use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;
use std::time::Duration;

pub fn success(msg: &str) {
    eprintln!("{} {}", "✓".green().bold(), msg);
}

pub fn info(msg: &str) {
    eprintln!("{} {}", "●".blue().bold(), msg);
}

pub fn warn(msg: &str) {
    eprintln!("{} {}", "⚠".yellow().bold(), msg);
}

pub fn error(msg: &str) {
    eprintln!("{} {}", "✗".red().bold(), msg);
}

pub fn hint(msg: &str) {
    eprintln!("  {} {}", "→".dimmed(), msg.dimmed());
}

pub fn active_edit_root(root: &str) {
    info(&format!("Active edit root: `{root}`"));
}

pub fn linked_worktree_warning(root: &str) {
    warn(&format!("Working in linked worktree `{root}`"));
    hint("Edit and run commands here, not in the main repo checkout.");
}

pub fn header(msg: &str) {
    eprintln!("\n{}", msg.bold());
}

pub fn branch_display(name: &str, is_current: bool) -> String {
    if is_current {
        format!("{}", name.cyan().bold())
    } else {
        format!("{}", name.white())
    }
}

pub fn pr_badge(number: u64, state: &str, is_draft: bool) -> String {
    let label = format!("#{number}");
    if is_draft {
        format!("{}", label.dimmed())
    } else {
        match state {
            "MERGED" => format!("{}", label.magenta()),
            "CLOSED" => format!("{}", label.red()),
            "OPEN" => format!("{}", label.green()),
            _ => label,
        }
    }
}

pub fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

pub fn tree_line(depth: usize, is_last: bool, ancestors_last: &[bool], text: &str) -> String {
    let mut prefix = String::new();
    for i in 0..depth.saturating_sub(1) {
        if i < ancestors_last.len() && ancestors_last[i] {
            prefix.push_str("   ");
        } else {
            prefix.push_str("│  ");
        }
    }
    if depth > 0 {
        if is_last {
            prefix.push_str("└─ ");
        } else {
            prefix.push_str("├─ ");
        }
    }
    format!("{prefix}{text}")
}

pub fn dim(text: &str) -> String {
    format!("{}", text.dimmed())
}

pub fn exit_status(code: i32, elapsed: std::time::Duration) {
    let duration = if elapsed.as_millis() < 1000 {
        format!("{}ms", elapsed.as_millis())
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    };
    let status = if code == 0 {
        "ok".to_string()
    } else {
        format!("exit:{code}")
    };
    eprintln!("{}", format!("[{status} | {duration}]").dimmed());
}

/// Emit a structured JSON receipt to stderr (dimmed).
/// Receipts let agents verify what a mutating command actually did.
pub fn receipt(value: &serde_json::Value) {
    eprintln!("{}", receipt_json(value).dimmed());
}

/// Pure function: format a receipt as a JSON string (testable without I/O).
pub fn receipt_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}

pub fn confirm(prompt: &str) -> bool {
    let term = Term::stderr();
    eprint!("{} {} ", "?".blue().bold(), prompt);
    eprint!("{} ", "(y/N)".dimmed());
    match term.read_char() {
        Ok(c) => {
            eprintln!();
            c == 'y' || c == 'Y'
        }
        Err(_) => {
            eprintln!();
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_line_draws_expected_prefixes() {
        assert_eq!(tree_line(0, true, &[], "root"), "root");
        assert_eq!(tree_line(1, false, &[], "child"), "├─ child");
        assert_eq!(tree_line(1, true, &[], "child"), "└─ child");
        assert_eq!(tree_line(2, false, &[false], "leaf"), "│  ├─ leaf");
        assert_eq!(tree_line(3, true, &[true, false], "leaf"), "   │  └─ leaf");
    }

    #[test]
    fn receipt_json_serializes_plain_json() {
        let value = serde_json::json!({"cmd": "sync", "branch": "feat/test"});
        assert_eq!(
            receipt_json(&value),
            r#"{"branch":"feat/test","cmd":"sync"}"#
        );
    }

    #[test]
    fn pr_badge_always_contains_pr_number() {
        for badge in [
            pr_badge(12, "OPEN", false),
            pr_badge(12, "CLOSED", false),
            pr_badge(12, "MERGED", false),
            pr_badge(12, "OPEN", true),
        ] {
            assert!(badge.contains("#12"));
        }
    }
}
