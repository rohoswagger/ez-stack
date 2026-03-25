# ez

**Stacked PRs for GitHub.**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![CI](https://github.com/rohoswagger/ez-stack/actions/workflows/ci.yml/badge.svg)](https://github.com/rohoswagger/ez-stack/actions/workflows/ci.yml)

---

`ez` is a fast, lightweight CLI for managing stacked pull requests on GitHub. It shells out to `git` and `gh` so there's nothing magical happening under the hood — just the tools you already know, orchestrated intelligently.

## Why stacked PRs?

Large pull requests are hard to review. Stacked PRs let you break work into a chain of small, focused branches where each branch builds on the one below it:

```
main
 └── feat/auth-types        ← PR #1 (data models)
      └── feat/auth-api     ← PR #2 (API routes, depends on #1)
           └── feat/auth-ui ← PR #3 (frontend, depends on #2)
```

Reviewers see small diffs. You keep working without waiting. When PR #1 merges, `ez` rebases the rest of the stack automatically.

The problem is that `git` doesn't know about stacks. Rebasing, reordering, and keeping GitHub PRs pointed at the right base branch is tedious and error-prone. `ez` handles all of that for you.

## Quick start

```bash
# Install
cargo install ez-stack

# Initialize in any git repo
cd your-repo
ez init

# Start building a stack
ez create feat/parse-config
# ... make changes ...
ez commit -m "add config parser"

ez create feat/use-config
# ... make changes ...
ez commit -m "wire config into app"

# Push the entire stack and open PRs for all branches
ez submit
```

That's it. Two PRs, correctly chained, with GitHub base branches set automatically.

## Using ez with AI Agents

`ez` is designed to work well in automated and agentic contexts:

- **JSON output** — `ez status --json` and `ez log --json` emit machine-readable data that agents can parse without screen-scraping.
- **Non-interactive flags** — `ez checkout <name>` or `ez checkout 42` bypasses the TUI picker. `ez commit --if-changed` exits cleanly when there's nothing to commit.
- **Autostash** — `ez sync --autostash` eliminates the `git stash && ... && git stash pop` dance.
- **Structured exit codes** — each failure mode has a distinct exit code (rebase conflict = 3, stale remote = 4, usage error = 5, unstaged changes = 6) so agents can take the right recovery action.

### Install the ez skill

If your agent supports Skills, install the repo's `ez-workflow` skill directly from GitHub:

```bash
npx skills add https://github.com/rohoswagger/ez-stack --skill ez-workflow
```

That installs the instructions from [`SKILL.md`](./SKILL.md), which teaches the agent when to use `ez` instead of raw `git`/`gh` commands in an `ez`-managed repo.

### Quick install check

```bash
cargo install ez-stack && ez --version
```

### Key agent patterns

```bash
# Parse current branch state
ez status --json
# → {"branch":"feat/x","parent":"main","pr_number":42,"children":[],"needs_restack":false,...}

# Only commit if there are staged changes
ez commit -m "chore: format" --if-changed

# Sync without a manual stash dance
ez sync --autostash

# Get just the PR URL for scripting
open $(ez pr-link)

# Find branches that need restacking
ez log --json | jq '.[] | select(.needs_restack)'
```

For agents that support local instruction files, [`AGENTS.md`](./AGENTS.md) contains a compact install-and-usage guide. For skills-based agents, install `ez-workflow` with `npx skills add` and use that skill directly.

### Agent setup prompt

Copy-paste this into any AI coding agent to get `ez` installed and configured:

<blockquote>

Install ez-stack, a CLI for stacked PRs on GitHub, and set it up in this repo.

**Steps:**

1. Install the binary: `cargo install ez-stack`
2. Verify it works: `ez --version`
3. Initialize in this repo: `ez init`
4. Add these rules to the project's CLAUDE.md (or equivalent agent instructions file):

```
## ez-stack

This repo uses `ez` for stacked PRs. When `.git/ez/stack.json` exists, use `ez` instead of raw git/gh commands:

- `ez create <name>` instead of `git checkout -b`
- `ez commit -m "msg"` instead of `git commit` (auto-restacks children, shows diff stat)
- `ez commit -m "msg" -- path1 path2` to stage specific files
- `ez diff --stat` instead of `git diff main...HEAD --stat`
- `ez parent` instead of parsing `ez status` for the parent branch
- `ez push` instead of `git push` / `gh pr create`
- `ez submit` to push the entire stack
- `ez sync --autostash` to sync with trunk

Key flags: `--json` on status/log for machine output, `--if-changed` on commit to no-op when nothing staged, `--from <base>` on create to skip checkout.

Output: every command appends `[ok | 45ms]` or `[exit:3 | 120ms]` to stderr.
Exit codes: 0=ok, 2=gh error, 3=conflict, 4=stale ref, 5=usage error, 6=unstaged changes.
```

</blockquote>

## Commands

### Stack creation & editing

| Command | Description |
|---------|-------------|
| `ez init` | Initialize `ez` in the current repository |
| `ez create <name>` | Create a new branch stacked on the current branch |
| `ez create <name> -m "msg"` | Create branch and commit staged changes in one step |
| `ez create <name> -am "msg"` | Create branch, stage all tracked changes, and commit |
| `ez create <name> --from <base>` | Create branch from a specific base without checking it out first |
| `ez commit -m <msg>` | Commit staged changes, restack children, show diff stat |
| `ez commit -m "subj" -m "body"` | Multi-paragraph commit (repeated `-m`, like git) |
| `ez commit -m <msg> -- <paths>` | Stage specific paths and commit |
| `ez commit -m <msg> --if-changed` | Commit only if there are staged changes (no-op otherwise) |
| `ez amend` | Amend the last commit and restack children |
| `ez delete [<name>]` | Delete a branch from the stack and restack |
| `ez move --onto <branch>` | Reparent the current branch onto another branch |

### Syncing & rebasing

| Command | Description |
|---------|-------------|
| `ez sync` | Fetch trunk, detect merged PRs, clean up, and restack |
| `ez sync --dry-run` | Preview what sync would do without making changes |
| `ez sync --autostash` | Stash uncommitted changes before sync, restore after |
| `ez restack` | Rebase each branch onto its parent |

### Navigation

| Command | Description |
|---------|-------------|
| `ez up` | Check out the branch above the current one |
| `ez down` | Check out the branch below the current one |
| `ez top` | Check out the top of the stack |
| `ez bottom` | Check out the bottom of the stack |
| `ez checkout` | Interactively select a branch to check out |
| `ez checkout <name>` | Switch directly to a branch by name (non-interactive) |
| `ez checkout <number>` | Switch directly to a branch by PR number (non-interactive) |

### GitHub integration

| Command | Description |
|---------|-------------|
| `ez push` | Push **current branch only** and create/update its PR |
| `ez push --title "..." --body "..."` | Push and set/update PR title and body |
| `ez push --base <branch>` | Push and override the PR base branch |
| `ez submit` | Push **all branches** in the stack and create/update all PRs |
| `ez pr` | Open the current branch's PR in the browser |
| `ez pr-link` | Print the PR URL to stdout (pipeable) |
| `ez pr-edit` | Edit the PR body in `$EDITOR` |
| `ez pr-edit --title "..." --body "..."` | Edit the PR title/body directly |
| `ez draft` | Mark the current PR as a draft |
| `ez ready` | Mark the current PR as ready for review |
| `ez merge` | Merge the bottom PR of the stack via GitHub |

### Inspection & diffing

| Command | Description |
|---------|-------------|
| `ez log` | Show the full stack with branch names, commit counts, and PR status |
| `ez log --json` | Show the full stack as a JSON array (machine-readable) |
| `ez status` | Show the current branch and its position in the stack |
| `ez status --json` | Show current branch info as JSON (machine-readable) |
| `ez diff` | Show diff of current branch vs parent (what the PR reviewer sees) |
| `ez diff --stat` | Show only the diffstat summary |
| `ez diff --name-only` | Show only changed file names |
| `ez parent` | Print the parent branch name to stdout (pipeable) |

### `ez push` vs `ez submit`

| | `ez push` | `ez submit` |
|--|-----------|-------------|
| **Scope** | Current branch only | All branches from trunk to current |
| **PRs** | Creates/updates PR for current branch | Creates/updates PRs for all branches |
| **When to use** | Iterating on a single branch | First push of a stack, or after restacking all branches |

> **Note:** Running `gh pr create` after `ez push` will fail — `ez push` already created the PR.

## Example workflow

Here's a complete session building a three-branch stack:

```bash
# 1. Start from main
git checkout main && git pull
ez init

# 2. Create the first branch in the stack
ez create feat/auth-types
cat > src/auth/types.rs << 'EOF'
pub struct User { pub id: u64, pub email: String }
pub struct Session { pub token: String, pub user_id: u64 }
EOF
ez commit -m "define User and Session types"

# 3. Stack a second branch on top
ez create feat/auth-api
cat > src/auth/api.rs << 'EOF'
pub fn login(email: &str) -> Session { /* ... */ }
pub fn logout(session: &Session) { /* ... */ }
EOF
ez commit -m "add login/logout API"

# 4. Stack a third branch on top
ez create feat/auth-middleware
cat > src/middleware/auth.rs << 'EOF'
pub fn require_auth(req: &Request) -> Result<User, AuthError> { /* ... */ }
EOF
ez commit -m "add auth middleware"

# 5. See the full stack
ez log
#   main
#   ├── feat/auth-types        (1 commit)
#   │   ├── feat/auth-api      (1 commit)
#   │   │   ├── feat/auth-middleware (1 commit)  ← you are here

# 6. Push everything and open PRs
ez submit
# Creates 3 PRs:
#   feat/auth-types        → main
#   feat/auth-api          → feat/auth-types
#   feat/auth-middleware    → feat/auth-api

# 7. After feat/auth-types is reviewed and merged on GitHub:
ez sync
# Fetches main (which now includes auth-types),
# rebases auth-api onto main, rebases auth-middleware onto auth-api,
# deletes the merged feat/auth-types branch,
# and updates PR base branches on GitHub.
```

### `ez create` with a commit message

`ez create` accepts `-m` to commit staged changes in one step:

```bash
git add src/auth.rs
ez create feat/auth -m "add auth module"
# equivalent to: ez create feat/auth && ez commit -m "add auth module"
```

Use `-a` to stage all tracked changes automatically (like `git commit -a`):

```bash
ez create feat/auth -am "add auth module"
# equivalent to: git add -A && ez create feat/auth -m "add auth module"
```

## How it works

`ez` is intentionally simple in its architecture:

- **No custom git internals.** Every git operation is a call to the `git` CLI. Every GitHub operation goes through `gh`. You can always see exactly what happened by reading your git log.
- **Stack metadata** is stored in `.git/ez/stack.json` — a single JSON file tracking branch order, parent relationships, and associated PR numbers. It's local to your repo and ignored by git.
- **Restacking** uses `git rebase --onto` to move each branch in the stack onto its updated parent. This is the same operation you'd do by hand; `ez` just does it for every branch in the right order.
- **PR management** calls `gh pr create` and `gh pr edit` to set base branches so GitHub shows the correct, minimal diff for each PR in the stack.

### Stack metadata format

```json
{
  "version": 1,
  "trunk": "main",
  "branches": [
    { "name": "feat/auth-types", "parent": "main", "pr": 101 },
    { "name": "feat/auth-api", "parent": "feat/auth-types", "pr": 102 },
    { "name": "feat/auth-middleware", "parent": "feat/auth-api", "pr": null }
  ]
}
```

## Prerequisites

- **git** 2.38+
- **gh** (GitHub CLI), authenticated via `gh auth login`
- A GitHub repository with push access

## Installation

### From crates.io

```bash
cargo install ez-stack
```

### From source

```bash
git clone https://github.com/rohoswagger/ez-stack.git
cd ez-stack
cargo install --path .
```

### Install script (recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/rohoswagger/ez-stack/main/install.sh | bash
```

To install a specific version:

```bash
curl -fsSL https://raw.githubusercontent.com/rohoswagger/ez-stack/main/install.sh | bash -s -- v0.1.0
```

### GitHub releases

Pre-built binaries for Linux and macOS are available on the [Releases](https://github.com/rohoswagger/ez-stack/releases) page.

## Contributing

Contributions are welcome! Please read [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, code style, and how to submit changes.

## License

MIT. See [LICENSE](LICENSE) for details.
