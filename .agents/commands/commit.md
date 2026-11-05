# Commit Changes

Commit the intended working-tree changes with a factual message. If `$ARGUMENTS` contains a commit message, use it after checking that it complies with the repository's contribution-writing rules.

## Instructions

1. Read `README.md`, `CONTRIBUTING.md`, and `STYLE.md`.
2. Run `git fetch origin main`, `git branch --show-current`, `git status --short`, `git diff --stat`, and `git diff --staged --stat`.
3. Do not commit on `main`. If the current branch is `main`, create a focused feature branch whose name describes the intended changes. If the current branch is unrelated to the changes or has already merged, stop and ask where the work belongs rather than moving commits or stashing changes automatically.
4. Check `git log HEAD..origin/main --oneline`. If upstream has advanced, report it; do not rebase or merge unless the user asks.
5. Review the diff and distinguish the intended changes from unrelated user work. Stage only the intended paths. Never use `git add -A` without first establishing that every changed and untracked path belongs to this commit.
6. Run checks appropriate to the staged changes. Always run both `git diff --check` and `git diff --cached --check` so unstaged and staged changes are covered. For Rust, manifest, toolchain, or build-related changes, run:

   ```sh
   cargo fmt --check
   cargo clippy --all-targets --locked -- -D warnings
   ```

   and focused `cargo nextest run --locked -E '<filter>'` runs that select the changed behavior, its callers and its failure paths. Record each filter and its selected scope. Do not run the full suite to commit; when the recipe selects the Rust gate, the one full run of a head that is ready to merge is its attestation, as in `.agents/commands/pr.md`.

   When `ui/` changes, also run `npm ci --ignore-scripts`, `npm run lint`, `npm run build` and `npm run notices -- --check` in `ui/`. Run `docker build -t agentscm . && .github/docker-smoke.sh agentscm` only when `Dockerfile`, `.dockerignore` or `.github/docker-smoke.sh` change, or when a new compile-time include falls outside the Dockerfile's `COPY` list. Run `docker compose -f compose.yaml config -q` only when `compose.yaml` changes. Hub container end-to-end qualification is a local release-only gate: run `.github/hub-e2e.sh local-release EVIDENCE_DIR` on the clean committed release candidate and retain both profile results as described in [releasing](../../docs/releasing.md). Routine PR validation does not run it; a green PR does not establish Hub end-to-end coverage. Missing, failed or stale candidate evidence blocks release readiness. An ordinary `src/` change does not require a local image build, because the `Container` workflow builds and smoke-tests the image on both architectures on every push to `main`. Tests needing PostgreSQL or S3 skip unless `AGENTSCM_TEST_DATABASE_URL` or `AGENTSCM_TEST_S3_*` are set; report such skips as coverage limits, not passes.

   Applicable reviews remain required. Runner or test-configuration changes still need a full nextest run and doctests; that run is the attestation of the ready head.

   If a required check cannot run, stop and report the failure or missing prerequisite explicitly.
7. Re-read `git diff --staged` and verify that it contains no credentials, confidential information, machine-specific paths, unrelated edits, or accidental generated files.
8. If `$ARGUMENTS` is empty, write a concise subject that describes the observable change. A conventional prefix such as `feat:`, `fix:`, `docs:`, `test:`, `refactor:`, or `chore:` is preferred when it fits existing history. Add a body when the reason or a material limitation is not clear from the subject. Keep the subject under 72 characters.
9. An `Assisted-by: <tool or model>` trailer is optional. Do not add vendor branding, generated-by footers, `Co-authored-by` trailers for tools, session links, promotional text, or false human-only authorship claims. Preserve genuine attribution and required provenance.
10. Commit without bypassing hooks. Do not push.
11. Report the commit hash, subject, committed paths, validation performed, and any limitations.
