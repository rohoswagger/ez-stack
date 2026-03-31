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
ez push -am "feat: add auth"     # 2. Ship: stage + commit + push + create PR
ez sync --autostash              # 3. Sync: pull trunk, clean merged, restack
ez delete feat/auth --yes        # 4. Done: remove worktree + branch + cd back
```

That's it for normal flows. No raw `git commit`, no raw `git push`, no `gh pr create`, no manual `cd`.
Use `git add -p` only when you need hunk-level selection before `ez commit`.

## Never use these directly

| Instead of | Use |
|------------|-----|
| `git checkout -b` | `ez create` |
| `git commit` | `ez commit` or `ez push -am` |
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
ez submit                          # pushes + creates PRs for entire stack
```

### Self-review before pushing
```bash
ez diff --stat       # what files changed vs parent
ez diff --name-only  # just file names
ez status            # stack info + working tree state
```

### Ship it
```bash
ez push -am "feat: done"               # stage + commit + push + create PR
ez push --title "feat: auth" --body "..." # with PR metadata
ez submit                                # push entire stack
```

### Sync with other agents' work
```bash
ez sync --autostash   # pulls trunk, cleans merged PRs, restacks your branches
```

### Finish
```bash
cd $(ez delete my-task --yes)   # removes worktree + branch, cd's to repo root
```

## Multi-Agent Rules

- **One worktree per agent.** Never share a worktree.
- **Always `--from main`** for independent tasks.
- **Sync before push** to pick up other agents' merged work.
- **Preferred commit flow:** `ez commit -m "msg" -- path1 path2`
- **Bulk update:** `ez commit -am "msg"`
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

See [reference.md](reference.md) for the full command reference: `ez commit`, `ez amend`, `ez diff`, `ez status`, `ez restack`, `ez log`, `ez move`, `ez merge`, `ez switch`, `ez pr-edit`, `ez draft`/`ez ready`, `ez pr-link`, `ez update`, `ez setup`, `ez skill install`.
