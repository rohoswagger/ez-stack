# ez-stack Advanced Command Reference

This is the full command reference. For the core workflow, see [SKILL.md](SKILL.md).

## Committing

| Intent | Command |
|--------|---------|
| Commit current staged set (restacks children) | `ez commit -m "msg"` |
| Stage all + commit | `ez commit -am "msg"` |
| Multi-paragraph commit | `ez commit -m "subject" -m "body"` |
| Commit specific files only | `ez commit -m "msg" -- path1 path2` |
| No-op if nothing staged | `ez commit -m "msg" --if-changed` |
| Amend last commit | `ez amend` |
| Amend with new message | `ez amend -m "new msg"` |

Preferred workflow:

- Focused commit: `ez commit -m "msg" -- path1 path2`
- Bulk update: `ez commit -am "msg"`
- Partial hunks: `git add -p` then `ez commit -m "msg"`

## Scope Guard

| Intent | Command |
|--------|---------|
| Show current branch scope | `ez scope show` |
| Add patterns to scope | `ez scope add 'src/auth/**' 'tests/auth/**'` |
| Replace scope | `ez scope set --mode strict 'src/auth/**'` |
| Clear scope | `ez scope clear` |

## Diffing and Inspecting

| Intent | Command |
|--------|---------|
| Full diff vs parent | `ez diff` |
| Diffstat summary | `ez diff --stat` |
| Changed file names only | `ez diff --name-only` |
| Parent branch name | `ez parent` |
| Current branch info | `ez status` |
| Current branch info (JSON) | `ez status --json` |
| Stack tree with PR status | `ez log` |
| Stack tree as JSON | `ez log --json` |

## Navigation

| Intent | Command |
|--------|---------|
| Switch to branch | `ez switch <name>` |
| Switch by PR number | `ez switch 42` |
| Move up/down in stack | `ez up` / `ez down` / `ez top` / `ez bottom` |

## PR Management

| Intent | Command |
|--------|---------|
| Push current branch and create/update PR | `ez push` |
| Push without creating/updating PR | `ez push --no-pr` |
| Force PR creation when `no_pr` config is true | `ez push --pr` |
| Print PR URL to stdout | `ez pr-link` |
| Edit PR title/body | `ez pr-edit --title "..." --body "..."` |
| Mark PR as draft / ready | `ez draft` / `ez ready` |
| Merge bottom PR | `ez merge` |
| Merge non-interactively | `ez merge --yes` |
| Merge current linear stack bottom-to-top | `ez merge --stack --yes` |

## Syncing

| Intent | Command |
|--------|---------|
| Sync with trunk | `ez sync` |
| Sync with dirty working tree | `ez sync --autostash` |
| Preview sync | `ez sync --dry-run` |
| Restack children | `ez restack` |

## Stack Operations

| Intent | Command |
|--------|---------|
| Move branch to new parent | `ez move --onto <branch>` |
| Fold one clean PR-less layer into its parent | `ez fold [branch] --yes` |
| Ensure every managed layer has a worktree | `ez worktree ensure` |
| Ensure selected layers have worktrees | `ez worktree ensure <branch...>` |
| Preview the deterministic fleet plan | `ez worktree ensure --dry-run --json` |
| Run a command in every managed worktree | `ez worktree exec -- <command> [args...]` |
| Run in selected layers, parent-first | `ez worktree exec <branch...> -- <command> [args...]` |
| Attempt all layers and report each result | `ez worktree exec --keep-going --json -- <command> [args...]` |
| Claim the current linked worktree for an agent | `ez worktree claim --owner <identity>` |
| Claim a layer for a custom duration | `ez worktree claim <branch> --owner <identity> --ttl 2h` |
| Inspect all lease and foreign-lock state | `ez worktree leases --json` |
| Release your lease | `ez worktree release [branch] --owner <identity>` |
| Explicitly take over an expired ez lease | `ez worktree claim <branch> --owner <identity> --break-stale` |
| Push entire stack | `ez submit` |

`restack`, `move`, `commit`, and `amend` rebase checked-out descendants in
their owning worktrees. ez verifies branch ownership and disables inherited
`rebase.autoStash`; dirty edits are preserved and reported instead of being
silently stashed during a fleet mutation.

Worktree claims are local/offline leases stored directly in Git's worktree lock
reason. The default TTL is four hours; accepted suffixes are `s`, `m`, `h`, and
`d`. Active and stale leases appear in `ez worktree leases` and under
`worktree_lock` in `ez list --json`. Stale leases are never broken implicitly,
and foreign Git locks are never overwritten or released by ez. Protected
worktrees block delete, fold, merge cleanup, and sync cleanup, including forced
cleanup.

`delete` claims the exact branch/worktree pair and atomically quarantines its
path before releasing the ownership lock. It removes the worktree and local
branch, then stops deterministic-port processes only when both their cwd and
start identity match the captured worktree process. Removal failure restores
the original path; branch deletion failure recreates the worktree. Stale
registered paths are never recursively deleted.

## Setup and Maintenance

| Intent | Command |
|--------|---------|
| Initialize ez non-interactively | `ez init --yes` |
| Install skill in repo | `ez skill install` |
| Shell integration | `ez setup --yes` |
| List repo config | `ez config list` |
| Read repo config | `ez config get default_from` |
| Update repo config | `ez config set draft true` |
| Update ez | `ez update` |
| Check for updates | `ez update --check` |

## Mutation Receipts

Every mutating command emits JSON to stderr:

| After | Key fields |
|-------|-----------|
| commit/amend | `files_changed`, `insertions`, `deletions`, `before`, `after` |
| sync (restack) | `redundant_commits`, `before`, `after` |
| sync (clean) | `action: "cleaned"`, `reason: "merged"` |
| push | `pr_number`, `pr_url`, `created` |
| create | `branch`, `parent`, `worktree` |
| worktree ensure | `dry_run`, `entries`, `created_count`, `reused_count`, `would_create_count` |
| worktree exec | `command`, `keep_going`, `attempted_count`, `succeeded_count`, `failed_count`, `skipped_count`, `stopped_early` |
| worktree claim | `branch`, `path`, `claimed`, `lease` |
| worktree release | `branch`, `path`, `released` |
| worktree leases | `entries`, `active_count`, `stale_count`, `foreign_lock_count` |
| delete | `branch`, `worktree`, `dev_port`, `killed_pids`, `reparented_children` |
| fold | `branch`, `into`, `before_parent`, `after_parent`, `removed_worktree`, `reparented_children`, `remote_preserved` |
| rebase conflict | `action: "conflict"`, `branch`, `parent`, `conflicting_files`, `git_stderr`, `next_command` |

Commit and push receipts also include scope fields when relevant:
- `scope_defined`
- `scope_mode`
- `out_of_scope_count`
- `out_of_scope_files`

Parse with: `echo "$OUTPUT" | grep '^{' | tail -1`
