use anyhow::{Context, Result, bail};
use std::io::Write;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

use crate::error::EzError;

const GITHUB_API_VERSION_HEADER: &str = "X-GitHub-Api-Version: 2026-03-10";
const GITHUB_JSON_ACCEPT_HEADER: &str = "Accept: application/vnd.github+json";
const MAX_NATIVE_STACK_MUTATION_ATTEMPTS: usize = 3;
const NATIVE_STACK_RETRY_BACKOFF: Duration = Duration::from_millis(50);

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

fn run_gh_in_repo(args: &[&str], repo: Option<&str>) -> Result<String> {
    let Some(repo) = repo else {
        return run_gh(args);
    };
    validate_repo_name(repo)?;
    let mut scoped_args = Vec::with_capacity(args.len() + 2);
    scoped_args.extend_from_slice(args);
    scoped_args.extend_from_slice(&["--repo", repo]);
    run_gh(&scoped_args)
}

fn run_gh_with_stdin(args: &[&str], stdin: &str) -> Result<String> {
    let mut child = Command::new("gh")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run gh {}", args.join(" ")))?;

    child
        .stdin
        .take()
        .context("failed to open gh stdin")?
        .write_all(stdin.as_bytes())
        .context("failed to write gh stdin")?;

    let output = child
        .wait_with_output()
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
    pub base: String,
    pub is_draft: bool,
    pub merged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeStackOutcome {
    NotNeeded,
    Created { number: u64 },
    Extended { number: u64, added: usize },
    Unchanged { number: u64 },
    Repaired { previous_number: u64, number: u64 },
    NotApplicable { reason: String },
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeStackInfo {
    pub number: u64,
    pub base_ref: Option<String>,
    pub open: Option<bool>,
    pub pull_requests: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeStackLookup {
    Found(NativeStackInfo),
    NotLinked,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergePrOutcome {
    Merged,
    Enqueued,
}

pub fn body_from_file(path: &str) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("failed to read body file `{path}`"))
}

/// Candidate locations GitHub checks for a pull request template.
const PR_TEMPLATE_CANDIDATES: &[&str] = &[
    ".github/PULL_REQUEST_TEMPLATE.md",
    ".github/pull_request_template.md",
    ".github/PULL_REQUEST_TEMPLATE/pull_request_template.md",
    ".github/PULL_REQUEST_TEMPLATE/PULL_REQUEST_TEMPLATE.md",
    "PULL_REQUEST_TEMPLATE.md",
    "pull_request_template.md",
    "docs/PULL_REQUEST_TEMPLATE.md",
];

/// Best-effort read of the repository's pull request template, if any.
///
/// GitHub pre-fills the body of a new PR with the repo's template when it is
/// created through the web UI. `ez push`/`ez submit` mirror that behavior so a
/// new PR is born with the template filled in rather than a bare one-liner.
///
/// Returns `None` (rather than an error) when no template exists, it is empty,
/// or it cannot be read, so callers can fall back to the default body.
pub fn read_pr_template() -> Option<String> {
    let root = crate::git::repo_root().ok()?;
    for candidate in PR_TEMPLATE_CANDIDATES {
        let Ok(content) = std::fs::read_to_string(std::path::Path::new(&root).join(candidate))
        else {
            continue;
        };
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

pub fn create_pr(
    title: &str,
    body: &str,
    base: &str,
    head: &str,
    draft: bool,
    repo: Option<&str>,
) -> Result<PrInfo> {
    let mut args = vec![
        "pr", "create", "--title", title, "--body", body, "--base", base, "--head", head,
    ];
    if draft {
        args.push("--draft");
    }
    let url = run_gh_in_repo(&args, repo)?;

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
        base: base.to_string(),
        is_draft: draft,
        merged: false,
    })
}

pub fn update_pr_base(pr_number: u64, new_base: &str, repo: Option<&str>) -> Result<()> {
    run_gh_in_repo(
        &["pr", "edit", &pr_number.to_string(), "--base", new_base],
        repo,
    )?;
    Ok(())
}

pub fn get_pr_status(branch: &str, repo: Option<&str>) -> Result<Option<PrInfo>> {
    let output = run_gh_in_repo(
        &[
            "pr",
            "view",
            branch,
            "--json",
            "number,url,state,title,isDraft,mergedAt,baseRefName",
        ],
        repo,
    );

    match output {
        Ok(json_str) => {
            let v: serde_json::Value = serde_json::from_str(&json_str)?;
            Ok(Some(PrInfo {
                number: v["number"].as_u64().unwrap_or(0),
                url: v["url"].as_str().unwrap_or("").to_string(),
                state: v["state"].as_str().unwrap_or("UNKNOWN").to_string(),
                title: v["title"].as_str().unwrap_or("").to_string(),
                base: v["baseRefName"].as_str().unwrap_or("").to_string(),
                is_draft: v["isDraft"].as_bool().unwrap_or(false),
                merged: v["mergedAt"].as_str().is_some_and(|s| !s.is_empty()),
            }))
        }
        Err(_) => Ok(None),
    }
}

pub fn get_all_pr_statuses(repo: Option<&str>) -> std::collections::HashMap<String, PrInfo> {
    let mut map = std::collections::HashMap::new();
    let mut page = 1;

    loop {
        let route = match repo {
            Some(repo) => {
                if validate_repo_name(repo).is_err() {
                    break;
                }
                format!("repos/{repo}/pulls?state=all&per_page=100&page={page}")
            }
            None => format!("repos/{{owner}}/{{repo}}/pulls?state=all&per_page=100&page={page}"),
        };
        let output = run_gh(&["api", &route]);

        let Ok(json_str) = output else {
            break;
        };
        let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(&json_str) else {
            break;
        };
        if values.is_empty() {
            break;
        }

        merge_pr_status_page(&mut map, &values);

        if values.len() < 100 {
            break;
        }
        page += 1;
    }

    map
}

fn merge_pr_status_page(
    map: &mut std::collections::HashMap<String, PrInfo>,
    values: &[serde_json::Value],
) {
    for value in values {
        let Some((head, pr)) = pr_info_from_rest_value(value) else {
            continue;
        };
        // Keep the first PR we see for a branch name. The REST API returns newest
        // PRs first, so later pages may contain stale historical PRs for reused names.
        map.entry(head).or_insert(pr);
    }
}

fn pr_info_from_rest_value(value: &serde_json::Value) -> Option<(String, PrInfo)> {
    let head = value["head"]["ref"].as_str()?.to_string();
    Some((
        head,
        PrInfo {
            number: value["number"].as_u64().unwrap_or(0),
            url: value["html_url"].as_str().unwrap_or("").to_string(),
            state: value["state"]
                .as_str()
                .unwrap_or("UNKNOWN")
                .to_ascii_uppercase(),
            title: value["title"].as_str().unwrap_or("").to_string(),
            base: value["base"]["ref"].as_str().unwrap_or("").to_string(),
            is_draft: value["draft"].as_bool().unwrap_or(false),
            merged: !value["merged_at"].is_null(),
        },
    ))
}

/// Fetch the most recent PR for each given branch in a single GraphQL request.
///
/// Avoids the catastrophic pagination of `get_all_pr_statuses` on repos with
/// thousands of historical PRs: that variant scans every PR ever opened (~1s
/// per 100), while this one issues a single round-trip with aliased fields per
/// branch (~0.5s total regardless of branch count).
///
/// Returns a map keyed by branch name. Branches with no matching PR are absent.
/// On any failure (network, parse, auth, or owner/repo resolution) the function
/// returns an empty map — matching the silent-failure semantics of
/// `get_all_pr_statuses`. Callers fall through to git-level merge detection.
///
/// `remote` is used to derive the GitHub owner/repo via the local git remote
/// URL, with a fallback to `gh repo view` if the URL is unparseable.
pub fn get_pr_statuses_for(
    remote: &str,
    repo: Option<&str>,
    branches: &[&str],
) -> std::collections::HashMap<String, PrInfo> {
    if branches.is_empty() {
        return std::collections::HashMap::new();
    }

    let Ok((owner, name)) = resolve_owner_repo(remote, repo) else {
        return std::collections::HashMap::new();
    };

    let query = build_pr_statuses_query(branches);
    let owner_arg = format!("owner={owner}");
    let name_arg = format!("name={name}");
    let query_arg = format!("query={query}");
    let Ok(json_str) = run_gh(&[
        "api", "graphql", "-F", &owner_arg, "-F", &name_arg, "-f", &query_arg,
    ]) else {
        return std::collections::HashMap::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&json_str) else {
        return std::collections::HashMap::new();
    };

    parse_pr_statuses_response(&value, branches)
}

/// Fetch exact pull requests by their stored upstream numbers in one GraphQL request.
///
/// Forks commonly reuse branch names, so branch-only lookup is not a safe identity
/// for cleanup decisions. Results are keyed by the expected local branch and are
/// included only when the stored pull request still reports that same head ref.
pub fn get_pr_statuses_by_number(
    remote: &str,
    repo: Option<&str>,
    branches: &[(&str, u64)],
) -> std::collections::HashMap<String, PrInfo> {
    if branches.is_empty() {
        return std::collections::HashMap::new();
    }

    let Ok((owner, name)) = resolve_owner_repo(remote, repo) else {
        return std::collections::HashMap::new();
    };

    let mut query =
        String::from("query($owner:String!,$name:String!){repository(owner:$owner,name:$name){");
    for (index, (_, number)) in branches.iter().enumerate() {
        query.push_str(&format!(
            "b{index}:pullRequest(number:{number}){{number url state title baseRefName headRefName isDraft mergedAt}}"
        ));
    }
    query.push_str("}}");

    let owner_arg = format!("owner={owner}");
    let name_arg = format!("name={name}");
    let query_arg = format!("query={query}");
    let Ok(json_str) = run_gh(&[
        "api", "graphql", "-F", &owner_arg, "-F", &name_arg, "-f", &query_arg,
    ]) else {
        return std::collections::HashMap::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&json_str) else {
        return std::collections::HashMap::new();
    };

    let mut statuses = std::collections::HashMap::new();
    let repository = &value["data"]["repository"];
    for (index, (branch, _)) in branches.iter().enumerate() {
        let alias = format!("b{index}");
        let node = &repository[&alias];
        if node["headRefName"].as_str() != Some(*branch) {
            continue;
        }
        if let Some(pr) = pr_info_from_graphql_node(node) {
            statuses.insert((*branch).to_string(), pr);
        }
    }
    statuses
}

/// Look up one PR by number. Returns `None` on any failure — callers surface
/// the missing-PR case as a user-facing error.
pub fn get_pr_by_number(remote: &str, repo: Option<&str>, number: u64) -> Option<(String, PrInfo)> {
    let (owner, name) = resolve_owner_repo(remote, repo).ok()?;

    let query = "query($owner:String!,$name:String!,$num:Int!){repository(owner:$owner,name:$name){pullRequest(number:$num){number url state title baseRefName headRefName isDraft mergedAt}}}";
    let owner_arg = format!("owner={owner}");
    let name_arg = format!("name={name}");
    // -F (capital) sends a typed value; required so $num arrives as Int.
    let num_arg = format!("num={number}");
    let query_arg = format!("query={query}");
    let json_str = run_gh(&[
        "api", "graphql", "-F", &owner_arg, "-F", &name_arg, "-F", &num_arg, "-f", &query_arg,
    ])
    .ok()?;
    let value: serde_json::Value = serde_json::from_str(&json_str).ok()?;

    parse_pr_by_number_response(&value)
}

fn parse_pr_by_number_response(value: &serde_json::Value) -> Option<(String, PrInfo)> {
    let node = &value["data"]["repository"]["pullRequest"];
    if node.is_null() {
        return None;
    }
    let head = node["headRefName"].as_str()?.to_string();
    let pr = pr_info_from_graphql_node(node)?;
    Some((head, pr))
}

/// Resolve `(owner, name)` for the GitHub repo backing `remote`.
///
/// Fast path: parse `git remote get-url <remote>` locally (~10ms). Falls back
/// to `gh repo view` (~400ms, network) if the URL is unparseable — e.g. an
/// SSH-config alias or a non-standard host. Errors only when both fail.
fn resolve_owner_repo(remote: &str, repo: Option<&str>) -> Result<(String, String)> {
    if let Some(repo) = repo {
        return configured_owner_repo(repo);
    }
    if let Ok(url) = crate::git::remote_url(remote) {
        if let Some(pair) = parse_owner_repo_from_remote_url(&url) {
            return Ok(pair);
        }
    }
    let repo = repo_name(None)?;
    repo.split_once('/')
        .map(|(o, n)| (o.to_string(), n.to_string()))
        .ok_or_else(|| anyhow::anyhow!("unexpected repo name format `{repo}`"))
}

/// Parse `owner` and `repo` from a GitHub remote URL.
///
/// Handles the common forms:
/// - `git@github.com:owner/repo.git`
/// - `git@github.com:owner/repo`
/// - `https://github.com/owner/repo.git`
/// - `https://github.com/owner/repo`
/// - `ssh://git@github.com/owner/repo.git`
/// - `git://github.com/owner/repo.git`
///
/// Returns `None` if the URL doesn't match a recognizable form (e.g. SSH host
/// aliases like `github:owner/repo` from `~/.ssh/config`). Callers fall back to
/// `gh repo view` in that case.
fn parse_owner_repo_from_remote_url(url: &str) -> Option<(String, String)> {
    let trimmed = url.trim();
    let stripped = trimmed.strip_suffix(".git").unwrap_or(trimmed);

    // SCP-style: git@host:owner/repo
    if let Some(rest) = stripped.strip_prefix("git@") {
        if let Some((_host, path)) = rest.split_once(':') {
            return split_owner_repo(path);
        }
    }

    // URL-style: <scheme>://[user@]host/owner/repo
    for prefix in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = stripped.strip_prefix(prefix) {
            // Drop optional user@ segment (e.g. ssh://git@github.com/...).
            let after_user = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
            if let Some((_host, path)) = after_user.split_once('/') {
                return split_owner_repo(path);
            }
        }
    }

    None
}

fn configured_owner_repo(repo: &str) -> Result<(String, String)> {
    validate_repo_name(repo)?;
    let (owner, name) = repo
        .split_once('/')
        .expect("validated repo name should contain slash");
    Ok((owner.to_string(), name.to_string()))
}

fn validate_repo_name(repo: &str) -> Result<()> {
    let invalid = || {
        anyhow::anyhow!(
            "invalid GitHub repository `{repo}` — expected `OWNER/REPO`, for example `octocat/Hello-World`"
        )
    };
    let (owner, name) = repo.split_once('/').ok_or_else(invalid)?;
    if repo.trim() != repo
        || owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || owner.chars().any(char::is_whitespace)
        || name.chars().any(char::is_whitespace)
    {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) fn remote_repo_differs(push_remote: &str, target_repo: &str) -> Option<bool> {
    let target = configured_owner_repo(target_repo).ok()?;
    let url = crate::git::remote_url(push_remote).ok()?;
    let push = parse_owner_repo_from_remote_url(&url)?;
    Some(!owner_repo_eq(&push, &target))
}

fn owner_repo_eq(left: &(String, String), right: &(String, String)) -> bool {
    left.0.eq_ignore_ascii_case(&right.0) && left.1.eq_ignore_ascii_case(&right.1)
}

fn split_owner_repo(path: &str) -> Option<(String, String)> {
    let mut parts = path.splitn(3, '/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    Some((owner.to_string(), repo.to_string()))
}

pub fn pull_request_head(
    push_remote: &str,
    target_repo: Option<&str>,
    fork_repo: Option<&str>,
    fork_workflow: bool,
    branch: &str,
) -> Result<String> {
    let target = match target_repo {
        Some(repo) => Some(configured_owner_repo(repo)?),
        None => None,
    };

    if let Some(repo) = fork_repo {
        let fork = configured_owner_repo(repo)?;
        if target
            .as_ref()
            .is_none_or(|target| !owner_repo_eq(target, &fork))
        {
            return Ok(format!("{}:{branch}", fork.0));
        }
        return Ok(branch.to_string());
    }

    let Ok(url) = crate::git::remote_url(push_remote) else {
        if fork_workflow {
            bail!(
                "could not resolve GitHub owner for push remote `{push_remote}` — set `fork_repo = \"OWNER/REPO\"` when pushing fork branches to an SSH alias or non-GitHub remote"
            );
        }
        return Ok(branch.to_string());
    };
    let Some(push_repo) = parse_owner_repo_from_remote_url(&url) else {
        if fork_workflow {
            bail!(
                "could not parse GitHub owner from push remote `{push_remote}` URL `{url}` — set `fork_repo = \"OWNER/REPO\"` to create fork pull requests"
            );
        }
        return Ok(branch.to_string());
    };

    if target
        .as_ref()
        .is_some_and(|target| !owner_repo_eq(target, &push_repo))
    {
        return Ok(format!("{}:{branch}", push_repo.0));
    }
    Ok(branch.to_string())
}

/// Construct a GraphQL query asking for the most recent PR for each branch.
///
/// Each branch is aliased as `b{i}` so the response order is stable and
/// independent of the branch name (which may contain characters not allowed in
/// a GraphQL alias). Branch names are JSON-escaped — GraphQL string literals
/// use the same escape rules as JSON, so this is safe for any name git allows.
fn build_pr_statuses_query(branches: &[&str]) -> String {
    let mut q =
        String::from("query($owner:String!,$name:String!){repository(owner:$owner,name:$name){");
    for (i, branch) in branches.iter().enumerate() {
        let escaped = serde_json::to_string(branch).unwrap_or_else(|_| "\"\"".to_string());
        q.push_str(&format!(
            "b{i}:pullRequests(headRefName:{escaped},first:1,orderBy:{{field:CREATED_AT,direction:DESC}}){{nodes{{number url state title baseRefName isDraft mergedAt}}}}"
        ));
    }
    q.push_str("}}");
    q
}

fn parse_pr_statuses_response(
    value: &serde_json::Value,
    branches: &[&str],
) -> std::collections::HashMap<String, PrInfo> {
    let mut map = std::collections::HashMap::new();
    let repo = &value["data"]["repository"];
    for (i, branch) in branches.iter().enumerate() {
        let alias = format!("b{i}");
        let Some(nodes) = repo[&alias]["nodes"].as_array() else {
            continue;
        };
        let Some(node) = nodes.first() else {
            continue;
        };
        let Some(pr) = pr_info_from_graphql_node(node) else {
            continue;
        };
        map.insert((*branch).to_string(), pr);
    }
    map
}

fn pr_info_from_graphql_node(node: &serde_json::Value) -> Option<PrInfo> {
    let number = node["number"].as_u64()?;
    let state = node["state"]
        .as_str()
        .unwrap_or("UNKNOWN")
        .to_ascii_uppercase();
    // GraphQL distinguishes MERGED from CLOSED in the state enum, but also
    // exposes `mergedAt`. Prefer the explicit state, fall back to mergedAt.
    let merged = state == "MERGED" || node["mergedAt"].as_str().is_some_and(|s| !s.is_empty());
    Some(PrInfo {
        number,
        url: node["url"].as_str().unwrap_or("").to_string(),
        state,
        title: node["title"].as_str().unwrap_or("").to_string(),
        base: node["baseRefName"].as_str().unwrap_or("").to_string(),
        is_draft: node["isDraft"].as_bool().unwrap_or(false),
        merged,
    })
}

pub fn merge_pr(pr_number: u64, method: &str, repo: Option<&str>) -> Result<MergePrOutcome> {
    merge_pr_with_poll_interval(pr_number, method, Duration::from_secs(1), repo)
}

fn merge_pr_with_poll_interval(
    pr_number: u64,
    method: &str,
    poll_interval: Duration,
    repo: Option<&str>,
) -> Result<MergePrOutcome> {
    let repo = repo_name(repo)?;
    let route = format!("repos/{repo}/pulls/{pr_number}/merge-async");
    let method_json = serde_json::to_string(method)?;
    let payload = format!(r#"{{"merge_method":{method_json},"merge_action":"default"}}"#);
    let response = match run_gh_with_stdin(
        &[
            "api",
            "-X",
            "PUT",
            &route,
            "--input",
            "-",
            "-H",
            GITHUB_API_VERSION_HEADER,
        ],
        &payload,
    ) {
        Ok(response) => response,
        Err(err) if is_not_found_gh_error(&err) => {
            return merge_pr_legacy(&repo, pr_number, method);
        }
        Err(err) => return Err(err),
    };

    handle_merge_async_response(&repo, pr_number, &response, poll_interval)
}

fn merge_pr_legacy(repo: &str, pr_number: u64, method: &str) -> Result<MergePrOutcome> {
    let route = format!("repos/{repo}/pulls/{pr_number}/merge");
    let response = run_gh(&[
        "api",
        "-X",
        "PUT",
        &route,
        "-f",
        &format!("merge_method={method}"),
    ])?;

    let value: serde_json::Value = serde_json::from_str(&response)?;
    if value["merged"].as_bool().unwrap_or(false) {
        return Ok(MergePrOutcome::Merged);
    }

    let message = value["message"].as_str().unwrap_or("merge failed");
    bail!(EzError::GhError(message.to_string()));
}

fn handle_merge_async_response(
    repo: &str,
    pr_number: u64,
    response: &str,
    poll_interval: Duration,
) -> Result<MergePrOutcome> {
    let mut value: serde_json::Value = serde_json::from_str(response)
        .with_context(|| "failed to parse GitHub async merge response")?;
    for _ in 0..600 {
        match parse_merge_async_status(&value)? {
            MergeAsyncStatus::Merged => return Ok(MergePrOutcome::Merged),
            MergeAsyncStatus::Enqueued => return Ok(MergePrOutcome::Enqueued),
            MergeAsyncStatus::Failed(message) => bail!(EzError::GhError(message)),
            MergeAsyncStatus::Pending(uuid) => {
                if poll_interval > Duration::ZERO {
                    std::thread::sleep(poll_interval);
                }
                let route = format!("repos/{repo}/pulls/{pr_number}/merge-async/{uuid}");
                let poll_response = run_gh(&["api", &route, "-H", GITHUB_API_VERSION_HEADER])?;
                value = serde_json::from_str(&poll_response)
                    .with_context(|| "failed to parse GitHub async merge poll response")?;
            }
        }
    }

    bail!(EzError::GhError(format!(
        "GitHub async merge for PR #{pr_number} is still pending after 600 polls; check the merge queue status on GitHub and retry `ez merge` if needed"
    )));
}

enum MergeAsyncStatus {
    Merged,
    Enqueued,
    Failed(String),
    Pending(String),
}

fn parse_merge_async_status(value: &serde_json::Value) -> Result<MergeAsyncStatus> {
    match value["status"].as_str() {
        Some("merged") => Ok(MergeAsyncStatus::Merged),
        Some("enqueued") => Ok(MergeAsyncStatus::Enqueued),
        Some("failed") => {
            let message = value["details"]["message"]
                .as_str()
                .unwrap_or("GitHub async merge failed")
                .to_string();
            Ok(MergeAsyncStatus::Failed(message))
        }
        Some("pending") => {
            let uuid = value["details"]["uuid"]
                .as_str()
                .context("GitHub async merge response is pending but missing details.uuid")?;
            Ok(MergeAsyncStatus::Pending(uuid.to_string()))
        }
        Some(status) => bail!(EzError::GhError(format!(
            "unknown GitHub async merge status `{status}`"
        ))),
        None => bail!(EzError::GhError(
            "GitHub async merge response missing status".to_string()
        )),
    }
}

pub fn ensure_native_stack(pr_numbers: &[u64], repo: Option<&str>) -> Result<NativeStackOutcome> {
    reconcile_native_stack(pr_numbers, "ez submit", true, repo)
}

pub fn reconcile_native_stack_exact(
    pr_numbers: &[u64],
    retry_command: &str,
    repo: Option<&str>,
) -> Result<NativeStackOutcome> {
    reconcile_native_stack(pr_numbers, retry_command, false, repo)
}

pub fn repair_native_stack_exact(
    pr_numbers: &[u64],
    retry_command: &str,
    repo: Option<&str>,
) -> Result<NativeStackOutcome> {
    if pr_numbers.len() < 2 {
        return Ok(NativeStackOutcome::NotNeeded);
    }
    validate_native_stack_length(pr_numbers)?;

    let repo = repo_name(repo)?;
    repair_native_stack_exact_in_repo(pr_numbers, retry_command, &repo)
}

fn reconcile_native_stack(
    pr_numbers: &[u64],
    retry_command: &str,
    allow_existing_superset: bool,
    repo: Option<&str>,
) -> Result<NativeStackOutcome> {
    if pr_numbers.len() < 2 {
        return Ok(NativeStackOutcome::NotNeeded);
    }
    validate_native_stack_length(pr_numbers)?;

    let repo = repo_name(repo)?;
    let mut mutation_attempts = 0;
    let mut existing = match list_native_stack_for_bottom(&repo, pr_numbers[0]) {
        Ok(stack) => stack,
        Err(err) if is_not_found_gh_error(&err) => return Ok(NativeStackOutcome::Unavailable),
        Err(err) => return Err(err),
    };

    loop {
        let Some(stack) = existing.as_ref() else {
            let number = create_native_stack(&repo, pr_numbers)?;
            return Ok(NativeStackOutcome::Created { number });
        };

        let number = stack.number;
        let existing_prs = &stack.pull_requests;

        if existing_prs == pr_numbers {
            return Ok(NativeStackOutcome::Unchanged { number });
        }
        if is_prefix(existing_prs, pr_numbers) {
            let delta = &pr_numbers[existing_prs.len()..];
            match add_native_stack_prs(&repo, number, delta) {
                Ok(()) => {
                    return Ok(NativeStackOutcome::Extended {
                        number,
                        added: delta.len(),
                    });
                }
                Err(err) if is_conflict_gh_error(&err) || is_not_found_gh_error(&err) => {
                    mutation_attempts += 1;
                    if mutation_attempts >= MAX_NATIVE_STACK_MUTATION_ATTEMPTS {
                        bail!(EzError::GhError(format!(
                            "could not update GitHub native stack #{number} after {MAX_NATIVE_STACK_MUTATION_ATTEMPTS} attempts due to concurrent changes; retry `{retry_command}`."
                        )));
                    }
                    sleep_before_native_stack_retry(mutation_attempts);
                    existing = list_native_stack_for_bottom(&repo, pr_numbers[0])?;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
        if allow_existing_superset && is_prefix(pr_numbers, existing_prs) {
            return Ok(NativeStackOutcome::Unchanged { number });
        }

        bail!(EzError::GhError(format!(
            "native stack #{number} diverges from desired PR chain; existing pull_requests={existing_prs:?}, desired pull_requests={pr_numbers:?}. Resolve the GitHub stack manually, then retry `{retry_command}`."
        )));
    }
}

pub fn native_stack_for_pr(pr_number: u64, repo: Option<&str>) -> Result<Option<NativeStackInfo>> {
    match lookup_native_stack_for_pr(pr_number, repo)? {
        NativeStackLookup::Found(info) => Ok(Some(info)),
        NativeStackLookup::NotLinked | NativeStackLookup::Unavailable => Ok(None),
    }
}

pub fn lookup_native_stack_for_pr(pr_number: u64, repo: Option<&str>) -> Result<NativeStackLookup> {
    let repo = repo_name(repo)?;
    let response =
        match run_native_stack_api_get(&format!("repos/{repo}/stacks?pull_request={pr_number}")) {
            Ok(json) => json,
            Err(err) if is_not_found_gh_error(&err) => return Ok(NativeStackLookup::Unavailable),
            Err(err) => return Err(err),
        };

    let stacks: Vec<serde_json::Value> = serde_json::from_str(&response)
        .with_context(|| "failed to parse GitHub native stack lookup response")?;
    let Some(stack) = stacks.first() else {
        return Ok(NativeStackLookup::NotLinked);
    };

    Ok(NativeStackLookup::Found(NativeStackInfo {
        number: parse_native_stack_info_number(stack)?,
        base_ref: parse_native_stack_base_ref(stack),
        open: stack["open"].as_bool(),
        pull_requests: parse_native_stack_pr_numbers(stack)?,
    }))
}

fn repair_native_stack_exact_in_repo(
    pr_numbers: &[u64],
    retry_command: &str,
    repo: &str,
) -> Result<NativeStackOutcome> {
    let mut mutation_attempts = 0;
    let mut existing = match list_native_stack_for_bottom(repo, pr_numbers[0]) {
        Ok(stack) => stack,
        Err(err) if is_not_found_gh_error(&err) => return Ok(NativeStackOutcome::Unavailable),
        Err(err) => return Err(err),
    };

    loop {
        let Some(stack) = existing.as_ref() else {
            let number = create_native_stack(repo, pr_numbers)?;
            return Ok(NativeStackOutcome::Created { number });
        };

        if stack.pull_requests == pr_numbers {
            return Ok(NativeStackOutcome::Unchanged {
                number: stack.number,
            });
        }

        if is_prefix(&stack.pull_requests, pr_numbers) {
            let delta = &pr_numbers[stack.pull_requests.len()..];
            match add_native_stack_prs(repo, stack.number, delta) {
                Ok(()) => {
                    return Ok(NativeStackOutcome::Extended {
                        number: stack.number,
                        added: delta.len(),
                    });
                }
                Err(err) if is_conflict_gh_error(&err) || is_not_found_gh_error(&err) => {
                    mutation_attempts += 1;
                    if mutation_attempts >= MAX_NATIVE_STACK_MUTATION_ATTEMPTS {
                        bail!(EzError::GhError(format!(
                            "could not repair GitHub native stack after {MAX_NATIVE_STACK_MUTATION_ATTEMPTS} attempts due to concurrent changes; retry `{retry_command}`."
                        )));
                    }
                    sleep_before_native_stack_retry(mutation_attempts);
                    existing = list_native_stack_for_bottom(repo, pr_numbers[0])?;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }

        match unstack_native_stack(repo, stack.number) {
            Ok(NativeUnstackOutcome::Dissolved) => {
                return create_native_stack_after_dissolve(
                    repo,
                    pr_numbers,
                    stack.number,
                    retry_command,
                );
            }
            Ok(NativeUnstackOutcome::Retained(retained)) => {
                if retained.pull_requests == pr_numbers {
                    return Ok(NativeStackOutcome::Unchanged {
                        number: retained.number,
                    });
                }
                bail!(EzError::GhError(format!(
                    "native stack #{} is queued or locked and retained a divergent PR chain; existing pull_requests={:?}, desired pull_requests={pr_numbers:?}. Resolve the retained GitHub stack manually, then retry `{retry_command}`.",
                    retained.number, retained.pull_requests
                )));
            }
            Err(err) if is_conflict_gh_error(&err) || is_not_found_gh_error(&err) => {
                mutation_attempts += 1;
                if mutation_attempts >= MAX_NATIVE_STACK_MUTATION_ATTEMPTS {
                    bail!(EzError::GhError(format!(
                        "could not repair GitHub native stack after {MAX_NATIVE_STACK_MUTATION_ATTEMPTS} attempts due to concurrent changes; retry `{retry_command}`."
                    )));
                }
                sleep_before_native_stack_retry(mutation_attempts);
                existing = list_native_stack_for_bottom(repo, pr_numbers[0])?;
            }
            Err(err) => return Err(err),
        }
    }
}

fn validate_native_stack_length(pr_numbers: &[u64]) -> Result<()> {
    if pr_numbers.len() > 100 {
        bail!(
            "GitHub native stacks support at most 100 pull requests; desired stack has {}",
            pr_numbers.len()
        );
    }
    Ok(())
}

fn run_native_stack_api_get(route: &str) -> Result<String> {
    run_gh(&[
        "api",
        route,
        "-H",
        GITHUB_API_VERSION_HEADER,
        "-H",
        GITHUB_JSON_ACCEPT_HEADER,
    ])
}

fn run_native_stack_api_post(route: &str, payload: &str) -> Result<String> {
    run_gh_with_stdin(
        &[
            "api",
            "-X",
            "POST",
            route,
            "--input",
            "-",
            "-H",
            GITHUB_API_VERSION_HEADER,
            "-H",
            GITHUB_JSON_ACCEPT_HEADER,
        ],
        payload,
    )
}

fn run_native_stack_api_post_without_body(route: &str) -> Result<String> {
    run_gh(&[
        "api",
        "-X",
        "POST",
        route,
        "-H",
        GITHUB_API_VERSION_HEADER,
        "-H",
        GITHUB_JSON_ACCEPT_HEADER,
    ])
}

fn list_native_stack_for_bottom(repo: &str, bottom: u64) -> Result<Option<NativeStackInfo>> {
    let route = format!("repos/{repo}/stacks?pull_request={bottom}");
    let response = run_native_stack_api_get(&route)?;
    let stacks: Vec<serde_json::Value> = serde_json::from_str(&response)
        .with_context(|| "failed to parse GitHub native stack lookup response")?;
    stacks.first().map(parse_native_stack_info).transpose()
}

fn create_native_stack(repo: &str, pr_numbers: &[u64]) -> Result<u64> {
    let payload = serde_json::json!({ "pull_requests": pr_numbers }).to_string();
    let response = run_native_stack_api_post(&format!("repos/{repo}/stacks"), &payload)?;
    parse_native_stack_number(&response)
}

fn create_native_stack_after_dissolve(
    repo: &str,
    pr_numbers: &[u64],
    previous_number: u64,
    retry_command: &str,
) -> Result<NativeStackOutcome> {
    match create_native_stack(repo, pr_numbers) {
        Ok(number) => Ok(NativeStackOutcome::Repaired {
            previous_number,
            number,
        }),
        Err(err) => bail!(EzError::GhError(format!(
            "previous native stack #{previous_number} was dissolved, but recreating the desired GitHub native stack failed: {err}. Please retry `{retry_command}`."
        ))),
    }
}

fn add_native_stack_prs(repo: &str, number: u64, delta: &[u64]) -> Result<()> {
    let payload = serde_json::json!({ "pull_requests": delta }).to_string();
    run_native_stack_api_post(&format!("repos/{repo}/stacks/{number}/add"), &payload)?;
    Ok(())
}

enum NativeUnstackOutcome {
    Dissolved,
    Retained(NativeStackInfo),
}

fn unstack_native_stack(repo: &str, number: u64) -> Result<NativeUnstackOutcome> {
    let response =
        run_native_stack_api_post_without_body(&format!("repos/{repo}/stacks/{number}/unstack"))?;
    if response.trim().is_empty() {
        return Ok(NativeUnstackOutcome::Dissolved);
    }
    Ok(NativeUnstackOutcome::Retained(parse_native_stack_info(
        &serde_json::from_str(&response)
            .with_context(|| "failed to parse GitHub native stack unstack response")?,
    )?))
}

fn sleep_before_native_stack_retry(attempts: usize) {
    std::thread::sleep(NATIVE_STACK_RETRY_BACKOFF * attempts as u32);
}

fn is_not_found_gh_error(err: &anyhow::Error) -> bool {
    let message = err.to_string().to_ascii_lowercase();
    message.contains("404") || message.contains("not found")
}

fn is_conflict_gh_error(err: &anyhow::Error) -> bool {
    let message = err.to_string().to_ascii_lowercase();
    message.contains("409") || message.contains("conflict")
}

fn parse_native_stack_info(stack: &serde_json::Value) -> Result<NativeStackInfo> {
    Ok(NativeStackInfo {
        number: parse_native_stack_info_number(stack)?,
        base_ref: parse_native_stack_base_ref(stack),
        open: stack["open"].as_bool(),
        pull_requests: parse_native_stack_pr_numbers(stack)?,
    })
}

fn parse_native_stack_info_number(stack: &serde_json::Value) -> Result<u64> {
    stack["number"]
        .as_u64()
        .context("GitHub native stack response missing stack number")
}

fn parse_native_stack_base_ref(stack: &serde_json::Value) -> Option<String> {
    stack["base"]["ref"].as_str().map(ToString::to_string)
}

fn parse_native_stack_number(response: &str) -> Result<u64> {
    let value: serde_json::Value = serde_json::from_str(response)
        .with_context(|| "failed to parse GitHub native stack mutation response")?;
    value["number"]
        .as_u64()
        .context("GitHub native stack mutation response missing stack number")
}

fn parse_native_stack_pr_numbers(stack: &serde_json::Value) -> Result<Vec<u64>> {
    let pull_requests = stack["pull_requests"]
        .as_array()
        .context("GitHub native stack response missing pull_requests")?;
    pull_requests
        .iter()
        .map(|pr| {
            pr["number"]
                .as_u64()
                .context("GitHub native stack pull_request missing number")
        })
        .collect()
}

fn is_prefix<T: PartialEq>(prefix: &[T], values: &[T]) -> bool {
    prefix.len() <= values.len() && values.starts_with(prefix)
}

pub fn edit_pr(
    pr_number: u64,
    title: Option<&str>,
    body: Option<&str>,
    repo: Option<&str>,
) -> Result<()> {
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
    run_gh_in_repo(&args, repo)?;
    Ok(())
}

pub fn is_gh_authenticated() -> bool {
    run_gh(&["auth", "status"]).is_ok()
}

pub fn repo_name(configured: Option<&str>) -> Result<String> {
    if let Some(repo) = configured {
        validate_repo_name(repo)?;
        return Ok(repo.to_string());
    }
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
pub fn get_pr_body(pr_number: u64, repo: Option<&str>) -> Result<String> {
    let body = run_gh_in_repo(
        &[
            "pr",
            "view",
            &pr_number.to_string(),
            "--json",
            "body",
            "-q",
            ".body",
        ],
        repo,
    )?;
    Ok(body)
}

/// Open the PR for a branch in the default browser.
pub fn open_pr_in_browser(branch: &str, repo: Option<&str>) -> Result<()> {
    run_gh_in_repo(&["pr", "view", "--web", branch], repo)?;
    Ok(())
}

/// Get the latest CI run status for a branch.
/// Returns a short status string: "✓", "✗", "⏳", or "" if no runs found.
/// Fetch CI status for all branches in one API call.
/// Returns a map of branch_name → status emoji (✓/✗/⏳).
/// Uses the most recent run per branch.
pub fn get_all_ci_statuses(repo: Option<&str>) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let route = match repo {
        Some(repo) => {
            if validate_repo_name(repo).is_err() {
                return map;
            }
            format!("repos/{repo}/actions/runs?per_page=50")
        }
        None => "repos/{owner}/{repo}/actions/runs?per_page=50".to_string(),
    };
    let output = run_gh(&[
        "api",
        &route,
        "--jq",
        r#".workflow_runs[] | "\(.head_branch)\t\(.status)\t\(.conclusion)""#,
    ]);
    if let Ok(text) = output {
        for line in text.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let branch = parts[0];
            let status = parts[1];
            let conclusion = parts.get(2).copied().unwrap_or("");
            // Only keep the first (most recent) run per branch.
            if map.contains_key(branch) {
                continue;
            }
            let emoji = match (status, conclusion) {
                ("completed", "success") => "✓",
                ("completed", _) => "✗",
                ("in_progress", _) | ("queued", _) | ("waiting", _) => "⏳",
                _ => "",
            };
            if !emoji.is_empty() {
                map.insert(branch.to_string(), emoji.to_string());
            }
        }
    }
    map
}

pub fn get_ci_status(branch: &str, repo: Option<&str>) -> String {
    let output = run_gh_in_repo(
        &[
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
        ],
        repo,
    );
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
pub fn set_pr_ready(pr_number: u64, ready: bool, repo: Option<&str>) -> Result<()> {
    let number = pr_number.to_string();
    if ready {
        run_gh_in_repo(&["pr", "ready", &number], repo)?;
    } else {
        run_gh_in_repo(&["pr", "ready", "--undo", &number], repo)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        CwdGuard, PathGuard, init_git_repo, install_fake_bin, take_env_lock, temp_dir, write_file,
    };

    fn install_fake_gh(name: &str) -> std::path::PathBuf {
        install_fake_bin(
            name,
            "gh",
            r#"#!/bin/sh
cmd="$1"
shift

case "$cmd" in
  repo)
    echo "org/repo"
    ;;
  pr)
    sub="$1"
    shift
    case "$sub" in
      create)
        echo "https://github.com/org/repo/pull/77"
        ;;
      edit)
        exit 0
        ;;
      view)
        if [ "$1" = "--web" ]; then
          exit 0
        fi
        if [ "$1" = "feature" ]; then
          echo '{"number":55,"url":"https://github.com/org/repo/pull/55","state":"OPEN","title":"Feature PR","isDraft":false,"mergedAt":null,"baseRefName":"main"}'
        elif [ "$1" = "123" ]; then
          echo 'Body text'
        fi
        ;;
      merge)
        exit 0
        ;;
      ready)
        exit 0
        ;;
    esac
    ;;
  api)
    if [ "$1" = "-X" ] && [ "$2" = "PUT" ] && [ "$3" = 'repos/org/repo/pulls/77/merge-async' ] && [ "$4" = "--input" ] && [ "$5" = "-" ] && [ "$6" = "-H" ] && [ "$7" = "X-GitHub-Api-Version: 2026-03-10" ]; then
      cat >/dev/null
      echo '{"status":"merged"}'
    elif [ "$1" = "graphql" ]; then
      # Capture the request so tests can assert what was sent.
      if [ -n "$EZ_FAKE_GH_LOG" ]; then
        printf 'graphql' >> "$EZ_FAKE_GH_LOG"
        for arg in "$@"; do printf '\t%s' "$arg" >> "$EZ_FAKE_GH_LOG"; done
        printf '\n' >> "$EZ_FAKE_GH_LOG"
      fi
      # Canned response: b0 has a merged PR, b1 has an open PR, b2 has no PRs.
      printf '%s' '{"data":{"repository":{"b0":{"nodes":[{"number":42,"url":"https://github.com/org/repo/pull/42","state":"MERGED","title":"Done","baseRefName":"main","isDraft":false,"mergedAt":"2026-04-01T00:00:00Z"}]},"b1":{"nodes":[{"number":43,"url":"https://github.com/org/repo/pull/43","state":"OPEN","title":"Wip","baseRefName":"main","isDraft":true,"mergedAt":null}]},"b2":{"nodes":[]}}}}'
    elif [ "$1" = 'repos/{owner}/{repo}/pulls?state=all&per_page=100&page=1' ]; then
      printf '%s' '[{"number":10,"html_url":"https://github.com/org/repo/pull/10","state":"closed","title":"Newest","draft":false,"merged_at":"2026-01-01T00:00:00Z","base":{"ref":"main"},"head":{"ref":"feat/reused"}},{"number":11,"html_url":"https://github.com/org/repo/pull/11","state":"open","title":"Other","draft":true,"merged_at":null,"base":{"ref":"develop"},"head":{"ref":"feat/other"}}]'
    elif [ "$1" = 'repos/{owner}/{repo}/pulls?state=all&per_page=100&page=2' ]; then
      printf '%s' '[{"number":4,"html_url":"https://github.com/org/repo/pull/4","state":"closed","title":"Old","draft":false,"merged_at":null,"base":{"ref":"main"},"head":{"ref":"feat/reused"}}]'
    elif [ "$1" = 'repos/{owner}/{repo}/actions/runs?per_page=50' ]; then
      printf 'feat/reused\tcompleted\tsuccess\nfeat/reused\tcompleted\tfailure\nfeat/other\tqueued\t\n'
    fi
    ;;
  auth)
    exit 0
    ;;
esac
"#,
        )
    }

    #[test]
    fn merge_pr_status_page_keeps_first_pr_for_reused_branch_names() {
        let mut map = std::collections::HashMap::new();
        let values = vec![
            serde_json::json!({
                "number": 12,
                "html_url": "https://example.com/pr/12",
                "state": "closed",
                "title": "Newest PR",
                "draft": false,
                "merged_at": "2026-03-31T10:00:00Z",
                "base": {"ref": "main"},
                "head": {"ref": "feat/reused"},
            }),
            serde_json::json!({
                "number": 4,
                "html_url": "https://example.com/pr/4",
                "state": "closed",
                "title": "Old PR",
                "draft": false,
                "merged_at": null,
                "base": {"ref": "main"},
                "head": {"ref": "feat/reused"},
            }),
        ];

        merge_pr_status_page(&mut map, &values);

        let pr = map.get("feat/reused").expect("branch should be present");
        assert_eq!(pr.number, 12);
        assert_eq!(pr.title, "Newest PR");
        assert!(pr.merged);
    }

    #[test]
    fn pr_info_from_rest_value_extracts_expected_fields() {
        let value = serde_json::json!({
            "number": 97,
            "html_url": "https://example.com/pr/97",
            "state": "open",
            "title": "Test PR",
            "draft": true,
            "merged_at": null,
            "base": {"ref": "develop"},
            "head": {"ref": "feat/test"},
        });

        let (head, pr) = pr_info_from_rest_value(&value).expect("valid PR payload");

        assert_eq!(head, "feat/test");
        assert_eq!(pr.number, 97);
        assert_eq!(pr.url, "https://example.com/pr/97");
        assert_eq!(pr.state, "OPEN");
        assert_eq!(pr.title, "Test PR");
        assert_eq!(pr.base, "develop");
        assert!(pr.is_draft);
        assert!(!pr.merged);
    }

    #[test]
    fn gh_wrappers_work_against_fake_cli() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_gh("wrappers");
        let _path = PathGuard::install(&fake_dir);

        let created = create_pr("Title", "Body", "main", "feature", true, None).expect("create pr");
        assert_eq!(created.number, 77);
        assert!(created.is_draft);

        update_pr_base(77, "develop", None).expect("update base");
        edit_pr(77, Some("New title"), Some("New body"), None).expect("edit pr");
        assert_eq!(
            merge_pr(77, "squash", None).expect("merge pr"),
            MergePrOutcome::Merged
        );
        set_pr_ready(77, true, None).expect("ready");
        open_pr_in_browser("feature", None).expect("open in browser");
        assert!(is_gh_authenticated());
        assert_eq!(repo_name(None).expect("repo name"), "org/repo");
        assert_eq!(get_pr_body(123, None).expect("body"), "Body text");

        let status = get_pr_status("feature", None)
            .expect("pr status")
            .expect("some pr");
        assert_eq!(status.number, 55);
        assert_eq!(status.base, "main");
        assert_eq!(status.state, "OPEN");
    }

    #[test]
    fn gh_wrappers_append_repo_only_when_configured() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-repo-argv",
            "gh",
            r#"#!/bin/sh
