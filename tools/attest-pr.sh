#!/usr/bin/env bash
# Sign the repository gate for a pull request's pushed head and post the attestation, which is what
# turns `agentscm/local-validation` green. See docs/advanced/local-validation.md.
#
# Usage: tools/attest-pr.sh [PR_NUMBER]    (default: the pull request for the current branch)
#        tools/attest-pr.sh --with-lock COMMAND [ARG...]    (run COMMAND under the same lock)
# Env:   AGENTSCM_LV_DIR     key and state root (default ~/.config/agentscm/local-validation)
#        AGENTSCM_LV_ISSUER  registry issuer and key name (default: the signed-in GitHub login)
#        AGENTSCM_LV_KEY     signing key file (default: $AGENTSCM_LV_DIR/$ISSUER.ed25519, else
#                            $AGENTSCM_LV_DIR/$ISSUER.key)
#
# One attestation or --with-lock command runs per machine at a time, across worktrees; concurrent
# full suites fail timing-sensitive tests. The lock is ~/.agentscm/attest-pr.lock, and a lock whose
# holder has died is reclaimed.
#
# Exits 0 without posting when this machine has no signing key; say so in the PR. Any other
# failure, including a recipe that does not pass, exits nonzero and posts nothing. A recipe that
# does not pass prints where its signed failure record and command output are kept.
#
# PostgreSQL and S3 come from the AGENTSCM_TEST_* variables when set; otherwise disposable containers
# start from the images pinned in .github/workflows/ci.yml. Tools the recipe selects for this change
# (see docs/contributor-testing.md) must be on PATH, or the run fails rather than skipping them.
set -euo pipefail
umask 077
cd "$(git rev-parse --show-toplevel)"

die() {
  echo "attest-pr: $*" >&2
  exit 1
}

# macOS has no flock(1): mkdir is the atomic step, and the pid file says who holds the lock.
lock="$HOME/.agentscm/attest-pr.lock"
take_lock() {
  local holder waiting_for=
  mkdir -p "${lock%/*}"
  until mkdir "$lock" 2>/dev/null; do
    holder=$(cat "$lock/pid" 2>/dev/null || true)
    # One reclaimer at a time, and only if the dead holder still owns the lock, so a waiter that
    # read a stale pid cannot remove a lock another waiter has just taken.
    # shortcut: a process killed between mkdir and writing its pid, or inside this reclaim step,
    # leaves a lock nobody reclaims; remove $lock and $lock.reclaim by hand if a wait never ends.
    if [[ -n "$holder" ]] && ! kill -0 "$holder" 2>/dev/null && mkdir "$lock.reclaim" 2>/dev/null; then
      if [[ "$(cat "$lock/pid" 2>/dev/null || true)" == "$holder" ]]; then
        echo "attest-pr: reclaiming the lock from $holder, which is no longer running" >&2
        rm -rf "$lock"
      fi
      rmdir "$lock.reclaim"
      continue
    fi
    if [[ "$holder" != "$waiting_for" ]]; then
      if [[ -n "$holder" ]]; then
        echo "attest-pr: waiting for $holder ($(ps -o command= -p "$holder" 2>/dev/null | cut -c1-120)) to release $lock" >&2
      else
        echo "attest-pr: waiting for a starting run to release $lock" >&2
      fi
      waiting_for=$holder
    fi
    sleep 2
  done
  echo $$ >"$lock/pid"
}
release_lock() {
  if [[ "$(cat "$lock/pid" 2>/dev/null || true)" == "$$" ]]; then rm -rf "$lock"; fi
}

