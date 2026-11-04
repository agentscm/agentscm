---
title: "Design: Work and pull request reconciliation"
---

# Design: Work and pull request reconciliation

> **Status: read-only slice of issue #312, not released until the change has merged.** It reports
> evidence. The report changes no Work, reservation or delivery state, stores nothing new and
> notifies no one. The catch-up preview it uses for coverage can run the existing publication
> backfill, which can write catch-up rows. Durable prerequisites, reservation reconciliation and
> multiple PR links are not implemented.

Read one Work item with `GET /v1/hub/standup/work/{id}/reconciliation` or
`agentscm work reconcile <work-id>`. Read-only tokens may call it. Work the token cannot read,
including Work of another repository, gets the same `404` as an unknown id.

## Fields and their evidence

| Field | Meaning and basis |
|---|---|
| `link` | The PR number and head the Work's lead reported with `pr-link-set` (`basis: caller_reported`). Provider approval is not implied. The link token is never returned. |
| `provider.status` | `merged` from a recorded provider merge row for the link's PR number in this repository (by repository id); a merge row wins over any open observation. `open_same_head` or `open_head_changed` from a retained open-PR observation, only when no merge row exists (heads compare case-insensitively). `not_linked` when no link exists. Otherwise `unknown`. |
| `provider.head_relation` | `same` or `changed` against the open observation's head. `unknown` for a merge. |
| `provider.open_observed_at_ms` | Unix milliseconds when the forge read behind an `open_*` status started; that status is as old as this. Absent otherwise. Outside the digest. |
| `provider.coverage` | The weakest merge-source state from the catch-up preview: `observed_only`, `incomplete` or `unavailable`. |
| `outcome` | The Work's reported outcome, as the operational projection holds it. A self-report. |
| `reservation` | The Work's scope reservation, copied unchanged. It includes `holder_agent` and `holder_session`, caller-chosen untrusted text, the same values `/operational` exposes. |
| `merged_holding_reservation` | A merged PR whose Work still holds its reservation, so the Work is not settled. |
| `prerequisites` | One entry per `depends_on` Work (at most 16 read). See below. |
| `reassess` | `needed` means a blocker exists and a prerequisite is satisfied. Relevance to that blocker is not assessed. A derived flag. |
| `limits` | Machine-readable reasons a fact is missing or weak. See below. |
| `atomic_snapshot` | Always `false`. |
| `digest` | Over every field except clock fields (`blocker_at`, `open_observed_at_ms`). It covers coverage, so it can change during a poll with no PR change. |

Work-level limits include `single_pr_link_only`, `no_pr_link`, `pr_link_unavailable`,
`merge_lookup_unavailable`, `merge_head_not_compared`, `merge_and_open_observed` (a merge row
with a stale open observation), `merge_retention_gap` (the catch-up preview reports merge history
older than retention and there is no merge row), `closed_state_not_readable` (the observation read
succeeded and found nothing open), `observation_malformed`, `observation_unavailable`,
`coverage_incomplete`, `coverage_unavailable`, `prerequisites_truncated`,
`work_changed_during_read` and `work_recheck_unavailable` (the final re-read of the Work
failed). The provider limits apply to the row they describe: a prerequisite's own observation
limits are on its row, not the Work's.

A merge never proves that the task succeeded, and never changes `outcome` or `reservation`.

## Prerequisites

A prerequisite is `satisfied` only from an explicit link on that prerequisite plus a provider
merge; retained merge evidence stays `satisfied` during an outage. An open PR is `not_satisfied`
only when coverage is `observed_only`; under `incomplete` or `unavailable` coverage the open
status may be stale, so it is `unknown`. Everything else is `unknown`, including a prerequisite whose
agent reported `completed` with no linked merge: a reported completion alone is not satisfied.
`reported_outcome` is shown beside the state and never changes it. A prerequisite the token cannot
read stays `unknown`, carries the limit `prerequisite_unreadable` and is not named beyond its
id. Each row also shows its own `link` (basis, PR number and head; never the token) and its own
sorted `limits`.

## Cases

- **Multiple PRs or partial work.** One link per Work. The report carries `single_pr_link_only`.
  Work that spans several PRs is not described by this report.
- **Branch reuse and forks.** The report keys on the PR number in the explicit link, never on a
  branch name, so neither a reused branch nor a fork can settle Work here. The existing
  branch-based `settled_count` filter is unchanged and can still hide Work from the overview.
- **Repository rename.** Merge lookups use the repository id, so they survive a rename. Work
  still stored under the old repository name is not readable by a token for the new name, and
  reads as not found, until `agentscm work repair-repository` moves it to the new name.
- **Legacy unlinked items.** `not_linked` with the `no_pr_link` limit. An unbound repository that
  cannot read links reports `unknown` with `pr_link_unavailable`.
- **Close without merge.** Only `unknown`, with `closed_state_not_readable`. A closed state is not
  readable through the Store (ingestion tombstones the observation), so "closed unmerged" cannot
  be told from "not yet synced".
- **Merge row with an open observation.** Merged PRs cannot be reopened and only a re-read
  tombstones the observation, so the observation is stale. The report says `merged` with
  `merge_and_open_observed`, and a Work still holding its reservation is
  `merged_holding_reservation`. A PR closed unmerged and then reopened has no merge row and
  reports `open_*`.
- **Outage or missed sync.** Nothing is stored by this feature, so each read recomputes. Retained
  merges are still reported. A missed webhook writes no source status, so coverage can stay
  `observed_only` while an open status is stale until the next successful poll; check
  `provider.open_observed_at_ms` for its age. Coverage shows `incomplete` or `unavailable` with
  the limit `coverage_incomplete` or `coverage_unavailable`, and open prerequisites are then
  `unknown`.

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
