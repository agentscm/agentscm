#!/usr/bin/env bash
# Keep Cargo's target directory bounded. Cargo never removes stale artifacts,
# so a long-lived checkout's target/ grows without limit. For the main
# checkout and the current one (when it has a Cargo.toml), run `cargo clean`
# if the target directory exceeds AGENTSCM_TARGET_LIMIT_GB GiB (default 30).
# CARGO_TARGET_DIR, when set, is the one shared target directory and is
# checked once. Silent when nothing is over the limit.
#
# A full clean forces a full rebuild on the next build. Cargo does
# not wait for a build already running in that checkout (checked: a clean
# started mid-build returned immediately), so avoid running it while another
# session builds in that checkout.
#
# Usage: .agents/bound-target.sh [-n|--dry-run]
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

limit=${AGENTSCM_TARGET_LIMIT_GB:-30}
if ! [[ "$limit" =~ ^[1-9][0-9]*$ ]]; then
  echo "AGENTSCM_TARGET_LIMIT_GB must be a positive integer" >&2
  exit 2
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found; target not checked" >&2
  exit 0
fi

main_wt=$(git worktree list --porcelain | awk '/^worktree /{print substr($0, 10); exit}')
here=$(git rev-parse --show-toplevel)
[[ "$main_wt" == "$here" ]] && checkouts=("$here") || checkouts=("$main_wt" "$here")

seen=""
for co in "${checkouts[@]}"; do
  [[ -f "$co/Cargo.toml" ]] || continue
  dir=${CARGO_TARGET_DIR:-$co/target}
  [[ -d "$dir" ]] || continue
  case "$seen" in *"|$dir|"*) continue ;; esac
  seen+="|$dir|"
  kb=$(du -sk "$dir" | awk '{print $1}')
  (( kb > limit * 1024 * 1024 )) || continue
  gib=$(awk -v kb="$kb" 'BEGIN{printf "%.1f", kb / 1048576}')
  if [[ -n "$dry" ]]; then
    echo "would clean $dir ($gib GiB)"
  elif err=$(cargo clean --manifest-path "$co/Cargo.toml" 2>&1 </dev/null); then
    echo "cleaned $dir ($gib GiB)"
  else
    echo "kept $dir: ${err%%$'\n'*}"
  fi
done
