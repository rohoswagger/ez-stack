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
    pub base: String,
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
        base: base.to_string(),
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
        "number,url,state,title,isDraft,mergedAt,baseRefName",
    ]);

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

pub fn get_all_pr_statuses() -> std::collections::HashMap<String, PrInfo> {
    let mut map = std::collections::HashMap::new();
    let mut page = 1;

    loop {
        let route = format!("repos/{{owner}}/{{repo}}/pulls?state=all&per_page=100&page={page}");
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
    branches: &[&str],
) -> std::collections::HashMap<String, PrInfo> {
    if branches.is_empty() {
        return std::collections::HashMap::new();
    }

    let Ok((owner, name)) = resolve_owner_repo(remote) else {
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

/// Look up one PR by number. Returns `None` on any failure — callers surface
/// the missing-PR case as a user-facing error.
pub fn get_pr_by_number(remote: &str, number: u64) -> Option<(String, PrInfo)> {
    let (owner, name) = resolve_owner_repo(remote).ok()?;

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
fn resolve_owner_repo(remote: &str) -> Result<(String, String)> {
    if let Ok(url) = crate::git::remote_url(remote) {
        if let Some(pair) = parse_owner_repo_from_remote_url(&url) {
            return Ok(pair);
        }
    }
    let repo = repo_name()?;
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

pub fn merge_pr(pr_number: u64, method: &str) -> Result<()> {
    let repo = repo_name()?;
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
        return Ok(());
    }

    let message = value["message"].as_str().unwrap_or("merge failed");
    bail!(EzError::GhError(message.to_string()));
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
/// Fetch CI status for all branches in one API call.
/// Returns a map of branch_name → status emoji (✓/✗/⏳).
/// Uses the most recent run per branch.
pub fn get_all_ci_statuses() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let output = run_gh(&[
        "api",
        "repos/{owner}/{repo}/actions/runs?per_page=50",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{PathGuard, install_fake_bin, take_env_lock, temp_dir};

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
    if [ "$1" = "-X" ] && [ "$2" = "PUT" ] && [ "$3" = 'repos/org/repo/pulls/77/merge' ] && [ "$4" = "-f" ] && [ "$5" = 'merge_method=squash' ]; then
      echo '{"merged":true,"message":"merged"}'
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

        let created = create_pr("Title", "Body", "main", "feature", true).expect("create pr");
        assert_eq!(created.number, 77);
        assert!(created.is_draft);

        update_pr_base(77, "develop").expect("update base");
        edit_pr(77, Some("New title"), Some("New body")).expect("edit pr");
        merge_pr(77, "squash").expect("merge pr");
        set_pr_ready(77, true).expect("ready");
        open_pr_in_browser("feature").expect("open in browser");
        assert!(is_gh_authenticated());
        assert_eq!(repo_name().expect("repo name"), "org/repo");
        assert_eq!(get_pr_body(123).expect("body"), "Body text");

        let status = get_pr_status("feature")
            .expect("pr status")
            .expect("some pr");
        assert_eq!(status.number, 55);
        assert_eq!(status.base, "main");
        assert_eq!(status.state, "OPEN");
    }

    #[test]
    fn gh_bulk_helpers_parse_fake_cli_output() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_gh("bulk");
        let _path = PathGuard::install(&fake_dir);

        let prs = get_all_pr_statuses();
        assert_eq!(prs.get("feat/reused").expect("reused").number, 10);
        assert_eq!(prs.get("feat/other").expect("other").base, "develop");

        let ci = get_all_ci_statuses();
        assert_eq!(ci.get("feat/reused").expect("ci"), "✓");
        assert_eq!(ci.get("feat/other").expect("ci"), "⏳");
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

        let err = create_pr("Title", "Body", "main", "feature", false)
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

        let err = get_pr_status("feature").expect_err("bad json should bubble up");
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

        let err = repo_name().expect_err("empty repo name should fail");
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

        let err = merge_pr(12, "squash").expect_err("merge should fail");
        assert!(
            err.to_string().contains("permission denied"),
            "unexpected error: {err:#}"
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

        assert_eq!(get_ci_status("feature"), "");
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

        let map = get_pr_statuses_for("origin", &[]);
        assert!(map.is_empty());
    }

    #[test]
    fn get_pr_statuses_for_returns_canned_graphql_response() {
        let _guard = take_env_lock();
        let fake_dir = install_fake_gh("graphql-canned");
        let _path = PathGuard::install(&fake_dir);

        // We pass three branches matching the fake gh's canned aliases:
        // b0 → merged, b1 → open, b2 → no PR.
        let map = get_pr_statuses_for("origin", &["feat/merged", "feat/open", "feat/no-pr"]);

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
        let _ = get_pr_statuses_for("origin", &refs);
        unsafe {
            std::env::remove_var("EZ_FAKE_GH_LOG");
        }

        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let graphql_calls = log.lines().filter(|l| l.starts_with("graphql")).count();
        assert_eq!(graphql_calls, 1, "expected 1 graphql call, got log:\n{log}");
    }
}
