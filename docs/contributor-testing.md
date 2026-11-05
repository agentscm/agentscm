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

On macOS or Linux, also compile the Windows build before pushing a change that touches `#[cfg(unix)]` code or calls into a Unix-only module (`client_plugin`, `client_setup`, `tray` and others). The gate above only compiles the host platform, so these mistakes otherwise surface only in the `windows` CI job. `tools/windows-check.sh` compiles what that job compiles, with warnings as errors, and checks the library, the binary and `--test integration`; the Windows test set is the modules in `tests/integration/main.rs` declared without `#[cfg(unix)]`. It needs the target and a MinGW C compiler once:

```sh
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64   # Debian/Ubuntu: apt install mingw-w64
tools/windows-check.sh
```

It checks compilation only; Windows test behaviour is still CI's job. It builds into `target/x86_64-pc-windows-gnu`, which takes several GB; delete it with the rest of `target` when space is tight.

`.agents/bound-target.sh` runs `cargo clean` in the main and current checkout when its `target` exceeds `AGENTSCM_TARGET_LIMIT_GB` GiB (default 30). `/implement` runs it; manual workflows can run it directly, or with `-n` to only report what it would clean.

`[profile.test]` in `Cargo.toml` sets `incremental = false`, so test builds (nextest, doctests and local validation) do not write `target/debug/incremental`. A gate run builds each revision once, and before this setting incremental state was about 60% of the gate's `target` size. `cargo clippy` and interactive `cargo build`/`cargo check` use the dev profile and still keep incremental state.

The nextest profile disables fail-fast and retries. It writes JUnit output to `target/nextest/default/junit.xml`, including ignored tests as skipped rather than treating them as executed. The `backend_s3` tests (the `backend_s3` module of the `integration` test binary) are limited to one process because its tests share an in-process `CreateBucket` mutex. PostgreSQL fixture tests are also limited to one process because independently named schemas still share fixed database-wide migration and event advisory locks; the store's five-second lock timeout can otherwise expire while another test holds one of those locks. These limits do not serialize unrelated tests, internal concurrency within one test or separate nextest invocations; do not run multiple suites against the same bucket or PostgreSQL database concurrently.

The `two_git_worktrees_*` cases in the `hub_workspace_execution` module share a test group
limited to two concurrent processes. These cases each run a Hub, Git worktrees,
collectors and subprocess fixtures; overlapping the whole family can exhaust the
local fixture's existing HTTP deadline. The group bounds that resource load while
preserving the tests' deadlines, assertions and zero-retry policy. Other cases in
the `hub_workspace_execution` module remain outside this group.

Quickstart's Docker fixture uses a tracked executable launcher and per-test script
data under its temporary HOME. The Hub workflow fixture uses the tracked Python
Docker fixture, with a sidecar beside each fake Unix socket selecting that daemon's
state directory. Keep those tracked fixtures executable; rewriting a new executable
for each test can add host startup latency to the existing probe deadlines.

## Integration test layout

Top-level integration tests are one test binary, `integration`, built from `tests/integration/main.rs`; the crate links once instead of once per file. Add a new integration test as `tests/integration/<name>.rs` and declare it in `main.rs` with `mod <name>;` (a guard test fails when a file is not declared). Tests are named `<name>::<test>`, so select them with `-E 'test(/^<name>::/)'` under nextest, or `cargo test --locked --test integration <name>::` under Cargo. Cargo's filter is a substring match (`sync::` also selects `hub_sync::`), while `-E 'test(/^<name>::/)'` selects exactly one module. Helpers shared by several modules are declared once in `main.rs` and imported with `use crate::<helper>;`; `tests/support/` holds fixture sources and scripts that modules include or run, and `tests/fixtures/` holds data. Only the modules in `main.rs` without `#[cfg(unix)]` build on Windows, and the `windows` CI job runs them with `--test integration`. A file placed directly in `tests/` still builds as its own binary, which is rarely wanted. The one deliberate exception is `tests/setup.rs` (select it with `-E 'binary(setup)'`): its tests package the running test executable, which must stay under the 128 MiB bundle limit, so it cannot be part of the large `integration` binary.

## Focused iteration

During implementation, select tests from the behavior that changed, its callers and its failure paths. A filename or similarly named test alone is not enough evidence. Nextest filter expressions make the selected scope explicit:

```sh
cargo nextest run --locked -E 'test(/^git_collisions::/)'
```

Record the command and the selected scope. For a two-line deletion of test diagnostics, rerun the affected tests during iteration and retain unchanged check evidence with its original scope. For behavior changes, include callers and failure paths. Focused checks support iteration; they do not replace final whole-branch validation or waive the applicable review policy.

