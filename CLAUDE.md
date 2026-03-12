# ez-stack — Project CLAUDE.md

> **This is a living document.** Update it whenever the product mission evolves, a significant design decision is made, a pattern is established, or a new version ships. It is the source of truth for anyone (human or AI) working on this codebase.

---

## Product Mission

**ez-stack makes stacked PRs on GitHub effortless for both humans and AI agents.**

The two audiences are equally important:
- **Humans** need intuitive commands, good error messages, and a clear mental model
- **AI agents** need machine-readable output, non-interactive flags, structured exit codes, and predictable behavior

Every feature should pass both tests: "Would a developer reach for this naturally?" and "Can an agent use this programmatically?"

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

---

## Working In This Codebase

- Use `ez` (not raw git) for all branching and PR operations — see SKILL.md
- Rust edition 2024 — `let-else` and `let-chains` are available
- All human-facing output to stderr; machine output to stdout
- `cargo fmt --all` before every commit, no exceptions
