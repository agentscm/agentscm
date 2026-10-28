#!/usr/bin/env bash
# Compile what the `windows` CI job compiles, from macOS or Linux, so Unix-only code is caught before pushing.
# Needs: rustup target add x86_64-pc-windows-gnu, and a MinGW C compiler (macOS: brew install mingw-w64).
# Usage: tools/windows-check.sh
set -euo pipefail
cd "$(dirname "$0")/.."
target=x86_64-pc-windows-gnu
# The integration tests the CI job runs, read from the workflow so the two lists cannot drift.
tests=$(sed -n 's/^ *TESTS: "\(.*\)"$/\1/p' .github/workflows/windows.yml)
[[ -n "$tests" ]] || { echo "could not read TESTS from .github/workflows/windows.yml" >&2; exit 2; }
args=()
for t in $tests; do args+=(--test "$t"); done
export RUSTFLAGS="${RUSTFLAGS:-} -D warnings"
cargo check --locked --target "$target" --bin agentscm
cargo check --locked --target "$target" --profile test --lib --bin agentscm "${args[@]}"