printf '%s' "$1" >> "$EZ_FAKE_GH_LOG"
shift
for arg in "$@"; do printf ' %s' "$arg" >> "$EZ_FAKE_GH_LOG"; done
printf '\n' >> "$EZ_FAKE_GH_LOG"
if [ "$1" = "create" ]; then
  echo "https://github.com/upstream/repo/pull/77"
elif [ "$1" = "view" ] && [ "$2" = "feature" ]; then
  echo '{"number":55,"url":"https://github.com/upstream/repo/pull/55","state":"OPEN","title":"Feature PR","isDraft":false,"mergedAt":null,"baseRefName":"main"}'
fi
exit 0
"#,
        );
        let log_path = fake_dir.join("args.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
        }
        let _path = PathGuard::install(&fake_dir);

        create_pr(
            "Title",
            "Body",
            "main",
            "fork-owner:feature",
            true,
            Some("upstream/repo"),
        )
        .expect("create");
        get_pr_status("feature", Some("upstream/repo")).expect("status");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }
        let log = std::fs::read_to_string(log_path).expect("log");
        assert!(log.contains(
            "pr create --title Title --body Body --base main --head fork-owner:feature --draft --repo upstream/repo"
        ));
        assert!(log.contains(
            "pr view feature --json number,url,state,title,isDraft,mergedAt,baseRefName --repo upstream/repo"
        ));
    }

    #[test]
    fn repo_name_prefers_configured_repo_without_invoking_gh() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-repo-config-no-exec",
            "gh",
            r#"#!/bin/sh
