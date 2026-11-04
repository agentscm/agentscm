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
| `provider.status` | `merged` from a recorded provider merge row for the link's PR number in this repository (by repository id); a merge row wins over any open observation. `open_same_head` or `open_head_changed` from a retained open-PR observation, only when a merge row is known to be absent (heads compare case-insensitively). If the merge lookup itself failed, the Work still reports `open_*` with `open_observed_at_ms`, because the observation is a real fact, but it carries `merge_lookup_unavailable`: a merge row may exist that the lookup could not see. `not_linked` when no link exists. Otherwise `unknown`. |
| `provider.head_relation` | `same` or `changed` against the open observation's head. `same` or `changed` for a single retained merge, and `unknown` when exact merge-head evidence is unavailable or ambiguous. |
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
`merge_lookup_unavailable`, `merge_head_not_compared` (exact head unavailable), `merge_head_changed`, `multiple_merge_observations`, `merge_time_unknown`, `merge_and_open_observed` (a merge row
with a stale open observation), `merge_retention_gap` (the catch-up preview reports merge history
older than retention and there is no merge row). The gap is repository-wide and persists after the first prune, so it does not say this PR's merge was pruned; a per-PR answer needs a store read, which is out of scope here, `closed_state_not_readable` (the observation read
succeeded and found nothing open), `observation_malformed`, `observation_unavailable`,
`coverage_incomplete`, `coverage_unavailable`, `prerequisites_truncated`,
`work_changed_during_read` and `work_recheck_unavailable` (the final re-read of the Work
failed). The provider limits apply to the row they describe: a prerequisite's own observation
limits are on its row, not the Work's.

A merge never proves that the task succeeded, and never changes `outcome` or `reservation`. Exact merge reads use provider, host, stable repository identity and PR number, ordered by merge commit and bounded to 17 rows; multiple rows cannot establish one exact prerequisite head. Missing occurrence timestamps stay unknown (`merge_time_unknown`). The strict V1 response retains its existing fields and enum values; exact retained merge records are available to the internal store reader for later versioned reconciliation.

## Prerequisites

A prerequisite is `satisfied` only from an explicit link on that prerequisite plus one provider
merge whose head matches the link; retained merge evidence stays `satisfied` during an outage. An open PR is `not_satisfied`
only when coverage is `observed_only` and the absence of a merge row is known; under `incomplete`
or `unavailable` coverage, or when the merge lookup failed (`merge_lookup_unavailable` on its
row), the open status may be stale, so it is `unknown`. Everything else is `unknown`, including a prerequisite whose
agent reported `completed` with no linked merge: a reported completion alone is not satisfied.
`reported_outcome` is shown beside the state and never changes it. A prerequisite that is not
satisfied by evidence stays `unknown` and its row says why it could not be read: `prerequisite_unreadable`
(the token cannot read it; it is not named beyond its id), `prerequisite_not_read` (past the
16-prerequisite cap) or `prerequisite_read_failed` (a store error on its link read). Each row also shows its own `link` (basis, PR number and head; never the token) and its own
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
- **Close without merge.** The V1 status stays `unknown` for compatibility. An exact retained observation with `state: closed` and `merged: false` adds `closed_unmerged_observed`. A generic tombstone or absent observation never proves closure. A strictly newer open observation supersedes the tombstone; older observations cannot resurrect it. Conflicting pull-request observations with the same read timestamp are retained as unknown (`observation_order_ambiguous`) until a strictly newer read; exact duplicate observations create no new event. This conflict rule does not apply to branch observations.
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

- The merge head is compared with the explicit linked head. A different head has `merge_head_changed`; multiple retained merge observations have `multiple_merge_observations` and leave the head comparison unknown. Only one exact matching retained merge satisfies a prerequisite.
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


## Durable caller declarations

The separate `GET /v1/hub/standup/work/{id}/declarations` response has `schema_version: 2`. It contains a caller-declared set of additional PR links and typed prerequisite conditions. It does not establish provider facts or claim that a Work item completed. Read-only tokens can read it. A foreign repository receives the same not-found response as unknown Work. The compatibility single-link endpoint owns an independent `compatibility_link` slot; setting or clearing that slot cannot erase additional links or conditions.

`POST /v1/hub/standup/declarations` accepts `work_id`, `operation_id`, `generation`, `session`, `expected_updated_seq` and a complete `declarations` replacement. Only the current lead token/session may write. A successful mutation advances the Work sequence without rewriting its progress report, blocker, outcome or reservation. Exact retries return the original receipt and current view; operation ID reuse for a different canonical request is refused. Link and condition order is canonicalized by stable `id`. Explicit empty arrays remove the corresponding declarations.

