---
name: pr-gate
description: Follow the pull-request gate: task branch, tests, independent review, fixes, CI, approval, merge and a post-merge check, recording evidence in AgentSCM at each gate.
---

# pr-gate v1

Record each gate with `agentscm skill evidence --gate G --kind K --summary S` or the MCP tool
`agentscm_skill_evidence`. Read where you are with `agentscm skill status` or
`agentscm_skill_status`; its `next_action` says what is needed.

1. **branch**: work on a task branch, never on the default branch. Record `task_branch`.
2. **tests**: run the tests on the exact head commit and record `revision_tests` with the command
   and exit code. A new push makes earlier evidence stale; run and record again.
3. **review**: get an independent review of the head from another session. The hub refuses a review
   recorded with the same agent token and session as an author; it cannot tell two sessions apart
   beyond that, so independence is on you. The reviewer records `independent_review` with any
   findings (`blocking: true` for ones that must be fixed).
4. **fix**: fix every blocking finding, then record `findings_resolved` listing the finding ids
   (or a `dismissed` entry with a reason). Pushing fixes moves the head, so the tests and the
   independent review must be recorded again for the new head.
5. **ci**: add the CI label if the adoption sets `ci_label`, then wait for green CI. Record `hosted_ci`
   (with `detail.label` when a label is required).
6. **approval**: a maintainer approves. Record `human_approval`.
7. **merge**: a maintainer merges, using `merge_method` if the adoption sets one (`detail.method`).
   Retarget any stacked pull requests onto the default branch unless `retarget_stacked` is false.
   Record `merged`.
8. **post_merge**: run the post-merge regression pass on the default branch and record
   `post_merge_check`.

Evidence recorded this way is self-reported: the hub stores what the agent says and labels it
`self-reported`. It is not proof that the tests ran or the review happened. Do not stop work while
the status reports a blocking gate (tests, review, fix). That block applies only where the opt-in
Stop hook is installed (a hook running `agentscm skill stop --agent claude` or `--agent codex`).

Evidence is bound to a head commit. After a push, record the tests again for the new head and get a
new review; evidence for an older head is refused and cannot move the run back.

pr-gate gates work on a branch. A detached checkout (no branch) is not gated: the Stop hook lets
the session finish and records nothing.
