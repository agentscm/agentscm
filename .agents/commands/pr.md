# Create Pull Request

Prepare and create a pull request for the current branch. Invoking `/pr` is the maintainer's explicit approval to create this pull request; it does not authorize merging or enabling auto-merge.

## Instructions

1. Read `README.md`, `CONTRIBUTING.md`, `STYLE.md`, and `.github/pull_request_template.md`.
2. Run `git fetch origin main`, identify the current branch, and stop if it is `main` or detached.
3. Inspect the exact PR scope with `git log --oneline origin/main..HEAD`, `git diff --stat origin/main...HEAD`, `git diff origin/main...HEAD`, and `git status --short`. If there are uncommitted changes, stop and report them rather than silently including or discarding them. If there are no branch commits, stop.
4. If `origin/main` advanced after the branch diverged, report the divergence. Do not rebase, merge, or force-push automatically.
5. Complete the final whole-branch repository gate:

   ```sh
   cargo fmt --check
   cargo clippy --all-targets --locked -- -D warnings
   cargo nextest run --locked
   cargo test --locked --doc
   git diff --check
   ```

   When `ui/` changes, also run `npm ci --ignore-scripts`, `npm run lint`, `npm run build` and `npm run notices -- --check` in `ui/`; when the container image or its inputs change, also run `docker build -t agentscm . && .github/docker-smoke.sh agentscm`. Tests needing PostgreSQL or S3 skip unless `AGENTSCM_TEST_DATABASE_URL` or `AGENTSCM_TEST_S3_*` are set; report such skips as coverage limits, not passes.

   Reuse a completed final gate only under `docs/contributor-testing.md`, after comparing the tested inputs and retaining the original commands, results, snapshot and comparison. Relevant changes invalidate affected evidence; runner or test-configuration changes require the full nextest suite and doctests. Applicable reviews remain required.

   Stop if a check fails. Keep missing prerequisites and unverified behavior explicit.
   Optional: when the maintainer has an enrolled key and local PostgreSQL and S3 test services, after the gate run `agentscm local-validation run` and then `post` (see `docs/advanced/local-validation.md`) once the PR exists. Never post commit statuses yourself, and report a Pending, Failure or Error verdict as it is.
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

11. Report the PR URL, title, head SHA, validation results, and known limitations. Do not merge or enable auto-merge.
