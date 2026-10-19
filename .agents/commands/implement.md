# Implement an Issue End to End

Implement a GitHub issue through a reviewed, validated, open pull request with as little human intervention as possible. `$ARGUMENTS` must contain an issue number, `#number`, or issue URL. If it does not, ask for the issue and stop.

Invoking this issue-to-PR command authorizes creating a feature branch, editing files, running local checks, making focused commits, pushing the feature branch, and opening its pull request. This supplies the explicit approval required by `.agents/commands/pr.md`; do not stop for a second approval at the PR gate. Never merge or enable auto-merge.

Act as the orchestrator. Use fresh subagents for repository reconnaissance, each implementation phase, and independent review. Keep the issue text, verified reconnaissance, phase state, and validation results in the main context. Subagents share the working tree, so implementation phases must run sequentially, not concurrently.

## Phase 1: Read and validate the issue

1. Read `AGENTS.md`, `README.md`, `CONTRIBUTING.md`, `STYLE.md`, `docs/README.md`, `docs/architecture.md` and `docs/roadmap.md`.
2. Resolve the issue with:

   ```sh
   gh issue view <issue> --json number,title,body,labels,comments,state,closedByPullRequestsReferences
   ```

3. Use the returned issue number as the canonical number. Stop if the issue is closed or already has a closing pull request. If it references earlier work, inspect those exact issues, PRs, or commits; do not rely on loose search matches.
4. Derive a short kebab-case slug from the issue title and choose `fix`, `feat`, `refactor`, or `chore` from the requested behavior.

## Phase 2: Reconnaissance

Launch one fresh read-only subagent on a high-capability model, with the full issue title, body, relevant comments, repository root, and these instructions:

> Read `AGENTS.md`, `README.md`, `CONTRIBUTING.md`, `STYLE.md`, `docs/README.md`, `docs/architecture.md` and `docs/roadmap.md`. Inspect the repository thoroughly but do not edit files. Open every file needed to support a claim; do not infer behavior from filenames or the issue alone. Check relevant history when claiming that a regression or refactor caused the problem. Recount file lists and numeric claims from the issue. Mark unverified claims in place.
>
> Return: (1) files that must change with `path:line` anchors; (2) the closest existing patterns to mirror; (3) a two-to-five-phase implementation order whose phases are independently testable and committable; (4) existing and new tests; (5) affected interfaces and evidence boundaries; (6) structural questions; (7) applicable repository rules and likely pitfalls; (8) synthetic checks an agent can run; and (9) checks that genuinely require the user or unavailable external state.
>
> Pay particular attention to deterministic ordering and digests, typed outcomes, durable journals and crash recovery, expected-object checks on remote updates, bounded subprocesses and output, cancellation, strict decoding, untrusted-code isolation from credentials, and outcomes that must stay Unknown or blocked rather than report stale success. Check `docs/roadmap.md` so that nothing proposed is described as implemented. A successful command is not proof that the behavior is qualified.

Independently spot-check the report's key file and behavior claims. If it finds no implementable change, stop and explain what clarification is missing.

Ask the user only about structural forks where a wrong choice would discard substantial work or change externally visible behavior, trust boundaries, compatibility, or scope. Present two or three concrete options with a recommendation. Resolve naming, file placement, and ordinary internal factoring from repository patterns without asking.

## Phase 3: Establish the branch

1. Require `git status --porcelain` to be empty before changing branches. Do not stash or discard existing work.
2. Run `git fetch origin main` and record the exact repository root from `git rev-parse --show-toplevel` for every subagent prompt.
3. If already on a feature branch, confirm its name and commits correspond to this issue. Continue there when they do. If it is unrelated, stop rather than mixing work.
4. If on `main`, ensure `<type>/<number>-<slug>` does not already exist locally or remotely, then create it from `origin/main`.
5. Present the verified phase list and proceed without an extra approval stop unless Phase 2 found a structural question.

## Phase 4: Implement sequential phases

For each phase, record `HEAD` and launch one fresh implementation subagent on a fast, cost-efficient model, with a self-contained prompt containing the repository root, expected branch, issue behavior, the current phase only, relevant file anchors and patterns, applicable rules, prior-phase corrections, and required tests. Include these instructions:

> Confirm the expected branch before editing and stop if it differs. Read `AGENTS.md`, `README.md`, `CONTRIBUTING.md`, and `STYLE.md`. Work only on the assigned phase. Verify reconnaissance claims against the current files and report corrections. Use synthetic fixtures and keep untrusted code isolated from credentials and trusted reporting. Preserve licenses and notices. Add focused tests for observable behavior and failure modes. Run `cargo fmt` before committing, run the phase-specific tests, inspect the diff for confidential data and machine-specific paths, and commit without bypassing hooks. Use a factual commit message; an `Assisted-by:` trailer is optional, but use no generated-by footer, vendor branding, `Co-authored-by` trailer for a tool, session link, or false attribution. Return changed paths, commit SHA, exact test results, corrected assumptions, and unfinished work.

After each phase, verify that `HEAD` advanced, the commit matches the assigned scope, and the working tree is clean. Read the changed files and test output needed to validate the subagent's claims. If the phase did not land cleanly, give one fresh correction subagent on a high-capability model, the failure evidence and one attempt to fix and commit it. Stop after a second failure rather than accumulating uncertain changes.

Propagate corrected reconnaissance to every later phase. If a late structural fork appears, choose the option most consistent with documented behavior only when compatibility and scope remain unchanged; otherwise stop and ask the user.

## Phase 5: Whole-branch validation

Run the complete repository gate:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
git diff --check
```

When `ui/` changes, also run `npm ci --ignore-scripts`, `npm run lint`, `npm run build` and `npm run notices -- --check` in `ui/`; when the container image or its inputs change, also run `docker build -t agentscm . && .github/docker-smoke.sh agentscm`. Tests needing PostgreSQL or S3 skip unless `AGENTSCM_TEST_DATABASE_URL` or `AGENTSCM_TEST_S3_*` are set; report such skips as coverage limits, not passes.

Also run focused synthetic CLI or integration checks identified during reconnaissance when they materially test behavior not covered by the suite. Do not use real credentials, private documents, production systems, or untrusted repository code with ambient credentials.

If a check fails because of the branch, send the exact failure to one fresh correction subagent on a fast, cost-efficient model, then rerun the full gate. If that coding correction fails, use one fresh correction subagent on a high-capability model, for the second and final attempt. Allow at most two correction attempts. If a prerequisite or external artifact is unavailable, record the resulting coverage limit; never convert it into a pass.

## Phase 6: Independent review

Launch a fresh reviewer on a high-capability model, that did not implement any phase. Give it the concrete issue contract, changed-file list, commit range, and the review criteria from `.agents/commands/review.md`. Require it to inspect every changed file and return severity-ranked findings with `path:line`, reasoning, and proposed action, plus check results and limitations.

Verify each finding against the current code. Delegate every valid, actionable fix to a fresh correction subagent on a fast, cost-efficient model, then rerun the complete gate and repeat independent review. If a coding correction fails, route its retry to a fresh correction subagent on a high-capability model. Stop if the same correction fails twice. A review pass count is not a reason to stop while every remaining finding is an actionable fix. Continue correction, validation, and independent review until no material findings remain.

Resolve a truly structural ambiguity with the user only when repository conventions and the documented issue contract do not provide a safe answer. Otherwise choose the most consistent option, record the assumption, and continue without human intervention. Stop only when the same correction fails twice, a structural fork cannot be resolved without materially changing the requested contract, or required external state or authority is unavailable.

## Phase 7: Exercise observable behavior

Run the synthetic checks from reconnaissance that are safe and available. Prefer deterministic fixtures and exact output, exit-status, report, and interruption assertions. When Docker, PostgreSQL, S3, a live forge, or network access are absent, state exactly which behavior remains unobserved.

Keep these evidence classes separate in the result:

- synthetic fixtures and local fake transports;
- real local services and transports (Git, Docker, PostgreSQL, S3);
- live forge or network results;
- build and test health.

Try to replace avoidable manual checks with synthetic fixtures or existing local harnesses. Leave irreversible actions, production access, unavailable credentials, and genuine human judgment to the user.

## Phase 8: PR gate and final report

Confirm the branch is clean and summarize:

- implemented behavior and commits;
- independent review findings and their disposition;
- exact validation commands and results;
- assumptions and deviations from the issue;
- evidence obtained in each evidence class;
- remaining limitations or user-only verification.

Follow `.agents/commands/pr.md`, push the feature branch, open its pull request, and report the URL. The `/implement` invocation already authorizes this phase. Never merge or enable auto-merge.
