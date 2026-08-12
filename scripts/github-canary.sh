#!/usr/bin/env bash
set -Eeuo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/github-canary.sh [--list-commands] [--help]

Runs a destructive, isolated GitHub canary for ez-stack against a temporary
trunk and branches under a unique ez-canary/<id> prefix.

Options:
  --list-commands  Print every operational top-level command from ez --help,
                   excluding help, one per line. This does not touch GitHub.
  --help           Show this help text.

Environment:
  EZ_BIN           Path to the ez executable. Required for live canary runs.
  EZ_CANARY_REPO   owner/repo to clone. Defaults to GITHUB_REPOSITORY or
                   rohoswagger/ez-stack.
  EZ_CANARY_ID     Unique id. Defaults to GITHUB_RUN_ID-GITHUB_RUN_ATTEMPT or
                   timestamp-pid.
USAGE
}

canonical_executable() {
  local path="$1"
  local dir
  local base
  if [ "$path" = "" ]; then
    return 1
  fi
  case "$path" in
    */*) ;;
    *) path="$(command -v "$path")" ;;
  esac
  if [ ! -x "$path" ]; then
    return 1
  fi
  dir="$(cd "$(dirname "$path")" && pwd -P)"
  base="$(basename "$path")"
  printf '%s/%s\n' "$dir" "$base"
}

list_commands() {
  # Keep this manifest static. The contract test compares it with `ez --help`,
  # so adding a CLI command fails CI until the real canary covers it.
  cat <<'COMMANDS'
init
adopt
create
commit
amend
push
submit
sync
restack
up
down
top
bottom
switch
log
status
list
diff
parent
track
delete
fold
split
move
merge
pr-edit
draft
ready
pr-link
pr
update
setup
scope
skill
shell-init
config
worktree
COMMANDS
}

case "${1:-}" in
  --help|-h)
    usage
    exit 0
    ;;
  --list-commands)
    list_commands
    exit 0
    ;;
  "")
    ;;
  *)
    usage >&2
    exit 64
    ;;
esac

log_group() {
  local name="$1"
  printf '\n::group::%s\n' "$name"
}

end_group() {
  printf '::endgroup::\n'
}

run() {
  printf '+'
  printf ' %q' "$@"
  printf '\n'
  "$@"
}

capture() {
  {
    printf '+'
    printf ' %q' "$@"
    printf '\n'
  } >&2
  "$@"
}

switch_to() {
  local target="$1"
  local target_path
  target_path="$(capture "$EZ_BIN_CANON" switch "$target" --no-cd-required)"
  if [ "$target_path" = "" ] && [ "$(git branch --show-current)" = "$target" ]; then
    target_path="$(pwd -P)"
  fi
  [ -d "$target_path" ] || fail "ez switch did not return a worktree path: $target_path"
  cd "$target_path"
}

navigate_to_output() {
  local target_path
  target_path="$(capture env EZ_SHELL_INTEGRATION=1 "$EZ_BIN_CANON" "$@")"
  [ -d "$target_path" ] || fail "ez $1 did not return a worktree path: $target_path"
  cd "$target_path"
}

fail() {
  printf 'github-canary: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  local command_name="$1"
  command -v "$command_name" >/dev/null 2>&1 || fail "required command not found: $command_name"
}

sanitize_id() {
  printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '-'
}

validate_canary_ref_parts() {
  local id="$1"
  case "$id" in
    ""|.*|*..*|*-|*/*|*\\*|*.lock|*~|*^*|*:*|*[\?\[]*|*' '*)
      fail "unsafe EZ_CANARY_ID after sanitization: $id"
      ;;
  esac
}

