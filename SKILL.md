---
name: ez-workflow
description: Use when about to run git branching, commit, push, or PR operations in a repo managed by ez-stack. Provides the complete command reference and agent-specific usage patterns.
---

# ez-stack

ez-stack makes version control invisible for AI coding agents. Four commands cover the full development lifecycle. Stacked PRs, worktree isolation, and auto-restacking happen automatically.

**When to use ez:** If `.git/ez/stack.json` exists, ALL git operations go through `ez`.

## The 4 Commands

```bash
ez create feat/auth              # 1. Start: worktree + branch + cd
# ... do your work ...
ez push -am "feat: add auth"     # 2. Ship tracked changes
ez push -Am "feat: add auth"     #    or include untracked files too
ez sync --autostash              # 3. Sync: pull trunk, clean merged, restack
ez delete feat/auth --yes        # 4. Done: remove worktree + branch, stop branch dev server, + cd back
```

That's it for normal flows. No raw `git commit`, no raw `git push`, no `gh pr create`, no manual `cd`.
Use `git add -p` only when you need hunk-level selection before `ez commit`.

## Never use these directly

| Instead of | Use |
|------------|-----|
| `git checkout -b` | `ez create` |
| `git commit` | `ez commit`, `ez push -am`, or `ez push -Am` |
| `git push` | `ez push` |
| `gh pr create` | `ez push` |
| `git diff main...HEAD` | `ez diff` |
| `git branch` | `ez list` |

## Agent Startup

```bash
# 1. Check what's happening
ez list

# 2. Create your isolated workspace (always use --from main for independent work)
cd $(ez create my-task --from main)

# 3. You're in .worktrees/my-task with your own branch. Work here.
```

After any `ez create`, `ez switch`, `ez checkout`, `ez delete`, `ez fold`, or `ez sync` that may change directories, immediately re-anchor file operations to the active worktree root:

```bash
pwd
git rev-parse --show-toplevel
```

Use that path, not the main repo checkout, for every subsequent read or write. Never reuse an absolute file path captured before switching into a linked worktree.

**Always use `--from main`** for independent tasks. Without it, ez stacks on the current branch.

**Hooks:** If `.ez/hooks/post-create/default.md` exists in the repo, ez prints its instructions after worktree creation. Follow them to set up the worktree (install deps, copy env, etc.). Use `--hook <name>` for a specific hook: `ez create feat/auth --hook setup-node` reads `.ez/hooks/post-create/setup-node.md`.

Hooks are markdown instructions for agents, not executable scripts. ez prints them, you follow them.

## Working

### Commit specific files (keeps changes focused)
```bash
ez commit -m "feat: add types" -- src/types.rs src/mod.rs
```

### Bulk update when the whole tracked diff belongs together
```bash
ez commit -am "chore: regenerate fixtures"
```

### Bulk update including untracked files
```bash
ez commit -Am "feat: add new docs and generated fixtures"
```

### Partial hunks when one file mixes concerns
```bash
git add -p
ez commit -m "fix: keep intended hunks only"
```

### Stack changes (multiple PRs from one workflow)
```bash
ez create feat/auth-api            # stacks on current branch
ez commit -m "feat: add API"
ez create feat/auth-middleware     # stacks on auth-api
ez commit -m "feat: add middleware"
ez submit                          # atomically pushes + creates PRs for entire stack
```

`ez submit` pushes all stack branches first with one atomic `--force-with-lease` push. For 2+ PRs it also asks GitHub to register the PR chain as a native stack when the public-preview API is available; if unavailable, the ordinary base-chained PRs still succeed.

### Self-review before pushing
```bash
ez diff --stat       # what files changed vs parent
ez diff --name-only  # just file names
ez status            # stack info + working tree state
```

### Ship it
```bash
ez push -am "feat: done"               # stage tracked changes + commit + push + create PR
ez push -Am "feat: done"               # include untracked files too
ez push --title "feat: auth" --body "..." # with PR metadata
ez push --no-pr                        # push branch only
ez push --pr                           # create/update PR even if no_pr config is true
ez submit                                # atomically push entire stack
ez merge --yes                           # merge bottom PR non-interactively
ez merge --stack --yes                   # atomically land a native stack; sequential fallback
ez fold feat/child --yes                 # locally fold one PR-less layer into its parent
```

`ez merge` uses GitHub's asynchronous merge API when available. After a direct
merge, ez removes each merged branch's clean linked worktree and returns the
main worktree path for shell integration. For an exact native-stack match,
`--stack` makes one atomic request through the top PR and reconciles all local
worktrees together. If GitHub enqueues the merge, ez preserves the worktrees,
branches, and stack state until the queue finishes.

