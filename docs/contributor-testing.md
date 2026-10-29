---
title: "Contributor testing"
---

# Contributor testing

The preferred full test gate uses nextest for unit and integration tests, followed by Cargo's doctest runner because nextest does not run doctests:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo nextest run --locked
cargo test --locked --doc
```

Install the version required by `.config/nextest.toml` so local and CI behavior stays reproducible:

```sh
cargo install cargo-nextest --version 0.9.148 --locked
```

On macOS or Linux, also compile the Windows build before pushing a change that touches `#[cfg(unix)]` code or calls into a Unix-only module (`client_plugin`, `client_setup`, `tray` and others). The gate above only compiles the host platform, so these mistakes otherwise surface only in the `windows` CI job. `tools/windows-check.sh` compiles what that job compiles, with warnings as errors, and reads its test list from `.github/workflows/windows.yml`. It needs the target and a MinGW C compiler once:

```sh
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64   # Debian/Ubuntu: apt install mingw-w64
tools/windows-check.sh
```

It checks compilation only; Windows test behaviour is still CI's job. It builds into `target/x86_64-pc-windows-gnu`, which takes several GB; delete it with the rest of `target` when space is tight.

`.agents/bound-target.sh` runs `cargo clean` in the main and current checkout when its `target` exceeds `AGENTSCM_TARGET_LIMIT_GB` GiB (default 30). `/implement` runs it; manual workflows can run it directly, or with `-n` to only report what it would clean.

`[profile.test]` in `Cargo.toml` sets `incremental = false`, so test builds (nextest, doctests and local validation) do not write `target/debug/incremental`. A gate run builds each revision once, and before this setting incremental state was about 60% of the gate's `target` size. `cargo clippy` and interactive `cargo build`/`cargo check` use the dev profile and still keep incremental state.

The nextest profile disables fail-fast and retries. It writes JUnit output to `target/nextest/default/junit.xml`, including ignored tests as skipped rather than treating them as executed. The `backend_s3` test binary is limited to one process because its tests share an in-process `CreateBucket` mutex. PostgreSQL fixture tests are also limited to one process because independently named schemas still share fixed database-wide migration and event advisory locks; the store's five-second lock timeout can otherwise expire while another test holds one of those locks. These limits do not serialize unrelated tests, internal concurrency within one test or separate nextest invocations; do not run multiple suites against the same bucket or PostgreSQL database concurrently.

The `two_git_worktrees_*` cases in `hub_workspace_execution` share a test group
limited to two concurrent processes. These cases each run a Hub, Git worktrees,
collectors and subprocess fixtures; overlapping the whole family can exhaust the
local fixture's existing HTTP deadline. The group bounds that resource load while
preserving the tests' deadlines, assertions and zero-retry policy. Other cases in
the binary remain outside this group.

Quickstart's Docker fixture uses a tracked executable launcher and per-test script
data under its temporary HOME. The Hub workflow fixture uses the tracked Python
Docker fixture, with a sidecar beside each fake Unix socket selecting that daemon's
state directory. Keep those tracked fixtures executable; rewriting a new executable
for each test can add host startup latency to the existing probe deadlines.

## Focused iteration

During implementation, select tests from the behavior that changed, its callers and its failure paths. A filename or similarly named test alone is not enough evidence. Nextest filter expressions make the selected scope explicit:

```sh
cargo nextest run --locked -E 'binary(git_collisions)'
```

Record the command and the selected scope. For a two-line deletion of test diagnostics, rerun the affected test binary during iteration and retain unchanged check evidence with its original scope. For behavior changes, include callers and failure paths. Focused checks support iteration; they do not replace final whole-branch validation or waive the applicable review policy.

Before a branch is submitted, complete the whole-branch gate above and the applicable reviews. A final whole-branch gate completed earlier in the workflow may satisfy a later workflow step without rerunning it only when its exact source and test inputs, parent/candidate mapping, `Cargo.lock`, toolchain, feature flags, nextest version and configuration, and service environment remain applicable and unchanged. Retain the original commands and results, the tested snapshot and environment, and a comparison showing that the later candidate matches them; a workflow-step or command-invocation label alone is not evidence of reuse.

Any later change invalidates the evidence for affected checks. Rerun those checks and complete the required final validation before submission. Changes to the test runner or its configuration always require the full nextest suite, separate doctests and corresponding documentation checks even when the configuration diff is small.

## Compare coverage

Use one fixed source revision, Rust toolchain, nextest version, feature set and service environment. Retain the inventories from these commands:

```sh
cargo test --locked -- --list > cargo-test-list.txt
cargo nextest list --locked --run-ignored all --message-format oneline > nextest-list.txt
cargo test --locked --doc -- --list > doctest-list.txt
```

Record the discovered test counts, normalize names where the runners format them differently, and investigate every missing or additional entry. Compare the nextest inventory plus the separate doctest inventory with the ordinary Cargo inventory. Also record ignored tests and tests that return early because an external service variable is absent; discovery is not proof that a test body completed.

The default local gate does not execute ignored tests. PostgreSQL tests can return early unless `AGENTSCM_TEST_DATABASE_URL` is set, and S3 tests can return early unless the `AGENTSCM_TEST_S3_*` variables are set. Linux CI supplies both services. Ignored live-Docker tests remain a separate serial qualification surface and are not covered by the default nextest run.

