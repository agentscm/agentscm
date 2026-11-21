# Implement an Issue

Take a GitHub issue to a reviewed, open pull request, and stop where its tier says. `$ARGUMENTS` must name an issue: a number, `#number` or URL. If it does not, ask for one and stop.

Invoking this command authorizes a feature branch, edits, local checks, commits, pushing that branch and opening its pull request. That is the approval `/pr` needs. It does not authorize merging or auto-merge; `AGENTS.md` says who merges.

## 1. Read the issue

1. If `.agents/prune-worktrees.sh` and `.agents/bound-target.sh` exist, run them first to reclaim disk from finished work. Report what they removed in one line, and continue if they fail.
2. `gh issue view <issue> --json number,title,body,labels,comments,state,closedByPullRequestsReferences`. Stop if it is closed or already has a closing pull request. Read the exact issues, pull requests or commits it cites, not loose search matches.
3. Read `AGENTS.md`, `README.md` and the README of each area the issue names.

## 2. Reconnaissance

Give one fresh read-only subagent on a high-capability model the issue text, the repository root and this brief:

> Do not edit files. Open every file a claim rests on, recount any number the issue gives, and mark what you could not verify. Answer in this order. (1) Does this change need to exist, or does something already do it? (2) Is there a concept to reuse: an existing type, table, command, skill, config key or doc section that already means this and could be extended instead of adding a new one? (3) Only then, the closest pattern to mirror. Return: the concept to reuse, or "none" and why; files to change, with `path:line`; what the change makes obsolete and should delete; one to three phases, each testable and committable; tests to add or change; estimated changed lines outside tests, lockfiles and generated snapshots; anything it adds that is new, or touches that is sensitive, as `AGENTS.md` defines them; questions the issue does not settle; checks an agent can run, and checks only a person can.

Spot-check its key claims yourself. If it finds nothing to implement, stop and say what the issue needs.

## 3. Propose a tier and stop where it says

Size the change with the tier table in `AGENTS.md`. It takes the highest tier any rule gives it. Report the proposed tier, the line estimate and each new or sensitive item, next to the tier the issue states.

- **At or below the issue's tier** (its `tier-*` label or its text): continue. An L change also needs a plan one owner approved in the issue. If an owner approves it in this session, post the plan on the issue, saying an agent posted it and who approved. Otherwise post the step 2 plan and stop until an owner approves. The other owner reviews it later, and that does not block the work.
- **No tier on the issue**: an S change continues. For M or L, comment the proposal on the issue and stop.
- **Above the issue's tier**: stop and ask. The person chooses: split the work, move it up a tier, or drop it.
- If a question from step 2 would change behavior, scope or a trust boundary, ask before writing code, with two or three options and a recommendation. Settle naming and file placement from the repository's patterns without asking.

## 4. Branch and claim

1. `git status --porcelain` must be empty. Never stash or discard someone's work. Run `git fetch origin main`.
2. On a feature branch for this issue, continue there; on an unrelated one, stop. From `main`, create `<type>/<number>-<slug>` from `origin/main`, where type is `fix`, `feat`, `refactor`, `docs` or `chore`.
3. Call `agentscm_start` with your session's title as `task`, the step 2 files as `paths` and the branch as `branch`.

## 5. Implement

Run the phases one after another; subagents share the working tree. Do each phase yourself, or give a fresh subagent on a fast model the repository root, the branch, the issue's behavior, this phase only, its file anchors, the concept to reuse and the tests to write. Write the test first where there is a seam. Commit each phase with a factual message, then check that `HEAD` moved, the diff matches the phase and the tree is clean.

Recount changed lines after each phase. If the change passes its tier's limit, or something new or sensitive appears, stop and ask: split the work, move it up a tier, or drop it.

Run the checks `AGENTS.md` and `README.md` give for each changed area, and `git diff --check`. A check that fails three times, or fails for a reason outside the change, is a stop. Report a skipped test as a coverage limit, never as a pass.

## 6. Review

Run `/review` on the head commit, then handle its findings as it says for the author.

## 7. Pull request

Follow `/pr`: it pushes the branch, opens the pull request with its tier and reads the hosted checks. Stay with it through checks and merge as `AGENTS.md` says, then call `agentscm_done` with `outcome` `complete`. If you stop first, checkpoint a pause naming the pull request and what it waits on.

Report the tier and why, the commits, each review finding and what happened to it, the checks you ran and their results, and what was not verified. Keep fixture, local-service and live evidence apart.
