---
title: "Design: Work outcomes and scope reservations"
---

# Design: Work outcomes and scope reservations

> **Status: design for issue #310; the rules, store operations, routes and MCP tool are in the
> tree.** Nothing here is a released capability until the change has merged. Not part of this
> change: a harness preview and apply flow (#313), status relabeling (#315) and claim repair
> (#316). Liveness leases on every reservation (#311) are section 8.

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
  checkpoint, including the hook and MCP automatic pause, converts `admitted` to it. `retained` stays
  `retained`, so an explicit decision survives the end of a session; a takeover sets `admitted`.
- `admitted`: acquired by fresh admission: a new start, takeover or resume of released Work, or reopen.
- `retained`: a lead or owner kept the reservation through the `retain` transition, with a reason.
- NULL: no reservation. Unfinished Work may have NULL. Finished Work always has NULL.

A held reservation covers the current declared `scope`. Every overlap check, in both stores, reads
`reservation_basis IS NOT NULL` instead of `state <> 'finished'`: scope overlap, preflight overlap and
rename-alias scan, overview overlaps and request resolution. Work with no reservation changes its
own scope without an overlap check. The bound on overlap results (101 rows, `WorkOverlapIncomplete`)
counts reservations. Takeover or resume of Work with no reservation needs fresh admission: the
overlap check runs, a conflict is returned with no change on overlap, and otherwise the basis
becomes `admitted`. With a matching `takeover_generation` any token of the owner may resume such
Work in any state, even when the claim is not stale, so a released `blocked` item can be resumed
and a rotated token works; other owners keep the ordinary takeover rules. Progress, decision and
blocker checkpoints on unfinished Work with no reservation are refused with `HubError::Invalid`
saying to resume first. Pause, handover and finish checkpoints stay allowed, and an exact replay
resolves before this check.

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

The same owner and the same session keep the lead when the presented token or agent label changes.
That carry updates `lead_token` and `lead_agent` in one transaction, appends `work_lead_credential`,
and leaves the generation where it is. The event names the session and the previous and new agent
labels. It carries no token id. A different owner is refused with `LEAD_SESSION_MISMATCH` and
learns nothing about the lead. The same owner on a different session gets that code, the recorded
lead agent and session, and the takeover call: `agentscm_start` with `work_id` and the current
`takeover_generation`. HTTP and MCP pass the code, and any message that starts with it, through
the way they pass `SESSION_REQUIRED`. Carry runs on checkpoint and on a session touch that
identified the session, either by naming it or by arriving through a hook. An omitted MCP session
does not carry, including when session resolution would land on the lead session. Before the touch
heals the credential, startup context and `agentscm_status` say so, for the same owner only. The
touch bumps `updated_seq`, so the next checkpoint sends that newer seq. A checkpoint compares
`expected_updated_seq` before it carries, and a refused checkpoint writes nothing. Generation
fencing and session resolution stay as they are. This is not a lease. A later renewal that matches
`lead_token` and `lead_session` still matches, because the carry writes both of those fields.

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
- Release and terminal transitions, and a `finish` checkpoint, run one shared cleanup. It releases
  the live intent of every participant session and the lead through `release_session_intent`, and
  the session-less intents (announced through the plain route, tied to no session) of the tokens of
  those participants whose paths overlap the Work's scope. This must work when tokens are revoked.
- Open coordination requests that target the Work, or name it as their proposed Work, get a new
  event kind `coordination_target_released` and a saved version with their status unchanged, so a
  responded request stays responded. No request is resolved automatically. All of them are
  processed; creation already bounds how many exist. A reopen that changes the lead session
  retargets the requests that target the Work, as a takeover does, so they wait for the new lead. A resume or takeover retargets them only when the lead token or session changes; the same lead resuming leaves their status alone.
- Release and terminal transitions are cleanup and succeed at the payload quota; the accounting
  records the change without refusing. A `finish` checkpoint keeps the normal capacity check for
  itself and records its cleanup rows without one, so the cleanup never causes a refusal. Reopen
  (including its retarget) and retain use the normal capacity check.
- The new event kinds are added to the `hub_events_no_delete` trigger in the new migration (recreated
  with the full list), the compaction and history lists in both stores, and the UI event list.

The legacy fallback is closed in the same change. The managed-touch guard (named-session routes: touch,
check, hooks and MCP) refuses sessions linked to Work that is finished, fenced by generation or has no
reservation, and `announce_intent` runs a guard in its transaction too. The plain announce route has
no session field and an intent there is advisory, not a reservation, so its guard is narrower. It
refuses a path only when the path lies in the declared scope of unfinished Work on which the token
is a participant or lead but none of its sessions is currently eligible (reservation not held,
`paused` or `handover`, or fenced by generation), and no Work on which one of the token's sessions
is eligible covers the path. The refusal names the Work and says to resume it. Every other path keeps
the legacy advice: a token never linked to Work, a token whose only link is finished Work, Work with an
empty scope, and paths outside every linked scope. A token whose Work is finished can therefore post
advisory intents; it acquires no reservation and passes no admission. A token that invents a new
session name is not linked and keeps the legacy advice. These are documented limits. The guard looks
the token up by its stored repository id and does not refuse a token whose repository was renamed, as
the announce never did.

Release and terminal transitions and the finish checkpoint release the live intent of each
participant session and the lead, and, when the Work held a reservation, the session-less intents of
the participants' tokens that overlap the scope. A session that is currently eligible on other Work
keeps its intent, and a session-less intent whose paths other Work with an eligible session of the
token covers is kept. A takeover releases only the previous lead's session intent; the previous
lead's session-less intents remain until they expire.

Resuming released Work (no reservation) needs a start with the current `takeover_generation`. Any
token of the owner may do it in any state, and the claim need not be stale; the admission check then
runs. While no reservation is held, progress, decision and blocker checkpoints are refused with a
request to resume first, so a re-reading lead cannot flip the state to active with nothing held.

The `agentscm_done` and hook flow that releases the intent and then pauses the Work stays two
separate steps. A crash between them leaves active Work with a reservation and no intent. The
projection has no intent field, so that state looks like ordinary held Work; a release clears it.

## 6. The WorkOperationalV1 projection

`WorkOperationalV1` has `schema_version: 2` and `deny_unknown_fields`; version 2 added the lease
to `activity`. Fields: `work_id`, `state`,
`generation`, `updated_seq`; `outcome` (`value` of `completed`, `abandoned`, `superseded`, `unstated`
or `none`, with a `provenance` of `agent_report`, `person`, `admin` or `unknown`); `reservation`
(`status`, `basis`, `scope`, `holder_agent`, `holder_session`; never a token id); `report` (time and
kind of the last agent report); `freshness` (the existing claim status); `activity` (`live` or
`lapsed` for a held reservation, from its lease at request time, with `lease_renewed_at` and
`lease_expires_at`; `unknown` when nothing is held); `merge`
(`hidden_by_merged_pr` and a status: `observed` when hidden by a merged pull request, `none` only when
the Hub knows its forge pull-request read reached back to the Work's creation and the merge rule applies
to the Work, otherwise `unknown`); `actionable` (a recipient of
`lead`, `owner`, `admin` or `none`, with a reason); `remedies`; and `completeness` (`complete` and
the list of `unknown` field names).

`actionable` is derived from stored facts only (state, reservation, merge observation), so the only
field that follows the request clock is `freshness`. Finished Work and Work with no reservation name
no recipient. A held reservation with a legacy merged-branch observation, or held by paused or handed-over
Work, names the owner. A held reservation on active or blocked Work names the lead. The reason says
which in plain words. `completeness.unknown` also lists `freshness` when the claim status is unknown
and `outcome_provenance` when a stated outcome has no surviving event that says who stated it.

Remedies are the transitions the matrix allows from the state and reservation, sorted by name and
marked `requires_authorization`. They are not filtered by caller. Activity is always listed as
unknown, so `complete` is false for every item in this change. Ordering is by work id and no
request-time value enters any digest; no digest is added here.

Recent outcomes list a Work item as a completion only when its last version is a `finish`
checkpoint, newest first. Abandoned, superseded, released, retained and reopened Work do not
appear, and neither does Work ended by a `complete` transition: a transition leaves the Work's
current summary unchanged and records no author for one, so listing it would show a stale summary
under the lead's name. A `complete` transition is visible through the Work's history and the
operational projection (outcome and provenance). The Hub records no completion observation for a
`complete` transition; completion observations stay checkpoint-only.

Delivery without breaking strict older decoders: `GET /v1/hub/standup/work/{id}/operational`;
`GET /v1/hub/standup/operational` for every reservation-holding Work in the repository including
merged Work that still holds scope, ordered by id, at most 100, with a `complete` flag. `WorkOverview`
itself is unchanged; the HTTP and MCP surfaces add an `operational` key to its JSON (the projection of every
reservation-holding Work). `WorkStartConflict` and `WorkPreflightResult` gain an `operational` list of the
overlapping Work, skipped when empty. Over HTTP all of these are filled only when the request carries
`operational=1`; MCP always fills them, so they appear whenever there is something to report. Only
a conflict result carries `operational`: the HTTP error bodies for `WorkPreflight` and
`WorkCoordinationRequired` do not. The `operational` key of an overview is fitted to what the overview
leaves of `MAX_BRIEFING_BYTES`: entries that do not fit are shed and `complete` turns false. The
`operational` flag on `start` and `preflight` ignores other unknown query parameters, as those routes
always did; the overview query stays strict. The agent route is
`POST /v1/hub/standup/work-transition`, the person route is
`POST /v1/hub/session/work/transition` with a web sign-in and CSRF check, and the MCP tool is
`agentscm_work_transition`. There is no UI action or CLI command in this change; #313 owns
user-facing recovery. The UI only renders the new event kind in history.

## 7. Migration and mixed-client behavior

- Old records keep a NULL outcome. Finished Work reported before the migration reads as `unstated`.
- Every unfinished row is backfilled `legacy_implicit`, so what blocked before blocks after.
- The lease migration starts every held reservation's lease at migration time: 2 hours for
  `retained`, 20 minutes for the others. The clock row starts then too.
- Older clients decode all existing responses unchanged and read `abandoned` and `superseded` Work
  as `finished`. They do not see the optional `operational` fields unless they ask for them.
- The schema change is a named migration. After it has applied, an older server binary is refused,
  so rolling the server back requires restoring a backup taken before the migration.
- Older CLIs that send a start or checkpoint keep working. Taking over finished Work is now refused
  for every client, and the error names reopen.

## Reconciliation visibility

Issue #312 keeps every held reservation visible in overview and briefing pages. The legacy `hidden_by_merged_pr` operational field retains the old heuristic observation for strict compatibility; it does not mean a held reservation is now hidden. Durable reconciliation uses explicit stable-repository PR/head conditions, retains owner-controlled reservation disposition and existing authorized remedies, and never changes outcome or reservation from a merge alone. See [Work reconciliation](work-reconciliation.md).

## 8. Liveness leases

A reservation blocks other Work only while its lead session is live. Every non-NULL basis has a
lease, stored as `reservation_renewed_at` and `reservation_expires_at` in server Unix seconds, both
NULL when nothing is held.

- `admitted` and `implicit` expire 20 minutes (`LEASE_SECONDS`) after the admission, pause or last
  renewal. There is no absolute cap: a lead that keeps working keeps its reservation.
- `retained` expires 2 hours (`RETAIN_CAP_SECONDS`) after the retain. Lead activity can extend it
  by the idle lease but never shortens it, and it never holds indefinitely.
- Only the lead session's own authenticated activity renews: a hook touch or a named MCP session
  call (`touch_session`), and a checkpoint, at most once a minute. The renewal is a conditional
  update that matches only a live lease, so a late touch never revives a lapsed one. Another session
  of the same token, a helper and another token renew nothing.
- A lapsed reservation is advice. Overlap checks, preflight and coordination ignore it and the
  projection reports `lapsed`, but it keeps its basis until an admission over its scope: a start, a
  takeover or resume of released Work, a reopen, a scope-changing checkpoint, or the lead starting
  its own lapsed Work again. That admission clears it in the same transaction. The basis is
  cleared, the generation advances so the old lead and its helpers are fenced, `active` becomes
  `paused`, and the participants' intents are released. A `reservation_lapsed` event records the
  prior lead agent and session, the basis, the last renewal, the expiry, the branch, the scope, the
  paths the Hub saw the lead session hold, `push_state: unknown` and the admitting Work. Compaction
  never deletes that event kind.
- The Hub observes its own clock once a minute in `hub_lease_clock`. After a gap longer than a lease
  (a restart, or the clock jumping forward), the first observation gives every reservation that was
  live at the previous observation one more lease. Until then the clock is stale and nothing
  lapses. A backward jump only moves the observation back, so leases can last longer by the jump.
- A woken lead is refused by the managed-edit guard, not by the advisory overlap guard, so the
  pass-through after three refusals in ten minutes never applies to it. It resumes with a takeover
  start, which runs fresh admission.
- In PostgreSQL an admission holds the repository's Work lock, then locks the lapsed candidate rows,
  then checks overlaps. A touch holds the same lock in shared mode and a checkpoint in exclusive
  mode before renewing, so a renewal never lands between the check and the lapse. The heartbeat
  locks only the clock row before extending leases, and admissions only read that row.

Limits: renewal comes from hook edit checks, named MCP session calls and checkpoints. A long
read-only stretch or a long-running tool call with none of those lapses after 20 minutes; renewal on
every tool call needs the hook client and is not built yet. Hub fencing cannot stop edits made
offline, so the lapse event says the branch may hold unpushed work.
