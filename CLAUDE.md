# ez-stack — Project CLAUDE.md

> **This is a living document.** Update it whenever the product mission evolves, a significant design decision is made, a pattern is established, or a new version ships. It is the source of truth for anyone (human or AI) working on this codebase.

---

## Product Mission

**ez-stack makes stacked PRs on GitHub effortless, primarily for AI coding agents.**

Agents are the primary audience. Humans benefit too, but when there's a design tradeoff, optimize for agents. Every command should be simpler, more intuitive, and more efficient than the raw git/gh equivalent — the goal is to make version control easier for agents, not to expose git's complexity through a different interface.

Every feature should be inherently more useful than the git commands it replaces.

---

## Core Design Principles

1. **Shell out, don't reimplement.** `ez` orchestrates `git` and `gh`. It doesn't reimplement git logic. When in doubt, run the real command.

2. **Auto-restack on every mutation.** Any command that moves a branch tip (`commit`, `amend`, `move`) automatically restacks children. Users should never have to think about rebasing.

3. **State is append-only metadata.** Stack state lives in `.git/ez/stack.json` — just branch names, parents, parent SHAs, and PR numbers. It is never the source of truth for git state; git is.

4. **Fail loudly with hints.** Every error message tells the user what went wrong AND what to do next. No silent failures.

5. **Human output on stderr, machine output on stdout.** Colored tree output and spinners go to stderr. JSON output (`--json`), URLs (`ez pr-link`), and anything meant to be piped goes to stdout.

6. **Structured exit codes.** Agents branch on failure type. Exit codes are documented and stable:
   - 0 = success
   - 1 = unexpected error
   - 2 = GitHub API / `gh` CLI error
   - 3 = rebase conflict (manual resolution required)
   - 4 = stale remote ref
   - 5 = usage error (on trunk, branch not tracked, etc.)
   - 6 = unstaged changes

7. **Progressive help discovery.** The CLI is the agent's documentation. Three levels:
   - Level 0: `ez` (no args) → full command list with one-line descriptions (exit 0)
   - Level 1: `ez worktree` (no subcommand) → subcommand list (exit 0)
   - Level 2: `ez create --help` → full parameter details
   Discovery commands always exit 0. Agents drill down on-demand instead of loading all docs upfront.

8. **Errors are navigation.** Every error message contains both "what went wrong" AND "what to do instead." Agents can't Google — the error itself must point to the fix. One-step correction, not blind guessing.

9. **Consistent output metadata.** Every command appends `[ok | 45ms]` or `[exit:3 | 120ms]` to stderr. Agents learn command cost over time and can branch on exit status without parsing. This is always on — no `--agent` flag.

10. **Show work by default.** `ez commit` prints the diff stat after committing. Agents need to verify what happened without running a separate command. Default to more information, not less.

---

## Architecture

```
src/
  main.rs          — CLI entry point, dispatch, structured exit codes
  cli.rs           — clap 4 derive definitions (all commands + flags)
  error.rs         — EzError enum (thiserror)
  stack.rs         — StackState: load/save .git/ez/stack.json, all stack queries
  git.rs           — git shell helpers (run_git, run_git_with_status)
  github.rs        — gh shell helpers (run_gh, PrInfo)
  ui.rs            — terminal output helpers (all to stderr)
  stack_body.rs    — pure module: build PR bodies with ancestor stack section
  cmd/
    init.rs        — ez init
    create.rs      — ez create
    commit.rs      — ez commit
    amend.rs       — ez amend
    push.rs        — ez push (+ shared push_or_update_pr)
    submit.rs      — ez submit
    sync.rs        — ez sync
    restack.rs     — ez restack
    navigate.rs    — ez up/down/top/bottom
    checkout.rs    — ez checkout
    list.rs        — ez list (replaces ez branch)
    log.rs         — ez log
    status.rs      — ez status
    delete.rs      — ez delete
    move_branch.rs — ez move
    merge.rs       — ez merge
    pr_edit.rs     — ez pr-edit
    draft.rs       — ez draft / ez ready
    pr_link.rs     — ez pr-link
```

**Key invariant:** `cmd/` files contain business logic. `git.rs` and `github.rs` are pure I/O wrappers. `stack.rs` is pure data — no I/O except load/save.

---

## Agent-Friendly Features (v0.1.5+)

These features exist specifically to make ez useable by AI agents:

| Feature | Flag/Command | Why |
|---------|-------------|-----|
| Machine-readable output | `ez status --json`, `ez log --json` | Parse stack state without scraping colored text |
| Non-interactive checkout | `ez checkout <name>` or `ez checkout <pr-number>` | No TTY required |
| Autostash on sync | `ez sync --autostash` | Don't fail when there are dirty files |
| Conditional commit | `ez commit --if-changed` | No-op exit 0 when nothing staged |
| Create from base | `ez create --from <branch>` | Create branch without checking it out |
| PR URL to stdout | `ez pr-link` | Pipeable: `open $(ez pr-link)` |
| Structured exit codes | (all commands) | Agent can branch on failure type |

---

## Versioning and Release

- Crate: `ez-stack` on crates.io
- Binary: `ez`
- Version: set in `Cargo.toml`, tagged as `v<semver>`
- Release process: push tag `v*` → GitHub Actions builds binaries + `cargo publish`
- Publish requires `CARGO_REGISTRY_TOKEN` repository secret
- **NEVER reuse a version tag.** crates.io permanently rejects republished versions. If you need to add changes after a tag was pushed, bump to the next version. If a tag was accidentally deleted, restore it on the original commit — don't retag a different commit with the same version.

### Version History

| Version | Key changes |
|---------|-------------|
| 0.1.1 | Initial release |
| 0.1.2 | Bug fixes |
| 0.1.3 | `--title`/`--body` on push/submit, `ez pr-edit`, `ez sync --dry-run`, autofetch before push, StaleRemoteRef error |
| 0.1.4 | Stack links in PR bodies, `ez push --stack`, `filter_map` in stack_body |
| 0.1.5 | `--autostash`, `--json`, non-interactive checkout, `--from`, `--if-changed`, `ez draft`/`ez ready`, `ez pr-link`, structured exit codes, SKILL.md |
| 0.1.6 | Worktree support: `meta_dir()` uses `--git-common-dir`, `ez sync` worktree-safe, restack/commit/amend skip branches in other worktrees, `ez log` shows `[wt: <dirname>]` |
| 0.1.7 | `ez sync` prunes merged worktrees; fix trunk fast-forward when current branch is trunk |
| 0.1.8 | Fix `ez push` clobbering manual `gh pr edit --base` changes; only update PR base if stack parent is a git ancestor of the branch |
| 0.1.9 | Fix `ez sync` non-fast-forward trunk warning (skip update when local trunk is equal/ahead/diverged); auto-clean stack entries for branches deleted outside ez |
| 0.1.10 | `ez worktree create/delete/list`; `ez sync --force` to force-remove worktrees with uncommitted changes |
| 0.1.11 | Agent CLI UX: `ez diff`/`ez diff --stat`/`ez diff --name-only`, `ez parent`, `ez commit -m "a" -m "b"` multi-line, `ez commit -- <paths>` path-scoped staging, diff stat after commit/amend, timing metadata on all output (`[ok \| 45ms]`), progressive help discovery (bare commands exit 0), actionable error hints on all errors, worktree filter fix (only `.worktrees/`) |
| 0.1.12 | `ez update` — self-update command, auto-detects install method (cargo vs script), `--check` for version check without install, `--version` for specific version |
| 0.1.13 | Examples in every `--help`, commit SHA in output, `ez worktree delete --yes` from inside worktree, `worktree_path` fix for linked worktrees |
| 0.1.14 | Phased worktree create/delete: validate→mutate→output with rollback on failure; chdir before worktree remove; prune stale entries; recover from broken worktree state |
| 0.1.15 | `ez branch` — list all branches with PR numbers and worktree paths |
| 0.1.16 | (skipped) |
| 0.1.17 | `ez shell-init` — shell integration for auto-cd on worktree create/delete (zoxide pattern); remove redundant sync summary |
| 0.1.18 | `ez setup` — one-command shell configuration (PATH + shell-init); first-run hint prompts users to run it; `ez setup --yes` for agents |
| 0.1.19 | `ez checkout` auto-cd's into worktree if branch is checked out there; shell wrapper intercepts checkout for cd |
| 0.1.20 | Auto-drop redundant commits during sync/restack via `git cherry` + `git rebase`; remove redundant restack summary |
| 0.1.21 | Fix `ez sync` not switching off a branch before deleting it (stayed on cleaned-up branch) |
| 0.1.22 | Mutation receipts: every mutating command (commit, amend, sync, restack, push, create) emits structured JSON receipt to stderr; `git::diff_stat_numbers()` helper; gh abstraction + scope-aware stacking documented as deferred |
| 0.1.23 | Fix `ez sync` not cleaning up merged branches in worktrees; add git-level merge detection (branch tip is ancestor of trunk) for branches without PR numbers |
| 0.1.24 | Agent audit fixes: receipts on delete/move/merge/submit/worktree; fix delete.rs state corruption (git delete before state removal); worktree guard on delete; exit code 5 for all usage errors; branch/worktree list to stdout; amend hint fix |
| 0.1.25 | `ez skill install` — bundles SKILL.md into the binary, installs to `.claude/skills/ez-workflow/SKILL.md` in the current repo. Agents in the repo auto-discover the skill. |
| 0.1.26 | Fix `ez branch` to show trunk, all managed branches, and current branch even if untracked; hint when current branch was created outside ez |
| 0.1.27 | Fix `-a` flag to use `git add -u` (tracked only, not untracked); hook failure detection (shows which files pre-commit hooks modified); push error messages wrapped with context; `ez status` shows working tree (staged/modified/untracked counts); SKILL.md discoverability improvements |
| 0.2.0 | Flagship command redesign: `ez create` defaults to worktree (`--no-worktree` for old behavior); `ez list` replaces `ez branch` (adds `--json`, working tree state, worktree paths); `ez delete` auto-detects and removes worktrees; `ez push -am "msg"` for stage+commit+push; `ez worktree create/delete/list` become aliases |
| 0.2.1 | Declarative hooks: `.ez/hooks/<event>/<name>.md` are markdown instructions printed to agents (not executable scripts). `--hook <name>` selects a specific hook. Events: post-create, pre-push, post-sync, post-delete. Multiple named hooks per event. |
| 0.2.2 | Version bump (v0.2.1 already published) |
| 0.2.3 | Progressive hook discovery: `--hook` with no value lists available hooks. Agent flow: `--help` → sees `--hook`, tries `--hook` → gets list, picks one → gets instructions. |
| 0.2.4 | Fix squash-merge detection in `ez sync`: adds diff-level check (empty diff against trunk) so squash-merged branches are cleaned up even when `is_ancestor` fails |