echo "gh should not be invoked" >&2
exit 1
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        assert_eq!(
            repo_name(Some("upstream/repo")).expect("configured repo"),
            "upstream/repo"
        );
    }

    #[test]
    fn configured_repo_names_must_be_owner_repo() {
        for repo in ["", "owner", "owner/", "/repo", "owner/repo/extra"] {
            let err = repo_name(Some(repo)).expect_err("invalid configured repo");
            assert!(
                err.to_string().contains("expected `OWNER/REPO`"),
                "repo={repo}, err={err:#}"
            );
        }
    }

    #[test]
    fn gh_bulk_helpers_parse_fake_cli_output() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_gh("bulk");
        let _path = PathGuard::install(&fake_dir);

        let prs = get_all_pr_statuses(None);
        assert_eq!(prs.get("feat/reused").expect("reused").number, 10);
        assert_eq!(prs.get("feat/other").expect("other").base, "develop");

        let ci = get_all_ci_statuses(None);
        assert_eq!(ci.get("feat/reused").expect("ci"), "✓");
        assert_eq!(ci.get("feat/other").expect("ci"), "⏳");
    }

    #[test]
    fn graphql_helpers_use_configured_repo_instead_of_remote_discovery() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_gh("graphql-configured-repo");
        let _path = PathGuard::install(&fake_dir);
        let log_path = fake_dir.join("graphql.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
        }

        let map = get_pr_statuses_for("missing-remote", Some("upstream/repo"), &["feat/merged"]);

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }
        assert_eq!(map.get("feat/merged").expect("pr").number, 42);
        let log = std::fs::read_to_string(log_path).expect("graphql log");
        assert!(log.contains("\t-F\towner=upstream\t-F\tname=repo\t-f\tquery="));
    }

    #[test]
    fn create_pr_fails_when_gh_returns_non_pr_url() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-bad-pr-url",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "pr" ] && [ "$2" = "create" ]; then
  echo "https://github.com/org/repo/not-a-pr"
  exit 0