For a maintainer with a signing key, the attestation from `tools/attest-pr.sh` satisfies the final whole-branch validation above, subject to the evidence rules in this section. When the recipe selects the Rust gate it is the one full suite run of a head that is ready to merge; an attestation of a head confined to the recipe's excluded paths (the UI and design paths and the Markdown documentation listed under [routine local checks](#routine-local-checks)) runs no Rust commands, while files such as `ui/package.json` and `ui/vite.config.ts` select the Rust gate. Before pushing, for Rust, manifest, toolchain or build changes, run `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, `git diff --check` and focused nextest filters for the changed behavior, its callers and failure paths. A contributor without a signing key, whose change has any path outside the recipe's excluded set, runs all four Rust commands under one lock on the head to be merged: `tools/attest-pr.sh --with-lock sh -c 'cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings && cargo nextest run --locked && cargo test --locked --doc'`. They report the results as unsigned and, for a same-repository branch, may dispatch the hosted workflows with `gh workflow run ci.yml --ref <branch>` and match the run's `headSha` to the PR head; the head stays not ready until it has an accepted attestation. A head confined to the excluded paths runs none of them. Applicable reviews remain required. A whole-branch gate that you run by hand may satisfy a later workflow step without rerunning it only when its exact source and test inputs, parent/candidate mapping, `Cargo.lock`, toolchain, feature flags, nextest version and configuration, and service environment remain applicable and unchanged. Retain the original commands and results, the tested snapshot and environment, and a comparison showing that the later candidate matches them; a workflow-step or command-invocation label alone is not evidence of reuse.

Any later change invalidates the evidence for affected checks. Rerun those checks and complete the required final validation before the head is reported ready. That validation is an accepted attestation; a hand-run gate's result is reported as unsigned and does not make the head ready. Changes to the test runner or its configuration always require the full nextest suite, separate doctests and corresponding documentation checks even when the configuration diff is small.

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

