# Local validation attestations

> **Experimental (alpha).** Unix only, GitHub only, trusted-local. One maintainer key is enrolled. The verifier workflow was qualified before merge by running it under `act` against live GitHub on pull request #86: a signed failure (`failure` on that head), then a fix and a signed pass (`success` on the new head). Tampered, wrong-repository and other-head attestations were checked with `local-validation verify` against the same live comment bodies and gave `pending`. That workflow has since been replaced by `agentscm local-validation reconcile`, which is tested only against a fake GitHub; its first live run, on GitHub-hosted Actions, happens after merge. Contributors without an enrolled key or local PostgreSQL and S3 request `ci-extended` for hosted test coverage.

A maintainer can run the repository's gate on their own machine and attach a signed, Git-bound record of the result to a pull request. A separate verifier workflow checks that record and publishes the `agentscm/local-validation` commit status. For pull requests from branches in this repository, this attestation is the expected evidence for the test gate: the expensive hosted suites no longer run on ordinary updates. See [Hosted CI and required checks](#hosted-ci-and-required-checks).

## What exists

- `agentscm local-validation run|show|post|verify|reconcile`: see the [CLI reference](../reference/cli.md#local-validation-attestations). `run` refuses a dirty tree, runs `.github/local-validation/recipe.json` with an allowlisted environment (see the trust boundary), retains each command's output as `<sha>.cmd<N>.stdout|stderr` in the state directory, and signs a portable-evidence-v1 envelope plus a `record` artifact bound to the repository, commit, tree, base ref and merge-base. Failed runs are signed too.
- Repository configuration in `.github/local-validation/`: `recipe.json` (the commands from `AGENTS.md`), `policy.json` (repository ID, recipe digest, required PostgreSQL and S3 services, 30-day maximum age, epoch) and `registry.json` (issuer keys). A test fails when `policy.json` drifts from `recipe.json`.
- Optional recipe fields for non-Rust repositories: `dependency_files` (relative paths, 1 to 16, no duplicates, only letters, digits and `-_.:/`; each is digested at the commit and bound into the signed evidence as a dependency, and a missing file fails `run` before any command runs) and `toolchain_probes` (argv lists, 1 to 15, each recorded as its first output line or `<argv>: unavailable`). When absent they default to `Cargo.lock`, `rust-toolchain.toml` and `rustc -V`, `cargo -V`, `cargo nextest --version`; `git --version` is always appended. Declaring either field changes the recipe digest, so the policy must pin the new digest.
- `agentscm local-validation reconcile` is the verifier. It reads `policy.json`, `recipe.json` and `registry.json` from `--config-dir` (default `.github/local-validation`) in the default-branch checkout `--repo`, and the token from the environment variable named by `--token-env` (default `GH_TOKEN`). It lists every open pull request whose base is `--base`; one with a trusted attestation comment (`OWNER`, `MEMBER` or `COLLABORATOR`, starting with the marker), or with an existing `agentscm/local-validation` status on its head, is opted in, and the rest are left alone. It sets `pending` on every opted-in head first, so a crash can leave `pending` but not a stale `success`. It then fetches each head from `origin` as Git data without credentials, verifies the signed comments and, immediately before each write, re-reads the open pull requests and the trusted comments (id and update time); if the head, base or comments changed it writes nothing and a newer event owns the verdict. It posts `success`, `failure`, `error` or `pending` on exactly the verified head SHA. A head shared by several open pull requests gets `pending` with a reason saying it is unsupported. A deleted attestation comment, or an edit that removes the marker, leaves no applicable attestation and so replaces the old verdict with `pending`. An incomplete listing of pull requests, comments or statuses fails closed: nothing is written for that read. It exits 0 when every pull request was handled, 1 on any error, unread input or auth or rate-limit stop, and 2 when the policy's repository is not `--github-repo` (nothing written).
- `.github/workflows/local-validation.yml` is a thin wrapper that builds `agentscm` from the default branch and runs `reconcile`. It runs on a PR comment that starts with the attestation marker from an `OWNER`, `MEMBER` or `COLLABORATOR` (created, edited or deleted; an edit that removes the marker also runs it; other commenters are ignored and the signature still decides), on pushes to `main` that touch `src/**`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `.github/local-validation/**` or the workflow itself, and daily. There is no manual dispatch. All runs share one repository-wide concurrency group (`local-validation`, queued rather than cancelled) and every run reconciles all open pull requests on the default branch, whatever triggered it. It never checks out, builds or runs PR code.
- Each opted-in head costs two statuses per run (the `pending` and the verdict). GitHub keeps at most 1000 statuses per SHA and context, so a head that is re-verified daily reaches that ceiling only after years.

## Compact comment display and compatibility

`local-validation show` renders a compact comment with the AgentSCM icon, reported outcome, abbreviated source and coverage summary visible at first. Coverage omissions, source and environment fields, command results and the signed machine-readable evidence are four closed disclosure sections, so supporting Markdown renderers show them collapsed by default. Missing PostgreSQL or S3 coverage remains visible in the summary.

The summary and disclosure markup are display only. The exact signed envelope and record bytes remain in their named fences, and the first-line marker still commits to the envelope digest. `verify` extracts those exact bytes while ignoring surrounding Markdown, so the compact layout does not change signatures, verification or adoption of legacy flat comments.

To replace an older flat presentation, rerun `local-validation post` for the same retained attestation. On GitHub it edits the posting user's existing comment with the same marker in place instead of creating another comment. Posting and the hosted verifier workflow are GitHub-only. The Markdown carrier and standalone verifier are forge-neutral, but no GitLab posting or verifier integration exists, and GitLab rendering or sanitization of this presentation has not been qualified.

## Trust boundary

- A valid signature proves only that the holder of a registered key signed bytes bound to that commit, tree and base. The execution basis is `worker_asserted`: nothing proves the commands ran as recorded.
- The signing user account and host are trusted. Recipe commands run as the signing user with `HOME` passed, so attested code can read `~/.config/gh`, the signing key, cargo configuration such as `.cargo/config.toml` aliases and anything else that user can read. The environment allowlist only avoids passing tokens through environment variables; it is not a sandbox. Never attest code you would not run as that user, and never attest third-party or fork code on a signing host.
- Anyone with `statuses: write` can post the same status context. Required-status configuration is operator-owned, and nothing here changes branch protection.
- Policy, recipe and registry are read from the default branch, not from the PR.
- This repository's policy requires the PostgreSQL and S3 test services (`AGENTSCM_TEST_DATABASE_URL`, `AGENTSCM_TEST_S3_*`). A run without a service the policy requires is recorded and verifies as a failure; a service the policy does not require that is absent is listed as a coverage omission only.

## Maintainer workflow

1. Create a key with `agentscm agent-task key-generate` (owner-private file).
2. Enroll the public key in `.github/local-validation/registry.json` by a reviewed pull request. Only enrolled, unrevoked and unexpired keys verify; revoke by setting `revoked_at_ms` in a reviewed change.
3. On a clean checkout of the PR head, run `git fetch origin main` (the merge-base is computed against the local `origin/main`), start the PostgreSQL and S3 test services, then run `agentscm local-validation run --recipe .github/local-validation/recipe.json --policy .github/local-validation/policy.json --registry .github/local-validation/registry.json --key-file KEY --issuer-id ID --state-dir DIR`. `run` has no defaults for the recipe, policy or registry; pass all three. Note the printed envelope digest. Ctrl-C (SIGINT) cancels the running command's process group and signs a failure that records the command as `cancelled`; a second Ctrl-C exits immediately.
4. `local-validation post` the attestation to the PR (it refuses if the PR head differs from the attested commit). Do not post statuses by hand.
5. The verifier workflow reports the status. If it cannot read or write something it exits 1 and leaves that head `pending` or `error`; the next comment, push or daily run retries, or re-run the failed job. Report `pending` or `failure` as it is.

`verify` is forge-neutral and also runs standalone against a local Git checkout; its `--recipe` default, `.github/local-validation/recipe.json`, is relative to the current directory. `reconcile` fetches PR heads from `origin` without credentials, so it supports public repositories only. The standalone `evidence-verifier` tool has not been exercised on these envelopes.

## Hosted CI and required checks

Automatic on every pull request update: `CI / static` (formatting and clippy), dependency and workflow-security checks, and the installer and UI workflows when their paths match. The test suites depend on where the branch lives:

- Same-repository pull requests: the `CI / tests` job (hosted PostgreSQL and S3), Windows, container, hub end-to-end and live execution run only while the pull request has the `ci-extended` label, on labeling and on each later update. The attestation replaces them otherwise. If you have no enrolled key or services, say so in the pull request and request `ci-extended` instead of claiming coverage. Adding any other label later records skipped label-gated checks on the same head, so read the `ci-extended` workflow runs themselves (or remove and re-add the label) rather than the pull request's checks list.
- Fork pull requests cannot carry an attestation. `CI / tests` stays automatic; a maintainer adds `ci-extended` after reading the change to run the extended jobs once against that head.
- Pushes to `main` still run every suite, and `release.yml` independently rebuilds and smoke-tests the tagged commit, so release provenance is unchanged.
- To request hosted coverage on demand, add `ci-extended` or run the workflow with `workflow_dispatch` on the branch.

The `main` ruleset has no required status checks today (it requires no deletion, no non-fast-forward, signed commits and a pull request). Skipped jobs report as skipped and path-filtered workflows may not report at all. If a requirement is added, require only `CI / static` and `agentscm/local-validation`; never require a label-gated job.

## What invalidates an attestation

- A new head commit or tree, or a different merge-base.
- A change to the policy, recipe or registry. The verifier re-runs on pushes to `main` that touch the verifier inputs, and daily, so an aged-out attestation's `success` is replaced. A run writes nothing for a head if the pull request's head, base or trusted attestation comments changed while it ran; the newer run owns the verdict.
- Age beyond the policy maximum.

## Limits

- The status is attached to the commit, so PRs that share a head commit (for example one branch opened against two bases) share it; that case is unsupported.
- Retargeting a PR to a new base fires no event. The next comment, push or daily run refreshes it.
- Pull requests are read in bounded pages (30 pages of open pull requests, 10 of comments per pull request); a larger listing fails closed with exit 1. Runs process pull requests serially within the 30-minute job and the token rate limit, which is practical for tens of open PRs, not hundreds.
- An aged-out (expired) `success` is replaced by the next run, at the latest the daily one, so within about a day of expiry.

Advancing the base branch tip does not invalidate it; only the merge-base is bound. The base tip is deliberately not bound, and the tests ran on the PR head, not on the merge result. The maintainers accept this limit.

## Coverage omissions

Every attestation records these as not covered: ignored tests, Windows, container smoke, hub end-to-end and live execution. Hosted CI remains the way to get platform and runtime coverage; request it as described above.

## Not done

Hosted jobs are gated on the `ci-extended` label, not on the presence of an attestation, so a pull request with neither runs no hosted test suite. The verifier status is not a required check, `reconcile` is tested only against a fake GitHub, and its first live run, on GitHub-hosted Actions, is after merge.

## Adopting in another repository

This is a design for adoption, not a qualified one: no repository other than this one uses it, and `reconcile` has had no live run. Only public repositories work, because heads are fetched without credentials.

1. Add `.github/local-validation/policy.json` with `repository` (`github.com/OWNER/NAME`), `recipe_sha256`, `required_services` set to `{"postgres":false,"s3":false}`, `max_age_seconds` and `epoch`, and `registry.json` with the maintainer's enrolled public key. Create keys and enroll them as in the maintainer workflow above.
2. Add a recipe. For a Bun repository:

```json
{
  "recipe_id": "bun-gate-v1",
  "dependency_files": ["bun.lock", "package.json"],
  "toolchain_probes": [["bun", "--version"]],
  "commands": [{ "id": "test", "argv": ["bun", "test"], "timeout_secs": 600 }]
}
```

Set the policy's recipe digest to the new recipe's digest.

3. Add a consumer workflow. No published AgentSCM release contains `reconcile` yet, so until one does the binary must be built from source (for example in a step that checks out and builds AgentSCM at a pinned commit); the `AGENTSCM_BIN` below stands for that binary.

```yaml
name: Local validation
on:
  issue_comment:
    types: [created, edited, deleted]
  push:
    branches: [main]
  schedule:
    - cron: '17 6 * * *'
permissions: {}
concurrency:
  group: local-validation
  cancel-in-progress: false
jobs:
  reconcile:
    if: ${{ github.event_name != 'issue_comment' || (github.event.issue.pull_request && startsWith(github.event.comment.body, '<!-- agentscm-local-validation:v1 ')) }}
    runs-on: ubuntu-latest
    timeout-minutes: 30
    permissions: { contents: read, pull-requests: read, statuses: write }
    steps:
      - uses: actions/checkout@<pinned sha>
        with: { ref: "${{ github.event.repository.default_branch }}", fetch-depth: 0, persist-credentials: false }
      # Install or build agentscm here; see above.
      - run: "$AGENTSCM_BIN" local-validation reconcile --github-repo "$REPO" --base "$BASE" --target-url "$RUN_URL"
        env:
          GH_TOKEN: ${{ github.token }}
          REPO: ${{ github.repository }}
          BASE: ${{ github.event.repository.default_branch }}
          RUN_URL: ${{ github.server_url }}/${{ github.repository }}/actions/runs/${{ github.run_id }}
```

Keep the trusted-association filter from this repository's workflow if you want the same compute limit; the signature still decides either way.
