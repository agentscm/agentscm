#!/usr/bin/env bash
# End-to-end test of the hub containers: two agents on two "machines" and one warning.
# Usage: .github/hub-e2e.sh default|postgres|local-release EVIDENCE_DIR
# local-release qualifies locally built images, not published release artifacts.
# Needs Docker, curl and Python 3; builds the images, starts a uniquely named compose project and
# removes it, volumes included, on exit. Single profiles use port 7332; local-release
# uses ephemeral loopback ports and requires Compose support for !override (2.24.4+).
set -euo pipefail
profile=${1:?usage: hub-e2e.sh default|postgres|local-release EVIDENCE_DIR}
cd "$(dirname "$0")/.."

# Shared stage budget; cleanup uses its own bounded allowance after a failure.
stage_started=$SECONDS
bounded() {
  python3 .github/hub-e2e-timeout.py "$((2400 - SECONDS + stage_started))" "$@"
}
timeout_status() {
  if grep -q '^TIMEOUT:' "$1"; then printf 'timeout\n'; else printf 'fail\n'; fi
}

local_release() (
  local evidence candidate tree context runtime seed image role stage result=0
  evidence=${1:?local-release requires a new evidence directory outside the checkout}
  candidate=$(git rev-parse HEAD)
  tree=$(git rev-parse 'HEAD^{tree}')
  if [ -n "$(git status --porcelain --untracked-files=all)" ]; then
    echo "local-release requires a clean candidate" >&2
    return 1
  fi
  # Canonicalize the existing parent before creating anything; never overwrite evidence.
  evidence="$(cd "$(dirname "$evidence")" && pwd -P)/$(basename "$evidence")"
  case "$evidence/" in "$(pwd -P)/"*) echo "evidence must be outside the checkout" >&2; return 1 ;; esac
  (umask 077; mkdir "$evidence")
  printf 'qualification=locally-built-only\ncommit=%s\ntree=%s\n' "$candidate" "$tree" >"$evidence/source"
  for stage in build default postgres; do printf 'not-run\n' >"$evidence/$stage.status"; done
  printf 'incomplete\n' >"$evidence/result"
  date -u +%Y-%m-%dT%H:%M:%SZ >"$evidence/started_utc"
  context=$(mktemp -d)
  # shellcheck disable=SC2329 # Invoked by the EXIT trap.
  release_cleanup() {
    local status=$?
    date -u +%Y-%m-%dT%H:%M:%SZ >"$evidence/finished_utc"
    rm -rf "$context"
    if [ "$status" -ne 0 ]; then echo "Local release qualification incomplete or failed: $evidence" >&2; fi
  }
  trap release_cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  # Build only committed bytes, including when ignored files exist in the checkout.
  git archive "$candidate" | tar -x -C "$context"
  date -u +%Y-%m-%dT%H:%M:%SZ >"$evidence/build.started_utc"
  printf 'running\n' >"$evidence/build.status"
  if (
    set -e
    for role in runtime seed; do
      stage=runtime
      [ "$role" != seed ] || stage=e2e-seed
      bounded docker build --target "$stage" --build-arg "AGENTSCM_BUILD_COMMIT=$candidate" --iidfile "$evidence/$role.iid" "$context" || exit 1
      image=$(cat "$evidence/$role.iid") || exit 1
      [[ "$image" =~ ^sha256:[0-9a-f]{64}$ ]] || exit 1
      [ "$(bounded docker image inspect --format '{{.Id}}' "$image")" = "$image" ] || exit 1
      bounded docker image inspect --format '{{.Os}}/{{.Architecture}}' "$image" >"$evidence/$role.platform" || exit 1
      grep -Eq '^[a-z0-9]+/[a-z0-9]+$' "$evidence/$role.platform" || exit 1
    done
  ) >"$evidence/build.log" 2>&1; then
    date -u +%Y-%m-%dT%H:%M:%SZ >"$evidence/build.finished_utc"
    printf 'pass\n' >"$evidence/build.status"
  else
    date -u +%Y-%m-%dT%H:%M:%SZ >"$evidence/build.finished_utc"
    timeout_status "$evidence/build.log" >"$evidence/build.status"
    return 1
  fi
  runtime=$(cat "$evidence/runtime.iid")
  seed=$(cat "$evidence/seed.iid")
  # Compose replaces the normal fixed port only for this qualification. Image IDs
  # cannot drift if local tags are rebuilt while the two profiles run.
  cat >"$evidence/compose.yaml" <<EOF
services:
  hub:
    image: $runtime
    pull_policy: never
    ports: !override ["127.0.0.1::7332"]
  hub-postgres:
    image: $runtime
    pull_policy: never
    ports: !override ["127.0.0.1::7332"]
  client:
    image: $runtime
    pull_policy: never
  client-postgres:
    image: $runtime
    pull_policy: never
  seed:
    image: $seed
    pull_policy: never
  seed-postgres:
    image: $seed
    pull_policy: never
