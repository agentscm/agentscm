---
title: "Design: Work outcomes and scope reservations"
---

# Design: Work outcomes and scope reservations

> **Status: design for issue #310; the rules, store operations, routes and MCP tool are in the
> tree.** Nothing here is a released capability until the change has merged. Liveness leases on
> every reservation (#311) are section 8, and the recovery inspection, previews and batches
> (#313) are section 9. Not part of this design: status relabeling (#315) and claim repair (#316).

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
Finished Work holds nothing under any outcome. A merged pull request changes outcome or reservation
only through the two Hub steps below: an opted-in release, and the completion of merged Work that
no live session holds.

Shared status names the lead agent of unfinished Work whose merge was observed, and says that only its close is left: the lead completes it, or resumes it if more remains. A routine handover also names the lead agent and sets `needs_you: false`, whether or not a merge is observed. A handover carrying an unresolved `item.blocker` still asks the person and shows the specific reported blocker. Routing uses structured state and blocker fields, without interpreting `next_action` prose. A hold still held lapses by itself; the Hub does not complete Work or release its reservation from a handover. From a merge it does two things. It completes active or paused Work whose every pull request merged, whose source issue (when it has one) that pull request closed as completed, and whose lead's lease is not live; see [Completion of merged Work](../standup.md#completion-of-merged-work). And a repository can opt in to releasing a hold whose Work's merged pull request closed its source issue as completed. Both are transitions by the policy actor (`via` `policy`, the owner's standing, no login, release or complete only). The release is at most once per Work, and only when every pull request of the Work merged, with the reason "Released by repository policy" and an operation id beginning `repository-policy:`; see [Automatic release of a merged Work's hold](../standup.md#automatic-release-of-a-merged-works-hold). The completion's outcome reads `hub_observed_merge`, so it is never mistaken for the agent's own report.

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

The `agentscm_done` and hook session end release the intent and pause the Work in one transaction,
on both stores, except that a hook's end leaves Work whose hold is live as it is (see the next
paragraph). The Work is the active Work the session leads, read inside that transaction, and the
pause is the checkpoint a lead would write: the reservation keeps its basis, so a `retained` hold
stays `retained`. If the pause fails, nothing is written: the intent stays held and the end can be
repeated. Blocked Work stays blocked, and paused, handed-over and finished Work are not found, so a
repeat of an end that committed changes nothing. A session with no Work still releases its intent.

A hook's session end does not pause Work whose hold is still live. Some clients end the session
after every turn and start it again under the same session id, so the end may be only the gap
between two turns. The Hub then releases the intent and ends the session, renews the Work's lease
(at most once a minute, as every renewal) and writes no Work event: such a client costs no Work
event per turn, and nothing automatic reaches other agents' catch-up. The Work stays active. A
session that returns leads it and renews its lease as a session that never ended does. One that
does not return leaves a hold that lapses with its lease, about 20 minutes after the end unless the
hold already ran longer; the Work then reads as any Work whose lead went quiet, and an admission
that takes its files pauses it. The end also removes the session from the live sessions shared
status reads (section 10), so between two turns the Work reads quiet once its lead's last report
is older than `CLAIM_STALE_SECONDS`, until the session calls again. A session that returns later for another task is told to end that
Work first, as any session that leads active Work is. `agentscm_done` always pauses, because it is
the agent's own word. So does a hook's end for Work that holds nothing or whose hold has already
lapsed: no lease bounds that wait.

`agentscm_done` with `outcome: complete` first applies the `complete` transition to the unfinished
Work the session leads (active, blocked or paused), fenced by the generation and sequence the Hub
reads just before, with operation id `mcp-done-complete-<work>-<generation>` so a retry replays.
The `summary` becomes the reason as one line of at most 500 bytes. The release then finds no Work
to pause. If the completion fails, the release still runs and pauses the Work, and the call returns
the completion's error, so the files are never left held; a retry completes the paused Work. After
a completion, the session's next edit needs a new `agentscm_start`, and the reply says so.

## 6. The WorkOperationalV1 projection

`WorkOperationalV1` has `schema_version: 2` and `deny_unknown_fields`; version 2 added the lease to `activity`. Fields: `work_id`, `state`, `generation`, `updated_seq`; `outcome` (`value` of `completed`, `abandoned`, `superseded`, `unstated` or `none`, with a `provenance` of `agent_report`, `person`, `admin`, `hub_observed_merge` (the Hub completed it after observing its pull request merge) or `unknown`); `reservation` (`status`, `basis`, `scope`, `holder_agent`, `holder_session`; never a token id); `report` (time and kind of the last agent report); `freshness` (the existing claim status); `activity` (`live` or `lapsed` for a held reservation, from its lease at request time, with `lease_renewed_at`, `lease_expires_at`, `lease_grace_at` (when the Hub last extended the lease after a restart or a clock gap, cleared by the next renewal, admission or decision), a reason that says what to do and who last extended the lease, and `coverage`: what renews the lead session's lease; `unknown` when nothing is held); `merge` (`hidden_by_merged_pr` and a status: `observed` when the legacy merged-branch predicate below matches, `none` only when the Hub knows its forge pull-request read reached back to the Work's creation and the merge rule applies to the Work, otherwise `unknown`); `actionable` (a recipient of `lead`, `owner`, `admin` or `none`, with a reason); `remedies`; and `completeness` (`complete` and the list of `unknown` field names).

`reservation.status` is `held` while the reservation blocks overlapping Work, `lapsed` once its
lease expired (it then blocks nobody and the next admission over its scope clears it) and `none`
when nothing is held; a lapsed reservation keeps its basis, scope and holder. `actionable` is
derived from stored facts and that status (state, reservation, merge observation), so the fields
that follow the request clock are `freshness`, `activity` and a lapse. Finished Work, Work with no
reservation and a lapsed reservation name no recipient. A held reservation with a legacy merged-branch observation, or held by paused or handed-over
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
`agentscm_work_transition`. The recovery read, previews, batches, CLI commands and the Hub task
page action of section 9 build on these routes.

## 7. Migration and mixed-client behavior

- Old records keep a NULL outcome. Finished Work reported before the migration reads as `unstated`.
- Every unfinished row is backfilled `legacy_implicit`, so what blocked before blocks after.
- The lease migration starts every held reservation's lease at migration time: 2 hours for
  `retained`, 20 minutes for the others. The clock row starts then too. A second migration adds the
  session columns for tool activity and running shell calls; existing sessions start with none.
- Older clients decode all existing responses unchanged and read `abandoned` and `superseded` Work
  as `finished`. They do not see the optional `operational` fields unless they ask for them, nor
  the `reclaimed` list of a start, checkpoint or transition unless the request has
  `?reclaimed=1`, which the current CLI sends. An older Hub ignores that parameter.
- The schema change is a named migration. After it has applied, an older server binary is refused,
  so rolling the server back requires restoring a backup taken before the migration.
- Older CLIs that send a start or checkpoint keep working. Taking over finished Work is now refused
  for every client, and the error names reopen.

## Reconciliation visibility

Issue #312 keeps every held reservation visible in overview and briefing pages. The legacy `hidden_by_merged_pr` operational field retains the old heuristic observation for strict compatibility; it does not mean a held reservation is now hidden. Its predicate requires a provider-confirmed merge on the same non-base branch, no reported blocker, and a provider `merged_at` at or after the Work's last check-in, with the existing automatic-pause exception. A subsequent handover can therefore leave an older merge visible in durable catch-up while operational `merge.status` is `unknown`. Routine handover routing does not relax that predicate or let an older merge describe resumed work. Durable reconciliation uses explicit stable-repository PR/head conditions, retains owner-controlled reservation disposition and existing authorized remedies, and never changes outcome or reservation from a merge alone. See [Work reconciliation](work-reconciliation.md).

## 8. Liveness leases

A reservation blocks other Work only while its lead session is live. Every non-NULL basis has a
lease, stored as `reservation_renewed_at` and `reservation_expires_at` in server Unix seconds, both
NULL when nothing is held.

### Expiry and renewal

- `admitted` and `implicit` expire 20 minutes (`LEASE_SECONDS`) after the admission, pause or last
  renewal. There is no absolute cap: a lead that keeps working keeps its reservation.
- `retained` expires 2 hours (`RETAIN_CAP_SECONDS`) after the retain. Lead activity can extend it by
  the idle lease but never shortens it, and it never holds indefinitely.
- Only the lead session's own authenticated activity renews, at most once a minute: its hooks'
  tool calls and edit checks, its named MCP session calls (`touch_session`) and its checkpoints.
  Renewal follows the lead credential (#337): a checkpoint and a touch renew after the credential
  carry, so the same owner and session renew under a new token in the same call. Another session
  of the same token, a helper and another token renew nothing, and neither does a call that names
  no session, even when the Hub resolves it to the lead session. Such an unnamed `agentscm_start`
  that repeats the lead's start renews nothing and cannot hold a lapsed scope again; the lead
  names its session for that.
- Renewal is a conditional update that matches only a live lease of `active` or `blocked` Work, so
  a late touch never revives a lapsed lease, and a paused or handed-over item lapses on its own
  clock even while its session works on something else.
- A pause or handover checkpoint restarts a live lease and a retain extends it. Neither revives a
  lapsed one: a pause or handover keeps the lapsed expiry, and a retain is refused with
  `LEASE_LAPSED`. Only fresh admission holds the scope again.
- A shell call may run longer than a lease. The hooks report its start and end, the session counts
  the calls still running (`open_calls`) and when the first of them started (`open_call_at`), and
  each start extends the session's live leases to 60 minutes (`IN_FLIGHT_CAP_SECONDS`) after that
  first start. When the last call ends they fall back to the idle lease; a retain keeps its own
  longer expiry. A count older than the cap is stale, its ends lost: the next start counts afresh
  and the next end falls back to the idle lease, so calls whose ends were lost hold for at most 60
  minutes. Nothing renews from the background: every
  renewal is a call the session made.

### Lapse

- A lapsed reservation is advice. Overlap checks, preflight and coordination ignore it and the
  projection reports `lapsed`, but it keeps its basis until an admission over its scope: a start, a
  takeover or resume of released Work, a reopen, a scope-changing checkpoint, or the lead starting
  its own lapsed Work again. That admission clears it in the same transaction. The basis is
  cleared, the generation advances so the old lead and its helpers are fenced, `active` becomes
  `paused`, and the participants' intents are released. A `reservation_lapsed` event records the
  prior lead agent and session, the basis, the last renewal, the expiry, the branch, the scope, the
  paths the Hub saw the lead session hold, `push_state: unknown` and the admitting Work. Compaction
  never deletes that event kind.
- The admission response lists every lapsed hold it took the scope of in `reclaimed`: the Work, its
  lead agent, its branch, when it lapsed and a notice that the branch may hold edits its lead never
  pushed. A start, a takeover, a scope-changing checkpoint and a reopen all report it. MCP always
  includes it and adds the notice to the start text; over HTTP it is sent only when the request has
  `?reclaimed=1`, so a strict older client decodes the reply unchanged.
- A woken lead whose scope was taken is refused by the managed-edit guard with `LEASE_LAPSED`: when
  the hold lapsed and how long after the session last renewed it, which Work took the scope, that
  the branch may hold unpushed work, and the `agentscm_start` with `work_id` and
  `takeover_generation` that resumes it through fresh admission. Its `agentscm_start` of the same
  Work without `takeover_generation` gets the same answer. Hooks, MCP and HTTP pass that text
  through. The guard's refusal is not the advisory overlap guard, so the pass-through after three
  refusals in ten minutes never applies to it.
- A lead whose lease lapsed while nobody took its scope is refused by the same guard with
  `LEASE_LAPSED`: nothing holds the scope, so it must start its Work again before editing.
- Taking over Work that holds a reservation needs its lease to have lapsed, for its owner's other
  sessions as for other owners. A revoked lead token never opens it early: token renewal revokes
  the old token before the lead's next call carries the lead to the new one (#337), so a live lead
  can look revoked for up to a lease. A stale claim, a progress report older than
  `CLAIM_STALE_SECONDS`, never opens it either. Work that holds nothing keeps the claim-age rule,
  and `handover` Work is open as before.
- A takeover of a lapsed hold is a fresh admission, like the resume of released Work: it checks
  the scope against live holds and clears the lapsed ones it overlaps.

### Clock

- The Hub observes its own clock once a minute in `hub_lease_clock`. After a gap longer than a lease
  (a restart, or the clock jumping forward), the first observation gives every reservation that was
  live at the previous observation one more lease. Until then the clock is stale and nothing
  lapses. A backward jump only moves the observation back, so leases can last longer by the jump.
- In PostgreSQL an admission holds the repository's Work lock, then locks the lapsed candidate rows,
  then checks overlaps. A touch and a reported tool call hold the same lock in shared mode and a
  checkpoint in exclusive mode before renewing, so a renewal never lands between the check and the
  lapse. Admissions only read the clock row. A heartbeat that extends leases after a gap takes the
  clock row, then each affected repository's Work lock in repository order before updating its
  rows, the lock an admission takes before any Work row, so it never waits in a cycle with an
  admission. If PostgreSQL still aborts a transaction for a deadlock or a serialization failure,
  the call fails as `Busy` with nothing written, and a retry is safe.

- A clock heartbeat that grants grace records `reservation_grace_at` on each reservation it
  extended (named migration `work_recovery`). The projection reports it as `lease_grace_at` and
  says the Hub, not the lead session, extended the lease. Renewal, admission, pause, handover,
  retain and release clear it.

### Coverage and participants

- `activity.coverage` says what renews the lead session: `tool_calls` once a hook reported one of
  its tool calls, `tool_ends` when its only hook reports each call's end and none's start (Cursor),
  `edits` for hooks that report only edits (an older `client setup`), `mcp_only`, or `unknown`
  when the Hub keeps no session record. Anything but `tool_calls` adds
  `activity_coverage` to `completeness.unknown` and a reason that names the fix. The per-agent
  table is in [Coding agents](../agents.md#holds-stay-live-while-the-agent-works).
- Any writer on the repository may join Work as a helper or reviewer, but a Work item takes at most
  25 participants from other owners (`MAX_OTHER_OWNER_PARTICIPANTS`). That bounds what other
  owners add to cleanup and `updated_seq`, and keeps the rest of the 100 participants for the
  owner's sessions, so other owners cannot block a resume or reopen.

### Fixed values

The 20-minute lease, the 2-hour retain cap, the 60-minute in-flight cap and the once-a-minute
renewal are constants, not configuration. They are what makes a hold mean the same thing to every
agent and owner on a repository: a per-Hub or per-owner setting would let one owner's long lease
block another owner's agents, which is the failure these leases remove, and it would add a
compatibility surface every client must read. A change to a value is a code change reviewed like
any other.

### Runtime limits

- A non-shell tool call that runs longer than 20 minutes, or any call in an agent whose hooks report
  no tool start (Cursor), lapses meanwhile; the lead re-admits when it next starts its Work.
- Hub fencing cannot stop edits made offline or by an agent with no hooks. The lapse event says the
  branch may hold unpushed work, and the woken lead is told to push or set it aside.
- Leases run on the Hub's clock; a client clock never decides a lapse. A backward jump of the Hub's
  clock lengthens leases by the jump.
- A shell call keeps its session's leases live for at most 60 minutes from the first call still
  running; a longer one lapses meanwhile, like a long non-shell call.
- A running shell call keeps the lease, not the claim. Shared status reads the lead session as
  there for 20 minutes after each call (section 10), so inside one call longer than that its row
  reads quiet while the lease is still live for up to 60 minutes.
- A session name is only a label within a token, so any holder of the token can send activity under
  the lead's session name and renew its lease. That gives nothing beyond what the token already
  allows.
- Renewal from each agent's hooks is tested with sample payloads written from the vendors'
  documentation, not in live sessions of every agent.

## 9. Recovery: inspect, preview, apply

`work_recovery.rs` turns the projection and the transition rules into answers for a harness agent
and for a person: what is active, what needs them, what can resume, why each item still exists,
what ending it would reach, and which remedies they may apply. It adds no authority. The store
operations of sections 3 and 8 stay the only writers.

### Inspection

One store read, `work_recovery_read`, runs in one transaction on both stores and never writes. It
reads the page of the reader owner's unfinished Work by id after a cursor (at most 50), or one
`work_id` of the repository whoever owns it; with up to 32 paths it also reads the live holds over
them (the admission overlap scan, so the exact path pairs and the 100-holder bound are the
admission's) and the owner's unfinished Work whose declared scope overlaps them (at most 100
scanned, 25 listed). For every listed Work it reads the open coordination requests that target or
propose it and the unfinished Work whose `depends_on` names it, 25 each plus one to say the list
was cut. The named migration `work_recovery` adds an index on `hub_work_relations(related)` for
that read.

Each item carries the caller's standing (`lead`: its token and named session lead it; `owner`: the
owner, or another session of an agent of the owner; `admin`: a Hub administrator signed in on the
web; `other`), a bucket (`active`, `needs_you`, `resumable`, `finished`, `other`), a sentence on why
it still exists, the follow-up that a release or an end keeps (next action and blocker), the
affected requests and dependents, the projection, and every remedy.

### Remedies

A remedy is a transition (`release`, `retain`, `complete`, `abandon`, `supersede`, `reopen`),
`resume` (a start with `takeover_generation`, a fresh admission) or `handover` (the lead's handover
checkpoint, optionally with a narrower scope). Each is permitted or refused for this caller with a
stable code, decided in this order: a read-only token; for a transition, the standing and the
matrix of section 3 (a retain of a lapsed hold is `lease_lapsed`); for a resume or handover, a
person or an unnamed agent (`needs_session`) and finished Work (`wrong_state`, reopen instead).
`handover` needs the lead. `resume` follows the start's takeover rules: the lead may resume its own
lapsed, paused or handed-over Work; any token of the owner may resume Work that holds nothing; a
token may resume its own paused Work; anyone may take Work over once its hold lapsed, or Work that
holds nothing once its claim is stale, or handed-over Work; otherwise `hold_live` (with the lease
expiry) or `claim_current`. A permitted remedy carries the exact call: the MCP tool (none for a
person), the route, and the arguments with `generation` and `expected_updated_seq` from the read,
plus the fields the caller supplies (`operation_id`, `reason` or `summary`).

The preview of one remedy adds what it would change: state, reservation and outcome after, whether
the generation advances, who is fenced (agent and login, never a token or another owner's session),
the freed and kept paths, the affected requests and dependents, the follow-up, and whether apply
runs a fresh admission. A handover scope must lie inside the current scope.

Previews mirror the store's rules but are not authority. They do not check the participant caps or
a reopen's or resume's overlap; the apply does, inside its own transaction, with the generation and
`updated_seq` fences, and refuses with nothing changed if anything moved.

### Batches

`apply_batch` runs up to 25 `work_transition` requests in order, each in its own transaction under
the same actor, and returns one outcome per item. A refused item never stops the rest; an exact
retry of the batch replays applied items through their `operation_id` and retries the others.
Refusals carry a fixed code and sentence, never the store's internal text, except a `LEASE_LAPSED`
refusal, which every surface already passes through. The shared error envelope (#314) will carry
the same codes for single calls.

### Resolving before a duplicate

An MCP lead start with paths and no `work_id` first reads the owner's resumable Work over those
paths. If one exists under another identity, the start creates and claims nothing and lists it
with the resume call; `new_work: true` starts different work. A start whose request key or forge
source is that Work's own goes ahead, since the store resolves it. HTTP and CLI starts are
unchanged, so older clients keep their behavior. A start refused for any reason in MCP carries
`recovery`: the inspection of the conflicting Work with the requested paths, cut in the compact
default to each item's permitted calls, the live holds and the resumable ids.

### People

The person routes take `repository`, pass the same allow-list and forge role check as a read-token
mint, and call the same functions with a person caller. The Hub's task page offers the owner's and
an administrator's permitted transitions after a preview, sends one-item batches with the preview's
fences, and shows the outcome and its retry advice. #289's close of an orphaned request stays on
the Inbox; when the target's lead token is gone too, the owner releases or ends the target Work, or
has another agent resume it so the Inbox can close the request for that agent.

## 10. Working and waiting sessions in shared status

A claim follows its lead's reports: a start or a checkpoint sets `last_checkin_at`, a tool call
does not. So the claim goes stale after `CLAIM_STALE_SECONDS` for a lead that works on without
reporting, and for one that stopped to wait. Neither says the session is gone. Before shared
status (`actionable_status.rs`) calls a row quiet, it reads what the session itself did: its calls,
and the Hub's in-memory record of why it stopped (`session_waiting.rs`).

- **Still calling.** While the lead session called the Hub within the last 20 minutes (the store's
  live sessions, which the conflict and check readers already use), a stale claim reads
  `freshness: active` on the row.
- **Waiting for its person** (a permission prompt, a question, or the end of a turn that may have asked for something): the row is `awaiting_party`, names that person and reads `freshness: active`, from the moment the session stops. It does not set `needs_you`, because the person's `session_waiting` action asks them, 60 seconds later, and one wait counts once. Active and paused Work read this way. A reported blocker, a handover and an observed merge say more and are shown instead on the Work row. The separate `session_waiting` action still asks the person for a question or permission; routing a routine handover to an agent does not remove that action.
- **Ended its turn and asked nothing**: a finished turn is read, not answered. The session's
  hook judges its whole last message where it is and sends only the answer. The message asked
  nothing when it is English prose or only identifiers (an id, a path, a number), with no
  question mark, no word to the person, no offer or choice and no word for waiting, needing or
  being stuck; a wrong "asked" only shows a session, a wrong "asked
  nothing" hides a request, so anything else counts as an ask. A hook that sends no answer
  leaves the Hub its excerpt, which decides only when it is the whole message and not its cut
  end. Where the turn asked nothing, no `session_waiting` action is listed, and the row stays
  `active`, names its lead agent and asks nobody, like a stop on background work: the session
  is there and the task is not stuck. One exception keeps a request from being hidden: when the
  Work is paused and its reported next action speaks to its person or names them or the owner,
  the row is `awaiting_party`, names that person and sets `needs_you`, since no action asks for
  it.
- **Stopped on its own background work**, which will resume it: the row stays `active` and asks
  nobody.
- **How long.** A wait for a person is believed until the session's next call or its end, for at
  most 24 hours and no longer than the agent token that reported it stays valid. A stop on
  background work is believed for 60 minutes (`IN_FLIGHT_CAP_SECONDS`), as long as a running shell
  call, because nothing shows that the background work still runs. After that the row reads from
  the claim again.
- **Unknown, never stale.** After a Hub restart that left no list of waits (the Hub died, or was
  down for 10 minutes or more), or when the Hub dropped a wait for room, a quiet
  Work whose lead session has not called since reads `freshness: unknown` with the limit
  `lead_wait_unknown`. It names nobody and needs nobody until that session calls again, or until
  24 hours after its last check-in, when no wait could still be believed. A Hub that was stopped
  leaves its waits for the process that starts next, which then knows them as before. What
  sessions reported while no Hub ran is still missing, so Work whose lead session called in the
  hour before the stop, has no wait in that list and has not called since reads unknown too.
- **Who is asked.** A row asks a person for Work its agent reported blocked, including a handover carrying an unresolved `item.blocker`, and for reassessment of such a blocker. The handover shows the specific reported blocker. A routine handover names the lead agent and sets `needs_you: false`, regardless of merge observation. The agent decides whether to complete the Work or resume it; shared status neither infers a blocker from `next_action` prose nor completes or releases the Work. A quiet or paused row asks nobody, held or not, because a hold its lead no longer renews may lapse, or be retained. A directed request to a quiet lead says its lead is not running only when its session is not live. The projection's `actionable` field still names the owner for a held pause or handover; shared status does not turn that into something the owner must do.
- **Not changed.** The store's `claim_status`, the operational projection's `freshness`, takeover
  rules and which scope overlaps are listed still follow the lead's reports. Leases are untouched:
  a waiting session's hold lapses 20 minutes after its last call like any other, so waiting never
  blocks other Work for longer.
