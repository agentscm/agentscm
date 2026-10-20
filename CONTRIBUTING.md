# Contributing

AgentSCM is alpha software. Small, focused pull requests are welcome; for anything large, open an issue first so we can agree on the shape. Maintainers merge.

## Set up

Users run the container image (see the [README](README.md#quickstart)). To work on the code you need `git` 2.45.0 or newer and the Rust toolchain pinned in `rust-toolchain.toml` (installed by `rustup` on first use; it is also the minimum supported Rust version). The Git snapshot bridge enforces that Git minimum before it inspects a repository.

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo nextest run --locked
cargo test --locked --doc
```

Install the pinned nextest version and read the focused-validation, coverage-audit and timing instructions in [contributor testing](docs/contributor-testing.md). Use synthetic fixtures only. Some tests also run against PostgreSQL (`AGENTSCM_TEST_DATABASE_URL`) or an S3-compatible server (`AGENTSCM_TEST_S3_*`); they are skipped locally when unset and required in Linux CI. Read [STYLE.md](STYLE.md), [docs/README.md](docs/README.md) and [docs/architecture.md](docs/architecture.md) before larger changes.

## Pull requests

- Work on a branch and open a pull request against `main`. Do not request reviewers; maintainers will pick it up.
- Say what changed, why, how you tested it, and what is not covered. State unsupported behavior instead of hiding it.
- Keep commit messages factual: the behavior, the reason and any limit. Do not include session links, transcripts, secrets or personal data anywhere in a commit or pull request.
- Signing your commits is welcome but not required. Maintainers sign theirs, and squash merges are signed by GitHub.
- CI must pass: formatting, clippy with warnings denied, the nextest suite, separate doctests, and a container build and smoke test. For pull requests from forks the formatting, clippy and test job runs automatically; a maintainer starts the container and end-to-end jobs by adding the `ci-extended` label after reading the change. Do not weaken CI, signing or branch protections in a change that is not about them.

## AI and agent assistance

AgentSCM exists to coordinate coding agents, so agent-written pull requests are welcome, with these rules:

- A named person is the author of record. They have read the whole diff, can explain it, and have run the checks. Unread bulk or autonomous sweeps are closed without review.
- Disclose assistance in the pull request template: none, assisted, or agent-authored, which tool, and what you verified yourself.
- You may add an `Assisted-by: <tool or model>` trailer to commits. Do not use `Co-authored-by` for tools; it is for people.
- Agents never merge their own work, and never edit CI, release, signing or agent-instruction files (`AGENTS.md`, `CLAUDE.md`) without a maintainer's sign-off.
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