safe_ref_under_prefix() {
  local ref="$1"
  case "$ref" in
    "$PREFIX"/*) return 0 ;;
    *) return 1 ;;
  esac
}

assert_contains() {
  local haystack="$1"
  local needle="$2"
  local label="$3"
  case "$haystack" in
    *"$needle"*) ;;
    *) fail "expected $label to contain: $needle" ;;
  esac
}

assert_file() {
  [ -f "$1" ] || fail "expected file to exist: $1"
}

close_open_prs() {
  if [ "${CAN_CLONE_READY:-0}" != "1" ]; then
    return 0
  fi
  gh pr list --repo "$CANARY_REPO" --state open --limit 100 \
    --json number,headRefName \
    --jq ".[] | select(.headRefName | startswith(\"${PREFIX}/\")) | [.number, .headRefName] | @tsv" \
    2>/dev/null |
  while IFS="$(printf '\t')" read -r number head; do
    [ "$number" != "" ] || continue
    if safe_ref_under_prefix "$head"; then
      printf 'Closing canary PR #%s (%s)\n' "$number" "$head" >&2
      gh pr close "$number" --repo "$CANARY_REPO" --comment "Closing ez canary cleanup for ${PREFIX}" >/dev/null 2>&1 || true
    fi
  done
}

delete_remote_refs() {
  if [ "${CAN_CLONE_READY:-0}" != "1" ] || [ ! -d "${CANARY_DIR:-}/.git" ]; then
    return 0
  fi
  (
    cd "$CANARY_DIR"
    git ls-remote --heads origin "${PREFIX}/*" 2>/dev/null |
    while IFS="$(printf '\t')" read -r _ full_ref; do
      ref="${full_ref#refs/heads/}"
      if safe_ref_under_prefix "$ref"; then
        printf 'Deleting remote branch %s\n' "$ref" >&2
        git push origin ":refs/heads/${ref}" >/dev/null 2>&1 || true
      fi
    done
    if git ls-remote --exit-code --heads origin "$TRUNK_BRANCH" >/dev/null 2>&1; then
      if safe_ref_under_prefix "$TRUNK_BRANCH"; then
        printf 'Deleting remote trunk %s\n' "$TRUNK_BRANCH" >&2
        git push origin ":refs/heads/${TRUNK_BRANCH}" >/dev/null 2>&1 || true
      fi
    fi
  )
}

cleanup() {
  local status="$?"
  trap - EXIT INT TERM
  set +e
  close_open_prs
  delete_remote_refs
  if [ "${TMP_ROOT:-}" != "" ] && [ -d "$TMP_ROOT" ]; then
    rm -rf "$TMP_ROOT"
  fi
  exit "$status"
}

require_cmd gh
require_cmd git
require_cmd awk
require_cmd tr
require_cmd mktemp
require_cmd grep

EZ_BIN_CANON="$(canonical_executable "${EZ_BIN:-}")" || fail "EZ_BIN must name an executable ez binary for live canary runs"
CANARY_REPO="${EZ_CANARY_REPO:-${GITHUB_REPOSITORY:-rohoswagger/ez-stack}}"
RAW_CANARY_ID="${EZ_CANARY_ID:-}"
if [ "$RAW_CANARY_ID" = "" ]; then
  if [ "${GITHUB_RUN_ID:-}" != "" ]; then
    RAW_CANARY_ID="${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT:-1}"
  else
    RAW_CANARY_ID="$(date +%Y%m%d%H%M%S)-$$"
  fi
fi
CANARY_ID="$(sanitize_id "$RAW_CANARY_ID")"
validate_canary_ref_parts "$CANARY_ID"
PREFIX="ez-canary/${CANARY_ID}"
TRUNK_BRANCH="${PREFIX}/trunk"
FIXTURE_DIR="canary-fixtures/${CANARY_ID}"
CAN_CLONE_READY=0
TMP_ROOT=""
CANARY_DIR=""
SECOND_DIR=""
trap cleanup EXIT INT TERM

log_group "preflight"
case "$CANARY_REPO" in
  */*) ;;
  *) fail "EZ_CANARY_REPO must be owner/repo, got: $CANARY_REPO" ;;
esac
printf 'EZ_BIN=%s\n' "$EZ_BIN_CANON"
printf 'EZ_CANARY_REPO=%s\n' "$CANARY_REPO"
printf 'EZ_CANARY_ID=%s\n' "$CANARY_ID"
printf 'PREFIX=%s\n' "$PREFIX"
gh auth status >/dev/null
run gh auth setup-git
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ez-canary.XXXXXX")"
end_group