fi
exit 0
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        let err = create_pr("Title", "Body", "main", "feature", false, None)
            .expect_err("invalid PR URL should fail");
        assert!(
            err.to_string()
                .contains("could not parse PR number from URL"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn get_pr_status_returns_error_on_malformed_json() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-bad-pr-json",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo "{not-json"
  exit 0
fi
exit 0
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        let err = get_pr_status("feature", None).expect_err("bad json should bubble up");
        assert!(
            err.to_string().contains("key must be a string")
                || err.to_string().contains("expected ident")
                || err.to_string().contains("expected value"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn repo_name_errors_when_gh_returns_empty_string() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-empty-repo",
            "gh",
            r#"#!/bin/sh
exit 0
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        let err = repo_name(None).expect_err("empty repo name should fail");
        assert!(
            err.to_string()
                .contains("could not determine repository name"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn gh_error_stderr_is_preserved_for_failed_commands() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-merge-fail",
            "gh",
            r#"#!/bin/sh
echo "permission denied" >&2
exit 1
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        let err = merge_pr(12, "squash", None).expect_err("merge should fail");
        assert!(
            err.to_string().contains("permission denied"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn merge_pr_async_immediate_merged_sends_exact_request_and_payload() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-merge-async-immediate",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "PUT" ] && [ "$4" = "repos/org/repo/pulls/77/merge-async" ] && [ "$5" = "--input" ] && [ "$6" = "-" ] && [ "$7" = "-H" ] && [ "$8" = "X-GitHub-Api-Version: 2026-03-10" ]; then
  printf '%s\n' "$*" > "$EZ_FAKE_GH_LOG"
  cat > "$EZ_FAKE_GH_PAYLOAD"
  echo '{"status":"merged"}'
  exit 0
fi
exit 2
"#,
        );
        let log_path = fake_dir.join("args.log");
        let payload_path = fake_dir.join("payload.json");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
            std::env::set_var("EZ_FAKE_GH_PAYLOAD", &payload_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let outcome = merge_pr_with_poll_interval(77, "squash", std::time::Duration::ZERO, None)
            .expect("async merge");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
            std::env::remove_var("EZ_FAKE_GH_PAYLOAD");
        }
        assert_eq!(outcome, MergePrOutcome::Merged);
        assert_eq!(
            std::fs::read_to_string(log_path).expect("args"),
            "api -X PUT repos/org/repo/pulls/77/merge-async --input - -H X-GitHub-Api-Version: 2026-03-10\n"
        );
        assert_eq!(
            std::fs::read_to_string(payload_path).expect("payload"),
            r#"{"merge_method":"squash","merge_action":"default"}"#
        );
    }

    #[test]
    fn merge_pr_async_pending_polls_until_merged() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-merge-async-pending",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "PUT" ]; then
  cat >/dev/null
  echo '{"status":"pending","details":{"uuid":"abc-123"}}'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/pulls/77/merge-async/abc-123" ] && [ "$3" = "-H" ] && [ "$4" = "X-GitHub-Api-Version: 2026-03-10" ]; then
  echo poll >> "$EZ_FAKE_GH_LOG"
  echo '{"status":"merged"}'
  exit 0
fi
exit 2
"#,
        );
        let log_path = fake_dir.join("polls.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let outcome = merge_pr_with_poll_interval(77, "squash", std::time::Duration::ZERO, None)
            .expect("pending merge");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }
        assert_eq!(outcome, MergePrOutcome::Merged);
        assert_eq!(std::fs::read_to_string(log_path).expect("polls"), "poll\n");
    }

    #[test]
    fn merge_pr_async_enqueued_returns_enqueued() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-merge-async-enqueued",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "PUT" ]; then
  cat >/dev/null
  echo '{"status":"enqueued"}'
  exit 0
fi
exit 2
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        assert_eq!(
            merge_pr_with_poll_interval(77, "squash", std::time::Duration::ZERO, None)
                .expect("enqueued"),
            MergePrOutcome::Enqueued
        );
    }

    #[test]
    fn merge_pr_async_failed_uses_details_message() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-merge-async-failed",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "PUT" ]; then
  cat >/dev/null
  echo '{"status":"failed","details":{"message":"merge queue rejected it"}}'
  exit 0
fi
exit 2
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        let err = merge_pr_with_poll_interval(77, "squash", std::time::Duration::ZERO, None)
            .expect_err("failed merge");

        assert!(
            err.to_string().contains("merge queue rejected it"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn merge_pr_async_404_falls_back_to_legacy_merge() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-merge-async-404-fallback",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "PUT" ] && [ "$4" = "repos/org/repo/pulls/77/merge-async" ]; then
  echo "HTTP 404: Not Found" >&2
  exit 1
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "PUT" ] && [ "$4" = "repos/org/repo/pulls/77/merge" ] && [ "$5" = "-f" ] && [ "$6" = "merge_method=squash" ]; then
  echo legacy >> "$EZ_FAKE_GH_LOG"
  echo '{"merged":true,"message":"merged"}'
  exit 0
fi
exit 2
"#,
        );
        let log_path = fake_dir.join("legacy.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let outcome = merge_pr_with_poll_interval(77, "squash", std::time::Duration::ZERO, None)
            .expect("legacy fallback");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }
        assert_eq!(outcome, MergePrOutcome::Merged);
        assert_eq!(
            std::fs::read_to_string(log_path).expect("legacy"),
            "legacy\n"
        );
    }

    #[test]
    fn body_from_file_surfaces_missing_file_path() {
        let path = temp_dir("gh-body-file").join("missing.md");
        let err = body_from_file(path.to_str().expect("utf8 path"))
            .expect_err("missing file should fail");
        assert!(
            err.to_string().contains("failed to read body file"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn read_pr_template_returns_content_from_github_dir() {
        let _guard = take_env_lock();
        let repo = init_git_repo("gh-pr-template");
        write_file(
            &repo,
            ".github/pull_request_template.md",
            "## Description\n\nFill me in.\n",
        );
        let _cwd = CwdGuard::enter(&repo);
        let template = read_pr_template().expect("template should be found");
        assert_eq!(template, "## Description\n\nFill me in.");
    }

    #[test]
    fn read_pr_template_prefers_pull_request_template_md() {
        let _guard = take_env_lock();
        let repo = init_git_repo("gh-pr-template-pref");
        write_file(&repo, "pull_request_template.md", "root level template");
        write_file(
            &repo,
            ".github/pull_request_template.md",
            "github dir template",
        );
        let _cwd = CwdGuard::enter(&repo);
        let template = read_pr_template().expect("template should be found");
        assert_eq!(template, "github dir template");
    }

    #[test]
    fn read_pr_template_returns_none_without_template() {
        let _guard = take_env_lock();
        let repo = init_git_repo("gh-pr-template-none");
        let _cwd = CwdGuard::enter(&repo);
        assert!(read_pr_template().is_none());
    }

    #[test]
    fn read_pr_template_ignores_empty_template_file() {
        let _guard = take_env_lock();
        let repo = init_git_repo("gh-pr-template-empty");
        write_file(&repo, ".github/pull_request_template.md", "   \n");
        let _cwd = CwdGuard::enter(&repo);
        assert!(read_pr_template().is_none());
    }

    #[test]
    fn get_ci_status_returns_empty_string_for_malformed_json() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-bad-ci-json",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "run" ] && [ "$2" = "list" ]; then
  echo "{bad-json"
  exit 0
fi
exit 0
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        assert_eq!(get_ci_status("feature", None), "");
    }

    #[test]
    fn ensure_native_stack_not_needed_for_fewer_than_two_prs() {
        assert_eq!(
            ensure_native_stack(&[10], None).expect("single pr"),
            NativeStackOutcome::NotNeeded
        );
    }

    #[test]
    fn native_stack_for_pr_parses_first_stack_in_order() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-read-parse",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=20" ] && [ "$3" = "-H" ] && [ "$4" = "X-GitHub-Api-Version: 2026-03-10" ]; then
  echo '[{"number":88,"base":{"ref":"main"},"open":true,"pull_requests":[{"number":10},{"number":20},{"number":30}]},{"number":99,"pull_requests":[{"number":20}]}]'
  exit 0
fi
exit 2
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        let stack = native_stack_for_pr(20, None)
            .expect("lookup")
            .expect("stack should exist");

        assert_eq!(
            stack,
            NativeStackInfo {
                number: 88,
                base_ref: Some("main".to_string()),
                open: Some(true),
                pull_requests: vec![10, 20, 30],
            }
        );
    }

    #[test]
    fn native_stack_for_pr_returns_none_for_empty_list() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-read-empty",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=20" ] && [ "$3" = "-H" ] && [ "$4" = "X-GitHub-Api-Version: 2026-03-10" ]; then
  echo '[]'
  exit 0
fi
exit 2
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        assert_eq!(native_stack_for_pr(20, None).expect("lookup"), None);
    }

    #[test]
    fn native_stack_for_pr_returns_none_for_404_unavailable() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-read-404",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ]; then
  echo "HTTP 404: Not Found" >&2
  exit 1
fi
exit 2
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        assert_eq!(native_stack_for_pr(20, None).expect("lookup"), None);
    }

    #[test]
    fn ensure_native_stack_creates_with_ordered_payload() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-create",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=10" ] && [ "$3" = "-H" ] && [ "$4" = "X-GitHub-Api-Version: 2026-03-10" ]; then
  echo '[]'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "POST" ] && [ "$4" = "repos/org/repo/stacks" ] && [ "$5" = "--input" ] && [ "$6" = "-" ] && [ "$7" = "-H" ] && [ "$8" = "X-GitHub-Api-Version: 2026-03-10" ]; then
  cat > "$EZ_FAKE_GH_PAYLOAD"
  echo '{"number":88}'
  exit 0
