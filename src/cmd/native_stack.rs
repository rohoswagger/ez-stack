use crate::github;
use crate::ui;

pub(crate) fn report_outcome(outcome: &github::NativeStackOutcome) {
    match outcome {
        github::NativeStackOutcome::NotNeeded => {}
        github::NativeStackOutcome::Created { number } => {
            ui::info(&format!("Linked PRs into GitHub native stack #{number}"));
        }
        github::NativeStackOutcome::Extended { number, added } => {
            ui::info(&format!(
                "Extended GitHub native stack #{number} with {added} PR(s)"
            ));
        }
        github::NativeStackOutcome::Unchanged { number } => {
            ui::info(&format!("GitHub native stack #{number} is up to date"));
        }
        github::NativeStackOutcome::Unavailable => {
            ui::info("GitHub native stacks unavailable; ordinary PR chain succeeded");
        }
    }
}

pub(crate) fn receipt_value(
    command: &str,
    branches: &[String],
    pr_numbers: &[u64],
    outcome: &github::NativeStackOutcome,
) -> serde_json::Value {
    let mut value = match outcome {
        github::NativeStackOutcome::NotNeeded => serde_json::json!({
            "cmd": command,
            "native_stack_action": "not_needed",
        }),
        github::NativeStackOutcome::Created { number } => serde_json::json!({
            "cmd": command,
            "native_stack_action": "created",
            "native_stack_number": number,
        }),
        github::NativeStackOutcome::Extended { number, added } => serde_json::json!({
            "cmd": command,
            "native_stack_action": "extended",
            "native_stack_number": number,
            "native_stack_added": added,
        }),
        github::NativeStackOutcome::Unchanged { number } => serde_json::json!({
            "cmd": command,
            "native_stack_action": "unchanged",
            "native_stack_number": number,
        }),
        github::NativeStackOutcome::Unavailable => serde_json::json!({
            "cmd": command,
            "native_stack_action": "unavailable",
        }),
    };
    if !branches.is_empty() {
        value["branches"] = serde_json::json!(branches);
    }
    if !pr_numbers.is_empty() {
        value["pull_requests"] = serde_json::json!(pr_numbers);
    }
    value
}

pub(crate) fn error_receipt_value(
    command: &str,
    branches: &[String],
    pr_numbers: &[u64],
    error: &str,
) -> serde_json::Value {
    serde_json::json!({
        "cmd": command,
        "branches": branches,
        "pull_requests": pr_numbers,
        "native_stack_action": "error",
        "native_stack_error": error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_records_context_and_created_stack_number() {
        let branches = vec!["feat/a".to_string(), "feat/b".to_string()];
        let value = receipt_value(
            "sync",
            &branches,
            &[101, 102],
            &github::NativeStackOutcome::Created { number: 88 },
        );

        assert_eq!(value["cmd"], "sync");
        assert_eq!(value["branches"], serde_json::json!(["feat/a", "feat/b"]));
        assert_eq!(value["pull_requests"], serde_json::json!([101, 102]));
        assert_eq!(value["native_stack_action"], "created");
        assert_eq!(value["native_stack_number"], 88);
    }

    #[test]
    fn receipt_omits_empty_optional_context() {
        let value = receipt_value("submit", &[], &[], &github::NativeStackOutcome::Unavailable);

        assert_eq!(value["native_stack_action"], "unavailable");
        assert!(value.get("branches").is_none());
        assert!(value.get("pull_requests").is_none());
        assert!(value.get("native_stack_number").is_none());
    }

    #[test]
    fn error_receipt_preserves_desired_chain() {
        let value = error_receipt_value(
            "sync",
            &["feat/a".to_string(), "feat/b".to_string()],
            &[101, 102],
            "diverged",
        );

        assert_eq!(value["native_stack_action"], "error");
        assert_eq!(value["native_stack_error"], "diverged");
        assert_eq!(value["pull_requests"], serde_json::json!([101, 102]));
    }
}