Each additional link has an `id` and a `target` containing the authorized stable `repository_id`, PR `number` and full lowercase `head_sha`. Each prerequisite has an `id` and a tagged `requirement`: `merged_pull_request` names a readable prerequisite `work_id` and an exact `target`; `legacy_work` names a dependency whose evidence target has not been declared. IDs are bounded visible labels. Explicit updates allow at most 32 additional links and 32 conditions. Imported legacy dependencies may retain up to the existing 100-relation bound; none is silently dropped. All PRs in a partial or multiple-PR Work remain separate declarations; satisfying all declared merge conditions still cannot prove whole-task success.

The named migration copies existing `depends_on` relations once into deterministic `legacy-<work-id>` conditions with `legacy_work` requirements. New Work starts seed the same independent record, including an empty record. These conditions are unbound caller reports and remain unknown until explicitly replaced with evidence-targeted requirements. Legacy checkpoint omission still clears its legacy relation hints, but cannot delete the durable declaration record. The initial `declaration_seq` is the Work sequence at migration or admission; later updates use their committed event sequence. Every Work has a declaration record, so there is no absent-record sequence sentinel. The mutation records the existing `work_pr_link` audit kind with `operation: declarations_set` for history and catch-up compatibility.

CLI equivalents are `agentscm work declarations <work-id>` and `agentscm work declarations-set --request <json-file>`. The v1 read-only reconciliation response remains wire-compatible and still projects legacy `depends_on`; durable evaluation, reassessment and additional surface integration are separate from this declaration operation.

## Durable evaluated reconciliation

`GET /v1/hub/standup/work/{id}/reconciliation-v2` reads the durable v2 evaluation. `POST /v1/hub/standup/reconciliation` explicitly evaluates retained server evidence and persists its result. The request contains only `work_id`, `operation_id`, `generation`, `session`, `expected_updated_seq` and `declaration_seq`; callers cannot submit evidence or evaluated states. The current lead session must authorize the operation. An exact retry returns its committed receipt and the latest view; reusing its operation ID for another request is refused. CLI equivalents are `agentscm work reconciliation <id>` and `agentscm work reconcile-apply --request <file>`.

Both stores gather declarations, prerequisite Work versions, exact merge rows, retained PR observations, source coverage and retention boundaries in one transaction. SQLite uses an immediate write transaction; PostgreSQL uses a repeatable-read transaction and the existing Work/capacity locks. A concurrent later report or declaration update cannot be overwritten. The successful operation advances the Work sequence for fencing and records `work_pr_link` audit history with `operation: reconciliation_applied`; it does not change the original progress report, blocker, Work state, reported outcome, reservation or meaningful-report clock.

The persisted snapshot binds the resulting Work sequence, declaration sequence and a deterministic digest of the retained provider and prerequisite facts. `retained` is historical evidence. `current` is present only while those three bindings still match a new atomic read. A later checkpoint, prerequisite report, declaration/link change, source poll, observation or retention change makes the snapshot stale, with explicit reasons and no current condition success. Poll timestamps can invalidate a snapshot even when the apparent PR state is unchanged. These bindings describe retained observations, not live forge freshness; a missed webhook cannot be detected before another provider read.

Each exact-head merge condition is independent. One matching retained merge satisfies that condition even during an outage; changed or ambiguous merged heads remain unknown. An open or explicitly closed-unmerged observation establishes a negative only with complete observed coverage and no merge-retention gap. Malformed, missing or equal-stamp-ambiguous observations remain unknown. Missing merge occurrence time is recorded as `merge_time_unknown`, separate from the retained merge fact. Legacy unbound conditions remain unknown. All conditions satisfied is not whole-task completion.

A satisfied condition clears only its own structured obstruction. If a free-text blocker remains, the operation records a stable reassessment request bound to that blocker report, condition declaration and exact merge evidence. It never rewrites or clears the free-text blocker or unrelated conditions. Reapplying identical evidence uses the same request identity; audit history preserves earlier requests when a later report changes them. The derived next action names the remaining conditions or asks for blocker reassessment. A merged Work link with a held reservation reports `held_owner_decision_required`; an explicit retained basis remains `explicitly_retained`. Remedies come from the existing lifecycle policy and still require that transition's actor authorization. No merge releases a reservation or supplies a completed outcome.

Evaluation is bounded by the declaration limits, 17 retained merge rows per target and a 128 KiB persisted snapshot ceiling. Capacity accounting includes the current evaluation row, operation receipt and audit history. Recovery after missed synchronization is an explicit new fenced apply after provider synchronization; there is no new background reconciliation worker or live fetch in this operation. Synthetic store tests are distinct from live provider qualification, which has not been performed.