fi
exit 2
"#,
        );
        let payload_path = fake_dir.join("payload.json");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_PAYLOAD", &payload_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let outcome = ensure_native_stack(&[10, 20, 30], None).expect("create stack");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_PAYLOAD");
        }
        assert_eq!(outcome, NativeStackOutcome::Created { number: 88 });
        assert_eq!(
            std::fs::read_to_string(payload_path).expect("payload"),
            r#"{"pull_requests":[10,20,30]}"#
        );
    }

    #[test]
    fn ensure_native_stack_extends_with_delta_only() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-extend",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=10" ] && [ "$3" = "-H" ] && [ "$4" = "X-GitHub-Api-Version: 2026-03-10" ]; then
  echo '[{"number":88,"pull_requests":[{"number":10},{"number":20}]}]'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "POST" ] && [ "$4" = "repos/org/repo/stacks/88/add" ] && [ "$5" = "--input" ] && [ "$6" = "-" ] && [ "$7" = "-H" ] && [ "$8" = "X-GitHub-Api-Version: 2026-03-10" ]; then
  cat > "$EZ_FAKE_GH_PAYLOAD"
  echo '{"number":88}'
  exit 0
fi
exit 2
"#,
        );
        let payload_path = fake_dir.join("payload.json");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_PAYLOAD", &payload_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let outcome = ensure_native_stack(&[10, 20, 30, 40], None).expect("extend stack");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_PAYLOAD");
        }
        assert_eq!(
            outcome,
            NativeStackOutcome::Extended {
                number: 88,
                added: 2
            }
        );
        assert_eq!(
            std::fs::read_to_string(payload_path).expect("payload"),
            r#"{"pull_requests":[30,40]}"#
        );
    }

    #[test]
    fn ensure_native_stack_unchanged_skips_post() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-unchanged",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=10" ] && [ "$3" = "-H" ] && [ "$4" = "X-GitHub-Api-Version: 2026-03-10" ]; then
  echo '[{"number":88,"pull_requests":[{"number":10},{"number":20}]}]'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ]; then
  echo post >> "$EZ_FAKE_GH_LOG"
  exit 2
