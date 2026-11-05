# Create Pull Request

Prepare and create a pull request for the current branch. Invoking `/pr` is the maintainer's explicit approval to create this pull request; it does not authorize merging or enabling auto-merge.

## Instructions

1. Read `README.md`, `CONTRIBUTING.md`, `STYLE.md`, and `.github/pull_request_template.md`.
2. Run `git fetch origin main`, identify the current branch, and stop if it is `main` or detached.
3. Inspect the exact PR scope with `git log --oneline origin/main..HEAD`, `git diff --stat origin/main...HEAD`, `git diff origin/main...HEAD`, and `git status --short`. If there are uncommitted changes, stop and report them rather than silently including or discarding them. If there are no branch commits, stop.
4. If `origin/main` advanced after the branch diverged, report the divergence. Do not rebase, merge, or force-push automatically.
5. Complete the pre-push checks for the whole branch:

   Always run `git diff --check`. For Rust, manifest, toolchain, or build-related changes, also run:

   ```sh
   cargo fmt --check
   cargo clippy --all-targets --locked -- -D warnings
   ```

   and focused `cargo nextest run --locked -E '<filter>'` runs that select the changed behavior, its callers and its failure paths. Record each filter and its selected scope. Do not run the full suite before pushing; step 11 handles attestation, including when there is no signing key.

   When `ui/` changes, also run `npm ci --ignore-scripts`, `npm run lint`, `npm run build` and `npm run notices -- --check` in `ui/`. Run `docker build -t agentscm . && .github/docker-smoke.sh agentscm` only when `Dockerfile`, `.dockerignore` or `.github/docker-smoke.sh` change, or when a new compile-time include falls outside the Dockerfile's `COPY` list. Run `docker compose -f compose.yaml config -q` only when `compose.yaml` changes. Run `.github/hub-e2e.sh default` when `compose.hub.yaml`, `.github/compose.e2e.yaml`, `.github/hub-e2e.sh`, `examples/hub_seed.rs` or the Dockerfile's `e2e-seed` stages change; the `postgres` profile runs only in the hosted `Hub end-to-end` workflow, which starts automatically on a same-repository pull request that changes a PostgreSQL migration, `src/hub/store_pg.rs`, `src/hub/store_pg/` or its harness; when any other change could affect it, report it as not run locally and, once the head is pushed, a same-repository author runs `gh workflow run hub-e2e.yml --ref <branch>` and matches the run's `headSha`. An ordinary `src/` change does not require a local image build, because the `Container` workflow builds and smoke-tests the image on both architectures on every push to `main`. Tests needing PostgreSQL or S3 skip unless `AGENTSCM_TEST_DATABASE_URL` or `AGENTSCM_TEST_S3_*` are set; report such skips as coverage limits, not passes.

   Stop if a check fails. Keep missing prerequisites and unverified behavior explicit.
   Do not decide here whether the attestation will happen; step 11 does that by running its script.
6. Determine related issues only from reliable evidence such as branch names, commit messages, and issue or PR links. Do not add `Fixes` or `Closes` based on a loose title match.
7. If `$ARGUMENTS` supplies a title, use it after checking it against the diff and repository writing rules. Otherwise write a factual title, under 72 characters, that describes the primary observable change. Follow the branch's established commit style; do not invent a scope that the repository does not use.
8. Draft the body from the repository template:

   - **What and why**: describe the behavior and why it changed.
   - **How it was tested**: list exact commands and results, and state what did not run.
   - **Limits**: known gaps, unsupported cases, evidence not obtained, and follow-ups.
   - **AI assistance**: check the option that applies and name the tool and what the author verified.
   - **Checklist**: retain it and check only items actually verified. The validation item covers the pre-push checks and the attestation, so leave it unchecked until step 11 updates the body.
   - Add an issue-closing line only when the PR genuinely resolves that issue.

   Keep fixture, real-transport and live-service evidence distinct. Do not equate a zero exit status with qualified behavior. Do not add generated-by footers, `Co-authored-by` trailers for tools, vendor branding, session links, promotional text, or false human-only authorship claims.
9. Push normally with `git push -u origin <branch>` if the remote is not current. Never force-push. Verify the remote branch SHA matches local `HEAD`.
10. Check whether an open PR already exists for the branch. If one exists, report its URL instead of creating a duplicate. Otherwise create the PR against `main`, assigning the authenticated maintainer:

    ```sh
    gh pr create --base main --assignee @me --title "<title>" --body-file <temporary-file>
    ```

11. Attest the pushed head by running `tools/attest-pr.sh <PR#>` from a clean checkout. This is a required step, not optional evidence: `agentscm/local-validation` is required on `main`, so the PR cannot go green without it. The script finds the key, starts disposable PostgreSQL and S3 services when none are configured, signs, verifies and posts. It runs the recipe's selected commands, so run it in the background.

    - When the recipe selects the Rust gate, this is the one full suite run of a head that is ready to merge; a head confined to the recipe's excluded paths (`ui/src`, `ui/public`, `ui/tests/unit`, `ui/tests/integration`, `ui/tests/e2e` and `DESIGN.md`) runs no Rust commands, and zero selected commands is a valid pass, reported as "no commands ran". Files such as `ui/package.json` and `ui/vite.config.ts` fall outside that set and select the Rust gate. The selected commands are listed by `local-validation show --state-dir <dir> --attestation <sha>` and in the posted comment; the script's output does not list them. Attest the head of the last fix round, not each intermediate push. A head with no attestation is not ready; report it that way.
    - If it reports no signing key and any changed path falls outside the recipe's excluded set, run all four Rust commands under one lock on that head: `tools/attest-pr.sh --with-lock sh -c 'cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings && cargo nextest run --locked && cargo test --locked --doc'`. Report the results as unsigned and name the `gh workflow run ci.yml --ref <branch>` dispatch fallback (same-repository branches; a fork author dispatches in their own fork or asks for maintainer adoption) in the PR body. A head confined to the excluded paths runs none of them. The head stays not ready until it has an accepted attestation. No signing key is the only reason to run the full suite outside attestation. Reuse a completed full run only under `docs/contributor-testing.md`, after comparing the tested inputs and retaining the original commands, results, snapshot and comparison.
    - If the recipe fails, `/pr` authorizes creating the PR, not changing code. Report the failure and why it failed, leave the head not ready, claim no coverage, and point to `/fix-pr`. A failure does not undo the PR. When `/implement` invoked this step, it follows its own correction path instead.
    - A failure shown to be unrelated to the change, such as a known timing-sensitive test (retries are off), may be attested again on the same head without a push. Report that this happened and which test failed. "Shown to be unrelated" needs evidence, for example the same test failing on `origin/main` or a failure that does not involve changed code; never call a failure a flake because it is convenient.
    - Afterwards confirm that `agentscm/local-validation` reports on the head. The verifier takes a few minutes, and a PR with no status yet is not green. Never post commit statuses yourself, and report a Pending, Failure or Error verdict as it is.
    - Update the PR body with the attestation result, the envelope SHA-256 and the commands the recipe selected (`gh pr edit <PR#> --body-file <temporary-file>`), and check the validation checklist item only if it is true.

12. Report the PR URL, title, head SHA, validation results, attestation result, and known limitations. Do not merge or enable auto-merge.
