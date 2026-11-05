# Address PR Feedback

Verify and address unhandled feedback on the current branch's pull request. Invoking this command authorizes the fixes, commits, push, and review replies described below, but not merging or enabling auto-merge.

## Instructions

### 1. Identify the pull request

Run:

```sh
gh pr view --json number,url,state,author,headRefName,headRefOid
```

Stop if there is no open pull request for the current branch. Record the PR number and author login; `gh` substitutes `{owner}` and `{repo}` in API paths, but not the PR number.

Run `git branch --show-current` and `git status --short`. Stop if the working tree contains staged, unstaged, or untracked changes; do not mix, overwrite, stash, or discard existing work while addressing review feedback.

### 2. Fetch all feedback

Always use `--paginate`. Fetch all three comment surfaces because they are disjoint:

```sh
gh api "repos/{owner}/{repo}/pulls/$PR/comments" --paginate \
  --jq '.[] | {id, author: .user.login, path, line, body, in_reply_to_id, created_at, updated_at}'

gh api "repos/{owner}/{repo}/pulls/$PR/reviews" --paginate \
  --jq '.[] | select(.body | length > 0) | {id, author: .user.login, state, body, submitted_at, commit_id}'

gh api "repos/{owner}/{repo}/issues/$PR/comments" --paginate \
  --jq '.[] | {id, author: .user.login, body, created_at, updated_at}'
```

Include human and automated feedback. Do not wait for or assume a particular review bot. Filter out inline replies as independent findings, and do not reprocess a root inline comment that the PR author has already answered unless a later reply clearly reopens it. Avoid double-counting summaries that repeat inline findings.

### 3. Verify and classify

Open the current code and check each claim before acting. A finding can point toward a real defect while describing the wrong mechanism; fix the verified defect and explain the distinction.

Classify each unhandled item as:

1. **Fix** — valid and actionable.
2. **Dismiss** — incorrect or outside the change's justified scope.
3. **Stale** — already addressed or no longer applies to the current head.
4. **Ask** — two defensible responses would cause materially different behavior or scope.

Present a compact table with source, file and line when available, summary, decision, and rationale. Apply every unambiguous Fix after showing the table. If Ask items exist, still complete the unambiguous work, then request the needed decision before changing those items.

### 4. Fix and validate

Make focused changes and add or update deterministic synthetic tests when appropriate. Before each push, run:

Always run `git diff --check`. For Rust, manifest, toolchain, or build-related changes, also run:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
```

and focused `cargo nextest run --locked -E '<filter>'` runs that select the changed behavior, its callers and its failure paths. Record each filter and its selected scope. Do not run the full suite before pushing; step 11 of `.agents/commands/pr.md` handles attestation, including when there is no signing key.

When `ui/` changes, also run `npm ci --ignore-scripts`, `npm run lint`, `npm run build` and `npm run notices -- --check` in `ui/`. Run `docker build -t agentscm . && .github/docker-smoke.sh agentscm` only when `Dockerfile`, `.dockerignore` or `.github/docker-smoke.sh` change, or when a new compile-time include falls outside the Dockerfile's `COPY` list. Run `docker compose -f compose.yaml config -q` only when `compose.yaml` changes. Run `.github/hub-e2e.sh default` when `compose.hub.yaml`, `.github/compose.e2e.yaml`, `.github/hub-e2e.sh`, `examples/hub_seed.rs` or the Dockerfile's `e2e-seed` stages change; the `postgres` profile runs only in the hosted `Hub end-to-end` workflow, which starts automatically on a same-repository pull request that changes a PostgreSQL migration, `src/hub/store_pg.rs` or its harness; when any other change could affect it, report it as not run locally and run `gh workflow run hub-e2e.yml --ref <branch>`. An ordinary `src/` change does not require a local image build, because the `Container` workflow builds and smoke-tests the image on both architectures on every push to `main`. Tests needing PostgreSQL or S3 skip unless `AGENTSCM_TEST_DATABASE_URL` or `AGENTSCM_TEST_S3_*` are set; report such skips as coverage limits, not passes.

All applicable checks must pass before committing. Report missing prerequisites and failures explicitly; do not describe an incomplete check as successful verification. Applicable reviews remain required.

### 5. Commit, push, and verify

If code changed, stage only the files changed for the reviewed fixes, commit with a factual message such as `fix: address PR review feedback`, and push the current branch. Never bypass hooks and never force-push. Verify that the remote PR head matches local `HEAD` before replying:

```sh
git rev-parse HEAD
gh pr view --json headRefOid --jq .headRefOid
```

A new head has no valid attestation, so it is not ready to merge until it is attested. Do not attest each intermediate push. Once the review feedback is settled, run `tools/attest-pr.sh <PR#>` as in step 11 of `.agents/commands/pr.md` on the final head of the last fix round and report the result. A round with no code change on an unattested head still attests that head once the review feedback is settled. Until then, report the head as not ready, with no attestation. If the recipe fails because of the branch, treat the failure as a new item to fix in this step and attest the new head again. After two attestation failures caused by the change on successive heads, stop and report instead of looping. A failure shown to be unrelated to the change may be attested again on the same head without a push, under the evidence rule in that step.

If nothing changed, do not create an empty commit.

### 6. Reply after the push

Do not post any reply before required fixes are visible on the remote. Reply to inline comments through the pull-comments endpoint:

```sh
gh api "repos/{owner}/{repo}/pulls/$PR/comments" -X POST \
  -f body="Fixed in <sha> — <brief factual explanation>" \
  -F in_reply_to=<comment_id>
```

For a Dismiss or Stale decision, use the same endpoint with the verified rationale. Review-body and general PR comments have no inline reply target; post a concise top-level PR comment that references the original author and comment or review ID. Do not claim a fix is complete if its checks failed or its commit is not on the remote.

### 7. Report

Summarize fixed, dismissed, stale, and unresolved items; the commit and push state; replies posted; exact validation results; and limitations. Do not merge or enable auto-merge.
