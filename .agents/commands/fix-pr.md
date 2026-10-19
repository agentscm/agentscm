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

Make focused changes and add or update deterministic synthetic tests when appropriate. Run:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
git diff --check
```

When `ui/` changes, also run `npm ci --ignore-scripts`, `npm run lint`, `npm run build` and `npm run notices -- --check` in `ui/`; when the container image or its inputs change, also run `docker build -t agentscm . && .github/docker-smoke.sh agentscm`. Tests needing PostgreSQL or S3 skip unless `AGENTSCM_TEST_DATABASE_URL` or `AGENTSCM_TEST_S3_*` are set; report such skips as coverage limits, not passes.

All applicable checks must pass before committing. Report missing prerequisites and failures explicitly; do not describe an incomplete gate as successful verification.

### 5. Commit, push, and verify

If code changed, stage only the files changed for the reviewed fixes, commit with a factual message such as `fix: address PR review feedback`, and push the current branch. Never bypass hooks and never force-push. Verify that the remote PR head matches local `HEAD` before replying:

```sh
git rev-parse HEAD
gh pr view --json headRefOid --jq .headRefOid
```

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