---

## CI Requirements

Every commit must pass:
- `cargo fmt --all -- --check` (formatter, enforced by CI + pre-commit hook)
- `cargo clippy -- -D warnings` (zero warnings)
- `cargo test` (all tests pass)

**Pre-commit hook** at `.git/hooks/pre-commit` runs `cargo fmt --all -- --check` automatically.

---

## Testing Philosophy

- Unit tests live in `#[cfg(test)] mod tests { ... }` inside the source file they test
- Pure logic functions (JSON serialization, error mapping, exit codes, stack queries) get direct unit tests
- Commands that shell out to `git`/`gh` are not unit tested at the command level — the wrappers (`git.rs`, `github.rs`) are also hard to unit test without a real git repo
- Test the data transformations, not the I/O
- `stack_body.rs` is the model for pure, fully testable modules

---

## Known Deferred Features

These have been discussed and intentionally deferred:

- **`ez rename`** — GitHub ties PRs to branch names; renaming requires deleting the old remote branch which may close the PR. Complex to do correctly.
- **`ez split`** — Multi-commit splitting is high-complexity, high-risk.
- **`ez absorb`** — Requires semantic commit analysis.
- **`ez land`** — `ez merge` + `ez sync` already covers this flow.
- **CI status in `ez log`** — Requires `gh run list` per branch (slow, extra API calls). Separate feature.
- **Interactive reorder** — Complex TUI.
- **`ez co <pr-number>`** — Superseded by `ez checkout <pr-number>` (v0.1.5).
- **Remove `gh` dependency** — All `gh` usage in `github.rs` could be replaced with direct GitHub API calls via `reqwest` + token. Benefits: smaller binary, works in environments without `gh`, enables GitLab/Bitbucket support. Costs: ~500 lines of HTTP client code, auth management. Keep `gh` for now (auth handling is worth it); abstract when multi-platform support is needed.
- **Scope-aware stacking** — `ez create --scope "src/auth/**"` stores intent metadata per branch. `ez commit` warns when staged files are outside scope. Foundation (mutation receipts) shipped in v0.1.22; scope routing is the next layer.
- **Python wheel distribution** — `pip install ez-stack` for the Python-heavy AI agent ecosystem. Requires building a wheel that bundles the Rust binary.

---

## Working In This Codebase

- Use `ez` (not raw git) for all branching and PR operations — see SKILL.md
- Rust edition 2024 — `let-else` and `let-chains` are available
- All human-facing output to stderr; machine output to stdout
- `cargo fmt --all` before every commit, no exceptions
