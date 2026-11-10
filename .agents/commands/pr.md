# Create Pull Request

Prepare and create a pull request for the current branch. Invoking `/pr` is the maintainer's explicit approval to create this pull request; it does not authorize merging or enabling auto-merge.

## Instructions

1. Read `README.md`, `CONTRIBUTING.md`, `STYLE.md`, and `.github/pull_request_template.md`.
2. Run `git fetch origin main`, identify the current branch, and stop if it is `main` or detached.
3. Inspect the exact PR scope with `git log --oneline origin/main..HEAD`, `git diff --stat origin/main...HEAD`, `git diff origin/main...HEAD`, and `git status --short`. If there are uncommitted changes, stop and report them rather than silently including or discarding them. If there are no branch commits, stop.
4. If `origin/main` advanced after the branch diverged, report the divergence. Do not rebase, merge, or force-push automatically. Do not merge `main` into the branch unless GitHub reports a conflict: the merge queue tests the pull request against `main`, and each merge of `main` reruns the hosted checks.
5. Complete the pre-push checks for the whole branch:

   Always run `git diff --check`. For Rust, manifest, toolchain, or build-related changes, also run:

   ```sh
   cargo fmt --check
   cargo clippy --all-targets --locked -- -D warnings
   ```

   and focused `cargo nextest run --locked -E '<filter>'` runs that select the changed behavior, its callers and its failure paths. Record each filter and its selected scope. Do not run the full suite locally; the hosted pull request checks run it (step 11). Worktrees on one machine may share one `CARGO_TARGET_DIR` to avoid compiling every dependency once per worktree; concurrent builds then wait on Cargo's lock on that directory, and `.agents/bound-target.sh` then cleans it for every worktree at once, including under a running build (see the caveat in `docs/contributor-testing.md`, Focused iteration).

   When `ui/` changes, also run `npm ci --ignore-scripts`, `npm run lint`, `npm run build` and `npm run notices -- --check` in `ui/`. Run `docker build -t agentscm . && .github/docker-smoke.sh agentscm` only when `Dockerfile`, `.dockerignore` or `.github/docker-smoke.sh` change, or when a new compile-time include falls outside the Dockerfile's `COPY` list. Run `docker compose -f compose.yaml config -q` only when `compose.yaml` changes. Hub container end-to-end qualification is a local release-only gate: run `.github/hub-e2e.sh local-release EVIDENCE_DIR` on the clean committed release candidate and retain both profile results as described in [releasing](../../docs/releasing.md). Routine PR validation does not run it; a green PR does not establish Hub end-to-end coverage. Missing, failed or stale candidate evidence blocks release readiness. An ordinary `src/` change does not require a local image build, because the `Container` workflow builds and smoke-tests the image on both architectures on every push to `main`. Tests needing PostgreSQL or S3 skip unless `AGENTSCM_TEST_DATABASE_URL` or `AGENTSCM_TEST_S3_*` are set; report such skips as coverage limits, not passes.

   Stop if a check fails. Keep missing prerequisites and unverified behavior explicit.
6. Determine related issues only from reliable evidence such as branch names, commit messages, and issue or PR links. Do not add `Fixes` or `Closes` based on a loose title match.
7. If `$ARGUMENTS` supplies a title, use it after checking it against the diff and repository writing rules. Otherwise write a factual title, under 72 characters, that describes the primary observable change. Follow the branch's established commit style; do not invent a scope that the repository does not use.
8. Draft the body from the repository template:

   - **What and why**: describe the behavior and why it changed.
   - **How it was tested**: list exact commands and results, and state what did not run.
   - **Limits**: known gaps, unsupported cases, evidence not obtained, and follow-ups.
   - **Roadmap**: one `Roadmap:` line per affected `docs/roadmap.md` row (the row and its new status), or `Roadmap: none`. Unless this is the release roadmap update in `docs/releasing.md`, a branch that edits `docs/roadmap.md` restores it from `main` in a new commit and carries the change here instead.
   - **AI assistance**: check the option that applies and name the tool and what the author verified.
   - **Checklist**: retain it and check only items actually verified. The validation item covers the pre-push checks; say under "How it was tested" that the hosted checks are the remaining validation, so the body needs no edit after the push.
   - For a development issue, add `Closes #<issue>` when the requested behavior is implemented and appropriate automated tests pass. Deployment, activation, operator configuration and unavailable live qualification alone do not block closure; report these limits and concrete follow-ups separately. If those operational activities are the issue's primary deliverable, keep it open until they are complete. Missing implementation, failing relevant tests or unresolved correctness defects still block resolution. Closing on merge does not waive required checks or imply production readiness.

   Keep fixture, real-transport and live-service evidence distinct. Do not equate a zero exit status with qualified behavior. Do not add generated-by footers, `Co-authored-by` trailers for tools, vendor branding, session links, promotional text, or false human-only authorship claims.
9. Push normally with `git push -u origin <branch>` if the remote is not current. Never force-push. Verify the remote branch SHA matches local `HEAD`.
10. Check whether an open PR already exists for the branch. If one exists, report its URL instead of creating a duplicate. Otherwise create the PR against `main`, assigning the authenticated maintainer:

    ```sh
    gh pr create --base main --assignee @me --title "<title>" --body-file <temporary-file>
    ```

11. Read the hosted checks on the pushed head with `gh pr checks <PR#>`. These and `Merge batch gate` in the merge queue are what gate a merge: the `CI` jobs `static` (formatting, release metadata, Clippy) and `tests` (nextest and doctests with PostgreSQL and S3), `Dependency audit` (job `cargo-deny`), `Workflow lint` (`zizmor`), `Status page` (`ui`), `Documentation site` (`build`), `Conventional PR title`, and `Installer` when its paths change. `Merge batch gate` is the only required check on `main`, and admission does not read the hosted checks, so a head with a failing or unfinished hosted check is not ready; report it that way.

    - If a hosted check fails, `/pr` authorizes creating the PR, not changing code. Report the failure and why it failed, leave the head not ready, claim no coverage, and point to `/fix-pr`. A failure does not undo the PR. When `/implement` invoked this step, it follows its own correction path instead.
    - A failure shown to be unrelated to the change, such as a known timing-sensitive test (retries are off), may be rerun on the same head without a push (`gh run rerun <run-id> --failed`). Report that this happened and which test failed. "Shown to be unrelated" needs evidence, for example the same test failing on `origin/main` or a failure that does not involve changed code; never call a failure a flake because it is convenient.
    - Once the hosted checks are green and the PR's one review is recorded, it is ready to queue; do not wait for anything else. Queue it then only where `AGENTS.md` lets you merge; `/pr` itself does not authorize it.
    - Do not edit the title or body after the last push. If you must, run `gh pr update-branch <PR#>` afterwards, because an edit after the last push can leave `Merge batch gate` reading "expected" (a known defect). That merges `main`, the one allowed exception to the rule against merging it; if GitHub reports the branch is already up to date, push the next real change instead of editing.
    - The signed local attestation (`tools/attest-pr.sh <PR#>`) is optional evidence; only a signed failure refuses admission. Do not run it for every PR. Run it on a release candidate, or for a change to code that behaves differently on macOS, because hosted CI never runs the tests on macOS (its macOS release-target builds run nightly and on dispatch, build and Clippy only). It runs the recipe's full selected commands through the machine-wide queue, so run it in the background, and never post commit statuses yourself. The Hub end-to-end release gate in step 5 is unchanged.

12. Report the PR URL, title, head SHA, validation results, hosted check results, and known limitations. Do not merge or enable auto-merge.
