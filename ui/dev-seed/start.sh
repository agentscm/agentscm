#!/usr/bin/env bash
# Dev-only: fresh throwaway hub on 127.0.0.1:${AGENTSCM_DEV_HUB_PORT:-7332}, seeded with synthetic agents, kept live.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
# AGENTSCM_BIN runs the hub from another build (e.g. a PR worktree); it must share this schema.
bin="${AGENTSCM_BIN:-$root/target/debug/agentscm}"
data="$here/.data"
port="${AGENTSCM_DEV_HUB_PORT:-7332}"
export AGENTSCM_DEV_HUB_PORT="$port"

health=http://127.0.0.1:$port/v1/hub/health
if curl -sf "$health" >/dev/null; then echo "start.sh: something already answers on 127.0.0.1:$port; stop it first" >&2; exit 1; fi

# Always build: a stale binary refuses a database the freshly built mint has already migrated.
[ -n "${AGENTSCM_BIN:-}" ] || (cd "$root" && cargo build --locked -q --bin agentscm)
(cd "$here/mint" && CARGO_TARGET_DIR="$(cd "$root" && pwd -P)/target" cargo build --offline -q)

rm -rf "$data"
mkdir -p "$data/objects" "$data/secrets"
chmod 700 "$data" "$data/secrets"
export AGENTSCM_DATABASE_URL="sqlite://$data/hub.sqlite"
"$root/target/debug/agentscm-dev-seed-mint" "$data/tokens.env" > "$data/publications.txt"
(umask 077; rm -f "$here/.hub.env"; printf 'AGENTSCM_HUB=http://127.0.0.1:%s\nREAD_TOKEN=%s\n' "$port" "$(sed -n 's/^READ=//p' "$data/tokens.env")" > "$here/.hub.env")

# External CI/deployment outcomes are shown only for repositories listed here. The seeded GitHub
# connection never polls (mint pushes next_poll_at ten years out) and the API URL points at a dead
# local port, so nothing here can reach real GitHub (the PR-opened publication's merge check fails closed).
export AGENTSCM_GITHUB_EXTERNAL_RUNS=counterbranch/agentscm
# Set AGENTSCM_GITHUB_API_URL=https://api.github.com with AGENTSCM_GITHUB_CLIENT_ID/_SECRET_FILE to try real GitHub sign-in.
export AGENTSCM_GITHUB_API_URL="${AGENTSCM_GITHUB_API_URL:-http://127.0.0.1:9}"
"$bin" hub --no-telemetry serve --bind "127.0.0.1:$port" --objects "$data/objects" --secrets "$data/secrets" &
hub=$!
trap 'kill $hub 2>/dev/null' EXIT
for _ in $(seq 300); do curl -sf "$health" >/dev/null && break; sleep 0.2; done
kill -0 "$hub" 2>/dev/null && curl -sf "$health" >/dev/null || { echo "start.sh: the hub did not start on 127.0.0.1:$port" >&2; exit 1; }

python3 -I "$here/seed.py" "$data/tokens.env" "$root" --live
