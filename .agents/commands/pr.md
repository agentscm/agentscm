# Create Pull Request

Prepare and create a pull request for the current branch. Invoking `/pr` is the maintainer's explicit approval to create this pull request; it does not authorize merging or enabling auto-merge.

## Instructions

1. Read `README.md`, `CONTRIBUTING.md`, `STYLE.md`, and `.github/pull_request_template.md`.
2. Run `git fetch origin main`, identify the current branch, and stop if it is `main` or detached.
3. Inspect the exact PR scope with `git log --oneline origin/main..HEAD`, `git diff --stat origin/main...HEAD`, `git diff origin/main...HEAD`, and `git status --short`. If there are uncommitted changes, stop and report them rather than silently including or discarding them. If there are no branch commits, stop.
4. If `origin/main` advanced after the branch diverged, report the divergence. Do not rebase, merge, or force-push automatically.
5. Complete the pre-push checks for the whole branch:

   ```sh
   cargo fmt --check
   cargo clippy --all-targets --locked -- -D warnings
   git diff --check
   ```

   Also run focused `cargo nextest run --locked -E '<filter>'` runs that select the changed behavior, its callers and its failure paths. Record each filter and its selected scope. Do not run the full suite here when this machine has a signing key; step 11 runs it once, as the attestation.

   When `ui/` changes, also run `npm ci --ignore-scripts`, `npm run lint`, `npm run build` and `npm run notices -- --check` in `ui/`. Run `docker build -t agentscm . && .github/docker-smoke.sh agentscm` only when `Dockerfile`, `.dockerignore` or `.github/docker-smoke.sh` change, or when a new compile-time include falls outside the Dockerfile's `COPY` list. Run `.github/hub-e2e.sh default` only when `compose.hub.yaml`, `.github/compose.e2e.yaml` or `.github/hub-e2e.sh` change. An ordinary `src/` change does not require a local image build, because the `Container` workflow builds and smoke-tests the image on both architectures on every push to `main`. Tests needing PostgreSQL or S3 skip unless `AGENTSCM_TEST_DATABASE_URL` or `AGENTSCM_TEST_S3_*` are set; report such skips as coverage limits, not passes.

   Stop if a check fails. Keep missing prerequisites and unverified behavior explicit.
   Do not decide here whether the attestation will happen; step 11 does that by running its script.
6. Determine related issues only from reliable evidence such as branch names, commit messages, and issue or PR links. Do not add `Fixes` or `Closes` based on a loose title match.
7. If `$ARGUMENTS` supplies a title, use it after checking it against the diff and repository writing rules. Otherwise write a factual title, under 72 characters, that describes the primary observable change. Follow the branch's established commit style; do not invent a scope that the repository does not use.
8. Draft the body from the repository template:

   - **What and why**: describe the behavior and why it changed.
   - **How it was tested**: list exact commands and results, and state what did not run.
   - **Limits**: known gaps, unsupported cases, evidence not obtained, and follow-ups.
   - **AI assistance**: check the option that applies and name the tool and what the author verified.
   - **Checklist**: retain it and check only items actually verified.
   - Add an issue-closing line only when the PR genuinely resolves that issue.

   Keep fixture, real-transport and live-service evidence distinct. Do not equate a zero exit status with qualified behavior. Do not add generated-by footers, `Co-authored-by` trailers for tools, vendor branding, session links, promotional text, or false human-only authorship claims.
9. Push normally with `git push -u origin <branch>` if the remote is not current. Never force-push. Verify the remote branch SHA matches local `HEAD`.
10. Check whether an open PR already exists for the branch. If one exists, report its URL instead of creating a duplicate. Otherwise create the PR against `main`, assigning the authenticated maintainer:

    ```sh
    gh pr create --base main --assignee @me --title "<title>" --body-file <temporary-file>
    ```

11. Attest the pushed head by running `tools/attest-pr.sh <PR#>` from a clean checkout. This is a required step, not optional evidence: `agentscm/local-validation` is required on `main`, so the PR cannot go green without it. The script finds the key, starts disposable PostgreSQL and S3 services when none are configured, signs, verifies and posts. It re-runs the whole recipe, so run it in the background.

    - This is the one full suite run of a head that is ready to merge. Attest the head of the last fix round, not each intermediate push. A head with no attestation is not ready; report it that way.
    - If it reports no signing key, run `tools/attest-pr.sh --with-lock cargo nextest run --locked` and `cargo test --locked --doc` on that head, report the results as unsigned, and add a `ci-extended` label request to the PR body. No signing key is the only reason to run the full suite outside attestation. Reuse a completed full run only under `docs/contributor-testing.md`, after comparing the tested inputs and retaining the original commands, results, snapshot and comparison.
    - If the recipe fails, fix, push and attest again. Say why it failed and do not claim coverage. A failure does not undo the PR.
    - Afterwards confirm that `agentscm/local-validation` reports on the head. The verifier takes a few minutes, and a PR with no status yet is not green. Never post commit statuses yourself, and report a Pending, Failure or Error verdict as it is.

12. Report the PR URL, title, head SHA, validation results, attestation result, and known limitations. Do not merge or enable auto-merge.