log_group "clone and temporary trunk"
CANARY_DIR="${TMP_ROOT}/repo"
run gh repo clone "$CANARY_REPO" "$CANARY_DIR" -- --quiet
CAN_CLONE_READY=1
cd "$CANARY_DIR"
run git config user.name "ez canary"
run git config user.email "ez-canary@example.invalid"
DEFAULT_BRANCH="$(capture gh repo view "$CANARY_REPO" --json defaultBranchRef --jq '.defaultBranchRef.name')"
[ "$DEFAULT_BRANCH" != "" ] || fail "could not determine default branch"
run git fetch origin "$DEFAULT_BRANCH"
DEFAULT_BRANCH_HEAD="$(git rev-parse "origin/$DEFAULT_BRANCH")"
run git checkout -B "$TRUNK_BRANCH" "origin/$DEFAULT_BRANCH"
printf 'canary %s\n' "$CANARY_ID" > "${CANARY_ID}.trunk"
run git add "${CANARY_ID}.trunk"
run git commit -m "canary: seed temporary trunk ${CANARY_ID}"
run git push origin "HEAD:refs/heads/${TRUNK_BRANCH}"
# Seed one deliberately unmanaged branch before ez initialization so `ez track`
# can prove it adopts existing Git state without using raw branch
# mutations after the repository becomes ez-managed.
run git checkout -b "${PREFIX}/raw-local"
mkdir -p "$FIXTURE_DIR"
printf 'raw local\n' > "${FIXTURE_DIR}/raw-local.txt"
run git add "${FIXTURE_DIR}/raw-local.txt"
run git commit -m "canary: raw local branch"
run git checkout "$TRUNK_BRANCH"
end_group

log_group "init setup config skill"
run "$EZ_BIN_CANON" init --yes --trunk "$TRUNK_BRANCH"
run "$EZ_BIN_CANON" config set repo "$CANARY_REPO"
run "$EZ_BIN_CANON" config set remote origin
run "$EZ_BIN_CANON" config set default_from "$TRUNK_BRANCH"
run "$EZ_BIN_CANON" config set draft false
assert_contains "$(capture "$EZ_BIN_CANON" config get trunk)" "$TRUNK_BRANCH" "config get trunk"
run "$EZ_BIN_CANON" config list
run "$EZ_BIN_CANON" config unset draft
SANDBOX_HOME="${TMP_ROOT}/home"
mkdir -p "$SANDBOX_HOME"
HOME="$SANDBOX_HOME" SHELL="/bin/bash" run "$EZ_BIN_CANON" setup --yes
assert_file "${SANDBOX_HOME}/.bash_profile"
capture "$EZ_BIN_CANON" shell-init >/dev/null
HOME="$SANDBOX_HOME" USERPROFILE="$SANDBOX_HOME" run "$EZ_BIN_CANON" skill install
assert_file "${SANDBOX_HOME}/.agents/skills/ez-workflow/SKILL.md"
[ ! -e ".agents/skills/ez-workflow" ] || fail "skill install wrote into the cloned repository"
HOME="$SANDBOX_HOME" USERPROFILE="$SANDBOX_HOME" run "$EZ_BIN_CANON" skill uninstall
[ ! -e "${SANDBOX_HOME}/.agents/skills/ez-workflow" ] || fail "skill uninstall left the canonical skill installed"
end_group

log_group "local branch and stack operations"
run "$EZ_BIN_CANON" track "${PREFIX}/raw-local" --parent "$TRUNK_BRANCH"
run "$EZ_BIN_CANON" create "${PREFIX}/base" --from "${PREFIX}/raw-local" --no-worktree
switch_to "${PREFIX}/base"
mkdir -p "$FIXTURE_DIR"
printf 'base v1\n' > "${FIXTURE_DIR}/base.txt"
run "$EZ_BIN_CANON" commit -Am "canary: base fixture"
printf 'base v2\n' >> "${FIXTURE_DIR}/base.txt"
run "$EZ_BIN_CANON" amend -a
run "$EZ_BIN_CANON" scope set --mode warn "${FIXTURE_DIR}/base.txt"
run "$EZ_BIN_CANON" scope show
run "$EZ_BIN_CANON" scope add "${FIXTURE_DIR}/extra.txt"
run "$EZ_BIN_CANON" scope clear
run "$EZ_BIN_CANON" status
run "$EZ_BIN_CANON" status --json
run "$EZ_BIN_CANON" log
run "$EZ_BIN_CANON" log --json
run "$EZ_BIN_CANON" list
run "$EZ_BIN_CANON" list --json
run "$EZ_BIN_CANON" diff --stat
run "$EZ_BIN_CANON" diff --name-only
assert_contains "$(capture "$EZ_BIN_CANON" parent)" "${PREFIX}/raw-local" "parent"
run "$EZ_BIN_CANON" create "${PREFIX}/child" --no-worktree
switch_to "${PREFIX}/child"
printf 'child\n' > "${FIXTURE_DIR}/child.txt"
run "$EZ_BIN_CANON" commit -Am "canary: child fixture"
navigate_to_output down
navigate_to_output up
navigate_to_output bottom
navigate_to_output top
switch_to "${PREFIX}/base"
run "$EZ_BIN_CANON" move --onto "$TRUNK_BRANCH"
run "$EZ_BIN_CANON" restack
run "$EZ_BIN_CANON" sync --dry-run
run "$EZ_BIN_CANON" create "${PREFIX}/split-src" --from "$TRUNK_BRANCH" --no-worktree
switch_to "${PREFIX}/split-src"
mkdir -p "$FIXTURE_DIR"
printf 'split one\n' > "${FIXTURE_DIR}/split-one.txt"
run "$EZ_BIN_CANON" commit -Am "canary: split commit one"
printf 'split two\n' > "${FIXTURE_DIR}/split-two.txt"
run "$EZ_BIN_CANON" commit -Am "canary: split commit two"
run "$EZ_BIN_CANON" split --dry-run
run "$EZ_BIN_CANON" split
run "$EZ_BIN_CANON" log
navigate_to_output delete "${PREFIX}/split-src" --force --yes
# Not the current branch, so `ez delete` prints no path to cd into.
run "$EZ_BIN_CANON" delete "${PREFIX}/split-src-1" --force --yes

