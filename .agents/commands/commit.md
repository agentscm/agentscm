# Commit Changes

Commit the intended working-tree changes with a factual message. If `$ARGUMENTS` contains a commit message, use it after checking that it complies with the repository's contribution-writing rules.

## Instructions

1. Read `README.md`, `CONTRIBUTING.md`, and `STYLE.md`.
2. Run `git fetch origin main`, `git branch --show-current`, `git status --short`, `git diff --stat`, and `git diff --staged --stat`.
3. Do not commit on `main`. If the current branch is `main`, create a focused feature branch whose name describes the intended changes. If the current branch is unrelated to the changes or has already merged, stop and ask where the work belongs rather than moving commits or stashing changes automatically.
4. Check `git log HEAD..origin/main --oneline`. If upstream has advanced, report it; do not rebase or merge unless the user asks.
5. Review the diff and distinguish the intended changes from unrelated user work. Stage only the intended paths. Never use `git add -A` without first establishing that every changed and untracked path belongs to this commit.
6. Run checks appropriate to the staged changes. Always run both `git diff --check` and `git diff --cached --check` so unstaged and staged changes are covered. For Rust, manifest, toolchain, or build-related changes, run the full repository gate from `AGENTS.md`:

   ```sh
   cargo fmt --check
   cargo clippy --all-targets --locked -- -D warnings
   cargo test --locked
   ```

   When `ui/` changes, also run `npm ci --ignore-scripts`, `npm run lint`, `npm run build` and `npm run notices -- --check` in `ui/`; when the container image or its inputs change, also run `docker build -t agentscm . && .github/docker-smoke.sh agentscm`. Tests needing PostgreSQL or S3 skip unless `AGENTSCM_TEST_DATABASE_URL` or `AGENTSCM_TEST_S3_*` are set; report such skips as coverage limits, not passes.

   If a required check cannot run, stop and report the failure or missing prerequisite explicitly.
7. Re-read `git diff --staged` and verify that it contains no credentials, confidential information, machine-specific paths, unrelated edits, or accidental generated files.
8. If `$ARGUMENTS` is empty, write a concise subject that describes the observable change. A conventional prefix such as `feat:`, `fix:`, `docs:`, `test:`, `refactor:`, or `chore:` is preferred when it fits existing history. Add a body when the reason or a material limitation is not clear from the subject. Keep the subject under 72 characters.
9. An `Assisted-by: <tool or model>` trailer is optional. Do not add vendor branding, generated-by footers, `Co-authored-by` trailers for tools, session links, promotional text, or false human-only authorship claims. Preserve genuine attribution and required provenance.
10. Commit without bypassing hooks. Do not push.
11. Report the commit hash, subject, committed paths, validation performed, and any limitations.