`ez fold [branch] --yes` is local/offline and applies only to one PR-less stack
layer at a time. It advances the parent to the folded branch tip without
rewriting commits, removes the folded local branch/worktree, reparents direct
children, and preserves remote branches. It is not a squash, range fold, or
GitHub PR mutation. Fold only a non-bottom, clean, fully restacked layer; the
command aborts without mutation when an affected worktree is dirty or a
descendant needs restacking.

### Sync with other agents' work
```bash
ez sync --autostash   # pulls trunk, cleans merged PRs, restacks your branches
```

### Adopt branches from another machine or collaborator
```bash
ez adopt              # adopt all open PRs rooted on trunk
ez adopt --pr 42      # adopt its native stack/PR chain + provision each worktree
ez adopt --pr 42 --no-worktrees  # reconstruct metadata only
ez worktree ensure    # provision missing worktrees for every managed layer
ez worktree ensure --dry-run --json  # inspect the deterministic fleet plan
ez worktree exec -- cargo test  # test every layer in its own worktree, parent-first
ez worktree exec feat/base feat/child --json -- npm test
ez adopt feat/base    # adopt a local or remote branch without requiring a PR
ez adopt feat/base feat/child  # adopt an explicit bottom-to-top branch chain
```

`ez adopt` prefers GitHub's native stack order, falls back to the ordinary PR
base graph, reconstructs `stack.json`, and provisions one worktree per active
layer. With explicit branch names, the positional order is authoritative
bottom-to-top and no PRs or GitHub auth are required. Remote-only branches are
fetched and materialized locally. Adoption aborts before mutation when existing
local metadata conflicts with a native stack or when a local branch is behind or
diverged from its remote. Use it to continue working on someone else's stack
from a fresh clone.

Use `ez worktree ensure [branch...]` when stack metadata already exists but one
or more layers do not have worktrees. It reuses canonical, external, and main
worktrees without touching dirty state, creates only missing canonical
worktrees, preflights all paths before mutation, and rolls back worktrees
created earlier in the invocation if a later add fails. It is local/offline and
does not mutate branches, remotes, GitHub, or stack metadata.

Use `ez worktree exec [branch...] -- <command> [args...]` to operate on the
stack as a workspace fleet. Missing selected worktrees are materialized first,
existing worktrees are reused even when dirty, and commands run sequentially
in parent-first order. Execution stops on the first failure unless
`--keep-going` is set. `--json` captures a per-branch status, exit code,
stdout, stderr, and duration; the overall process preserves the first failing
child exit code. Child commands receive `EZ_BRANCH`, `EZ_WORKTREE`, `EZ_PORT`,
`EZ_STACK_INDEX`, and `EZ_STACK_SIZE`. The argv is executed directly; use
`sh -lc` explicitly for shell syntax.

### Finish
```bash
cd $(ez delete my-task --yes)   # removes worktree + branch, stops the branch dev server, cd's to repo root
```

## Multi-Agent Rules

- **One worktree per agent.** Never share a worktree.
- **Always `--from main`** for independent tasks.
- **Sync before push** to pick up other agents' merged work.
- **Preferred commit flow:** `ez commit -m "msg" -- path1 path2`
- **Bulk tracked update:** `ez commit -am "msg"`
- **Bulk tracked + untracked update:** `ez commit -Am "msg"`
- **Partial hunks:** `git add -p` then `ez commit -m "msg"`

## Receipts

Every mutating command emits a JSON receipt to stderr. Parse these to verify operations:

```json
{"cmd":"create","branch":"feat/auth","parent":"main","worktree":".worktrees/feat-auth"}
{"cmd":"push","branch":"feat/auth","pr_number":42,"pr_url":"...","created":true}
{"cmd":"delete","branch":"feat/auth","worktree":".worktrees/feat-auth"}
```

Check `redundant_commits > 0` after sync/restack — means commits were auto-dropped.

## Exit Codes

| Code | Meaning | Action |
|------|---------|--------|
| 0 | Success | Continue |
| 1 | Unexpected error | Log and stop |
| 2 | GitHub API error | `gh auth status` |
| 3 | Rebase conflict | Resolve, `ez restack` |
| 4 | Stale remote ref | `git fetch`, retry |
| 5 | Usage error | `ez status` |
| 6 | Unstaged changes | `--autostash` or `--if-changed` |

## Advanced Commands

See [reference.md](reference.md) for the full command reference: `ez adopt`, `ez worktree ensure`, `ez worktree exec`, `ez commit`, `ez amend`, `ez diff`, `ez status`, `ez restack`, `ez log`, `ez move`, `ez fold`, `ez merge`, `ez switch`, `ez pr-edit`, `ez draft`/`ez ready`, `ez pr-link`, `ez config`, `ez update`, `ez setup`, `ez skill install`.
