# ez

**Agent-first version control. Stacked PRs, worktree isolation, zero friction.**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![PyPI](https://img.shields.io/pypi/v/ez-stack)](https://pypi.org/project/ez-stack/)
[![CI](https://github.com/rohoswagger/ez-stack/actions/workflows/ci.yml/badge.svg)](https://github.com/rohoswagger/ez-stack/actions/workflows/ci.yml)

---

`ez` makes version control invisible for AI coding agents. Four commands cover the entire development lifecycle. Multiple agents work on the same repo without stepping on each other.

## Install

```bash
pip install ez-stack
ez setup --yes
ez init
```

## The 4 Commands

```bash
ez create feat/auth              # Start: worktree + branch + cd
ez push -am "feat: add auth"     # Ship: stage + commit + push + PR
ez sync --autostash              # Sync: pull trunk, clean merged, restack
ez delete feat/auth --yes        # Done: remove worktree + branch
```

That's it. No `git add`, no `git commit`, no `gh pr create`, no `cd`.

## Why ez?

**For agents:** Each `ez create` gives the agent an isolated worktree. Multiple agents work in parallel on the same repo without merge conflicts. Structured JSON output, mutation receipts, and exit codes let agents verify every operation.

**For humans:** Stacked PRs become effortless. Auto-restacking, auto-cleanup of merged branches, and a dashboard that shows everything at a glance.

## Dashboard

```bash
ez list
```

```
     BRANCH                         PR       CI     AGE    PORT    STATUS
--------------------------------------------------------------------------------
     main (trunk)                   -        -      2m     -       -
  *  feat/auth                      #42      ✓      5m     14832   clean
     feat/api                       #43      ⏳     15m    11247   2M 1U
     feat/ui                        -        -      1h     16503   no worktree
```

Shows all local branches with PR status, CI pass/fail, time since last commit, deterministic dev port per worktree, and working tree state. Branches not tracked by ez still appear and are labeled `not tracked`. `ez list --json` for machine output.

## Multi-Agent Workflow

```bash
# Agent 1 (terminal 1)
ez create feat/auth --from main
# ... works in .worktrees/feat-auth ...
ez push -am "feat: auth system"

# Agent 2 (terminal 2, same repo)
ez create feat/api --from main
# ... works in .worktrees/feat-api ...
ez push -am "feat: API routes"

# No conflicts. Each agent has its own worktree.
```

## Stacked PRs

```bash
# Build a stack of dependent changes
ez create feat/auth-types
ez commit -m "add auth types"

ez create feat/auth-api          # stacks on auth-types
ez commit -m "add auth API"

ez submit                        # pushes all, creates PRs with correct bases

# After the first PR merges:
ez sync                          # cleans up, restacks remaining branches
```

## Scope Guard

Keep an agent focused on the files a branch is supposed to touch:

```bash
ez create feat/auth --scope 'src/auth/**' --scope 'tests/auth/**'
ez scope show
ez scope add 'benches/auth/**'
ez scope set --mode strict 'src/auth/**' 'tests/auth/**'
```

With scope configured, `ez commit` and `ez push -am` check the staged file set before mutating git state. In `warn` mode they print drift and continue. In `strict` mode they stop.

## Worktree Hooks

Create `.ez/hooks/post-create/default.md` to give agents setup instructions:

```markdown
# Worktree Setup
1. `npm install`
2. `cp .env.example .env`
3. Start dev server on port $EZ_PORT
```

Hooks are markdown instructions, not scripts. ez prints them, the agent follows them.
Use `--hook <name>` for project-specific hooks, or `--hook` alone to list available hooks.

## All Commands

### Flagship

| Command | Description |
|---------|-------------|
| `ez create <name>` | Create worktree + branch (default). `--from main` for independent work. `--no-worktree` for branch only. |
| `ez list` | Dashboard for all local branches: PRs, CI, age, ports, and working tree state. `--json` for machine output. |
| `ez delete [name]` | Delete branch + worktree. Auto-detects worktrees and best-effort stops listeners on the branch dev port. `--yes` for agents. |
| `ez push` | Push + create/update PR. `-am "msg"` to stage+commit+push in one step. |

### Committing

| Command | Description |
|---------|-------------|
| `ez commit -m "msg"` | Commit the current staged set + restack children |
| `ez commit -am "msg"` | Stage tracked files + commit |
| `ez commit -m "msg" -- path1 path2` | Stage specific files + commit |
| `ez commit --if-changed` | No-op if nothing staged |
| `ez amend` | Amend last commit + restack |

Intended workflow:

- Focused commit: `ez commit -m "msg" -- path1 path2`
- Bulk update: `ez commit -am "msg"`
- Partial hunks: `git add -p` then `ez commit -m "msg"`

### Scope

| Command | Description |
|---------|-------------|
| `ez scope show` | Show the current branch's configured scope |
| `ez scope add <pattern...>` | Append patterns to the current branch's scope |
| `ez scope set <pattern...>` | Replace the current branch's scope |
| `ez scope clear` | Remove scope configuration from the current branch |

### Syncing

| Command | Description |
|---------|-------------|
| `ez sync` | Fetch trunk, clean merged branches, restack |
| `ez sync --autostash` | Stash before sync, restore after |
| `ez sync --dry-run` | Preview what sync would do |
| `ez restack` | Fetch trunk, refresh it locally, and rebase stale branches onto their latest parent tips |

### Navigation

| Command | Description |
|---------|-------------|
| `ez switch <name>` | Switch to branch (cd's to worktree if applicable) |
| `ez switch <pr-number>` | Switch by PR number |
| `ez up` / `ez down` | Navigate the stack |
| `ez top` / `ez bottom` | Jump to stack endpoints |

### Inspection

| Command | Description |
|---------|-------------|
| `ez log` | Visual stack tree with PR status |
| `ez log --json` | Stack as JSON |
| `ez status` | Branch info + working tree state |
| `ez status --json` | Branch info as JSON |
| `ez diff` | Diff vs parent (what the PR reviewer sees) |
| `ez diff --stat` | Diffstat summary |
| `ez diff --name-only` | Changed file names |
| `ez parent` | Print parent branch name to stdout |

### PRs

| Command | Description |
|---------|-------------|
| `ez submit` | Push entire stack + create/update all PRs |
| `ez pr-link` | Print PR URL to stdout |
| `ez pr-edit --title "..." --body "..."` | Edit PR metadata |
| `ez draft` / `ez ready` | Toggle PR draft status |
| `ez merge` | Merge bottom PR via GitHub |

### Setup

| Command | Description |
|---------|-------------|
| `ez setup --yes` | Configure shell integration |
| `ez skill install` | Install the ez-workflow skill for AI agents |
| `ez update` | Update to latest version |

## Agent Integration

Install the skill so agents auto-discover ez:

```bash
ez skill install
```

This writes the ez-workflow skill to `.claude/skills/ez-workflow/SKILL.md`. Agents using Claude Code (or any tool that reads `.claude/skills/`) will automatically use ez for all git operations.

See [SKILL.md](./SKILL.md) for the full agent workflow, and [reference.md](./reference.md) for the complete command reference.

## How It Works

- **Worktrees** give each agent an isolated copy of the repo with its own branch
- **Stack metadata** in `.git/ez/stack.json` tracks branch parents and PR numbers
- **Auto-restacking** via `git rebase --onto` keeps children up to date when parents change
- **Mutation receipts** (JSON on stderr) let agents verify every operation
- **Progressive help** — `ez`, `ez <cmd>`, `ez <cmd> --help` each give more detail

## Prerequisites

- **git** 2.38+
- **gh** (GitHub CLI), authenticated via `gh auth login`
- **Python 3.8+** (for `pip install`) or download binaries from [Releases](https://github.com/rohoswagger/ez-stack/releases)

## License

MIT. See [LICENSE](LICENSE) for details.
