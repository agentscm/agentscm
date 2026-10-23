# Local validation attestations

> **Experimental (alpha).** Unix only, GitHub only, trusted-local. One maintainer key is enrolled. The verifier workflow was qualified before merge by running it under `act` against live GitHub on pull request #86: a signed failure (`failure` on that head), then a fix and a signed pass (`success` on the new head). Tampered, wrong-repository and other-head attestations were checked with `local-validation verify` against the same live comment bodies and gave `pending`. Its first run on GitHub-hosted Actions happens after merge. Contributors without an enrolled key or local PostgreSQL and S3 request `ci-extended` for hosted test coverage.

A maintainer can run the repository's gate on their own machine and attach a signed, Git-bound record of the result to a pull request. A separate verifier workflow checks that record and publishes the `agentscm/local-validation` commit status. For pull requests from branches in this repository, this attestation is the expected evidence for the test gate: the expensive hosted suites no longer run on ordinary updates. See [Hosted CI and required checks](#hosted-ci-and-required-checks).

## What exists

- `agentscm local-validation run|show|post|verify`: see the [CLI reference](../reference/cli.md#local-validation-attestations). `run` refuses a dirty tree, runs `.github/local-validation/recipe.json` with an allowlisted environment (see the trust boundary), retains each command's output as `<sha>.cmd<N>.stdout|stderr` in the state directory, and signs a portable-evidence-v1 envelope plus a `record` artifact bound to the repository, commit, tree, base ref and merge-base. Failed runs are signed too.
- Repository configuration in `.github/local-validation/`: `recipe.json` (the commands from `AGENTS.md`), `policy.json` (repository ID, recipe digest, required PostgreSQL and S3 services, 30-day maximum age, epoch) and `registry.json` (issuer keys). A test fails when `policy.json` drifts from `recipe.json`.
- `.github/workflows/local-validation.yml`: runs on a PR comment that starts with the attestation marker from an `OWNER`, `MEMBER` or `COLLABORATOR` (created, edited or deleted; an edit that removes the marker re-verifies; other commenters are ignored and the signature still decides), on manual dispatch, on pushes to `main` that touch the verifier inputs (`src/**`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `.github/local-validation/**` and the workflow itself) and daily. The push and daily runs re-verify every open PR (and fail the job if 1000 or more are open); they skip a PR that has no trusted attestation comment and no existing `agentscm/local-validation` status on its head, and post `pending` where a status exists but the comment is gone. The workflow builds `agentscm` from the default branch, fetches the PR head only as Git data, runs `verify` and posts `success`, `failure`, `error` or `pending` on exactly the verified head SHA. It never checks out, builds or runs PR code.

## Trust boundary

- A valid signature proves only that the holder of a registered key signed bytes bound to that commit, tree and base. The execution basis is `worker_asserted`: nothing proves the commands ran as recorded.
- The signing user account and host are trusted. Recipe commands run as the signing user with `HOME` passed, so attested code can read `~/.config/gh`, the signing key, cargo configuration such as `.cargo/config.toml` aliases and anything else that user can read. The environment allowlist only avoids passing tokens through environment variables; it is not a sandbox. Never attest code you would not run as that user, and never attest third-party or fork code on a signing host.
- Anyone with `statuses: write` can post the same status context. Required-status configuration is operator-owned, and nothing here changes branch protection.
- Policy, recipe and registry are read from the default branch, not from the PR.
- The policy requires the PostgreSQL and S3 test services (`AGENTSCM_TEST_DATABASE_URL`, `AGENTSCM_TEST_S3_*`). A run without them is recorded and verifies as a failure.

## Maintainer workflow

1. Create a key with `agentscm agent-task key-generate` (owner-private file).
2. Enroll the public key in `.github/local-validation/registry.json` by a reviewed pull request. Only enrolled, unrevoked and unexpired keys verify; revoke by setting `revoked_at_ms` in a reviewed change.
3. On a clean checkout of the PR head, run `git fetch origin main` (the merge-base is computed against the local `origin/main`), start the PostgreSQL and S3 test services, then run `agentscm local-validation run --recipe .github/local-validation/recipe.json --policy .github/local-validation/policy.json --registry .github/local-validation/registry.json --key-file KEY --issuer-id ID --state-dir DIR`. `run` has no defaults for the recipe, policy or registry; pass all three. Note the printed envelope digest. Ctrl-C (SIGINT) cancels the running command's process group and signs a failure that records the command as `cancelled`; a second Ctrl-C exits immediately.
4. `local-validation post` the attestation to the PR (it refuses if the PR head differs from the attested commit). Do not post statuses by hand.
5. The verifier workflow reports the status. If the verifier errors for a PR it posts `error` on that head and the job fails; re-run it. Report `pending` or `failure` as it is.

`verify` is forge-neutral and also runs standalone against a local Git checkout; its `--recipe` default, `.github/local-validation/recipe.json`, is relative to the current directory. The verifier workflow fetches the PR head without credentials, so as written it supports public repositories only. The standalone `evidence-verifier` tool has not been exercised on these envelopes.

## Hosted CI and required checks

Automatic on every pull request update: `CI / static` (formatting and clippy), dependency and workflow-security checks, and the installer and UI workflows when their paths match. The test suites depend on where the branch lives:

- Same-repository pull requests: the `CI / tests` job (hosted PostgreSQL and S3), Windows, container, hub end-to-end and live execution run only while the pull request has the `ci-extended` label, on labeling and on each later update. The attestation replaces them otherwise. If you have no enrolled key or services, say so in the pull request and request `ci-extended` instead of claiming coverage. Adding any other label later records skipped label-gated checks on the same head, so read the `ci-extended` workflow runs themselves (or remove and re-add the label) rather than the pull request's checks list.
- Fork pull requests cannot carry an attestation. `CI / tests` stays automatic; a maintainer adds `ci-extended` after reading the change to run the extended jobs once against that head.
- Pushes to `main` still run every suite, and `release.yml` independently rebuilds and smoke-tests the tagged commit, so release provenance is unchanged.
- To request hosted coverage on demand, add `ci-extended` or run the workflow with `workflow_dispatch` on the branch.

The `main` ruleset has no required status checks today (it requires no deletion, no non-fast-forward, signed commits and a pull request). Skipped jobs report as skipped and path-filtered workflows may not report at all. If a requirement is added, require only `CI / static` and `agentscm/local-validation`; never require a label-gated job.

## What invalidates an attestation

- A new head commit or tree, or a different merge-base.
- A change to the policy, recipe or registry. The verifier re-runs on pushes to `main` that touch the verifier inputs, and daily, so an aged-out attestation's `success` is replaced. A run posts nothing if the verifier inputs listed above changed on the default branch since it checked out, or if the PR's trusted attestation comments changed while it ran; the newer run owns the verdict.
- Age beyond the policy maximum.

## Limits

- The status is attached to the commit, so PRs that share a head commit (for example one branch opened against two bases) share it; that case is unsupported.
- Retargeting a PR to a new base fires no event. The next daily run re-verifies it; re-run the workflow with `workflow_dispatch` to refresh sooner.
- Batch runs (push and daily) process PRs serially within the 30-minute job and the token rate limit. That is practical for tens of open PRs, not hundreds.
- An aged-out (expired) `success` is replaced by the daily run, so within about a day of expiry.

Advancing the base branch tip does not invalidate it; only the merge-base is bound. The base tip is deliberately not bound, and the tests ran on the PR head, not on the merge result. The maintainers accept this limit.

## Coverage omissions

Every attestation records these as not covered: ignored tests, Windows, container smoke, hub end-to-end and live execution. Hosted CI remains the way to get platform and runtime coverage; request it as described above.

## Not done

Hosted jobs are gated on the `ci-extended` label, not on the presence of an attestation, so a pull request with neither runs no hosted test suite. The verifier status is not a required check, and qualification on GitHub-hosted Actions is pending until the verifier runs from `main`.
