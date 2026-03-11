use anyhow::{Context, Result, bail};
use std::process::Command;

use crate::error::EzError;

fn run_gh(args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .with_context(|| format!("failed to run gh {}", args.join(" ")))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(EzError::GhError(stderr).into())
    }
}

#[derive(Debug, Clone)]
pub struct PrInfo {
    pub number: u64,
    pub url: String,
    pub state: String,
    pub title: String,
    pub is_draft: bool,
    pub merged: bool,
}

pub fn body_from_file(path: &str) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("failed to read body file `{path}`"))
}

pub fn create_pr(title: &str, body: &str, base: &str, head: &str, draft: bool) -> Result<PrInfo> {
    let mut args = vec![
        "pr", "create", "--title", title, "--body", body, "--base", base, "--head", head,
    ];
    if draft {
        args.push("--draft");
    }
    let url = run_gh(&args)?;

    // Extract PR number from URL
    let number = url
        .rsplit('/')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| anyhow::anyhow!("could not parse PR number from URL: {url}"))?;

    Ok(PrInfo {
        number,
        url,
        state: "OPEN".to_string(),
        title: title.to_string(),
        is_draft: draft,
        merged: false,
    })
}

pub fn update_pr_base(pr_number: u64, new_base: &str) -> Result<()> {
    run_gh(&["pr", "edit", &pr_number.to_string(), "--base", new_base])?;
    Ok(())
}

pub fn get_pr_status(branch: &str) -> Result<Option<PrInfo>> {
    let output = run_gh(&[
        "pr",
        "view",
        branch,
        "--json",
        "number,url,state,title,isDraft,mergedAt",
    ]);

    match output {
        Ok(json_str) => {
            let v: serde_json::Value = serde_json::from_str(&json_str)?;
            Ok(Some(PrInfo {
                number: v["number"].as_u64().unwrap_or(0),
                url: v["url"].as_str().unwrap_or("").to_string(),
                state: v["state"].as_str().unwrap_or("UNKNOWN").to_string(),
                title: v["title"].as_str().unwrap_or("").to_string(),
                is_draft: v["isDraft"].as_bool().unwrap_or(false),
                merged: v["mergedAt"].as_str().is_some_and(|s| !s.is_empty()),
            }))
        }
        Err(_) => Ok(None),
    }
}

pub fn merge_pr(pr_number: u64, method: &str) -> Result<()> {
    let flag = match method {
        "squash" => "--squash",
        "rebase" => "--rebase",
        _ => "--merge",
    };
    run_gh(&[
        "pr",
        "merge",
        &pr_number.to_string(),
        flag,
        "--delete-branch",
    ])?;
    Ok(())
}

pub fn edit_pr(pr_number: u64, title: Option<&str>, body: Option<&str>) -> Result<()> {
    let number_str = pr_number.to_string();
    let mut args: Vec<&str> = vec!["pr", "edit", &number_str];
    if let Some(t) = title {
        args.extend_from_slice(&["--title", t]);
    }
    if let Some(b) = body {
        args.extend_from_slice(&["--body", b]);
    }
    if args.len() == 3 {
        anyhow::bail!("No edits specified — provide --title, --body, or --body-file");
    }
    run_gh(&args)?;
    Ok(())
}

pub fn is_gh_authenticated() -> bool {
    run_gh(&["auth", "status"]).is_ok()
}

pub fn repo_name() -> Result<String> {
    let output = run_gh(&[
        "repo",
        "view",
        "--json",
        "nameWithOwner",
        "-q",
        ".nameWithOwner",
    ])?;
    if output.is_empty() {
        bail!("could not determine repository name — make sure you're in a GitHub repo");
    }
    Ok(output)
}

/// Fetch the current body of a PR (raw markdown, no stack section stripped).
pub fn get_pr_body(pr_number: u64) -> Result<String> {
    let body = run_gh(&[
        "pr",
        "view",
        &pr_number.to_string(),
        "--json",
        "body",
        "-q",
        ".body",
    ])?;
    Ok(body)
}

/// Open the PR for a branch in the default browser.
pub fn open_pr_in_browser(branch: &str) -> Result<()> {
    run_gh(&["pr", "view", "--web", branch])?;
    Ok(())
}

/// Get the latest CI run status for a branch.
/// Returns a short status string: "✓", "✗", "⏳", or "" if no runs found.
pub fn get_ci_status(branch: &str) -> String {
    let output = run_gh(&[
        "run",
        "list",
        "--branch",
        branch,
        "--limit",
        "1",
        "--json",
        "status,conclusion",
        "--jq",
        ".[0]",
    ]);
    match output {
        Ok(json_str) if !json_str.is_empty() && json_str != "null" => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json_str) {
                let status = v["status"].as_str().unwrap_or("");
                let conclusion = v["conclusion"].as_str().unwrap_or("");
                match (status, conclusion) {
                    ("completed", "success") => "✓".to_string(),
                    ("completed", _) => "✗".to_string(),
                    ("in_progress", _) | ("queued", _) | ("waiting", _) => "⏳".to_string(),
                    _ => String::new(),
                }
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// Set or unset draft status on a PR.
/// `ready = true` → mark ready for review; `ready = false` → mark as draft.
pub fn set_pr_ready(pr_number: u64, ready: bool) -> Result<()> {
    let number = pr_number.to_string();
    if ready {
        run_gh(&["pr", "ready", &number])?;
    } else {
        run_gh(&["pr", "ready", "--undo", &number])?;
    }
    Ok(())
}