fi
exit 2
"#,
        );
        let log_path = fake_dir.join("posts.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let outcome = ensure_native_stack(&[10, 20], None).expect("unchanged stack");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }
        assert_eq!(outcome, NativeStackOutcome::Unchanged { number: 88 });
        assert!(!log_path.exists(), "POST should not be called");
    }

    #[test]
    fn ensure_native_stack_partial_prefix_is_unchanged() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-prefix",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=10" ] && [ "$3" = "-H" ] && [ "$4" = "X-GitHub-Api-Version: 2026-03-10" ]; then
  echo '[{"number":88,"pull_requests":[{"number":10},{"number":20},{"number":30}]}]'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ]; then
  echo post >> "$EZ_FAKE_GH_LOG"
  exit 2
fi
exit 2
"#,
        );
        let log_path = fake_dir.join("posts.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let outcome = ensure_native_stack(&[10, 20], None).expect("partial prefix");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }
        assert_eq!(outcome, NativeStackOutcome::Unchanged { number: 88 });
        assert!(!log_path.exists(), "POST should not be called");
    }

    #[test]
    fn ensure_native_stack_returns_unavailable_on_404() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-404",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ]; then
  echo "HTTP 404: Not Found" >&2
  exit 1
fi
exit 2
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        assert_eq!(
            ensure_native_stack(&[10, 20], None).expect("unavailable"),
            NativeStackOutcome::Unavailable
        );
    }

    #[test]
    fn ensure_native_stack_divergence_errors_without_posting() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-diverged",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=10" ] && [ "$3" = "-H" ] && [ "$4" = "X-GitHub-Api-Version: 2026-03-10" ]; then
  echo '[{"number":88,"pull_requests":[{"number":10},{"number":99}]}]'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ]; then
  echo post >> "$EZ_FAKE_GH_LOG"
  exit 2
fi
exit 2
"#,
        );
        let log_path = fake_dir.join("posts.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let err = ensure_native_stack(&[10, 20, 30], None).expect_err("divergence");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }
        assert!(
            err.to_string().contains("diverges"),
            "unexpected error: {err:#}"
        );
        assert!(!log_path.exists(), "POST should not be called");
    }

    #[test]
    fn repair_native_stack_dissolves_and_recreates_divergent_stack() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-repair-recreate",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ]; then
  printf '%s\n' "$*" >> "$EZ_FAKE_GH_LOG"
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=10" ]; then
  echo '[{"number":88,"pull_requests":[{"number":10},{"number":99}]}]'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "POST" ] && [ "$4" = "repos/org/repo/stacks/88/unstack" ]; then
  cat >/dev/null
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "POST" ] && [ "$4" = "repos/org/repo/stacks" ]; then
  cat > "$EZ_FAKE_GH_PAYLOAD"
  echo '{"number":101}'
  exit 0
