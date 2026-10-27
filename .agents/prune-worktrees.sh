#!/usr/bin/env bash
# Remove worktrees whose work is finished: the branch's newest same-repo PR is
# merged or closed (none open), and the worktree's HEAD is that PR's head
# commit or behind it. Repo-agnostic; needs `gh`.
#
# Committed work is never lost — branches are kept — and `git worktree remove`
# without --force refuses dirty, untracked, locked, or submodule trees.
# Gitignored files (node_modules, .env, local settings) ARE deleted; that is
# the disk being reclaimed. Never touches the main checkout, the current one,
# or a branch with no PR (a fresh worktree hasn't opened one yet).
# To protect a worktree: `git worktree lock <path>`.
#
# Usage: .agents/prune-worktrees.sh [-n|--dry-run]
set -euo pipefail

dry=""
case "${1:-}" in
  "") ;;
  -n | --dry-run) dry=1 ;;
  *)
    echo "usage: $0 [-n|--dry-run]" >&2
    exit 2
    ;;
esac

if ! gh repo view --json nameWithOwner >/dev/null 2>&1; then
  echo "gh unavailable for this repo; nothing pruned" >&2
  exit 0
fi

git worktree prune ${dry:+-n}
main_wt=$(git worktree list --porcelain | awk '/^worktree /{print substr($0, 10); exit}')
here=$(git rev-parse --show-toplevel)

git worktree list --porcelain |
  awk '/^worktree /{p = substr($0, 10)} /^branch refs\/heads\//{print p "\t" substr($0, 19)}' |
  while IFS=$'\t' read -r path branch; do
    [[ "$path" == "$main_wt" || "$path" == "$here" ]] && continue
    # "STATE OID" of the newest same-repo PR, "OPEN" if any is open, "" if none.
    read -r state oid < <(gh pr list --head "$branch" --state all \
      --json state,headRefOid,isCrossRepository \
      --jq 'map(select(.isCrossRepository | not))
        | if any(.state == "OPEN") then "OPEN" else ((.[0] // {}) | "\(.state // "") \(.headRefOid // "")") end' \
      </dev/null 2>/dev/null || true) || true
    [[ "${state:-}" == MERGED || "${state:-}" == CLOSED ]] || continue
    # A reused branch name or commits after the merge mean this isn't the PR's work.
    git -C "$path" merge-base --is-ancestor HEAD "$oid" 2>/dev/null || continue
    if [[ -n "$dry" ]]; then
      if [[ -n "$(git -C "$path" status --porcelain 2>/dev/null)" ]]; then
        echo "would keep $path ($branch, PR $state): dirty"
      else
        echo "would remove $path ($branch, PR $state)"
      fi
    elif err=$(git worktree remove "$path" 2>&1 </dev/null); then
      echo "removed $path ($branch, PR $state)"
    else
      echo "kept $path ($branch, PR $state): ${err%%$'\n'*}"
    fi
  done