EOF
  for stage in default postgres; do
    date -u +%Y-%m-%dT%H:%M:%SZ >"$evidence/$stage.started_utc"
    printf 'running\n' >"$evidence/$stage.status"
    # No ambient forge credentials, Compose variables or .env file reach the tests.
    if env -i PATH="$PATH" HOME="$HOME" \
      DOCKER_HOST="${DOCKER_HOST:-}" DOCKER_CONTEXT="${DOCKER_CONTEXT:-}" \
      DOCKER_CONFIG="${DOCKER_CONFIG:-$HOME/.docker}" \
      AGENTSCM_E2E_OVERRIDE="$evidence/compose.yaml" \
      bash "$context/.github/hub-e2e.sh" "$stage" >"$evidence/$stage.log" 2>&1 &&
      grep -Eq "^PASS \[$stage [0-9]+s\] all steps$" "$evidence/$stage.log"; then
      printf 'pass\n' >"$evidence/$stage.status"
    else
      timeout_status "$evidence/$stage.log" >"$evidence/$stage.status"
      result=1
    fi
    date -u +%Y-%m-%dT%H:%M:%SZ >"$evidence/$stage.finished_utc"
  done
  if [ "$(git rev-parse HEAD)" != "$candidate" ] || [ -n "$(git status --porcelain --untracked-files=all)" ]; then
    echo "candidate changed during qualification" >&2
    result=1
  fi
  if [ "$result" -eq 0 ]; then
    printf 'pass\n' >"$evidence/result"
    echo "PASS locally built Hub profiles; published artifacts not qualified: $evidence"
  else
    printf 'fail\n' >"$evidence/result"
  fi
  return "$result"
)

if [ "$profile" = local-release ]; then
  local_release "${2:-}"
  exit $?
fi
case $profile in
  default) hub=hub seed=seed client=client up=(hub) ;;
  postgres) hub=hub-postgres seed=seed-postgres client=client-postgres up=(hub-postgres s3) ;;
  *) echo "profile must be default or postgres" >&2; exit 2 ;;
esac
project=agentscm-e2e-$profile-$$
work=$(mktemp -d)
dc() {
  if [ -n "${AGENTSCM_E2E_OVERRIDE:-}" ]; then
    set -- --env-file /dev/null -f "$AGENTSCM_E2E_OVERRIDE" --profile "$profile" "$@"
  else
    set -- --profile "$profile" "$@"
  fi
  bounded docker compose -p "$project" -f compose.hub.yaml -f .github/compose.e2e.yaml "$@"
}
health_url=http://127.0.0.1:7332/v1/hub/health
refresh_endpoint() {
  if [ -n "${AGENTSCM_E2E_OVERRIDE:-}" ]; then
    local endpoint
    endpoint=$(dc port "$hub" 7332)
    [[ "$endpoint" =~ ^127\.0\.0\.1:[0-9]+$ ]]
    health_url="http://$endpoint/v1/hub/health"
  fi
}
cleanup() {
  status=$?
  # Give diagnostics and teardown separate allowances even after stage timeout.
  stage_started=$((SECONDS - 2370))
  if [ "$status" -ne 0 ]; then
    echo "FAIL ($profile)" >&2
    dc logs --no-color >&2 || true
  fi
  stage_started=$((SECONDS - 2370))
  if ! dc down -v --remove-orphans >/dev/null; then
    if [ -n "${AGENTSCM_E2E_OVERRIDE:-}" ]; then
      echo "FAIL ($profile): compose cleanup failed for $project" >&2
      status=1
    fi
  fi
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT

pass() { echo "PASS [$profile ${SECONDS}s] $1"; }
wait_healthy() {
  for _ in $(seq 60); do
    if [ "$((SECONDS - stage_started))" -ge 2400 ]; then
      echo 'TIMEOUT: qualification health deadline exhausted' >&2
      return 124
    fi
    if curl --connect-timeout 2 --max-time 5 -fsS "$health_url" 2>/dev/null | grep -q '"status":"ok"'; then return 0; fi
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

if [ -z "${AGENTSCM_E2E_OVERRIDE:-}" ]; then
  dc build
  pass "images built"
fi
dc up --no-build -d --wait --wait-timeout 120 "${up[@]}"
refresh_endpoint
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

dc restart "${up[@]}"
refresh_endpoint
wait_healthy
out=$(check_b sb2)
assert_warned "$out"
pass "warning survived a hub restart"

if [ "$profile" = default ]; then
  # Cold backup: stop the hub so SQLite is quiescent, then copy /data as a tarball.
  dc stop "$hub"
  dc run --rm -T --no-deps --entrypoint tar "$hub" -C /data -czf - . >"$work/hub.tgz"
  dc rm -sf "$hub"
  bounded docker volume rm "${project}_hub-data" >/dev/null
  dc run --rm -T --no-deps --entrypoint tar "$hub" -C /data -xzf - <"$work/hub.tgz"
  dc up --no-build -d "$hub"
  refresh_endpoint
  wait_healthy
  out=$(check_b sb3)
  assert_warned "$out"
  pass "backup restored into a fresh volume still serves the intent"
fi
echo "PASS [$profile ${SECONDS}s] all steps"