run "$EZ_BIN_CANON" create "${PREFIX}/fold-parent" --from "$TRUNK_BRANCH" --no-worktree
switch_to "${PREFIX}/fold-parent"
mkdir -p "$FIXTURE_DIR"
printf 'fold parent\n' > "${FIXTURE_DIR}/fold-parent.txt"
run "$EZ_BIN_CANON" commit -Am "canary: fold parent"
run "$EZ_BIN_CANON" create "${PREFIX}/fold-child" --no-worktree
switch_to "${PREFIX}/fold-child"
printf 'fold child\n' > "${FIXTURE_DIR}/fold-child.txt"
run "$EZ_BIN_CANON" commit -Am "canary: fold child"
navigate_to_output fold "${PREFIX}/fold-child" --yes
navigate_to_output delete "${PREFIX}/fold-parent" --force --yes
end_group

log_group "worktree operations"
WT_BRANCH="${PREFIX}/wt"
WT_PATH="$(capture "$EZ_BIN_CANON" worktree create "$WT_BRANCH" --from "$TRUNK_BRANCH")"
[ -d "$WT_PATH" ] || fail "worktree create did not return a directory: $WT_PATH"
run "$EZ_BIN_CANON" worktree list
run "$EZ_BIN_CANON" worktree claim "$WT_BRANCH" --owner "ez-canary-${CANARY_ID}" --ttl 30m --json
run "$EZ_BIN_CANON" worktree leases --json
run "$EZ_BIN_CANON" worktree release "$WT_BRANCH" --owner "ez-canary-${CANARY_ID}" --json
run "$EZ_BIN_CANON" worktree ensure "$WT_BRANCH" --dry-run --json
run "$EZ_BIN_CANON" worktree ensure "$WT_BRANCH"
run "$EZ_BIN_CANON" worktree exec "$WT_BRANCH" -- git status --short
run "$EZ_BIN_CANON" worktree delete "$WT_BRANCH" --force --yes
run "$EZ_BIN_CANON" delete "${PREFIX}/child" --force --yes
run "$EZ_BIN_CANON" delete "${PREFIX}/base" --force --yes
run "$EZ_BIN_CANON" delete "${PREFIX}/raw-local" --force --yes
end_group

log_group "update check and sync"
run "$EZ_BIN_CANON" update --check
printf 'autostash survives sync\n' >> "${CANARY_ID}.trunk"
run "$EZ_BIN_CANON" sync --autostash
grep -F "autostash survives sync" "${CANARY_ID}.trunk" >/dev/null || fail "sync --autostash did not restore the dirty trunk file"
run git restore "${CANARY_ID}.trunk"
end_group

