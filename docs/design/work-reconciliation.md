---
title: "Design: Work and pull request reconciliation"
---

# Design: Work and pull request reconciliation

> **Status: read-only slice of issue #312, in the tree on this branch and not released until the
> change has merged.** It reports evidence. It does not change a Work outcome or reservation,
> store anything new, or notify anyone. Durable prerequisites, reservation reconciliation and
> multiple PR links are not implemented.

Read one Work item with `GET /v1/hub/standup/work/{id}/reconciliation` or
`agentscm work reconcile <work-id>`. Read-only tokens may call it. Work the token cannot read,
including Work of another repository, gets the same `404` as an unknown id.

## Fields and their evidence

| Field | Meaning and basis |
|---|---|
| `link` | The PR number and head the Work's lead reported with `pr-link-set` (`basis: caller_reported`). Provider approval is not implied. The link token is never returned. |
| `provider.status` | `merged` only from a recorded provider merge row for the link's PR number in this repository (by repository id). `open_same_head` or `open_head_changed` from a retained open-PR observation. `not_linked` when no link exists. Otherwise `unknown`. |
| `provider.head_relation` | `same` or `changed` against the open observation's head. `unknown` for a merge. |
| `provider.coverage` | The weakest merge-source state from the catch-up preview: `observed_only`, `incomplete` or `unavailable`. |
| `outcome` | The Work's reported outcome, as the operational projection holds it. A self-report. |
| `reservation` | The Work's scope reservation, copied unchanged. |
| `merged_holding_reservation` | A merged PR whose Work still holds its reservation, so the Work is not settled. |
| `prerequisites` | One entry per `depends_on` Work (at most 16 read). See below. |
| `reassess` | `needed` when the Work has a blocker and a prerequisite is satisfied. A derived flag. |
| `limits` | Machine-readable reasons a fact is missing or weak. |
| `atomic_snapshot` | Always `false`. |
| `digest` | Over every field except clock fields. |

A merge never proves that the task succeeded, and never changes `outcome` or `reservation`.

## Prerequisites

A prerequisite is `satisfied` only from an explicit link on that prerequisite plus a provider
merge. An open PR is `not_satisfied`. Everything else is `unknown`, including a prerequisite whose
agent reported `completed` with no linked merge: a reported completion alone is not satisfied.
`reported_outcome` is shown beside the state and never changes it. A prerequisite the token cannot
read stays `unknown` and is not named beyond its id.

## Cases

- **Multiple PRs or partial work.** One link per Work. The report carries `single_pr_link_only`.
  Work that spans several PRs is not described by this report.
- **Branch reuse and forks.** The report keys on the PR number in the explicit link, never on a
  branch name, so neither a reused branch nor a fork can settle Work here. The existing
  branch-based `settled_count` filter is unchanged and can still hide Work from the overview.
- **Repository rename.** Merge lookups use the repository id, so they survive a rename. Work
  still stored under the old repository name is not readable by a token for the new name until
  repaired (`agentscm work repair-repository`), and then reads as not found.
- **Legacy unlinked items.** `not_linked` with the `no_pr_link` limit. An unbound repository that
  cannot read links reports `unknown` with `pr_link_unavailable`.
- **Close without merge.** Only `unknown`, with `closed_state_not_readable`. A closed state is not
  stored, so "closed unmerged" cannot be told from "not yet synced".
- **Reopened PR.** An open observation outranks a retained merge row; the report carries
  `merge_and_open_observed`.
- **Outage or missed sync.** Nothing is stored by this feature, so each read recomputes. Retained
  merges are still reported. Coverage shows `incomplete` or `unavailable` and the limit
  `coverage_incomplete` or `coverage_unavailable` is added.

## Known limits

- The merge's head is not compared with the link's head (`merge_head_not_compared`); a later
  store read adds this.
- A checkpoint that omits `depends_on` replaces the Work's dependencies with an empty list on
  both stores (`replace_relations`, and `item.depends_on = checkpoint.depends_on` in the
  checkpoint path). The session-end pause carries them forward; a caller that omits them does
  not. Prerequisites reported here can therefore vanish after an ordinary checkpoint. Fixing it
  needs a store change.
- `reassess` is a derived flag, not a durable reassessment request.
- Reads are separate (`atomic_snapshot: false`), and coverage comes from the catch-up preview,
  which takes a short write transaction but creates no delivery. A change between the first and
  last read adds `work_changed_during_read`.
- Not exposed as an MCP tool yet; the Hub UI and notifications do not consume it.
- The overview's overlap list can still omit a hidden reservation holder;
  `/v1/hub/standup/operational` lists holders.
- PostgreSQL behavior needs `AGENTSCM_TEST_DATABASE_URL`, and no live forge was exercised.
