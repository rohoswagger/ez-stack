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
    std::fs::read_to_string(path)
        .with_context(|| format!("failed to read body file `{path}`"))
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
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

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
        anyhow::bail!("No edits specified — provide --title or --body");
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
