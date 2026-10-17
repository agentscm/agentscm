#!/bin/sh
# Installs the AgentSCM release binary into ~/.agentscm/bin and starts the setup guide.
# Mirrors packages/npm/lib/install.js; keep the two in step.
set -eu

AGENTSCM_DEFAULT_VERSION=""  # set by release.yml
VERSION="${AGENTSCM_VERSION:-$AGENTSCM_DEFAULT_VERSION}"
REPO="counterbranch/agentscm"
SUMS_MAX=65536
BINARY_MAX=209715200
tmp=""

die() { echo "agentscm: $*" >&2; exit 1; }

fetch() { # fetch <url> <output file> <max bytes>
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --proto-redir '=https' --tlsv1.2 --connect-timeout 15 --max-time 600 \
      --max-filesize "$3" -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q --https-only --timeout=30 -O "$2" "$1"
  else
    die "curl or wget is required"
  fi || return 1
  [ "$(wc -c <"$2" | tr -d ' ')" -le "$3" ] || die "download exceeds the $3 byte limit: $1"
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  else
    die "sha256sum or shasum is required"
  fi
}

# Prints why provenance cannot be checked; prints nothing when gh is installed and signed in.
provenance_gap() {
  if ! command -v gh >/dev/null 2>&1; then
    echo "the GitHub CLI (gh) is not installed"
  elif ! gh auth status --hostname github.com >/dev/null 2>&1; then
    echo "the GitHub CLI (gh) is not signed in to github.com or cannot reach it (see \`gh auth status\`)"
  fi
}

# Without a usable gh the checksum comes from the same release as the binary: that catches corruption, not forgery.
provenance() { # provenance <file>; uses $gap from main
  if [ -z "$gap" ]; then
    gh attestation verify "$1" --repo "$REPO" \
      --signer-workflow "$REPO/.github/workflows/release.yml" \
      --source-ref "refs/tags/v$VERSION" --deny-self-hosted-runners \
      || die "build provenance verification failed; not running the binary"
  elif [ "${AGENTSCM_REQUIRE_PROVENANCE:-}" = 1 ]; then
    die "AGENTSCM_REQUIRE_PROVENANCE=1 but $gap"
  else
    echo "Checksum-verified only: provenance was not checked ($gap)."
  fi
}

main() {
  cat <<'TEXT'
This script downloads the AgentSCM release binary for your platform from
GitHub Releases, verifies its SHA-256 checksum and build provenance, and then
starts the setup guide. It does not run install scripts, send telemetry or
change anything without asking. Everything it writes is listed before it
writes it, and can be undone with `agentscm uninstall`.

TEXT

  [ -n "$VERSION" ] || die "set AGENTSCM_VERSION (for example AGENTSCM_VERSION=0.1.0-alpha.1)"
  case "$VERSION" in
    *[!A-Za-z0-9._-]*) die "invalid version" ;;
  esac

  case "$(uname -s)/$(uname -m)" in
    Linux/x86_64) target=x86_64-unknown-linux-musl ;;
    Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-musl ;;
    Darwin/x86_64) target=x86_64-apple-darwin ;;
    Darwin/arm64) target=aarch64-apple-darwin ;;
    *)
      die "no AgentSCM release binary for $(uname -s)/$(uname -m).
Use Docker instead:
  docker run --rm -it ghcr.io/$REPO:$VERSION --help
See https://github.com/$REPO/blob/v$VERSION/docs/install.md"
      ;;
  esac

  gap="$(provenance_gap)" # checked once: `gh auth status` goes over the network
  if [ "${AGENTSCM_REQUIRE_PROVENANCE:-}" = 1 ] && [ -n "$gap" ]; then
    die "AGENTSCM_REQUIRE_PROVENANCE=1 but $gap"
  fi

  base="https://github.com/$REPO/releases/download/v$VERSION"
  asset="agentscm-$target"
  dir="$HOME/.agentscm/bin"
  link="$dir/agentscm"
  bin="$dir/agentscm-$VERSION"
  mkdir -p "$dir"
  chmod 700 "$dir"
  [ ! -e "$link" ] || [ -L "$link" ] || die "$link exists and is not a symlink; remove it"
  tmp="$(mktemp -d "$dir/.install.XXXXXX")"
  trap 'rm -rf "$tmp"' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" "$SUMS_MAX" || die "could not download SHA256SUMS for v$VERSION"
  echo "Checksums from the GitHub release."
  # Strict, like the npm package: every line must be well formed and the target listed exactly once.
  if grep -qv -E '^[0-9a-f]{64}  agentscm-[a-z0-9_-]+(\.exe)?$' "$tmp/SHA256SUMS"; then
    die "SHA256SUMS has a malformed line"
  fi
  [ "$(grep -c -E "^[0-9a-f]{64}  $asset\$" "$tmp/SHA256SUMS")" = 1 ] || die "SHA256SUMS must list $asset exactly once"
  expected="$(grep -E "^[0-9a-f]{64}  $asset\$" "$tmp/SHA256SUMS" | cut -d ' ' -f 1)"

  # Order: checksum, provenance, chmod, move, link, exec. Nothing is installed or linked before both checks pass.
  if [ -e "$bin" ]; then
    [ "$(sha256 "$bin")" = "$expected" ] || die "$bin does not match the release checksum; delete it and retry"
    provenance "$bin"
    echo "Using the verified $bin"
  else
    fetch "$base/$asset" "$tmp/$asset" "$BINARY_MAX" || die "could not download $asset"
    [ "$(sha256 "$tmp/$asset")" = "$expected" ] || die "checksum mismatch for $asset; nothing was installed"
    provenance "$tmp/$asset"
    chmod 755 "$tmp/$asset"
    mv "$tmp/$asset" "$bin"
  fi
  ln -s "agentscm-$VERSION" "$tmp/link"
  mv -f "$tmp/link" "$link"

  # Same rule as the npm package: no arguments or a leading flag starts the setup guide.
  if [ "$#" -eq 0 ]; then
    set -- quickstart
  else
    case "$1" in -*) set -- quickstart "$@" ;; esac
  fi
  rm -rf "$tmp"
  trap - EXIT # exec below skips the trap
  # Under `curl ... | sh` stdin is the script; give the guide the terminal so it can prompt.
  if [ ! -t 0 ] && (: </dev/tty) 2>/dev/null; then
    exec "$bin" "$@" </dev/tty
  fi
  exec "$bin" "$@"
}

main "$@"
