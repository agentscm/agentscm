#!/usr/bin/env bash
# End-to-end test of the hub containers: two agents on two "machines" and one warning.
# Usage: .github/hub-e2e.sh default|postgres   (default: SQLite; postgres: PostgreSQL and S3)
# Needs Docker and curl; builds the images, starts a uniquely named compose project and
# removes it, volumes included, on exit. Uses host port 7332.
set -euo pipefail
profile=${1:?usage: hub-e2e.sh default|postgres}
cd "$(dirname "$0")/.."
case $profile in
  default) hub=hub seed=seed client=client up=hub ;;
  postgres) hub=hub-postgres seed=seed-postgres client=client-postgres up='hub-postgres s3' ;;
  *) echo "profile must be default or postgres" >&2; exit 2 ;;
esac
project=agentscm-e2e-$profile-$$
work=$(mktemp -d)
dc() { docker compose -p "$project" -f compose.hub.yaml -f .github/compose.e2e.yaml --profile "$profile" "$@"; }
cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    echo "FAIL ($profile)" >&2
    dc logs --no-color >&2 || true
  fi
  dc down -v --remove-orphans >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

pass() { echo "PASS [$profile ${SECONDS}s] $1"; }
wait_healthy() {
  for _ in $(seq 60); do
    if curl -fsS http://127.0.0.1:7332/v1/hub/health 2>/dev/null | grep -q '"status":"ok"'; then return 0; fi
    sleep 1
  done
  echo "hub did not become healthy" >&2
  return 1
}
# client MACHINE TOKEN SCRIPT: a throwaway container in the hub's network namespace; stdin passes through.
client() {
  dc run --rm -T -e AGENTSCM_MACHINE="$1" -e AGENTSCM_HUB_TOKEN_FILE="/tokens/$2.token" "$client" "$3"
}
repo='git init -q /work/repo && git -C /work/repo remote add origin git@github.com:acme/widgets.git && cd /work/repo'
# check_b SESSION: bob's agent edits the file alice's agent announced; prints the hook reply.
check_b() {
  printf '{"session_id":"%s","cwd":"/work/repo","hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"file_path":"/work/repo/src/billing.rs","old_string":"round(x)","new_string":"round(x, 2)"}}' "$1" |
    client laptop-b b "$repo && agentscm intent check --hook claude"
}
assert_warned() {
  grep -q "alice's agent (claude-a) on laptop-a is already working on src/billing.rs" <<<"$1"
  grep -q 'Fix invoice rounding' <<<"$1"
  grep -q '"hookEventName":"PreToolUse"' <<<"$1"
  if grep -q permissionDecision <<<"$1"; then return 1; fi
}

dc build
pass "images built"
dc up -d --wait --wait-timeout 120 $up
wait_healthy
pass "hub healthy"

dc run --rm "$seed"
pass "seeded two users and two tokens"

out=$(client laptop-a a "$repo && AGENTSCM_TASK='Fix invoice rounding' agentscm intent check --session a src/billing.rs")
[ -z "$out" ]
pass "agent A announced src/billing.rs with no warning"

out=$(check_b sb1)
assert_warned "$out"
pass "agent B was warned about agent A"

dc restart $up
wait_healthy
out=$(check_b sb2)
assert_warned "$out"
pass "warning survived a hub restart"

if [ "$profile" = default ]; then
  # Cold backup: stop the hub so SQLite is quiescent, then copy /data as a tarball.
  dc stop "$hub"
  dc run --rm -T --no-deps --entrypoint tar "$hub" -C /data -czf - . >"$work/hub.tgz"
  dc rm -sf "$hub"
  docker volume rm "${project}_hub-data" >/dev/null
  dc run --rm -T --no-deps --entrypoint tar "$hub" -C /data -xzf - <"$work/hub.tgz"
  dc up -d "$hub"
  wait_healthy
  out=$(check_b sb3)
  assert_warned "$out"
  pass "backup restored into a fresh volume still serves the intent"
fi
echo "PASS [$profile ${SECONDS}s] all steps"
