#!/usr/bin/env bash
# Sign the repository gate for a pull request's pushed head and post the attestation, which is what
# turns `agentscm/local-validation` green. See docs/advanced/local-validation.md.
#
# Usage: tools/attest-pr.sh [PR_NUMBER]    (default: the pull request for the current branch)
# Env:   AGENTSCM_LV_DIR     key and state root (default ~/.config/agentscm/local-validation)
#        AGENTSCM_LV_ISSUER  registry issuer and key name (default: the signed-in GitHub login)
#
# Exits 0 without posting when this machine has no signing key; say so in the PR. Any other
# failure, including a recipe that does not pass, exits nonzero and posts nothing.
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

lv_dir=${AGENTSCM_LV_DIR:-$HOME/.config/agentscm/local-validation}
issuer=${AGENTSCM_LV_ISSUER:-$(gh api user --jq .login)}
key="$lv_dir/$issuer.ed25519"
state="$lv_dir/agentscm-attestations"
if [[ ! -f "$key" ]]; then
  echo "attest-pr: no signing key at $key; nothing signed or posted" >&2
  exit 0
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
}
trap cleanup EXIT

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
HOME="$tmp/home" CARGO_HOME="$cargo_home" RUSTUP_HOME="$rustup_home" \
  "$bin" local-validation run --repo . \
  --config-dir .github/local-validation \
  --verifier-workflow .github/workflows/local-validation.yml \
  --recipe "$tmp/recipe.json" --policy "$tmp/policy.json" --registry "$tmp/registry.json" \
  --key-file "$key" --issuer-id "$issuer" --state-dir "$state" \
  --base-ref main --base-rev "$base" >"$tmp/result.json"
jq -c '{outcome, envelope_sha256}' "$tmp/result.json"
[[ "$(jq -r .outcome "$tmp/result.json")" == pass ]] || die "recipe did not pass; not posting"
envelope=$(jq -r .envelope_sha256 "$tmp/result.json")

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