if [[ "${1:-}" == --with-lock ]]; then
  shift
  (($#)) || die "usage: tools/attest-pr.sh --with-lock COMMAND [ARG...]"
  trap release_lock EXIT
  take_lock
  "$@"
  exit
fi

lv_dir=${AGENTSCM_LV_DIR:-$HOME/.config/agentscm/local-validation}
issuer=${AGENTSCM_LV_ISSUER:-$(gh api user --jq .login)}
state="$lv_dir/agentscm-attestations"
# The binary refuses linked keys and keys in a folder that is not owner-private, so name the
# file rather than linking or copying it.
if [[ -n "${AGENTSCM_LV_KEY:-}" ]]; then
  key=$AGENTSCM_LV_KEY
  [[ -f "$key" ]] || die "AGENTSCM_LV_KEY is not a file: $key"
else
  key="$lv_dir/$issuer.ed25519"
  [[ -f "$key" ]] || key="$lv_dir/$issuer.key"
  if [[ ! -f "$key" ]]; then
    echo "attest-pr: no signing key at $lv_dir/$issuer.ed25519 or $key; nothing signed or posted" >&2
    exit 0
  fi
fi

pr=${1:-$(gh pr view --json number --jq .number)}
[[ -z "$(git status --porcelain)" ]] || die "working tree is dirty"
head=$(git rev-parse HEAD)
[[ "$(gh pr view "$pr" --json headRefOid --jq .headRefOid)" == "$head" ]] \
  || die "local HEAD is not the head of pull request $pr; push first"

tmp=$(mktemp -d)
pg_container="attest-pr-postgres-$$"
s3_container="attest-pr-s3-$$"
started=()
cleanup() {
  if ((${#started[@]})); then docker rm -f "${started[@]}" >/dev/null 2>&1 || true; fi
  rm -rf "$tmp"
  release_lock
}
trap cleanup EXIT
take_lock

# The producer runs the release the verifier workflow pins.
version=$(sed -n 's/^ *release-version: *//p' .github/workflows/local-validation.yml)
[[ -n "$version" ]] || die "no release-version in the verifier workflow"
bin="$HOME/.agentscm/bin/agentscm-$version"
if [[ ! -x "$bin" ]]; then
  (cd "$tmp" && AGENTSCM_REQUIRE_PROVENANCE=1 AGENTSCM_TELEMETRY=off \
    npx --yes --ignore-scripts --registry=https://registry.npmjs.org \
    "agentscm@$version" local-validation reconcile --help >/dev/null </dev/null)
fi
[[ "$("$bin" --version)" == "agentscm $version" ]] || die "$bin is not version $version"

ci_image() {
  local image
  image=$(sed -n "s|^.*\($1@sha256:[0-9a-f]\{64\}\).*\$|\1|p" .github/workflows/ci.yml | head -n 1)
  [[ -n "$image" ]] || die "no pinned $1 image in .github/workflows/ci.yml"
  echo "$image"
}

if [[ -z "${AGENTSCM_TEST_DATABASE_URL:-}" ]]; then
  image=$(ci_image postgres:18)
  started+=("$pg_container")
  docker run -d --name "$pg_container" -p 127.0.0.1::5432 -e POSTGRES_PASSWORD=postgres "$image" >/dev/null
  address=$(docker port "$pg_container" 5432/tcp | head -n 1)
  export AGENTSCM_TEST_DATABASE_URL="postgres://postgres:postgres@$address/postgres"
  for _ in $(seq 60); do
    docker exec "$pg_container" pg_isready -U postgres -q && break
    sleep 1
  done
  docker exec "$pg_container" pg_isready -U postgres -q || die "PostgreSQL did not become ready"
fi
if [[ -z "${AGENTSCM_TEST_S3_ENDPOINT:-}" ]]; then
  image=$(ci_image versity/versitygw)
  # Throwaway credentials, passed by name so they stay out of process arguments.
  ROOT_ACCESS_KEY=$(openssl rand -hex 10)
  ROOT_SECRET_KEY=$(openssl rand -hex 20)
  export ROOT_ACCESS_KEY ROOT_SECRET_KEY
  started+=("$s3_container")
  docker run -d --name "$s3_container" -p 127.0.0.1::7070 -e ROOT_ACCESS_KEY -e ROOT_SECRET_KEY \
    "$image" --port :7070 --iam-dir /tmp/vgw posix /tmp/vgw >/dev/null
  address=$(docker port "$s3_container" 7070/tcp | head -n 1)
  export AGENTSCM_TEST_S3_ENDPOINT="http://$address" AGENTSCM_TEST_S3_BUCKET=agentscm-ci \
    AGENTSCM_TEST_S3_ACCESS_KEY_ID=$ROOT_ACCESS_KEY AGENTSCM_TEST_S3_SECRET_ACCESS_KEY=$ROOT_SECRET_KEY
  for _ in $(seq 60); do
    curl -s -o /dev/null "$AGENTSCM_TEST_S3_ENDPOINT/" && break
    sleep 1
  done
  curl -s -o /dev/null "$AGENTSCM_TEST_S3_ENDPOINT/" || die "S3 did not become ready"
fi

# Trust configuration comes from the fetched default branch, even when this change edits it.
git fetch -q origin main
base=$(git rev-parse origin/main)
for file in recipe policy registry; do
  git show "$base:.github/local-validation/$file.json" >"$tmp/$file.json"
done

# A stable Git line-ending configuration for recipe fixtures, without touching the user's own.
mkdir "$tmp/home"
printf '[core]\n\tautocrlf = false\n' >"$tmp/home/.gitconfig"
cargo_home=${CARGO_HOME:-$HOME/.cargo}
rustup_home=${RUSTUP_HOME:-$HOME/.rustup}
# A failing recipe exits 1 after signing and retaining its record, so read the result either way.
status=0
HOME="$tmp/home" CARGO_HOME="$cargo_home" RUSTUP_HOME="$rustup_home" \
  "$bin" local-validation run --repo . \
  --config-dir .github/local-validation \
  --verifier-workflow .github/workflows/local-validation.yml \
  --recipe "$tmp/recipe.json" --policy "$tmp/policy.json" --registry "$tmp/registry.json" \
  --key-file "$key" --issuer-id "$issuer" --state-dir "$state" \
  --base-ref main --base-rev "$base" >"$tmp/result.json" || status=$?
outcome=$(jq -r '.outcome // empty' "$tmp/result.json" 2>/dev/null || true)
[[ -n "$outcome" ]] || die "local-validation run exited $status without a result; not posting"
jq -c '{outcome, envelope_sha256}' "$tmp/result.json"
envelope=$(jq -r .envelope_sha256 "$tmp/result.json")
if [[ "$outcome" != pass ]]; then
  {
    echo "attest-pr: recipe did not pass; not posting. The signed failure record is kept in $state:"
    echo "  record:   $state/$envelope.record.json"
    echo "  envelope: $state/$envelope.envelope.json"
    jq -r --arg out "$state/$envelope" '.results | to_entries[]
      | select(.value.stop != "exited" or .value.exit_code != 0)
      | "  failed:   \(.value.id) (\(.value.stop), exit \(.value.exit_code // "none")); output in \($out).cmd\(.key + 1).stdout and .stderr"' \
      "$state/$envelope.record.json" || true
    echo "  summary:  $bin local-validation show --state-dir $state --attestation $envelope"
  } >&2
  exit 1
fi

git fetch -q origin main
git diff --quiet "$base" origin/main -- .github/local-validation \
  || die "the default branch's trust configuration moved during the run; not posting"
"$bin" local-validation verify --repo . --head "$head" --base-rev origin/main \
  --repository github.com/agentscm/agentscm \
  --recipe "$tmp/recipe.json" --policy "$tmp/policy.json" --registry "$tmp/registry.json" \
  --envelope "$state/$envelope.envelope.json" --record "$state/$envelope.record.json"

gh auth token >"$tmp/github-token"
"$bin" local-validation post --state-dir "$state" --attestation "$envelope" \
  --github-repo agentscm/agentscm --pr "$pr" --token-file "$tmp/github-token"
