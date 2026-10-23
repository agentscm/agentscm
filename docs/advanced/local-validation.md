# Local validation attestations

> **Experimental (alpha).** Unix only, GitHub only, trusted-local. The verifier workflow has not been qualified against live GitHub, and no key is enrolled yet, so no attestation verifies today.

A maintainer can run the repository's gate on their own machine and attach a signed, Git-bound record of the result to a pull request. A separate verifier workflow checks that record and publishes the `agentscm/local-validation` commit status. Hosted CI is unchanged and still runs automatically; this does not replace it.

## What exists

- `agentscm local-validation run|show|post|verify`: see the [CLI reference](../reference/cli.md#local-validation-attestations). `run` refuses a dirty tree, runs `.github/local-validation/recipe.json` with an allowlisted environment (see the trust boundary), retains each command's output as `<sha>.cmd<N>.stdout|stderr` in the state directory, and signs a portable-evidence-v1 envelope plus a `record` artifact bound to the repository, commit, tree, base ref and merge-base. Failed runs are signed too.
- Repository configuration in `.github/local-validation/`: `recipe.json` (the commands from `AGENTS.md`), `policy.json` (repository ID, recipe digest, required PostgreSQL and S3 services, 30-day maximum age, epoch) and `registry.json` (issuer keys). A test fails when `policy.json` drifts from `recipe.json`.
- `.github/workflows/local-validation.yml`: runs on a PR comment that starts with the attestation marker from an `OWNER`, `MEMBER` or `COLLABORATOR` (created, edited or deleted; other commenters are ignored and the signature still decides), on manual dispatch, on pushes to `main` that touch the verifier inputs (`src/evidence/**`, `src/policy/**`, `src/local_validation/**`, `src/local_validation_cli.rs`, `src/main.rs`, `Cargo.lock`, `rust-toolchain.toml`, `.github/local-validation/**` and the workflow itself) and daily. The push and daily runs re-verify every open PR (and fail the job if 1000 or more are open); they skip a PR that has no trusted attestation comment and no existing `agentscm/local-validation` status on its head, and post `pending` where a status exists but the comment is gone. The workflow builds `agentscm` from the default branch, fetches the PR head only as Git data, runs `verify` and posts `success`, `failure`, `error` or `pending` on exactly the verified head SHA. It never checks out, builds or runs PR code.

## Trust boundary

- A valid signature proves only that the holder of a registered key signed bytes bound to that commit, tree and base. The execution basis is `worker_asserted`: nothing proves the commands ran as recorded.
- The signing user account and host are trusted. Recipe commands run as the signing user with `HOME` passed, so attested code can read `~/.config/gh`, the signing key, cargo configuration such as `.cargo/config.toml` aliases and anything else that user can read. The environment allowlist only avoids passing tokens through environment variables; it is not a sandbox. Never attest code you would not run as that user, and never attest third-party or fork code on a signing host.
- Anyone with `statuses: write` can post the same status context. Required-status configuration is operator-owned, and nothing here changes branch protection.
- Policy, recipe and registry are read from the default branch, not from the PR.
- The policy requires the PostgreSQL and S3 test services (`AGENTSCM_TEST_DATABASE_URL`, `AGENTSCM_TEST_S3_*`). A run without them is recorded and verifies as a failure.

## Maintainer workflow

1. Create a key with `agentscm agent-task key-generate` (owner-private file).
2. Enroll the public key in `.github/local-validation/registry.json` by a reviewed pull request. The shipped registry has no issuers, so nothing verifies until this happens.
3. On a clean checkout of the PR head, run `git fetch origin main` (the merge-base is computed against the local `origin/main`), start the PostgreSQL and S3 test services, then run `agentscm local-validation run --recipe .github/local-validation/recipe.json --policy .github/local-validation/policy.json --registry .github/local-validation/registry.json --key-file KEY --issuer-id ID --state-dir DIR`. `run` has no defaults for the recipe, policy or registry; pass all three. Note the printed envelope digest. Ctrl-C cancels the running command and signs a failure; that path is implemented but untested interactively.
4. `local-validation post` the attestation to the PR (it refuses if the PR head differs from the attested commit). Do not post statuses by hand.
5. The verifier workflow reports the status. If the verifier errors for a PR it posts `error` on that head and the job fails; re-run it. Report `pending` or `failure` as it is.

`verify` is forge-neutral and also runs standalone against a local Git checkout; its `--recipe` default, `.github/local-validation/recipe.json`, is relative to the current directory. The verifier workflow fetches the PR head without credentials, so as written it supports public repositories only. The standalone `evidence-verifier` tool has not been exercised on these envelopes.

## What invalidates an attestation

- A new head commit or tree, or a different merge-base.
- A change to the policy, recipe or registry. The verifier re-runs on pushes to `main` that touch the verifier inputs, and daily, so an aged-out attestation's `success` is replaced. A run posts nothing if the verifier inputs listed above changed on the default branch since it checked out, or if the PR's trusted attestation comments changed while it ran; the newer run owns the verdict.
- Age beyond the policy maximum.

Advancing the base branch tip does not invalidate it; only the merge-base is bound. The base tip is deliberately not bound, and the tests ran on the PR head, not on the merge result. The maintainers accept this limit.

## Coverage omissions

Every attestation records these as not covered: ignored tests, Windows, container smoke, hub end-to-end and live execution. Hosted CI remains the way to get platform and runtime coverage. For a fork pull request a maintainer still requests the extended jobs with the `ci-extended` label (the label must exist in the repository), as described in [CONTRIBUTING](../../CONTRIBUTING.md).

## Not done

No hosted job is conditional on an attestation, the verifier status is not a required check, and live GitHub qualification is pending.
