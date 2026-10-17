#!/usr/bin/env bash
# Smoke test for a built image: version, collisions on a mounted repo, init + serve + /health, hub serve + health.
# Usage: .github/docker-smoke.sh IMAGE
set -euo pipefail
image=${1:?usage: docker-smoke.sh IMAGE}
work=$(mktemp -d)
name=agentscm-smoke-$$
vol=$name-data
cleanup() {
  docker rm -f "$name" "$name-hub" >/dev/null 2>&1 || true
  docker volume rm "$vol" "$vol-hub" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

docker run --rm "$image" --version

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
docker run -d --name "$name" -v "$vol:/data" -p 127.0.0.1:17331:7331 "$image" >/dev/null
for _ in $(seq 30); do
  body=$(curl -fsS http://127.0.0.1:17331/health || true)
  if grep -q '"status":"ok"' <<<"$body"; then break; fi
  sleep 1
done
if ! grep -q '"status":"ok"' <<<"$body"; then
  docker logs "$name" >&2
  echo "serve did not answer on /health" >&2
  exit 1
fi

# The hub: a fresh volume, SQLite by default, published on loopback only.
hub=$name-hub
docker run -d --name "$hub" -v "$vol-hub:/data" -p 127.0.0.1:17332:7332 "$image" hub serve --bind 0.0.0.0:7332 --public >/dev/null
for _ in $(seq 30); do
  body=$(curl -fsS http://127.0.0.1:17332/v1/hub/health || true)
  if grep -q '"status":"ok"' <<<"$body"; then break; fi
  sleep 1
done
if ! grep -q '"status":"ok"' <<<"$body"; then
  docker logs "$hub" >&2
  echo "hub did not answer on /v1/hub/health" >&2
  exit 1
fi

# The status page is built into the image and is served with a content security policy.
headers=$(curl -fsS -D - -o /dev/null http://127.0.0.1:17332/ui/)
grep -qi '^content-security-policy:' <<<"$headers" || {
  echo "hub did not serve /ui/ with a Content-Security-Policy" >&2
  exit 1
}
body=$(curl -fsS http://127.0.0.1:17332/ui/)
if grep -q 'built without the status page' <<<"$body"; then
  echo "image was built without the status page" >&2
  exit 1
fi
