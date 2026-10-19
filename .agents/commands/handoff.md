# Session Handoff

Produce a self-contained handoff for a fresh session. This command reports state only and must not modify the working tree, push, open a pull request, or post comments.

## Instructions

Gather the current state:

```sh
git branch --show-current
git log --oneline --decorate -10
git status --short
git log --oneline origin/main..HEAD
git diff --stat origin/main...HEAD
gh pr view --json number,url,state,title,headRefOid 2>/dev/null || echo "No PR for this branch"
```

Inspect the changed files and relevant documentation rather than relying only on commit subjects. Then produce these sections:

## Handoff — {feature or topic}

Include the absolute date, branch, local `HEAD`, whether the working tree is clean, and the PR URL and state or `none`.

### Completed this session

Use a table with the completed item, status, and concrete artifact such as a commit SHA or repository-relative path. Record exact validation commands and results. Keep build and test results, fixture checks, real-transport checks and live-service results distinct.

### Current state

State what is committed, pushed, uncommitted, or untracked. Name any failing checks and whether they are caused by the current diff. Do not call missing context, a skipped test, or an incomplete run a successful verification.

### Next session: what to do

Give an ordered, concrete worklist with repository-relative files, functions or interfaces, and commands. Start with the most immediate action and include any review or PR follow-up that is already known.

### Decisions made

List only non-obvious decisions that constrain later work, each with its rationale.

### Claims not yet verified

Separate inferences and inherited claims from facts you checked directly. State what evidence would settle each claim. If subagents contributed, say which claims you independently checked and which remain relayed.

### Open questions or blockers

Include only unresolved items that require user input or external state. Omit this section if there are none.

### Key files

List the repository-relative files the next session should read first, in priority order.

Keep the handoff focused on resumption, not a transcript of the session. Never include credentials, confidential information, machine-specific paths, vendor promotion, generated-by footers, `Co-authored-by` trailers for tools, or session links.