A maintainer runs and posts a signed record for the exact PR head with `tools/attest-pr.sh`; see the [concrete signing workflow](advanced/local-validation.md#run-verify-and-post). The operator-managed `main` ruleset requires the resulting `agentscm/local-validation` status alongside `Conventional PR title`, retaining the existing administrator bypass. This experimental attestation does not replace the evidence rules above. The signed recipe includes the Rust gate, which is skipped only for changes confined to the excluded UI and design paths, and the path-selected routine checks below. When a PR changes that recipe, the accepted pre-merge attestation uses the current default branch's trusted configuration; qualify candidate-recipe changes separately. Routine hosted jobs require manual dispatch, and their success does not satisfy the signature gate. An ordinary push to `main` does not repeat the local gate; `Container` builds and smoke-tests the image on every push to `main`. The `CI` release-target builds (musl and macOS) and `Windows` run nightly on `main` and by manual dispatch, which release automation does once on the release branch. The trade-off is that breakage on those platforms can surface up to a day later and without a per-commit result, so a nightly failure may need bisecting. GitHub runs schedules on the default branch only, can delay or drop them under load, and disables them in a public repository after 60 days without repository activity. GitHub sends a scheduled-workflow failure notification only to the user who last changed the cron line, subject to that user's settings. There is no shared "main red" signal (that is [proposed](roadmap.md)), so check the run lists. See [hosted routing and fallback](advanced/local-validation.md#hosted-ci-and-required-checks).

Hub container end-to-end is a [local release-only qualification](releasing.md#local-hub-release-qualification), outside the routine PR gate and signed recipe. Both profiles must pass for the clean committed candidate with retained evidence; absent, failed or stale evidence blocks release readiness. A green PR does not establish that these scenarios executed.

### Dedicated attestation host

An always-on machine can run `tools/attest-pr.sh` instead of a laptop. It needs its own signing key, enrolled by a reviewed change to `.github/local-validation/registry.json`. Recipe commands run as the signing user, and that user can read the key and credentials, so the host may attest only code trusted on it, never fork code (see the [trust boundary](advanced/local-validation.md#trust-boundary)). No such host is set up yet.

## Routine local checks

Use the existing `agentscm local-validation` workflow for signed runs; [the maintainer steps](advanced/local-validation.md#maintainer-workflow) explain enrollment, the clean-checkout requirement and posting. It runs the commands in `.github/local-validation/recipe.json` in order and signs failures too. The four Rust commands at the top of this page run unless every changed path is under the excluded set: `DESIGN.md`, `ui/src`, `ui/public`, `ui/tests/unit`, `ui/tests/integration` and `ui/tests/e2e`; Markdown under `docs/`, `.agents/` and `.github/`; `docs-site`; and the top-level Markdown files that no Rust code reads (`AGENTS.md`, `CHANGELOG.md`, `CLAUDE.md`, `CODE_OF_CONDUCT.md`, `CONTRIBUTING.md`, `PRODUCT.md`, `README.md`, `SECURITY.md`, `STYLE.md` and `TRADEMARKS.md`). Any other path selects them, including a new one such as a new top-level `ui/` file, and the files Rust compiles in from outside `src`, such as `THIRD_PARTY_NOTICES.md`, `docs/assets/agentscm-icon.png` and `docs/openapi-work-insights.json`. A documentation-only head therefore runs only the documentation checks. A new Rust read of an excluded path (for example an `include_str!` or a test fixture) must remove that path from the recipe's excluded list. An attestation of a head confined to the excluded paths therefore does not exercise the `--features ui` embedding build: the nextest argv has no feature flags, so the gate never embeds `ui/dist`, and the `Container` workflow covers that build on `main`. Additional groups run when their declared paths changed from the merge-base; changing the local-validation configuration or verifier workflow forces every command. Missing tools fail a required command; they are never treated as a successful skip.

Install the pinned Rust toolchain, Git 2.45 or newer, nextest 0.9.148, cargo-deny 0.20.2, zizmor 1.21.0 and ShellCheck 0.11.0. Use Node 24 with npm for the local UI, documentation and installer checks; Node 24.21.0 is the local qualification baseline. Hosted UI/installer fallbacks retain Node 22, and documentation publishing uses its separately pinned Node version. Toolchain probes record versions but do not enforce these installation pins. For example:

```sh
cargo install cargo-deny --version 0.20.2 --locked
export AGENTSCM_CI_TOOLS="$(mktemp -d)"
python3 -m venv "$AGENTSCM_CI_TOOLS"
"$AGENTSCM_CI_TOOLS/bin/pip" install zizmor==1.21.0
export PATH="$AGENTSCM_CI_TOOLS/bin:$PATH"
```

Keep tool environments outside the tracked source or ignored locally; do not commit them. Install ShellCheck 0.11.0 through its official distribution or a package manager that supplies that version. PostgreSQL and S3 services are required for an accepted attestation under this repository's policy. The runner passes an allowlisted environment, so put the installed executables on `PATH` before invoking it. It is trusted-local execution, not a sandbox for unreviewed contributions.

| Group | Local commands and selection |
| --- | --- |
| Dependency audit | `cargo deny --all-features --config deny.toml check` for root manifest/lock/config changes; `cargo deny --manifest-path tools/portable-verifier/Cargo.toml --all-features --config deny.toml check` for the portable verifier. A deny configuration or toolchain change selects both. |
| Workflow security | `zizmor --offline --no-progress .github/workflows` for `.github/**` or zizmor configuration changes. Offline mode omits GitHub API-dependent audits and uploads no SARIF. |
| Installer | `shellcheck -s sh install.sh` for shell installer changes; `npm --prefix packages/npm test` for installer, package or release-installer changes. |
| UI | `npm --prefix ui ci --ignore-scripts`, then `npm --prefix ui run lint`, `npm --prefix ui run build` and `npm --prefix ui run notices -- --check` for UI or notice changes. |
| Documentation | `npm --prefix docs-site ci --ignore-scripts`, then `npm --prefix docs-site audit --audit-level=low`, `npm --prefix docs-site run build` and `npm --prefix docs-site run check` for docs/site changes. |
| Release metadata | Rustfmt checks `tools/release-files.rs`; rustc builds and runs its tests and `--check` tool under `target/local-validation`. Manifest, lockfile, helper, toolchain and release-workflow changes select this group. Exact argv and ordering are in the recipe. |

The recipe is the authoritative command/selector list, including workflow-path triggers omitted from this table. Individual commands can also be run directly during iteration without a signing key; retain their actual results and omissions. A direct run is not a signed attestation.

Run both dependency audit commands periodically even when no source changes select them: newly published advisories do not change the Git diff. There is no automatic weekly hosted audit. Audit results describe the advisory database at run time, not a continuously monitored guarantee.

### CodeQL repository setting

The local Rust Clippy gate, cargo-deny dependency audit and offline zizmor workflow lint are narrower checks than CodeQL. They do not provide equivalent multi-language or dataflow security analysis. CodeQL default setup is a GitHub repository setting, separate from tracked workflow files. Retiring it requires the maintainer to disable that setting and verify the readback; a merged documentation or workflow change alone does not disable it. With repository administration permission, inspect, disable or restore default setup using GitHub's [code-scanning API](https://docs.github.com/en/rest/code-scanning/code-scanning#update-a-code-scanning-default-setup-configuration):

```sh
gh api repos/agentscm/agentscm/code-scanning/default-setup
gh api --method PATCH repos/agentscm/agentscm/code-scanning/default-setup -f state=not-configured
gh api repos/agentscm/agentscm/code-scanning/default-setup
# Restore default setup when explicitly desired:
gh api --method PATCH repos/agentscm/agentscm/code-scanning/default-setup -f state=configured
```

Readback confirms configuration, not cancellation of already-running scans or measured runtime/cost savings. This change does not qualify private-repository billing or a replacement SAST engine.

## Resource audit boundaries

At the revision where nextest was introduced, the audit found temporary-directory isolation, ephemeral ports, random PostgreSQL schemas and process-local telemetry state. Those patterns support concurrent execution but do not prove every timing interaction is safe. A source and inventory audit found 109 PostgreSQL fixture tests across ten integration modules. Every fixture test uses one of four forms after its module prefix, selected by the `hub-postgres` group: `<module>::on_postgres::*`, `*::postgres`, `<module>::postgres_*` or `*_on_postgres`. Future tests that create a `PgSchema` must retain one of those forms. The similarly named SQLite test `on_sqlite::postgres_read_briefings_share_the_repository_maintenance_lock` is deliberately outside the group.

The PostgreSQL limit serializes tests that share one database's global advisory locks. It is not a connection-quota workaround and does not replace schema isolation. The hosted-laptop tests retain a process mutex, and repeated RSA key generation may affect runtime; both need empirical observation during qualification. The S3 backend keeps its single-process nextest group because its existing bucket-creation mutex is process-local.

Nextest overlaps different test binaries, increasing peak filesystem and subprocess activity compared with Cargo's binary-at-a-time ordering. Qualification exposed two timing-sensitive fixtures. The Git descendant test launcher now avoids directly executing newly written scripts while retaining the tests' output/timeout limits and late-write assertions. The separate `Io` failure and primary-stop classification weakness remain under investigation in [issue 49](https://github.com/agentscm/agentscm/issues/49) and may recur. The checkpoint crash test now uses a small payload, pauses at a test-only post-commit durable-pending boundary, confirms readiness and SIGKILLs the child before recovery; a separate recovery test retains the 32 MiB case through manually staged rows. The crash test does not establish 32 MiB checkpoint staging under a crash or a kill during the middle of checkpoint finishing. Neither fixture receives an exclusive scheduler slot. The crash test and its subprocess helper moved from the `workspace` integration binary into `workspace::tests` in the library; account for those two name changes when comparing inventories across the adoption.

This nextest adoption covers the documented contributor gate and the Linux CI test job. Windows and execution-live workflows keep their existing runners because equivalent nextest behavior has not been qualified there. No sharding is enabled.

## Initial qualification observations

The initial comparison used identical Rust/test inputs at `5e586acea5eb3f547c4a170685f7ce24fcce0c25`, Rust 1.96.0 and nextest 0.9.148 on one shared macOS arm64 host with 14 logical CPUs. Cargo artifacts were warm after a separate 64.27-second build. These are single observations with host contention, not a general speedup claim or measurements of the later fixture corrections:

| Selection | Cargo | Nextest | Coverage and result |
| --- | --- | --- | --- |
| Full discovered suite | 771.20 s | 397.33 s | 1,884 discovered, 52 ignored; Cargo had 42 failures, nextest had none; service bodies could return early |
| Same 42 tests in five integration binaries | 28.71 s | 17.14 s | Both passed all 42; same source and configuration for this pair |

The table durations and the later 457.46-second result are command wall-clock measurements, not nextest's internal summary. The initial inventories matched every test and ignored flag. Separate doctests passed with zero cases. A later full nextest run exposed the two fixture failures described above; its 457.46-second result was not clean. Corrected fixtures passed 20 isolated output-limit repetitions and five checkpoint-crash repetitions before final whole-branch validation.

With local PostgreSQL and the CI-pinned S3 emulator enabled, the serial service groups eliminated the observed advisory-lock failures and all 14 S3 cases passed. The 247-case service selection finished with 246 passes and one PostgreSQL scale-test failure at its existing one-second assertion; that case passed focused controls under both Cargo and nextest. These outcomes do not establish universal timing stability. Subprocess primary-stop classification remains a separate [follow-up](https://github.com/agentscm/agentscm/issues/49). The hosted Linux Cargo baseline took 406 seconds in the test step; the corresponding nextest CI result must be assessed on the PR rather than inferred from the macOS observations.

## Staged selected tests and merge batches

Optional recipe test mappings and a GitHub merge-batch adapter are implemented but not activated or live-qualified. The current repository recipe still requires the full Rust gate. Follow [selected tests and activation](advanced/local-validation.md#selected-tests-and-merge-batches-staged-unqualified) before using reduced final PR checks: a compatible published producer/verifier and an enforced full-suite merge-group gate are prerequisites. Selected PR evidence is never evidence for combined source. The unsigned contributor fallback and release-only Hub qualification above remain applicable during bootstrap.
