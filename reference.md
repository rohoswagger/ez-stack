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
| Inspect current branch native stack alignment | `ez status --native-stack` |
| Inspect current branch native stack alignment (JSON) | `ez status --json --native-stack` |
| Stack tree with PR status | `ez log` |
| Stack tree as JSON | `ez log --json` |
| Inspect stack tree native stack alignment | `ez log --native-stack` |
| Inspect stack tree native stack alignment (JSON) | `ez log --json --native-stack` |

`--native-stack` is opt-in. Without it, `status`/`log` human output and JSON
schema are unchanged. With it, ez compares the authoritative local
worktree/PR topology to GitHub REST native stack metadata using
`X-GitHub-Api-Version: 2026-03-10`. The inspection is read-only: it never
mutates stack metadata, refs, remotes, worktrees, or cached GitHub state. For
`log`, ez makes one stack API request per contiguous local PR segment.

Native stack states:

| State | Meaning |
|-------|---------|
| `in_sync` | Local ordered PR segment matches the GitHub native stack. |
| `diverged` | GitHub returned a stack, but its ordered PRs do not match local topology. |
| `not_linked` | Local PR segment is not currently linked as a GitHub native stack. |
| `unavailable` | GitHub returned public-preview 404 for the stack endpoint. |
| `unrepresentable` | Local graph is branching or invalid for a linear native stack. |
| `not_applicable` | The current branch/entry has no applicable local PR segment, or the stack crosses repositories and cannot use GitHub's native stack API. |
| `error` | GitHub inspection failed for another reason. |

JSON adds a `native_stack` object only when `--native-stack` is supplied. It
contains `provider`, `preview`, `state`, `local.branches`, and ordered
`local.pull_requests`. When GitHub returns a stack, `github` contains `number`,
`base_ref`, `open`, 1-based `position`, `size`, and ordered `pull_requests`.
The local graph remains authoritative; GitHub native stacks are only the
collaboration and merge layer, so divergence is reported rather than flattened
or cached.

## Navigation

| Intent | Command |
|--------|---------|
| Switch to branch through shell integration | `ez switch <name>` |
| Switch by PR number through shell integration | `ez switch 42` |
| Print target worktree path for scripts/agents | `ez switch <name> --no-cd-required` |
| Move up/down in stack | `ez up` / `ez down` / `ez top` / `ez bottom` |

`ez switch` can hand off to a linked worktree only when invoked through installed
shell integration (`EZ_SHELL_INTEGRATION=1`). A direct binary invocation that
would require that path handoff exits 5 before mutation and prints a manual
`cd`/setup hint. Automation that intentionally consumes the path must pass
`--no-cd-required`, read stdout, and re-anchor subsequent paths to that root.

## PR Management

| Intent | Command |
|--------|---------|
| Push current branch and create/update PR | `ez push` |
| Push to a fork and create/update PRs in upstream | `ez push --remote fork --repo upstream-owner/project --fork-repo my-user/project` |
| Push without creating/updating PR | `ez push --no-pr` |
| Force PR creation when `no_pr` config is true | `ez push --pr` |
| Print PR URL to stdout | `ez pr-link` |
| Edit PR title/body | `ez pr-edit --title "..." --body "..."` |
| Mark PR as draft / ready | `ez draft` / `ez ready` |
| Merge bottom PR | `ez merge` |
| Merge non-interactively | `ez merge --yes` |
| Merge current linear stack bottom-to-top | `ez merge --stack --yes` |
| Push the current stack through a fork | `ez submit --remote fork --repo upstream-owner/project --fork-repo my-user/project` |

### Fork/upstream targeting

Configure fork-based contribution once per repository:

```bash
git remote add upstream git@github.com:upstream-owner/project.git
git remote add fork git@github.com:my-user/project.git

ez config set remote fork
ez config set upstream_remote upstream
ez config set repo upstream-owner/project
ez config set fork_repo my-user/project
```

The settings have separate responsibilities:

| Setting | Responsibility |
|---------|----------------|
| `remote` | Push destination for local stack branches. |
| `upstream_remote` | Fetch source for trunk and PR refs. Falls back to `remote`. |
| `repo` | GitHub owner/name for PR create/update/view/edit/ready, statuses, adopt, merge, and native-stack inspection. |
| `fork_repo` | GitHub owner/name used to qualify PR heads as `owner:branch` when creating fork PRs. |

`ez push` and `ez submit` accept `--remote`, `--repo`, and `--fork-repo` for
one invocation. These overrides do not persist to `.git/ez/stack.json`; use
`ez config set` for durable defaults. Repositories without `upstream_remote` or
`fork_repo` keep the previous behavior. When `repo` is not configured, ez still
derives the GitHub repository from the configured push remote.

GitHub's native stacked PR API is same-repository only. If `fork_repo` targets a
different repository from `repo`, or the push remote clearly targets a different
repository from the upstream repo, ez reports native stack state as
`not_applicable` and skips native-stack mutation. The ordinary base-chained PR
stack, worktree fleet, `sync`, `adopt`, and `merge` workflows remain available
against the upstream GitHub repo.

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
| Configure fork push remote | `ez config set remote fork` |
| Configure upstream fetch remote | `ez config set upstream_remote upstream` |
| Configure upstream GitHub repo | `ez config set repo upstream-owner/project` |
| Configure fork GitHub repo | `ez config set fork_repo my-user/project` |
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
