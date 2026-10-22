#!/usr/bin/env bash
# Dev-only: fresh throwaway hub on 127.0.0.1:7332, seeded with synthetic agents, kept live.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
bin="$root/target/debug/agentscm"
data="$here/.data"

health=http://127.0.0.1:7332/v1/hub/health
if curl -sf "$health" >/dev/null; then echo "start.sh: something already answers on 127.0.0.1:7332; stop it first" >&2; exit 1; fi

[ -x "$bin" ] || (cd "$root" && cargo build --locked --bin agentscm)
(cd "$here/mint" && CARGO_TARGET_DIR="$root/target" cargo build --offline -q)

rm -rf "$data"
mkdir -p "$data/objects" "$data/secrets"
chmod 700 "$data" "$data/secrets"
export AGENTSCM_DATABASE_URL="sqlite://$data/hub.sqlite"
"$root/target/debug/agentscm-dev-seed-mint" "$data/tokens.env"
(umask 077; rm -f "$here/.hub.env"; printf 'AGENTSCM_HUB=http://127.0.0.1:7332\nREAD_TOKEN=%s\n' "$(sed -n 's/^READ=//p' "$data/tokens.env")" > "$here/.hub.env")

"$bin" hub --no-telemetry serve --objects "$data/objects" --secrets "$data/secrets" &
hub=$!
trap 'kill $hub 2>/dev/null' EXIT
for _ in $(seq 50); do curl -sf "$health" >/dev/null && break; sleep 0.2; done
kill -0 "$hub" 2>/dev/null && curl -sf "$health" >/dev/null || { echo "start.sh: the hub did not start on 127.0.0.1:7332" >&2; exit 1; }

python3 -I "$here/seed.py" "$data/tokens.env" "$root" --live
