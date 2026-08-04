use crate::git::RebaseConflict;
use crate::ui;

fn receipt_value(
    cmd: &str,
    branch: &str,
    parent: &str,
    conflict: &RebaseConflict,
    next_command: &str,
) -> serde_json::Value {
    serde_json::json!({
        "cmd": cmd,
        "action": "conflict",
        "branch": branch,
        "parent": parent,
        "conflicting_files": conflict.conflicting_files,
        "git_stderr": conflict.stderr,
        "next_command": next_command,
    })
}

pub fn report(
    cmd: &str,
    branch: &str,
    parent: &str,
    conflict: &RebaseConflict,
    next_command: &str,
) {
    ui::warn(&format!(
        "Rebase conflict while updating `{branch}` onto `{parent}`"
    ));

    if !conflict.conflicting_files.is_empty() {
        eprintln!("  Conflicting files:");
        for file in &conflict.conflicting_files {
            eprintln!("    {file}");
        }
    }

    if !conflict.stderr.is_empty() {
        eprintln!("  Git reported:");
        for line in conflict.stderr.lines() {
            eprintln!("    {line}");
        }
    }

    ui::hint("Review the files above and decide the intended merged result on this branch");
    ui::hint(&format!("After updating the branch, run `{next_command}`"));
    ui::receipt(&receipt_value(cmd, branch, parent, conflict, next_command));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_value_contains_agent_fields() {
        let conflict = RebaseConflict {
            conflicting_files: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            stderr: "CONFLICT (content): Merge conflict in src/a.rs".to_string(),
        };

        let receipt = receipt_value("sync", "feat/test", "main", &conflict, "ez restack");

        assert_eq!(receipt["cmd"], "sync");
        assert_eq!(receipt["action"], "conflict");
        assert_eq!(receipt["branch"], "feat/test");
        assert_eq!(receipt["parent"], "main");
        assert_eq!(receipt["next_command"], "ez restack");
        assert_eq!(receipt["conflicting_files"][0], "src/a.rs");
        assert_eq!(receipt["conflicting_files"][1], "src/b.rs");
        assert_eq!(
            receipt["git_stderr"],
            "CONFLICT (content): Merge conflict in src/a.rs"
        );
    }

    #[test]
    fn report_handles_file_and_multiline_git_details() {
        let conflict = RebaseConflict {
            conflicting_files: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            stderr: "first line\nsecond line".to_string(),
        };

        report("sync", "feat/test", "main", &conflict, "ez restack");
    }
}
