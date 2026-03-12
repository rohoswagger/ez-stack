---
name: ez-workflow
description: Use when about to run git branching, commit, push, or PR operations in a repo managed by ez-stack. Provides the complete command reference and agent-specific usage patterns.
---

# ez-stack Workflow Skill

ez-stack is a CLI for managing stacked PRs on GitHub. When `.git/ez/stack.json` exists in the repo, ALL git branching and PR operations MUST go through `ez`.

## Hard Rules

**NEVER use these raw commands when ez is initialized:**

| Raw command | Use instead |
|-------------|------------|
| `git checkout -b <name>` | `ez create <name>` |
| `git commit -m "..."` | `ez commit -m "..."` (auto-restacks children) |
| `gh pr create` | `ez push` (PR may already exist) |
| `git push` / `git push --force-with-lease` | `ez push` or `ez submit` |

## Command Reference

### Branching

| Intent | Command |
|--------|---------|
| Create stacked branch | `ez create <name>` |
| Create and commit | `ez create <name> -m "msg"` |
| Create and stage+commit | `ez create <name> -am "msg"` |
| Create from specific base (no checkout) | `ez create <name> --from <base>` or `--on <base>` |
| Switch to branch | `ez checkout <name>` |
| Switch by PR number | `ez checkout 42` |
| Navigate stack | `ez up` / `ez down` / `ez top` / `ez bottom` |
| Delete branch | `ez delete [branch]` |
| Move branch to new parent | `ez move --onto <branch>` |

### Committing

| Intent | Command |
|--------|---------|
| Commit (restacks children) | `ez commit -m "msg"` |
| Stage all + commit | `ez commit -am "msg"` |
| No-op if nothing staged | `ez commit -m "msg" --if-changed` |
| Amend last commit | `ez amend` |
| Amend with new message | `ez amend -m "new msg"` |

### Pushing and PRs

| Intent | Command |
|--------|---------|
| Push current branch + create/update PR | `ez push` |
| Push with title/body | `ez push --title "..." --body "..."` |
| Push entire stack | `ez submit` (or `ez push --stack`) |
| Open PR in browser | `ez pr` |
| Print PR URL to stdout | `ez pr-link` |
| Edit PR in $EDITOR | `ez pr-edit` |
| Edit PR title/body | `ez pr-edit --title "..." --body "..."` |
| Mark PR as draft | `ez draft` |
| Mark PR as ready | `ez ready` |

### Syncing and Inspecting

| Intent | Command |
|--------|---------|
| Sync with trunk | `ez sync` |
| Sync with dirty working tree | `ez sync --autostash` |
| Sync and force-remove worktrees with uncommitted changes | `ez sync --force` |
| Preview sync | `ez sync --dry-run` |
| Restack after parent changes | `ez restack` |
| Show stack tree (with CI status) | `ez log` |
| Show stack tree as JSON | `ez log --json` |
| Show current branch info | `ez status` |
| Show status as JSON | `ez status --json` |

### Worktrees

| Intent | Command |
|--------|---------|
| Create stacked branch + worktree at `.worktrees/<name>` | `ez worktree create <name>` |
| Create from specific base | `ez worktree create <name> --from <base>` |
| Delete worktree and clean up branch | `ez worktree delete <name>` |
| Force-delete worktree (discard uncommitted changes) | `ez worktree delete <name> --force` |
| List all worktrees (name, branch, path) | `ez worktree list` |

## Agent-Specific Patterns

### Parse stack state

```bash
# Current branch as JSON
ez status --json
# → {"branch":"feat/x","parent":"main","pr_number":42,"children":[],"needs_restack":false,...}

# Full stack as JSON array
ez log --json
# → [{"branch":"feat/a","depth":1,"pr_number":40,...},...]
```

### Conditional commits

```bash
# Only commit if there are staged changes (exits 0 cleanly if nothing staged)
ez commit -m "chore: format" --if-changed
```

### Sync without stash dance

```bash
# Instead of: git stash && ez sync && git stash pop
ez sync --autostash
```

### Scriptable checkout

```bash
# Direct by name (no TUI prompt)
ez checkout feat/my-branch
# Direct by PR number
ez checkout 42
```

### Open PR in browser

```bash
ez pr
# Or get just the URL for scripting:
open $(ez pr-link)
```

## Exit Codes

| Code | Meaning | Agent action |
|------|---------|-------------|
| 0 | Success | Continue |
| 1 | Unexpected error | Log and stop |
| 2 | GitHub API error | Check `gh auth status`, retry |
| 3 | Rebase conflict | Resolve conflicts, run `ez restack` |
| 4 | Stale remote ref | Run `git fetch origin <branch>`, retry push |
| 5 | Usage error (on trunk, not tracked, etc.) | Check branch state |
| 6 | Unstaged changes | Use `--autostash` or `--if-changed` |

## Typical Agent Workflow

```bash
# 1. Create a feature branch from a specific base
ez create feat/my-feature --from main

# 2. Make changes, commit (auto-restacks children)
ez commit -am "feat: implement my feature"

# 3. Push and create PR
ez push --title "feat: my feature"

# 4. After trunk moves, sync without losing work
ez sync --autostash

# 5. After PR merges, clean up
ez sync

# 6. Check stack state programmatically
ez log --json | jq '.[] | select(.needs_restack)'
```
