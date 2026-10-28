#!/bin/sh
# Installs the AgentSCM release binary into ~/.agentscm/bin and starts the setup guide.
# Mirrors packages/npm/lib/install.js; keep the two in step.
set -eu

AGENTSCM_DEFAULT_VERSION=""  # set by release.yml
VERSION="${AGENTSCM_VERSION:-$AGENTSCM_DEFAULT_VERSION}"
REPO="counterbranch/agentscm"
SUMS_MAX=65536
BINARY_MAX=209715200
VERIFY_OUTPUT_MAX=1048576
tmp=""

die() { echo "agentscm: $*" >&2; exit 1; }
verbose() { [ "${AGENTSCM_INSTALL_VERBOSE:-}" = 1 ]; }

show_verify_output() { # show_verify_output <file>
  bytes="$(wc -c <"$1" | tr -d ' ')"
  head -c "$VERIFY_OUTPUT_MAX" "$1" >&2
  [ "$bytes" -le "$VERIFY_OUTPUT_MAX" ] || echo >&2 "agentscm: GitHub CLI output truncated after $VERIFY_OUTPUT_MAX bytes"
}

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
    if GH_HOST=github.com gh attestation verify "$1" --repo "$REPO" \
      --signer-workflow "$REPO/.github/workflows/release.yml" \
      --source-ref "refs/tags/v$VERSION" --deny-self-hosted-runners \
      >"$tmp/gh-verify.log" 2>&1; then
      if verbose; then show_verify_output "$tmp/gh-verify.log"; fi
      provenance_verified=1
    else
      show_verify_output "$tmp/gh-verify.log"
      die "build provenance verification failed; not running the binary"
    fi
  elif [ "${AGENTSCM_REQUIRE_PROVENANCE:-}" = 1 ]; then
    die "AGENTSCM_REQUIRE_PROVENANCE=1 but $gap"
  else
    provenance_verified=0
  fi
  return 0
}

main() {
  cat <<'TEXT'
This script downloads the AgentSCM release binary for your platform from
GitHub Releases, verifies its SHA-256 checksum and build provenance, and then
starts the setup guide. It does not run install scripts. After verification it
marks that binary invocation as a verified install; the binary records one
installation indicator only after setup explains pseudonymous product telemetry
and the configured choice permits it. Disable this before any send with
`--no-telemetry`, `AGENTSCM_TELEMETRY=off`, or `DO_NOT_TRACK=1`.
The setup guide previews repository, hook and Hub changes; `agentscm uninstall`
reverses setup. Manage usage reporting with `agentscm telemetry status` and
`agentscm telemetry off`.

TEXT

  [ -n "$VERSION" ] || die "set AGENTSCM_VERSION (for example AGENTSCM_VERSION=0.1.0-alpha.N; see the GitHub Releases page)"
  case "$VERSION" in
    *[!A-Za-z0-9._-]*) die "invalid version" ;;
  esac
  # A release script trusts only its own release's signing identity.
  [ -z "$AGENTSCM_DEFAULT_VERSION" ] || [ "$VERSION" = "$AGENTSCM_DEFAULT_VERSION" ] ||
    die "this script is pinned to $AGENTSCM_DEFAULT_VERSION; download install.sh from the requested release"

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
  verbose && echo "Checksums from the GitHub release."
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
    verbose && echo "Using the verified $bin"
  else
    fetch "$base/$asset" "$tmp/$asset" "$BINARY_MAX" || die "could not download $asset"
    [ "$(sha256 "$tmp/$asset")" = "$expected" ] || die "checksum mismatch for $asset; nothing was installed"
    provenance "$tmp/$asset"
    chmod 755 "$tmp/$asset"
    mv "$tmp/$asset" "$bin"
  fi
  ln -s "agentscm-$VERSION" "$tmp/link"
  mv -f "$tmp/link" "$link"

  if [ "$provenance_verified" = 1 ]; then
    echo "Verified provenance with GitHub attestations. Set AGENTSCM_INSTALL_VERBOSE=1 for details."
  else
    echo "Checked the SHA-256 checksum from the release (provenance not checked: $gap)."
  fi

  # Same rule as the npm package: no arguments or a leading flag starts the setup guide.
  if [ "$#" -eq 0 ]; then
    set -- quickstart
  else
    case "$1" in -*) set -- quickstart "$@" ;; esac
  fi
  rm -rf "$tmp"
  trap - EXIT # exec below skips the trap
  # Process-only assertion from this verified installer. The binary records an installation only
  # after its telemetry disclosure/choice gates are effective and clears this from its sender.
  AGENTSCM_VERIFIED_INSTALL=1
  export AGENTSCM_VERIFIED_INSTALL
  # Under `curl ... | sh` stdin is the script; give the guide the terminal so it can prompt.
  if [ ! -t 0 ] && (: </dev/tty) 2>/dev/null; then
    exec "$bin" "$@" </dev/tty
  fi
  exec "$bin" "$@"
}

main "$@"
