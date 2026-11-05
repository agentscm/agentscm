# Contributing

AgentSCM is alpha software. Start with the [documentation index](docs/README.md) for user and contributor guides. Small, focused pull requests are welcome; for anything large, open an issue first so we can agree on the shape. Maintainers merge.

## Set up

Users install a release binary with `npx agentscm` (see the [README](README.md#quickstart)). To work on the code you need `git` 2.45.0 or newer and the Rust toolchain pinned in `rust-toolchain.toml` (installed by `rustup` on first use; it is also the minimum supported Rust version). The Git snapshot bridge enforces that Git minimum before it inspects a repository.

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo nextest run --locked
cargo test --locked --doc
```

Install the pinned nextest version and read the focused-validation, coverage-audit and timing instructions in [contributor testing](docs/contributor-testing.md). Use synthetic fixtures only. Some tests also run against PostgreSQL (`AGENTSCM_TEST_DATABASE_URL`) or an S3-compatible server (`AGENTSCM_TEST_S3_*`); they are skipped locally when unset and required by the signed local policy and hosted Linux fallback. Read [STYLE.md](STYLE.md), [docs/README.md](docs/README.md) and [docs/architecture.md](docs/architecture.md) before larger changes.

## Pull requests

- Work on a branch and open a pull request against `main`. Do not request reviewers; maintainers will pick it up.
- Say what changed, why, how you tested it, and what is not covered. State unsupported behavior instead of hiding it.
- Do not edit `docs/roadmap.md` in a feature pull request; nearly every change touches it, so per-PR edits conflict and each conflict forces another full validation. Instead, add a `Roadmap:` line to the pull request body for each roadmap row the change affects, with the row and its new status, or `Roadmap: none`. The roadmap is updated from those lines in one pull request before each release ([releasing](docs/releasing.md)). Between releases it can understate merged work, but it must never describe proposed work as implemented.
- Keep commit messages factual: the behavior, the reason and any limit. Do not include session links, transcripts, secrets or personal data anywhere in a commit or pull request.
- Signing your commits is welcome but not required. Maintainers sign theirs, and squash merges are signed by GitHub.
- Complete the local validation gate: formatting, Clippy with warnings denied, nextest, separate doctests and the path-selected audit, workflow, installer, UI, documentation and release-metadata checks in [contributor testing](docs/contributor-testing.md#routine-local-checks). The `main` ruleset requires the `agentscm/local-validation` status from a maintainer's signed [local validation attestation](docs/advanced/local-validation.md), alongside `Conventional PR title`; the existing administrator bypass remains operator-owned. Routine hosted lint, audit and tests run only by manual dispatch, including after a merge to `main`. Windows installer tests retain automatic path-selected coverage; `Container` runs on every push to `main` and live execution on matching pushes; Windows, musl and macOS release-target builds and Hub end-to-end run nightly on `main` and by manual dispatch, which release automation does once on the release branch; Hub end-to-end also starts on a same-repository pull request that changes a PostgreSQL migration, `src/hub/store_pg.rs`, `src/hub/store_pg/` or its harness. Documentation publishing, release automation, PR-title validation and local-evidence verification remain hosted. Contributors without a trusted signing path request maintainer help or, on a same-repository branch, dispatch the hosted workflows with `gh workflow run ci.yml --ref <branch>` (a fork author dispatches in their own fork or asks a maintainer to adopt the change); hosted tests alone do not satisfy the required attestation check. Never attest fork code on a signing host. Do not weaken CI, signing or branch protections in a change that is not about them.

## AI and agent assistance

AgentSCM exists to coordinate coding agents, so agent-written pull requests are welcome, with these rules:

- A named person is the author of record. They have read the whole diff, can explain it, and have run the checks. Unread bulk or autonomous sweeps are closed without review.
- Disclose assistance in the pull request template: none, assisted, or agent-authored, which tool, and what you verified yourself.
- You may add an `Assisted-by: <tool or model>` trailer to commits. Do not use `Co-authored-by` for tools; it is for people.
- Agents merge only under the interim rule in [AGENTS.md](AGENTS.md#rules): their own owner's non-draft PR, when GitHub reports it clean, with no `--admin` or other bypass, and never while another owner's open PR touches the same files.
- A 500 or a timeout on an agent's push or merge means the outcome is unknown: re-read the remote ref or the PR before retrying, and never force-push to recover.
- Agents never edit CI, release, signing or agent-instruction files (`AGENTS.md`, `CLAUDE.md`) without a maintainer's sign-off.
- Agent instructions for this repository are in [AGENTS.md](AGENTS.md).

## Licensing

AgentSCM is licensed under FSL-1.1-ALv2 (see LICENSE). By submitting a
contribution, you license it to DUVATL, Inc. and to every recipient of
AgentSCM under those same terms. You also grant DUVATL, Inc. a perpetual,
worldwide, non-exclusive, royalty-free, irrevocable license to distribute
your contribution under any license approved by the Open Source Initiative.
You confirm that you have the right to submit the contribution. We do not
require a separate contributor license agreement or a Signed-off-by line.

## Security and conduct

Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md). Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md).

## Release titles and publishing

Use `<type>[optional scope][!]: <subject>` for PR titles (at most 72 characters), for example `fix: Restore Hub reconnect` or `feat(cli): Add status output`. The Conventional PR title check runs again when a title is edited. Squash-merge using the PR title as the commit subject: release automation reads the merged subjects.

`fix:` and `perf:` request a patch; `feat:` requests a minor; `!` or a `BREAKING CHANGE:` footer requests a breaking release (a minor increment while the major version is zero, otherwise a major increment, following Cargo semantics). Other allowed types do not normally request a release. Version numbers belong in manifests, changelogs, tags and release notes, never in PR titles, bodies or comments. Release automation reads merged commit subjects and Cargo metadata; do not use `Release-As` footers in PR text.

Release-plz runs in Git-only mode and never publishes to crates.io. It prepares Rust versions and changelogs; the release-files tool synchronizes the npm manifest and the separate dev-seed lockfile. Automation opens one version-free release PR at a time and does not rewrite an existing candidate. Maintainers review and merge through normal CI. Only a merged release-plz branch can create a release tag. The existing Release workflow builds and qualifies artifacts and publishes npm behind its environment approval. Failed publishing needs investigation and a rerun; never move a published tag.

Repository setup: configure squash merges with PR-title commit subjects and make `Conventional PR title` a required check on main. Install Counterbranch Releases on this repository with Contents, Issues and Pull requests read/write permissions, and store its private key in the repository Actions secret `RELEASE_APP_PRIVATE_KEY`. The workflow generates a short-lived app token scoped to this repository; no personal token is required. Allow the app to create release tags without allowing tag updates/deletions or bypassing main's protections. The built-in `GITHUB_TOKEN` is unsuitable because its generated PRs and tags do not trigger downstream workflows. Keep the npm environment approval and existing release protections. These settings and credentials are not installed by committing workflow files; see [releasing](docs/releasing.md).
