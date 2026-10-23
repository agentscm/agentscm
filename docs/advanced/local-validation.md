# Local validation attestations

> **Experimental (alpha).** Unix only, GitHub only, trusted-local. The verifier workflow has not been qualified against live GitHub, and no key is enrolled yet, so no attestation verifies today.

A maintainer can run the repository's gate on their own machine and attach a signed, Git-bound record of the result to a pull request. A separate verifier workflow checks that record and publishes the `agentscm/local-validation` commit status. Hosted CI is unchanged and still runs automatically; this does not replace it.

## What exists

- `agentscm local-validation run|show|post|verify`: see the [CLI reference](../reference/cli.md#local-validation). `run` refuses a dirty tree, runs `.agentscm/validation/recipe.json` with an allowlisted environment, and signs a portable-evidence-v1 envelope plus a `record` artifact bound to the repository, commit, tree, base ref and merge-base. Failed runs are signed too.
- Repository configuration in `.agentscm/validation/`: `recipe.json` (the commands from `AGENTS.md`), `policy.json` (repository ID, recipe digest, required PostgreSQL and S3 services, 30-day maximum age, epoch) and `registry.json` (issuer keys). A test fails when `policy.json` drifts from `recipe.json`.
- `.github/workflows/local-validation.yml`: on a PR comment that starts with the attestation marker, on manual dispatch, and on pushes to `main` that touch `.agentscm/validation/**` (re-verifying up to 50 open PRs), it builds `agentscm` from the default branch, fetches the PR head only as Git data, runs `verify` and posts `success`, `failure`, `error` or `pending` on exactly the verified head SHA. It never checks out, builds or runs PR code.

## Trust boundary

- A valid signature proves only that the holder of a registered key signed bytes bound to that commit, tree and base. The execution basis is `worker_asserted`: nothing proves the commands ran as recorded.
- The signing user account and host are trusted. Recipe commands run as the same user as the key file, so never attest third-party or fork code on a signing host.
- Anyone with `statuses: write` can post the same status context. Required-status configuration is operator-owned, and nothing here changes branch protection.
- Policy, recipe and registry are read from the default branch, not from the PR.
- The policy requires the PostgreSQL and S3 test services (`AGENTSCM_TEST_DATABASE_URL`, `AGENTSCM_TEST_S3_*`). A run without them is recorded and verifies as a failure.

## Maintainer workflow

1. Create a key with `agentscm agent-task key-generate` (owner-private file).
2. Enroll the public key in `.agentscm/validation/registry.json` by a reviewed pull request. The shipped registry has no issuers, so nothing verifies until this happens.
3. On a clean checkout of the PR head, with the PostgreSQL and S3 test services running, run `agentscm local-validation run`. Note the printed envelope digest.
4. `local-validation post` the attestation to the PR (it refuses if the PR head differs from the attested commit). Do not post statuses by hand.
5. The verifier workflow reports the status. Report `pending` or `failure` as it is.

`verify` is forge-neutral and also runs standalone against a local Git checkout. The standalone `evidence-verifier` tool has not been exercised on these envelopes.

## What invalidates an attestation

- A new head commit or tree, or a different merge-base.
- A change to the policy, recipe or registry. The verifier re-runs on pushes to `main` that touch them.
- Age beyond the policy maximum.

Advancing the base branch tip does not invalidate it; only the merge-base is bound.

## Coverage omissions

Every attestation records these as not covered: ignored tests, Windows, container smoke, hub end-to-end and live execution. Hosted CI remains the way to get platform and runtime coverage. For a fork pull request a maintainer still requests the extended jobs with the `ci-extended` label (the label must exist in the repository), as described in [CONTRIBUTING](../../CONTRIBUTING.md).

## Not done

No hosted job is conditional on an attestation, the verifier status is not a required check, and live GitHub qualification is pending.
