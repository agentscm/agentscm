#!/usr/bin/env bash
# Compile what the `windows` CI job compiles, from macOS or Linux, so Unix-only code is caught before pushing.
# That is the lib, the bin and the single `integration` test binary; the modules that build on Windows are the
# ones in tests/integration/main.rs without `#[cfg(unix)]`.
# Needs: rustup target add x86_64-pc-windows-gnu, and a MinGW C compiler (macOS: brew install mingw-w64).
# Usage: tools/windows-check.sh
set -euo pipefail
cd "$(dirname "$0")/.."
target=x86_64-pc-windows-gnu
export RUSTFLAGS="${RUSTFLAGS:-} -D warnings"
cargo check --locked --target "$target" --bin agentscm
cargo check --locked --target "$target" --profile test --lib --bin agentscm --test integration
