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

## The Worktree Moat

Most stacked-PR tools treat worktrees as an edge case. ez treats the stack as a
persistent workspace fleet:

- Every PR layer can keep its own checked-out worktree, dependencies, editor,
  agent, and deterministic dev port.
- `ez worktree ensure` materializes the fleet, while `ez worktree exec` runs
  setup, tests, or builds across it in parent-first order.
- History mutations are worktree-native. `ez restack`, `ez move`, `ez commit`,
  and `ez amend` rebase descendants inside the worktrees that own them instead
  of detaching them or moving refs behind their indexes.
- Fleet mutations verify that each worktree still owns the expected branch.
  Dirty edits are never silently autostashed, even when the user's Git config
  enables `rebase.autoStash`; the affected layer stays attached and retryable.
- Conflicts are isolated to their owning layer and cleaned up without blocking
  independent siblings from converging.
- Workspace teardown is commit-like: `ez delete` removes the worktree before
  stopping verified worktree-owned dev processes, so a failed delete leaves
  the live environment, dirty edits, branch, and stack metadata intact.
  The claimed worktree is atomically quarantined before its lock is released,
  listener process identity is revalidated before signaling, branch-ref failure
  restores the worktree, and stale directories are never recursively deleted.

That makes a stack more than a list of PRs: it is a set of durable, independently
usable development environments that ez can operate as one unit.

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

ez submit                        # atomically pushes all, creates PRs with correct bases

