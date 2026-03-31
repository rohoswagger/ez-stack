# AGENTS.md

Instructions for AI agents working with `ez-stack`.

## Purpose

`ez` is a CLI for managing stacked pull requests on GitHub. If a repository has `.git/ez/stack.json`, branch management and PR operations should go through `ez`, not raw `git checkout -b`, `git commit`, `git push`, or `gh pr create`.

## Install or update the tool

```bash
# First install
pip install ez-stack
ez setup --yes    # configures shell integration (PATH + worktree auto-cd)

# Update to latest (auto-detects install method)
ez update

# Check for updates without installing
ez update --check
```

Requirements:

- `git`
- `gh`, authenticated via `gh auth login`
- Rust/Cargo available to install `ez-stack`

## Install the skill

Install the ez-workflow skill into the current repo so all agents auto-discover it:

```bash
ez skill install
# Installs to .claude/skills/ez-workflow/SKILL.md
# Commit the file so other agents in this repo use it too
```

This installs the workflow defined in [`SKILL.md`](./SKILL.md).

## Core rule

When `.git/ez/stack.json` exists, prefer these commands:

- Create branch: `ez create <name>`
- Commit specific files: `ez commit -m "msg" -- path1 path2`
- Commit all tracked changes: `ez commit -am "msg"`
- Commit patch-selected hunks: `git add -p` then `ez commit -m "msg"`
- Diff vs parent: `ez diff --stat` or `ez diff --name-only`
- Get parent branch: `ez parent`
- Push current branch and create/update PR: `ez push`
- Push the stack: `ez submit`
- Sync after trunk changes or merges: `ez sync` or `ez sync --autostash`
- Inspect state programmatically: `ez status --json` and `ez log --json`

Avoid these raw commands in an `ez`-managed repo:

- `git checkout -b ...` → `ez create`
- `git commit -m ...` → `ez commit -m`
- `git push` → `ez push`
- `gh pr create` → `ez push`
- `git diff main...HEAD` → `ez diff`

## Output format

Every command appends a status line to stderr: `[ok | 45ms]` or `[exit:3 | 120ms]`.
Use this to branch on exit status and learn command cost.

Discovery: `ez` (no args) lists all commands (exit 0). `ez <command> --help` shows full details.

## Good agent patterns

```bash
# Check whether the repo is managed by ez
test -f .git/ez/stack.json && echo "ez-managed"

# Create from a specific base branch
ez create feat/my-change --from main

# Preferred: stage specific files and commit in one step
ez commit -m "fix: update parser" -- src/parser.rs

# Bulk update: stage tracked files and commit in one step
ez commit -am "chore: update generated fixtures"

# Partial hunk selection stays in git
git add -p
ez commit -m "fix: keep only intended hunks"

# Multi-paragraph commit message
ez commit -m "feat: add parser" -m "Implements recursive descent parsing."

# Commit only if there are staged changes
ez commit -m "chore: update" --if-changed

# Self-review before push
ez diff --stat

# Get parent branch name for scripting
ez parent

# Sync safely with a dirty working tree
ez sync --autostash

# Read current stack state as JSON
ez status --json
ez log --json
```

## Exit codes

- `0`: success
- `1`: unexpected error
- `2`: GitHub / `gh` error
- `3`: rebase conflict
- `4`: stale remote ref
- `5`: usage error
- `6`: unstaged changes

## References

- [`README.md`](./README.md): user-facing overview and command reference
- [`SKILL.md`](./SKILL.md): full skill instructions for skill-compatible agents
- [`CLAUDE.md`](./CLAUDE.md): project context and design principles
