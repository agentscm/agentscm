#!/usr/bin/env bash
# Smoke test for a built image: runtime identity, native dependencies, collisions, persistent serve, and Hub UI.
# Usage: .github/docker-smoke.sh IMAGE
set -euo pipefail
image=${1:?usage: docker-smoke.sh IMAGE}
requested_run_id=${AGENTSCM_SMOKE_ID:-}
serve_port=${AGENTSCM_SMOKE_SERVE_PORT:-}
hub_port=${AGENTSCM_SMOKE_HUB_PORT:-}
[[ -z "$requested_run_id" || "$requested_run_id" =~ ^[A-Za-z0-9_.-]+$ ]] || {
  echo "AGENTSCM_SMOKE_ID contains invalid Docker name characters" >&2
  exit 2
}
for port in "$serve_port" "$hub_port"; do
  [[ -z "$port" || "$port" =~ ^[0-9]+$ && "$port" -ge 1 && "$port" -le 65535 ]] || {
    echo "smoke-test ports must be integers from 1 through 65535" >&2
    exit 2
  }
done
work=$(mktemp -d)
run_id=${requested_run_id:-${work##*/}}
name=agentscm-smoke-$run_id
vol=$name-data
for container in "$name" "$name-hub"; do
  if docker container inspect "$container" >/dev/null 2>&1; then
    rm -rf "$work"
    echo "refusing to reuse existing smoke-test container $container" >&2
    exit 2
  fi
done
for volume in "$vol" "$vol-hub"; do
  if docker volume inspect "$volume" >/dev/null 2>&1; then
    rm -rf "$work"
    echo "refusing to reuse existing smoke-test volume $volume" >&2
    exit 2
  fi
done
cleanup() {
  docker rm -f "$name" "$name-hub" >/dev/null 2>&1 || true
  docker volume rm "$vol" "$vol-hub" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

test "$(docker image inspect --format '{{.Config.User}}' "$image")" = "10001:10001"
test "$(docker run --rm --network none --entrypoint id "$image" -u)" = 10001
docker run --rm --network none --read-only --tmpfs /tmp "$image" --version
docker run --rm --network none --entrypoint git "$image" --version
ldd_out=$(docker run --rm --network none --entrypoint /usr/bin/ldd "$image" /usr/local/bin/agentscm)
grep -q 'libc\.so' <<<"$ldd_out"

# Two branches editing the same file must be reported by collisions.
git init -q -b main "$work/repo"
git -C "$work/repo" config user.email smoke@example.com
git -C "$work/repo" config user.name smoke
echo base >"$work/repo/f"
git -C "$work/repo" add f
git -C "$work/repo" commit -q -m base
for b in a b; do
  git -C "$work/repo" checkout -q -b "$b" main
  echo "$b" >>"$work/repo/f"
  git -C "$work/repo" commit -q -am "$b"
done
# Same user as the host so Git's ownership check passes; read-write because merge-tree writes objects.
out=$(docker run --rm --network none --user "$(id -u):$(id -g)" -v "$work/repo:/repo" "$image" collisions --git-dir /repo --base main)
grep -q '"level":"conflict"' <<<"$out"
# A read-only mount still works but degrades to path-level warnings (merge-tree cannot write).
out=$(docker run --rm --network none --user "$(id -u):$(id -g)" -v "$work/repo:/repo:ro" "$image" collisions --git-dir /repo --base main)
grep -q '"level":"same-file"' <<<"$out"

docker run --rm -v "$vol:/data" "$image" --data-dir /data init
if [[ -n "$serve_port" ]]; then
  serve_publish="127.0.0.1:$serve_port:7331"
else
  serve_publish="127.0.0.1::7331"
fi
docker run -d --name "$name" -v "$vol:/data" -p "$serve_publish" "$image" >/dev/null
serve_address=$(docker port "$name" 7331/tcp | head -n 1)
for _ in $(seq 30); do
  body=$(curl -fsS "http://$serve_address/health" || true)
  if grep -q '"status":"ok"' <<<"$body"; then break; fi
  sleep 1
done
if ! grep -q '"status":"ok"' <<<"$body"; then
  docker logs "$name" >&2
  echo "serve did not answer on /health" >&2
  exit 1
fi
docker restart "$name" >/dev/null
serve_address=$(docker port "$name" 7331/tcp | head -n 1)
for _ in $(seq 30); do
  body=$(curl -fsS "http://$serve_address/health" || true)
  if grep -q '"status":"ok"' <<<"$body"; then break; fi
  sleep 1
done
if ! grep -q '"status":"ok"' <<<"$body"; then
  docker logs "$name" >&2
  echo "serve did not recover with its persistent volume after restart" >&2
  exit 1
fi

# The hub: a fresh volume, SQLite by default, published on loopback only.
hub=$name-hub
if [[ -n "$hub_port" ]]; then
  hub_publish="127.0.0.1:$hub_port:7332"
else
  hub_publish="127.0.0.1::7332"
fi
docker run -d --name "$hub" -v "$vol-hub:/data" -p "$hub_publish" "$image" hub serve --bind 0.0.0.0:7332 --public >/dev/null
hub_address=$(docker port "$hub" 7332/tcp | head -n 1)
for _ in $(seq 30); do
  body=$(curl -fsS "http://$hub_address/v1/hub/health" || true)
  if grep -q '"status":"ok"' <<<"$body"; then break; fi
  sleep 1
done
if ! grep -q '"status":"ok"' <<<"$body"; then
  docker logs "$hub" >&2
  echo "hub did not answer on /v1/hub/health" >&2
  exit 1
fi

# The status page is built into the image and is served with a content security policy.
headers=$(curl -fsS -D - -o /dev/null "http://$hub_address/ui/")
grep -qi '^content-security-policy:' <<<"$headers" || {
  echo "hub did not serve /ui/ with a Content-Security-Policy" >&2
  exit 1
}
body=$(curl -fsS "http://$hub_address/ui/")
if grep -q 'built without the status page' <<<"$body"; then
  echo "image was built without the status page" >&2
  exit 1
fi