log_group "production github stack"
switch_to "$TRUNK_BRANCH"
run "$EZ_BIN_CANON" create "${PREFIX}/pr-base" --from "$TRUNK_BRANCH" --no-worktree
switch_to "${PREFIX}/pr-base"
mkdir -p "$FIXTURE_DIR"
printf 'pr base\n' > "${FIXTURE_DIR}/pr-base.txt"
run "$EZ_BIN_CANON" commit -Am "canary: pr base"
run "$EZ_BIN_CANON" create "${PREFIX}/pr-top" --no-worktree
switch_to "${PREFIX}/pr-top"
printf 'pr top\n' > "${FIXTURE_DIR}/pr-top.txt"
run "$EZ_BIN_CANON" commit -Am "canary: pr top"
SUBMIT_OUTPUT="$(capture "$EZ_BIN_CANON" submit --no-draft --body "GitHub canary ${CANARY_ID}")"
printf '%s\n' "$SUBMIT_OUTPUT"
PR_BASE="$(gh pr view "${PREFIX}/pr-base" --repo "$CANARY_REPO" --json number --jq '.number')"
PR_TOP="$(gh pr view "${PREFIX}/pr-top" --repo "$CANARY_REPO" --json number --jq '.number')"
[ "$PR_BASE" != "" ] || fail "missing base PR number"
[ "$PR_TOP" != "" ] || fail "missing top PR number"
printf 'PR_BASE=%s\nPR_TOP=%s\n' "$PR_BASE" "$PR_TOP"
run "$EZ_BIN_CANON" status --native-stack
run "$EZ_BIN_CANON" status --json --native-stack
run "$EZ_BIN_CANON" log --native-stack
run "$EZ_BIN_CANON" log --json --native-stack
run "$EZ_BIN_CANON" pr-edit --title "canary: edited top ${CANARY_ID}" --body "Edited by GitHub canary ${CANARY_ID}"
run "$EZ_BIN_CANON" draft
run "$EZ_BIN_CANON" ready
assert_contains "$(capture "$EZ_BIN_CANON" pr-link)" "$PR_TOP" "pr-link"
BROWSER=true GH_BROWSER=true run "$EZ_BIN_CANON" pr
switch_to "$PR_BASE"
run "$EZ_BIN_CANON" push --title "canary: updated base ${CANARY_ID}" --body "Updated by canary"
end_group

log_group "adopt from second clone"
SECOND_DIR="${TMP_ROOT}/second"
run gh repo clone "$CANARY_REPO" "$SECOND_DIR" -- --quiet
cd "$SECOND_DIR"
run git config user.name "ez canary"
run git config user.email "ez-canary@example.invalid"
run git fetch origin "$TRUNK_BRANCH"
run git checkout -B "$TRUNK_BRANCH" "origin/$TRUNK_BRANCH"
run "$EZ_BIN_CANON" init --yes --trunk "$TRUNK_BRANCH"
run "$EZ_BIN_CANON" config set repo "$CANARY_REPO"
run "$EZ_BIN_CANON" adopt --pr "$PR_TOP"
switch_to "$PR_TOP"
end_group

log_group "merge temporary stack"
cd "$CANARY_DIR"
switch_to "${PREFIX}/pr-top"
navigate_to_output merge --stack --yes
end_group

log_group "post-merge sync from adopted clone"
cd "$SECOND_DIR"
printf 'adopted clone autostash survives sync\n' >> "${CANARY_ID}.trunk"
run "$EZ_BIN_CANON" sync --autostash
grep -F "adopted clone autostash survives sync" "${CANARY_ID}.trunk" >/dev/null || fail "post-merge sync did not restore the adopted clone's dirty file"
if git show-ref --verify --quiet "refs/heads/${PREFIX}/pr-base"; then
  fail "post-merge sync preserved merged base branch"
fi
if git show-ref --verify --quiet "refs/heads/${PREFIX}/pr-top"; then
  fail "post-merge sync preserved merged top branch"
fi
run git restore "${CANARY_ID}.trunk"

DEFAULT_BRANCH_HEAD_AFTER="$(git ls-remote origin "refs/heads/${DEFAULT_BRANCH}" | awk '{print $1}')"
[ "$DEFAULT_BRANCH_HEAD_AFTER" = "$DEFAULT_BRANCH_HEAD" ] || fail "canary changed default branch ${DEFAULT_BRANCH}"
[ "$(gh pr view "$PR_BASE" --repo "$CANARY_REPO" --json state --jq '.state')" = "MERGED" ] || fail "base PR was not merged"
[ "$(gh pr view "$PR_TOP" --repo "$CANARY_REPO" --json state --jq '.state')" = "MERGED" ] || fail "top PR was not merged"
end_group

printf '\nGitHub canary completed for %s (PRs #%s, #%s)\n' "$PREFIX" "$PR_BASE" "$PR_TOP"
