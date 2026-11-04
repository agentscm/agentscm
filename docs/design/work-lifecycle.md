---
title: "Design: Work outcomes and scope reservations"
---

# Design: Work outcomes and scope reservations

> **Status: design for issue #310; the rules, store operations, routes and MCP tool are in the
> tree.** Nothing here is a released capability until the change has merged. Not part of this
> change: lease expiry and activity observation (#311), forge reconciliation (#312), a harness
> preview and apply flow (#313), status relabeling (#315) and claim repair (#316). Until #311
> exists, the `activity` field of the projection is always `unknown`.

Today `WorkState::Finished` is both the end of the Work and the release of its scope. Two things
are mixed: why the Work ended, and whether it still blocks other agents. This design separates them.

## 1. Outcome is metadata, not a state

`WorkState` keeps its five values: `active`, `blocked`, `paused`, `finished`, `handover`. A new
nullable column `hub_work_items.terminal_outcome` holds `completed`, `abandoned` or `superseded`
and is set only when the state is `finished`. A `finished` row with a NULL outcome means it was
reported finished before this version and the outcome was never stated. The migration does not fill
it in. A `finish` checkpoint written by this version sets `completed`, attributed to an agent
report.

Reason: `WorkItem`, `WorkStartResult`, `WorkOverview` and `WorkStartConflict` use
`deny_unknown_fields` and older CLIs decode them into typed values. A new `WorkState` value would
fail to decode in those clients. A new optional column read only by new code does not.

Cost: an older client shows abandoned and superseded Work as `finished`, and cannot tell them from
completed Work. Review remains the `handover` state; no review sub-kind is added here.

## 2. Reservation is separate from declared scope

`scope` stays the declared intended scope. A new nullable column `hub_work_items.reservation_basis`
says whether the Work holds a reservation on that scope and how it got it:

- `legacy_implicit` (wire name `implicit`): the migration sets it for every row whose state is not
  `finished`, so blocking behavior does not change and no decision is invented. A pause or handover
  checkpoint, including the hook and MCP automatic pause, converts `admitted` or `retained` to it.
- `admitted`: acquired by fresh admission: a new start, takeover or resume of released Work, or reopen.
- `retained`: a lead or owner kept the reservation through the `retain` transition, with a reason.
- NULL: no reservation. Unfinished Work may have NULL. Finished Work always has NULL.

A held reservation covers the current declared `scope`. Every overlap check, in both stores, reads
`reservation_basis IS NOT NULL` instead of `state <> 'finished'`: scope overlap, preflight overlap and
rename-alias scan, overview overlaps and request resolution. Work with no reservation changes its
own scope without an overlap check. The bound on overlap results (101 rows, `WorkOverlapIncomplete`)
counts reservations. Takeover or resume of Work with no reservation needs fresh admission: the
overlap check runs, a conflict is returned with no change on overlap, and otherwise the basis
becomes `admitted`.

## 3. Transitions and authorization

A new store operation `work_transition` runs in one transaction. It is idempotent by `operation_id`
and fenced by both `expected_updated_seq` and `generation`. An exact retry returns the stored
receipt and the current item and acquires nothing. A different request with the same `operation_id`
is refused. A bounded, sanitized reason is required.

Actors:

- `lead`: an agent token whose session equals the lead session, not read-only.
- `owner`: a web person whose user id equals the item owner, or an agent token of that owner, not
  read-only, with a named session.
- `admin`: a web person with the administrator role.

Anything else, and any read-only token, is unauthorized. A revoked or expired lead token does not
block the owner or an administrator. The actor is recorded as itself and never written as the lead.

| Transition | Allowed from | Lead | Owner | Admin | Effect |
|---|---|---|---|---|---|
| release | unfinished, reservation held | yes | yes | yes | reservation cleared, generation +1, `active` becomes `paused`, other states kept |
| retain | unfinished, reservation held | yes | yes | no | basis `retained`, no generation change |
| complete | unfinished | yes | yes | no | `finished`, outcome `completed`, reservation cleared |
| abandon | unfinished | yes | yes | yes | `finished`, outcome `abandoned`, reservation cleared |
| supersede | unfinished | yes | yes | yes | `finished`, outcome `superseded`, reservation cleared; names another Work in the same repository |
| reopen | finished | yes, agent only | yes, agent only | no | fresh admission, described below |

Every other combination is refused with `HubError::Invalid` or, for an unauthorized actor,
`Unauthorized`. `complete`, `abandon` and `supersede` bump the generation unless the actor is the
lead session, so an older session is fenced. `release` never changes `current`, the blocker or the
outcome. A blocker stays in the snapshot through release and terminal transitions as history. A
terminal transition by anyone does not write a checkpoint in the agent's name; the event records
the actor and reason.

Reopen requires a session, generation, `expected_updated_seq`, `operation_id` and reason, and always
runs the overlap check whether or not the client ran preflight. On overlap it returns a conflict and
changes nothing. On success the state is `active`, the outcome is NULL (the old outcome stays in
event history), the basis is `admitted`, the generation is +1 and the caller is the lead. The
previous lead session is fenced and its released intent stays released. A person cannot reopen,
because a lead needs an agent session.

A checkpoint on finished Work is refused; an exact retry still returns its receipt. A start with
`takeover_generation` on finished Work is refused with `HubError::Invalid` naming reopen, even when
the claim is stale. A helper or reviewer joining finished Work is still allowed and acquires nothing.
Ordinary takeover permissions are unchanged; tightening stale takeover belongs to #311.

The pure decision function and the sorted remedies are implemented in `work_lifecycle.rs` and
tested against an independent expected table for every kind, actor, state and reservation.

## 4. Retention and release semantics

Release drops the reservation and keeps the Work, its history and its claim data. Retain keeps the
reservation on purpose and records why. Terminal transitions end the Work and drop the reservation.
Finished Work holds nothing under any outcome, and a merged pull request never changes outcome or
reservation by itself; it is a provider fact shown beside them.

## 5. Atomic side effects

In the same transaction, in both stores:

- An audit event `work_transition` records the transition, the actor (kind, id, login, how it
  authenticated), the reason, the prior state, basis and outcome, the generation and the
  `operation_id`. For an owner or administrator it also records the lead it acted on. A Work version
  is saved the way a checkpoint saves one.
- Release and terminal transitions release the live intent of every participant session and the
  lead through `release_session_intent`. This must work when tokens are revoked.
- Open coordination requests that target the Work get a new event kind
  `coordination_target_released` and a saved version with their status unchanged, so a responded
  request stays responded. No request is resolved automatically. All of them are processed; creation
  already bounds how many exist.
- Release and terminal transitions are cleanup and succeed at the payload quota; the accounting
  records the change without refusing. Reopen and retain use the normal capacity check.
- The new event kinds are added to the `hub_events_no_delete` trigger in the new migration (recreated
  with the full list), the compaction and history lists in both stores, and the UI event list.

The legacy fallback is closed in the same change: `announce_intent` calls `guard_managed_touch` in
its transaction, and the guard refuses sessions linked to Work that is finished, fenced by generation
or has no reservation. A session never linked to Work keeps the legacy advice. The plain intent announce route has no
session field, so its guard applies to the token's default session only, and intents announced that
way are not tied to a session and are not released by a transition.

The `agentscm_done` and hook flow that releases the intent and then pauses the Work stays two
separate steps. A crash between them leaves active Work with a reservation and no intent. The
projection reports that state and release can clear it.

## 6. The WorkOperationalV1 projection

`WorkOperationalV1` has `schema_version: 1` and `deny_unknown_fields`. Fields: `work_id`, `state`,
`generation`, `updated_seq`; `outcome` (`value` of `completed`, `abandoned`, `superseded`, `unstated`
or `none`, with a `provenance` of `agent_report`, `person`, `admin` or `unknown`); `reservation`
(`status`, `basis`, `scope`, `holder_agent`, `holder_session`; never a token id); `report` (time and
kind of the last agent report); `freshness` (the existing claim status); `activity` (always
`unknown` with a reason until #311, and never inferred from a heartbeat); `merge`
(`hidden_by_merged_pr` and a status: `observed` when hidden by a merged pull request, `none` only when
the Hub knows its forge pull-request read reached back to the Work's creation and the merge rule applies
to the Work, otherwise `unknown`); `actionable` (a recipient of
`lead`, `owner`, `admin` or `none`, with a reason); `remedies`; and `completeness` (`complete` and
the list of `unknown` field names).

`actionable` is derived from stored facts only (state, reservation, merge observation), so the only
field that follows the request clock is `freshness`.

Remedies are the transitions the matrix allows from the state and reservation, sorted by name and
marked `requires_authorization`. They are not filtered by caller. Activity is always listed as
unknown, so `complete` is false for every item in this change. Ordering is by work id and no
request-time value enters any digest; no digest is added here.

Delivery without breaking strict older decoders: `GET /v1/hub/standup/work/{id}/operational`;
`GET /v1/hub/standup/operational` for every reservation-holding Work in the repository including
Work hidden by a merged pull request, ordered by id, at most 100, with a `complete` flag. `WorkOverview`
itself is unchanged; the HTTP and MCP surfaces add an `operational` key to its JSON (the projection of every
reservation-holding Work). `WorkStartConflict` and `WorkPreflightResult` gain an `operational` list of the
overlapping Work, skipped when empty. Over HTTP all of these are filled only when the request carries
`operational=1`; MCP always fills them, so they appear whenever there is something to report. The agent route is
`POST /v1/hub/standup/work-transition`, the person route is
`POST /v1/hub/session/work/transition` with a web sign-in and CSRF check, and the MCP tool is
`agentscm_work_transition`. There is no UI action or CLI command in this change; #313 owns
user-facing recovery. The UI only renders the new event kind in history.

## 7. Migration and mixed-client behavior

- Old records keep a NULL outcome. Finished Work reported before the migration reads as `unstated`.
- Every unfinished row is backfilled `legacy_implicit`, so what blocked before blocks after.
- Older clients decode all existing responses unchanged and read `abandoned` and `superseded` Work
  as `finished`. They do not see the optional `operational` fields unless they ask for them.
- The schema change is a named migration. After it has applied, an older server binary is refused,
  so rolling the server back requires restoring a backup taken before the migration.
- Older CLIs that send a start or checkpoint keep working. Taking over finished Work is now refused
  for every client, and the error names reopen.