fi
exit 2
"#,
        );
        let log_path = fake_dir.join("calls.log");
        let payload_path = fake_dir.join("payload.json");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
            std::env::set_var("EZ_FAKE_GH_PAYLOAD", &payload_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let outcome =
            repair_native_stack_exact(&[10, 20, 30], "ez submit", None).expect("repair stack");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
            std::env::remove_var("EZ_FAKE_GH_PAYLOAD");
        }
        assert_eq!(
            outcome,
            NativeStackOutcome::Repaired {
                previous_number: 88,
                number: 101
            }
        );
        assert_eq!(
            std::fs::read_to_string(payload_path).expect("payload"),
            r#"{"pull_requests":[10,20,30]}"#
        );
        let log = std::fs::read_to_string(log_path).expect("calls");
        assert!(log.contains("repos/org/repo/stacks/88/unstack"));
        assert!(log.contains("repos/org/repo/stacks"));
    }

    #[test]
    fn repair_native_stack_extends_existing_prefix_without_unstacking() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-repair-prefix",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ]; then
  printf '%s\n' "$*" >> "$EZ_FAKE_GH_LOG"
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=10" ]; then
  echo '[{"number":88,"pull_requests":[{"number":10},{"number":20}]}]'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$3" = "POST" ] && [ "$4" = "repos/org/repo/stacks/88/add" ]; then
  cat > "$EZ_FAKE_GH_PAYLOAD"
  echo '{"number":88}'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$4" = "repos/org/repo/stacks/88/unstack" ]; then
  echo unstack-called >&2
  exit 2
fi
exit 2
"#,
        );
        let log_path = fake_dir.join("calls.log");
        let payload_path = fake_dir.join("payload.json");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
            std::env::set_var("EZ_FAKE_GH_PAYLOAD", &payload_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let outcome =
            repair_native_stack_exact(&[10, 20, 30], "ez submit", None).expect("extend stack");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
            std::env::remove_var("EZ_FAKE_GH_PAYLOAD");
        }
        assert_eq!(
            outcome,
            NativeStackOutcome::Extended {
                number: 88,
                added: 1
            }
        );
        assert_eq!(
            std::fs::read_to_string(payload_path).expect("payload"),
            r#"{"pull_requests":[30]}"#
        );
        let log = std::fs::read_to_string(log_path).expect("calls");
        assert!(log.contains("repos/org/repo/stacks/88/add"));
        assert!(!log.contains("repos/org/repo/stacks/88/unstack"));
    }

    #[test]
    fn repair_native_stack_retained_exact_is_unchanged_without_create() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-repair-retained-exact",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=10" ]; then
  echo '[{"number":88,"pull_requests":[{"number":10},{"number":99}]}]'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$4" = "repos/org/repo/stacks/88/unstack" ]; then
  cat >/dev/null
  echo '{"number":88,"pull_requests":[{"number":10},{"number":20}]}'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$4" = "repos/org/repo/stacks" ]; then
  echo create >> "$EZ_FAKE_GH_LOG"
  exit 2
fi
exit 2
"#,
        );
        let log_path = fake_dir.join("posts.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let outcome =
            repair_native_stack_exact(&[10, 20], "ez submit", None).expect("retained exact");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }
        assert_eq!(outcome, NativeStackOutcome::Unchanged { number: 88 });
        assert!(!log_path.exists(), "POST should not be called");
    }

    #[test]
    fn repair_native_stack_retained_divergent_after_unstack_errors_without_create() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-repair-retained-divergent",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=10" ]; then
  count_file="$EZ_FAKE_GH_COUNT"
  count=0
  if [ -f "$count_file" ]; then
    count=$(cat "$count_file")
  fi
  count=$((count + 1))
  echo "$count" > "$count_file"
  echo '[{"number":88,"pull_requests":[{"number":10},{"number":99}]}]'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$4" = "repos/org/repo/stacks/88/unstack" ]; then
  cat >/dev/null
  echo '{"number":88,"pull_requests":[{"number":10},{"number":99}]}'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$4" = "repos/org/repo/stacks" ]; then
  echo create >> "$EZ_FAKE_GH_LOG"
  exit 2
fi
exit 2
"#,
        );
        let count_path = fake_dir.join("count");
        let log_path = fake_dir.join("posts.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_COUNT", &count_path);
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let err = repair_native_stack_exact(&[10, 20], "ez submit", None)
            .expect_err("retained divergent should fail");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_COUNT");
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }
        assert!(
            err.to_string().contains("queued or locked"),
            "unexpected error: {err:#}"
        );
        assert!(!log_path.exists(), "create should not be called");
    }

    #[test]
    fn repair_native_stack_rereads_after_409_and_succeeds_when_stack_matches() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-repair-409-reread",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=10" ]; then
  count_file="$EZ_FAKE_GH_COUNT"
  count=0
  if [ -f "$count_file" ]; then
    count=$(cat "$count_file")
  fi
  count=$((count + 1))
  echo "$count" > "$count_file"
  if [ "$count" = "1" ]; then
    echo '[{"number":88,"pull_requests":[{"number":10},{"number":99}]}]'
  else
    echo '[{"number":88,"pull_requests":[{"number":10},{"number":20}]}]'
  fi
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$4" = "repos/org/repo/stacks/88/unstack" ]; then
  cat >/dev/null
  echo "HTTP 409: Conflict" >&2
  exit 1
fi
exit 2
"#,
        );
        let count_path = fake_dir.join("count");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_COUNT", &count_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let outcome =
            repair_native_stack_exact(&[10, 20], "ez submit", None).expect("reread success");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_COUNT");
        }
        assert_eq!(outcome, NativeStackOutcome::Unchanged { number: 88 });
        assert_eq!(std::fs::read_to_string(count_path).expect("count"), "2\n");
    }

    #[test]
    fn repair_native_stack_repeated_409_exhausts_bounded_attempts() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-repair-409-exhaust",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=10" ]; then
  echo list >> "$EZ_FAKE_GH_LOG"
  echo '[{"number":88,"pull_requests":[{"number":10},{"number":99}]}]'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$4" = "repos/org/repo/stacks/88/unstack" ]; then
  cat >/dev/null
  echo "HTTP 409: Conflict" >&2
  exit 1
fi
exit 2
"#,
        );
        let log_path = fake_dir.join("list.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let err = repair_native_stack_exact(&[10, 20], "ez submit", None)
            .expect_err("409 should exhaust");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }
        assert!(
            err.to_string().contains("after 3 attempts"),
            "unexpected error: {err:#}"
        );
        let list_count = std::fs::read_to_string(log_path)
            .expect("log")
            .lines()
            .count();
        assert_eq!(
            list_count, 3,
            "initial list plus rereads before the second and third mutation attempts"
        );
    }

    #[test]
    fn repair_native_stack_mutation_404_restarts_and_repairs() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-repair-404-restart",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=10" ]; then
  count_file="$EZ_FAKE_GH_COUNT"
  count=0
  if [ -f "$count_file" ]; then
    count=$(cat "$count_file")
  fi
  count=$((count + 1))
  echo "$count" > "$count_file"
  if [ "$count" = "1" ]; then
    echo '[{"number":88,"pull_requests":[{"number":10},{"number":99}]}]'
  else
    echo '[{"number":89,"pull_requests":[{"number":10},{"number":99}]}]'
  fi
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$4" = "repos/org/repo/stacks/88/unstack" ]; then
  cat >/dev/null
  echo "HTTP 404: Not Found" >&2
  exit 1
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$4" = "repos/org/repo/stacks/89/unstack" ]; then
  cat >/dev/null
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$4" = "repos/org/repo/stacks" ]; then
  cat >/dev/null
  echo '{"number":101}'
  exit 0
fi
exit 2
"#,
        );
        let count_path = fake_dir.join("count");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_COUNT", &count_path);
        }
        let _path = PathGuard::install(&fake_dir);

        let outcome = repair_native_stack_exact(&[10, 20], "ez submit", None).expect("404 restart");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_COUNT");
        }
        assert_eq!(
            outcome,
            NativeStackOutcome::Repaired {
                previous_number: 89,
                number: 101
            }
        );
    }

    #[test]
    fn repair_native_stack_rejects_more_than_100_before_repo_discovery() {
        let _guard = take_env_lock();
        let empty_dir = temp_dir("ez-native-too-many-no-gh");
        let _path = PathGuard::install(&empty_dir);
        let pr_numbers: Vec<u64> = (1..=101).collect();

        let err = repair_native_stack_exact(&pr_numbers, "ez submit", None)
            .expect_err("too many PRs should fail");

        assert!(
            err.to_string().contains("at most 100"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn repair_native_stack_sends_rest_stack_headers() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-repair-headers",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ]; then
  printf '%s\n' "$*" >> "$EZ_FAKE_GH_LOG"
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=10" ]; then
  echo '[{"number":88,"pull_requests":[{"number":10},{"number":99}]}]'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$4" = "repos/org/repo/stacks/88/unstack" ]; then
  cat >/dev/null
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$4" = "repos/org/repo/stacks" ]; then
  cat >/dev/null
  echo '{"number":101}'
  exit 0
fi
exit 2
"#,
        );
        let log_path = fake_dir.join("calls.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
        }
        let _path = PathGuard::install(&fake_dir);

        repair_native_stack_exact(&[10, 20], "ez submit", None).expect("repair");

        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }
        let log = std::fs::read_to_string(log_path).expect("calls");
        for line in log.lines() {
            assert!(
                line.contains("-H X-GitHub-Api-Version: 2026-03-10")
                    && line.contains("-H Accept: application/vnd.github+json"),
                "missing required stack REST headers in: {line}"
            );
        }
    }

    #[test]
    fn repair_native_stack_recreate_failure_mentions_dissolved_stack_and_retry() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_bin(
            "gh-native-repair-recreate-fail",
            "gh",
            r#"#!/bin/sh
if [ "$1" = "repo" ]; then
  echo "org/repo"
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "repos/org/repo/stacks?pull_request=10" ]; then
  echo '[{"number":88,"pull_requests":[{"number":10},{"number":99}]}]'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$4" = "repos/org/repo/stacks/88/unstack" ]; then
  cat >/dev/null
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "-X" ] && [ "$4" = "repos/org/repo/stacks" ]; then
  cat >/dev/null
  echo "HTTP 500: create failed" >&2
  exit 1