## Measure build and execution

Measure on an otherwise idle host and record the exact revision, operating system, hardware, Rust toolchain, nextest version, service configuration and cache state. Build once, then time warm execution without deleting Cargo artifacts between commands:

```sh
/usr/bin/time -p cargo test --locked --no-run
/usr/bin/time -p cargo test --locked
/usr/bin/time -p cargo nextest run --locked
/usr/bin/time -p cargo test --locked --doc
```

Report build time separately from each warm execution time, along with test counts, failures, ignored tests, early returns and service availability. Do not claim an improvement from unmatched revisions or environments.

## Local validation attestations

A maintainer may attach a signed record of a local run of this gate to a pull request; see [local validation](advanced/local-validation.md). It is experimental and does not replace the evidence rules above. For same-repository pull requests it is the expected evidence for the test gate: the hosted test suites, Windows, musl and macOS release-target builds, container, hub end-to-end and live execution jobs run only with the `ci-extended` label or a manual `workflow_dispatch`, while `CI / static` and the security checks stay automatic and `main` still runs everything.

## Resource audit boundaries

At the revision where nextest was introduced, the audit found temporary-directory isolation, ephemeral ports, random PostgreSQL schemas and process-local telemetry state. Those patterns support concurrent execution but do not prove every timing interaction is safe. A source and inventory audit found 109 PostgreSQL fixture tests across ten integration binaries. Every fixture test uses one of four full-name forms selected by the `hub-postgres` group: `on_postgres::*`, `*::postgres`, `postgres_*` or `*_on_postgres`. Future tests that create a `PgSchema` must retain one of those forms. The similarly named SQLite test `on_sqlite::postgres_read_briefings_share_the_repository_maintenance_lock` is deliberately outside the group.

The PostgreSQL limit serializes tests that share one database's global advisory locks. It is not a connection-quota workaround and does not replace schema isolation. The hosted-laptop tests retain a process mutex, and repeated RSA key generation may affect runtime; both need empirical observation during qualification. The S3 backend keeps its single-process nextest group because its existing bucket-creation mutex is process-local.

Nextest overlaps different test binaries, increasing peak filesystem and subprocess activity compared with Cargo's binary-at-a-time ordering. Qualification exposed two timing-sensitive fixtures. The Git descendant test launcher now avoids directly executing newly written scripts while retaining the tests' output/timeout limits and late-write assertions. The separate `Io` failure and primary-stop classification weakness remain under investigation in [issue 49](https://github.com/agentscm/agentscm/issues/49) and may recur. The checkpoint crash test now uses a small payload, pauses at a test-only post-commit durable-pending boundary, confirms readiness and SIGKILLs the child before recovery; a separate recovery test retains the 32 MiB case through manually staged rows. The crash test does not establish 32 MiB checkpoint staging under a crash or a kill during the middle of checkpoint finishing. Neither fixture receives an exclusive scheduler slot. The crash test and its subprocess helper moved from the `workspace` integration binary into `workspace::tests` in the library; account for those two name changes when comparing inventories across the adoption.

This nextest adoption covers the documented contributor gate and the Linux CI test job. Windows, execution-live and Hub end-to-end workflows keep their existing runners because equivalent nextest behavior has not been qualified there. No sharding is enabled.

## Initial qualification observations

The initial comparison used identical Rust/test inputs at `5e586acea5eb3f547c4a170685f7ce24fcce0c25`, Rust 1.96.0 and nextest 0.9.148 on one shared macOS arm64 host with 14 logical CPUs. Cargo artifacts were warm after a separate 64.27-second build. These are single observations with host contention, not a general speedup claim or measurements of the later fixture corrections:

| Selection | Cargo | Nextest | Coverage and result |
| --- | --- | --- | --- |
| Full discovered suite | 771.20 s | 397.33 s | 1,884 discovered, 52 ignored; Cargo had 42 failures, nextest had none; service bodies could return early |
| Same 42 tests in five integration binaries | 28.71 s | 17.14 s | Both passed all 42; same source and configuration for this pair |

The table durations and the later 457.46-second result are command wall-clock measurements, not nextest's internal summary. The initial inventories matched every test and ignored flag. Separate doctests passed with zero cases. A later full nextest run exposed the two fixture failures described above; its 457.46-second result was not clean. Corrected fixtures passed 20 isolated output-limit repetitions and five checkpoint-crash repetitions before final whole-branch validation.

With local PostgreSQL and the CI-pinned S3 emulator enabled, the serial service groups eliminated the observed advisory-lock failures and all 14 S3 cases passed. The 247-case service selection finished with 246 passes and one PostgreSQL scale-test failure at its existing one-second assertion; that case passed focused controls under both Cargo and nextest. These outcomes do not establish universal timing stability. Subprocess primary-stop classification remains a separate [follow-up](https://github.com/agentscm/agentscm/issues/49). The hosted Linux Cargo baseline took 406 seconds in the test step; the corresponding nextest CI result must be assessed on the PR rather than inferred from the macOS observations.