# After the first PR merges:
ez sync                          # cleans up, restacks remaining branches
```

`ez submit` pushes every branch in one atomic `--force-with-lease` operation before creating or updating PRs. When GitHub native stacked PRs are enabled, it links 2+ PRs into a native stack; if that preview API is unavailable, the ordinary base-chained PRs still succeed.

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
| `ez adopt --pr <number>` | Materialize a PR chain or native GitHub stack locally, with one worktree per active layer. `--no-worktrees` for metadata only. |
| `ez adopt <branch>...` | Adopt an explicit bottom-to-top branch chain without requiring PRs or GitHub auth. Remote-only branches are fetched and materialized into worktrees by default. |
| `ez worktree ensure [branch...]` | Materialize missing worktrees for the whole managed stack, or selected layers. Reuses existing checkouts wherever they live. |
| `ez worktree exec [branch...] -- <command>` | Materialize selected layers and run one command in every worktree, parent-first. `--keep-going` and `--json` support test matrices and agents. |
| `ez list` | Dashboard for all local branches: PRs, CI, age, ports, and working tree state. `--json` for machine output. |
| `ez delete [name]` | Delete branch + worktree. Auto-detects worktrees and best-effort stops listeners on the branch dev port. `--yes` for agents. |
| `ez fold [branch] --yes` | Locally fold one PR-less stack layer into its parent without rewriting commits. Removes the folded local worktree/branch, reparents children, and preserves remotes. |
| `ez push` | Push + create/update PR. `-am "msg"` to stage+commit+push in one step. `--no-pr` skips PR updates, `--pr` overrides `no_pr` config. |

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
| `ez submit` | Atomically push entire stack, create/update all PRs, and link native GitHub stacks when available |
| `ez pr-link` | Print PR URL to stdout |
| `ez pr-edit --title "..." --body "..."` | Edit PR metadata |
| `ez draft` / `ez ready` | Toggle PR draft status |
| `ez merge` | Merge bottom PR via GitHub |
| `ez merge --yes` | Merge non-interactively for agents/scripts |
| `ez merge --stack --yes` | Atomically merge a native GitHub stack; fall back to bottom-to-top for ordinary PR chains |

Merges use GitHub's asynchronous merge API when available, including native
stack and merge-queue support, with a legacy fallback for repositories where
that API is unavailable. For an exact native-stack match, `--stack` sends one
request for the top PR and reconciles the whole local worktree fleet. A
successfully merged branch has its clean linked worktree removed; a queued
branch keeps its worktree, local branch, remote branch, and stack metadata until
GitHub actually merges it.

### Adopt a remote stack into worktrees

```bash
ez adopt --pr 42                 # native stack when available; PR base chain otherwise
ez adopt --pr 42 --no-worktrees  # reconstruct stack metadata without provisioning worktrees
ez adopt feat/base               # adopt a local or remote branch without requiring a PR
ez adopt feat/base feat/child    # adopt an explicit bottom-to-top branch chain
ez adopt feat/base --no-worktrees # reconstruct metadata only
```

Native stack order from GitHub is authoritative when it is available. For
PR-less explicit branch adoption, the positional order is authoritative
bottom-to-top. Ez fetches each active PR or requested branch, reconstructs the
local parent graph, and provisions an isolated worktree for every layer by
default. Remote-only branches are materialized locally. If an existing local
branch is behind or diverged from its remote, adoption stops before changing
branches or worktrees. Explicit branch-only adoption does not need GitHub CLI
auth because it works from git refs instead of PR metadata.

### Materialize a worktree fleet

```bash
ez worktree ensure                  # every managed non-trunk layer
ez worktree ensure feat/api feat/ui # selected layers, parent-first
ez worktree ensure --dry-run --json # deterministic plan for agents
```

`ez worktree ensure` turns an existing stack into an isolated workspace fleet
without moving or deleting any checkout. Existing canonical, external, and main
worktrees are reused—even when dirty—and their staged, modified, and untracked
counts are reported. Missing worktrees are created at their canonical
`.worktrees/<branch>` paths. Before changing anything, ez validates every
selected branch and destination, including path collisions caused by sanitized
branch names. If a later creation fails, worktrees created earlier in the same
invocation are rolled back. The command is local/offline and does not change
stack metadata, branches, remotes, or GitHub state.

### Execute across the workspace fleet

```bash
ez worktree exec -- cargo test
ez worktree exec feat/api feat/ui -- npm test
ez worktree exec --keep-going --json -- sh -lc 'make check'
```

`ez worktree exec` treats the stack as a runnable workspace fleet. It first
applies the same transactional materialization and reuse rules as
`ez worktree ensure`, then executes the argv directly in each selected
worktree in deterministic parent-first order. It stops on the first failure by
default; `--keep-going` attempts every layer. The process exits with the first
failing child exit code.

Human mode streams child output. JSON mode captures stdout, stderr, exit code,
duration, and status per branch without polluting stdout, including explicit
`skipped` entries after a fail-fast stop. Every child receives:

- `EZ_BRANCH`
- `EZ_WORKTREE`
- `EZ_PORT` (the branch's deterministic development port)
- `EZ_STACK_INDEX` (one-based)
- `EZ_STACK_SIZE`

Commands are not interpreted by a shell. Pass `sh -lc '<command>'` explicitly
when pipelines, redirects, globs, or other shell syntax are required.

### Claim worktrees for concurrent agents

```bash
ez worktree claim --owner codex-1              # current linked worktree, 4h
ez worktree claim feat/api --owner codex-2 --ttl 90m
ez worktree leases --json                      # fleet-wide ownership dashboard
ez worktree release feat/api --owner codex-2
```

Claims turn the workspace fleet into a coordination layer. Each lease is stored
in Git's native worktree lock reason, so Git, ez, shell scripts, and other
agents all observe one source of truth—there is no sidecar ownership database
to drift. `ez list --json` exposes the owner, creation time, expiry, and stale
status under `worktree_lock`; `ez worktree leases` shows both ez leases and
foreign Git locks.

Active leases block deletion, fold, merge cleanup, and sync cleanup even when
those commands use `--force`. An expired lease is reported as stale but is
never broken by a read or unrelated mutation. Take it over explicitly with
`ez worktree claim <branch> --owner <new-owner> --break-stale`, or release an ez
lease with `ez worktree release <branch> --force`. Ez never overwrites or
releases a foreign Git lock. Claim and release are local/offline and do not
change stack metadata, branch tips, remotes, or GitHub state.

### Tear down a worktree safely

`ez delete <branch> --yes` claims the exact branch/worktree pair, atomically
moves it to an invocation-unique quarantine path, removes the linked worktree
and local branch, then stops only deterministic-port listeners whose working
directory belonged to that worktree and whose process start identity still
matches. This prevents a replacement at the original path or a reused PID from
being destroyed. If removal fails, ez restores the original worktree path; if
local branch deletion fails, ez recreates the worktree. Dirty, leased, locked,
stale, or otherwise invalid worktrees leave processes, files, refs, and
metadata intact.

### Fold a local stack layer down

```bash
ez fold feat/child --yes
```

`ez fold` is the local/offline first step toward stack collapse workflows. It
folds exactly one PR-less layer into its direct parent by advancing the parent
branch to the folded branch tip, then removes the folded local branch and linked
worktree. Commit IDs are preserved; this is not a squash or range fold. Direct
children are reparented to the surviving parent, remote branches are left
untouched, and shell integration cd's to the parent worktree if the current
worktree was removed. The first release intentionally accepts only non-bottom,
PR-less layers whose parent and descendants form a clean, fully restacked
linear history. If any affected worktree is dirty or a descendant is stale,
`ez fold` aborts before changing refs, worktrees, or metadata.

### Setup

| Command | Description |
|---------|-------------|
| `ez init --yes` | Initialize ez and accept recommended non-interactive defaults |
| `ez setup --yes` | Configure shell integration |
| `ez config list/get/set` | View or update repo settings such as `default_from`, `draft`, `no_pr`, and `rerere` |
| `ez skill install` | Install the ez-workflow skill for AI agents |
| `ez update` | Update to latest version |

## Agent Integration

Install the skill so agents auto-discover ez:

```bash
ez skill install
```

This writes the canonical skill to `.agents/skills/ez-workflow/SKILL.md`, then creates compatibility symlinks for agent-specific skill roots such as `.claude/skills/ez-workflow` and `.codex/skills/ez-workflow`.

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