fi
exit 2
"#,
        );
        let _path = PathGuard::install(&fake_dir);

        let err = repair_native_stack_exact(&[10, 20], "ez submit --retry", None)
            .expect_err("create failure");

        assert!(
            err.to_string()
                .contains("previous native stack #88 was dissolved")
                && err.to_string().contains("retry `ez submit --retry`"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn parse_owner_repo_from_remote_url_handles_common_forms() {
        let cases = [
            (
                "git@github.com:onyx-dot-app/onyx.git",
                "onyx-dot-app",
                "onyx",
            ),
            ("git@github.com:onyx-dot-app/onyx", "onyx-dot-app", "onyx"),
            (
                "https://github.com/onyx-dot-app/onyx.git",
                "onyx-dot-app",
                "onyx",
            ),
            (
                "https://github.com/onyx-dot-app/onyx",
                "onyx-dot-app",
                "onyx",
            ),
            (
                "ssh://git@github.com/onyx-dot-app/onyx.git",
                "onyx-dot-app",
                "onyx",
            ),
            (
                "git://github.com/onyx-dot-app/onyx.git",
                "onyx-dot-app",
                "onyx",
            ),
            // Subpaths beyond owner/repo are ignored.
            (
                "https://github.com/onyx-dot-app/onyx/tree/main",
                "onyx-dot-app",
                "onyx",
            ),
            // Leading/trailing whitespace from git output.
            (
                "  https://github.com/onyx-dot-app/onyx.git\n",
                "onyx-dot-app",
                "onyx",
            ),
        ];
        for (url, want_owner, want_repo) in cases {
            let got = parse_owner_repo_from_remote_url(url)
                .unwrap_or_else(|| panic!("expected Some for `{url}`"));
            assert_eq!(
                got,
                (want_owner.to_string(), want_repo.to_string()),
                "url={url}"
            );
        }
    }

    #[test]
    fn parse_owner_repo_from_remote_url_returns_none_for_unrecognized_forms() {
        // SSH host alias (e.g. ~/.ssh/config) — has no protocol prefix.
        assert!(parse_owner_repo_from_remote_url("github:onyx-dot-app/onyx").is_none());
        // Empty / missing path.
        assert!(parse_owner_repo_from_remote_url("https://github.com/").is_none());
        assert!(parse_owner_repo_from_remote_url("git@github.com:onyx-dot-app").is_none());
        // Total junk.
        assert!(parse_owner_repo_from_remote_url("not a url").is_none());
    }

    #[test]
    fn pull_request_head_uses_explicit_fork_owner_when_fork_differs_from_target() {
        assert_eq!(
            pull_request_head(
                "origin",
                Some("upstream/project"),
                Some("fork-owner/project"),
                true,
                "feat/x"
            )
            .expect("head"),
            "fork-owner:feat/x"
        );
    }

    #[test]
    fn pull_request_head_leaves_same_repo_heads_unqualified() {
        assert_eq!(
            pull_request_head(
                "origin",
                Some("upstream/project"),
                Some("upstream/project"),
                false,
                "feat/x"
            )
            .expect("head"),
            "feat/x"
        );
    }

    #[test]
    fn pull_request_head_qualifies_parsed_push_remote_when_target_differs() {
        let _guard = take_env_lock();
        let repo = temp_dir("ez-pr-head-remote");
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&repo)
            .status()
            .expect("git init");
        std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "fork",
                "https://github.com/fork-owner/project.git",
            ])
            .current_dir(&repo)
            .status()
            .expect("remote add");
        let _cwd = CwdGuard::enter(&repo);

        assert_eq!(
            pull_request_head("fork", Some("upstream/project"), None, true, "feat/x")
                .expect("head"),
            "fork-owner:feat/x"
        );
    }

    #[test]
    fn pull_request_head_errors_actionably_when_target_differs_and_owner_cannot_be_resolved() {
        let _guard = take_env_lock();
        let repo = temp_dir("ez-pr-head-alias");
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&repo)
            .status()
            .expect("git init");
        std::process::Command::new("git")
            .args(["remote", "add", "fork", "github-fork:fork-owner/project"])
            .current_dir(&repo)
            .status()
            .expect("remote add");
        let _cwd = CwdGuard::enter(&repo);

        let err = pull_request_head("fork", Some("upstream/project"), None, true, "feat/x")
            .expect_err("unparseable fork remote should require fork_repo");
        assert!(
            err.to_string().contains("set `fork_repo = \"OWNER/REPO\"`"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn pull_request_head_preserves_unqualified_head_for_unresolved_same_repo_workflow() {
        let _guard = take_env_lock();
        let repo = temp_dir("ez-pr-head-same-repo-alias");
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&repo)
            .status()
            .expect("git init");
        std::process::Command::new("git")
            .args(["remote", "add", "origin", "github-work:owner/project"])
            .current_dir(&repo)
            .status()
            .expect("remote add");
        let _cwd = CwdGuard::enter(&repo);

        assert_eq!(
            pull_request_head("origin", Some("owner/project"), None, false, "feat/x")
                .expect("same-repo head"),
            "feat/x"
        );
    }

    #[test]
    fn pull_request_head_without_target_repo_preserves_legacy_branch_head() {
        assert_eq!(
            pull_request_head("origin", None, None, false, "feat/x").expect("head"),
            "feat/x"
        );
    }

    #[test]
    fn build_pr_statuses_query_aliases_each_branch_in_order() {
        let q = build_pr_statuses_query(&["feat/a", "feat/b"]);
        assert!(q.starts_with(
            "query($owner:String!,$name:String!){repository(owner:$owner,name:$name){"
        ));
        assert!(q.contains("b0:pullRequests(headRefName:\"feat/a\""));
        assert!(q.contains("b1:pullRequests(headRefName:\"feat/b\""));
        assert!(q.contains("first:1"));
        assert!(q.contains("orderBy:{field:CREATED_AT,direction:DESC}"));
        assert!(q.ends_with("}}"));
        // b0 must appear before b1 — alias order is how we map response → branch.
        let b0 = q.find("b0:").expect("b0 alias present");
        let b1 = q.find("b1:").expect("b1 alias present");
        assert!(b0 < b1);
    }

    #[test]
    fn build_pr_statuses_query_escapes_special_characters_in_branch_names() {
        // GraphQL string literals share JSON escape rules: quote → \" and
        // backslash → \\. Branch names with these characters must survive intact.
        let q = build_pr_statuses_query(&["feat/has\"quote", "back\\slash"]);
        assert!(q.contains(r#""feat/has\"quote""#), "query: {q}");
        assert!(q.contains(r#""back\\slash""#), "query: {q}");
    }

    #[test]
    fn parse_pr_statuses_response_maps_aliased_nodes_back_to_branches() {
        let value = serde_json::json!({
            "data": {
                "repository": {
                    "b0": {"nodes": [{
                        "number": 12,
                        "url": "https://example.com/pr/12",
                        "state": "OPEN",
                        "title": "Hi",
                        "baseRefName": "main",
                        "isDraft": false,
                        "mergedAt": null,
                    }]},
                    "b1": {"nodes": []},
                }
            }
        });
        let map = parse_pr_statuses_response(&value, &["feat/a", "feat/missing"]);
        let pr = map.get("feat/a").expect("present");
        assert_eq!(pr.number, 12);
        assert_eq!(pr.state, "OPEN");
        assert!(!pr.merged);
        // A branch with no PR must be absent from the map (not present with default values).
        assert!(!map.contains_key("feat/missing"));
    }

    #[test]
    fn parse_pr_statuses_response_handles_empty_data() {
        // Network/auth failure shape: data is null/absent.
        let value = serde_json::json!({});
        let map = parse_pr_statuses_response(&value, &["feat/a"]);
        assert!(map.is_empty());
    }

    #[test]
    fn pr_info_from_graphql_node_marks_merged_state_correctly() {
        let value = serde_json::json!({
            "number": 5,
            "url": "https://example.com/pr/5",
            "state": "MERGED",
            "title": "Merged",
            "baseRefName": "main",
            "isDraft": false,
            "mergedAt": "2026-04-01T00:00:00Z",
        });
        let pr = pr_info_from_graphql_node(&value).expect("valid node");
        assert_eq!(pr.state, "MERGED");
        assert!(pr.merged);
    }

    #[test]
    fn pr_info_from_graphql_node_distinguishes_closed_from_merged() {
        let value = serde_json::json!({
            "number": 6,
            "url": "https://example.com/pr/6",
            "state": "CLOSED",
            "title": "Closed",
            "baseRefName": "main",
            "isDraft": false,
            "mergedAt": null,
        });
        let pr = pr_info_from_graphql_node(&value).expect("valid node");
        assert_eq!(pr.state, "CLOSED");
        assert!(!pr.merged);
    }

    #[test]
    fn pr_info_from_graphql_node_returns_none_when_number_missing() {
        // A malformed node (no number) must be skipped rather than yielding a
        // PrInfo with number=0 that callers might treat as real.
        let value = serde_json::json!({
            "url": "https://example.com/pr/0",
            "state": "OPEN",
            "title": "Broken",
        });
        assert!(pr_info_from_graphql_node(&value).is_none());
    }

    #[test]
    fn get_pr_statuses_for_returns_empty_without_invoking_gh_when_no_branches() {
        // Critical: an empty branch list must short-circuit without any
        // subprocess call. Run with PATH pointed at a directory that does not
        // contain `gh` to prove no exec happens.
        let _guard = take_env_lock();
        let empty_dir = temp_dir("ez-empty-path");
        let _path = PathGuard::install(&empty_dir);

        let map = get_pr_statuses_for("origin", None, &[]);
        assert!(map.is_empty());
    }

    #[test]
    fn get_pr_statuses_for_returns_canned_graphql_response() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_gh("graphql-canned");
        let _path = PathGuard::install(&fake_dir);

        // We pass three branches matching the fake gh's canned aliases:
        // b0 → merged, b1 → open, b2 → no PR.
        let map = get_pr_statuses_for("origin", None, &["feat/merged", "feat/open", "feat/no-pr"]);

        let merged = map.get("feat/merged").expect("merged branch present");
        assert_eq!(merged.number, 42);
        assert_eq!(merged.state, "MERGED");
        assert!(merged.merged);
        assert_eq!(merged.base, "main");

        let open = map.get("feat/open").expect("open branch present");
        assert_eq!(open.number, 43);
        assert_eq!(open.state, "OPEN");
        assert!(!open.merged);
        assert!(open.is_draft);

        assert!(!map.contains_key("feat/no-pr"));
    }

    #[test]
    fn get_pr_statuses_for_sends_one_request_for_many_branches() {
        // Latency win is contingent on a single round-trip regardless of branch
        // count. Lock that contract by counting subprocess invocations.
        let _guard = take_env_lock();
        let fake_dir = install_fake_gh("graphql-once");
        let _path = PathGuard::install(&fake_dir);

        let log_path = fake_dir.join("calls.log");
        unsafe {
            std::env::set_var("EZ_FAKE_GH_LOG", &log_path);
        }
        let branches: Vec<String> = (0..25).map(|i| format!("feat/b{i}")).collect();
        let refs: Vec<&str> = branches.iter().map(String::as_str).collect();
        let _ = get_pr_statuses_for("origin", None, &refs);
        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }

        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let graphql_calls = log.lines().filter(|l| l.starts_with("graphql")).count();
        assert_eq!(graphql_calls, 1, "expected 1 graphql call, got log:\n{log}");
    }
}
