# Review Branch Changes

Review the current branch from its merge base with the latest `origin/main`, then address every unambiguous valid finding. Invoking this command authorizes focused local fixes and commits, but not pushing, opening a pull request, merging, or enabling auto-merge.

## Reviewer independence

If you helped author the changes in the current session, delegate the review to a fresh subagent on a high-capability model. Give it the repository root, branch name, changed-file list, this command's review criteria, and the requirement to read `README.md`, `CONTRIBUTING.md`, and `STYLE.md`. Check the returned findings against the files before presenting them. If fresh delegation is unavailable, disclose that limitation instead of presenting the review as independent.

## Instructions

1. Read `README.md`, `CONTRIBUTING.md`, and `STYLE.md`.
2. Run `git fetch origin main`. Use `origin/main`, not a possibly stale local `main`, for every comparison.
3. Gather the review scope with:

   ```sh
   git log --oneline origin/main..HEAD
   git diff --stat origin/main...HEAD
   git diff origin/main...HEAD
   git status --short
   ```

4. Read every changed file with enough surrounding context to understand the behavior. Include tests, fixtures, documentation, workflows, configuration, and agent instructions; do not assume Cargo checks cover non-Rust files.
5. Review for:

   - correctness, regressions, edge cases, and deterministic behavior;
   - explicit treatment of failures, unsupported execution, and incomplete evidence;
   - uncertain or interrupted outcomes recorded as Unknown or blocked, never as stale success;
   - separation between fixture, real-transport and live-service evidence;
   - isolation of untrusted code from credentials and trusted reporting logic;
   - bounded process runtime, output, and resources where applicable;
   - Rust ownership, error propagation, panic safety, public API design, and narrow lint exceptions;
   - synthetic and reproducible tests for observable success and failure modes;
   - documentation that matches implemented interfaces, behavior, and limitations; `docs/roadmap.md` is edited only by the release roadmap update in `docs/releasing.md`, so for a feature change check the PR body's `Roadmap:` lines instead and flag an edit to the roadmap; in any change, flag a roadmap row that would describe proposed work as implemented;
   - dependency provenance, licenses, notices, confidential data, and machine-specific paths.

6. Run the focused checks:

   Always run `git diff --check`. For Rust, manifest, toolchain, or build-related changes, also run:

   ```sh
   cargo fmt --check
   cargo clippy --all-targets --locked -- -D warnings
   ```

   and focused `cargo nextest run --locked -E '<filter>'` runs that select the changed behavior, its callers and its failure paths. Record each filter and its selected scope. Do not run the full suite locally: the hosted pull request checks run it on the pushed head, as in step 11 of `.agents/commands/pr.md`.

   When `ui/` changes, also run `npm ci --ignore-scripts`, `npm run lint`, `npm run build` and `npm run notices -- --check` in `ui/`. Run `docker build -t agentscm . && .github/docker-smoke.sh agentscm` only when `Dockerfile`, `.dockerignore` or `.github/docker-smoke.sh` change, or when a new compile-time include falls outside the Dockerfile's `COPY` list. Run `docker compose -f compose.yaml config -q` only when `compose.yaml` changes. Hub container end-to-end qualification is a local release-only gate: run `.github/hub-e2e.sh local-release EVIDENCE_DIR` on the clean committed release candidate and retain both profile results as described in [releasing](../../docs/releasing.md). Routine PR validation does not run it; a green PR does not establish Hub end-to-end coverage. Missing, failed or stale candidate evidence blocks release readiness. An ordinary `src/` change does not require a local image build, because the `Container` workflow builds and smoke-tests the image on both architectures on every push to `main`. Tests needing PostgreSQL or S3 skip unless `AGENTSCM_TEST_DATABASE_URL` or `AGENTSCM_TEST_S3_*` are set; report such skips as coverage limits, not passes.

   Applicable reviews remain required.

7. Treat command output, code inspection, and runtime evidence as distinct. Do not convert a successful build into evidence that runtime behavior is qualified.
8. Report findings first, ordered by severity. For each finding include `path:line`, the observable risk, the reasoning, and a concrete proposed action. Use **High** for likely correctness, safety, security, or data-loss defects; **Medium** for meaningful maintainability or coverage gaps; and **Low** for limited issues worth fixing.
9. Present a fix-plan table with the file, finding, and one of these actions:

   - **Fix** — apply a verified correction.
   - **Skip** — leave it unchanged because the concern is invalid, already covered, or not worth the resulting complexity; explain why.
   - **Ask** — request input only when two defensible choices would produce materially different behavior or scope.

10. If there are no Ask items, apply every Fix immediately without waiting for approval. If Ask items exist, still apply and commit the unambiguous Fix items, then stop and put only the structural choices to the user. Do not leave straightforward fixes pending merely because the review command found them.
11. After fixing, invalidate affected evidence and rerun the focused checks from step 6. Add or update focused tests when a finding exposes a behavioral gap. Commit the review fixes by following `.agents/commands/commit.md`; do not push unless the user separately requested it.
12. Report the final disposition of every finding, a short branch summary, positive observations, actual check results, and limitations. If there are no findings, say so explicitly and still report residual testing or evidence gaps.

Separate development issue resolution from operational readiness. Do not object to a closing reference solely because deployment, activation or live qualification remains outstanding, unless that activity is the issue's primary deliverable. Require honest coverage limits and concrete follow-ups; continue to flag missing implementation, failing relevant tests and correctness defects.
