---
title: "Work insights"
---

# Work insights

> **Alpha: not for production.** The API and storage schema may change without migration.

Work insights provide a bounded, repository-scoped view of reported work duration, coordination signals, usage reports and stored publication observations. They are available to the same repository readers as the Hub dashboard. The API defaults to a seven-day window and returns at most 20 work items per page.

The projections keep different kinds of evidence separate:

- **Reported work state** comes from stand-up checkpoints. “Reported finished” means an agent reported a finish; it does not mean the work was accepted, merged or landed.
- **Usage** is a set of retained reports. Coverage is `unknown` when there are none and `partial` when at least one exists. A report does not prove that every attempt was captured.
- **Publication state and checks** are the last retained controller observations attached through a claimed source association. Check records keep the latest result per pull-request context; superseded heads’ checks are not retained. They do not establish current approval, qualification, acceptance or landing.

## Observed contributions

The All repositories homepage, `GET /v1/hub/session/value-metrics` (and per repository
`GET /v1/hub/standup/value-metrics`), and
`agentscm_value_metrics` share the deterministic `agentscm.observed.v1` projection.
The MCP app uses that tool's structured result. Repository tokens remain limited to their
own repository; the signed-in homepage aggregates only repositories its reader can access.
No read runs a model, polls a provider, acknowledges context, or grants execution authority.

The result separates prepared context returns and explicit acknowledgements, unresolved and
resolved refusal episodes, configured follow-up evidence, recorded resources, and coverage.
Context preparation does not establish transport delivery, application, or that an agent read
it before editing. A refusal is not necessarily a defect. Retained self-reported check evidence
keeps its assurance and source/recipe/policy applicability; it is not independent execution
verification. Unknown observation populations and AgentSCM's token overhead remain unknown;
hook overhead is counted as described in [contested edits and hook overhead](#contested-edits-and-hook-overhead).
Group `availability` and evidence `basis`, `assurance`, `availability`, and `applicability`
explain the evidence available behind each number.

False-trigger classifications, failed follow-ups and cancelled follow-ups currently have no
observation producer. Their fields are `null` and the contribution surfaces say **Not captured**.
This does not establish zero such outcomes. `issues.dismissed` counts only contested edits an
agent pushed through after a refusal (below); it is `null` for a single-Work query and when no
hook session was seen in the window. Supported counts can still
be observed zeros; other missing values, including context application and usage, stay unknown.

## Contested edits and hook overhead

Each contested-edit refusal is recorded with the session, file and the hold (intent) that caused
it. Later Hub calls settle it:

| Outcome | What the Hub observed |
|---|---|
| `pushed_through` | The same session's hook reported an edit of the file while its holder still kept it. |
| `asked` | The session asked for the file itself (`agentscm_ask`) and no grant has followed. The holder may have declined. The Hub's own request after a refusal does not count as asking. |
| `handed_off` | The holder granted a request for the file. |
| `pending` | None of these, and a hold of the same holder still keeps the file. A hold renewed past its 30-minute cap under a new id still counts. The refused session's own hold never counts: two sessions of one token are two agents, so a session that waited and then took the file went elsewhere. |
| `went_elsewhere` | None of these, and the holder no longer keeps the file. |
| `unknown` | None of these, and the hold's record is gone. |

Each overlap the Hub reported without refusing (a notice, for example to an agent whose hook runs
after the edit, or to an MCP claim) is recorded as `proceeded`: the call that drew it took the file.
Its hook edit went ahead, or it claimed the file over MCP.

`issues.contested_edits` reports `refusals` and `notices` by outcome. Only refusals feed the issue
totals: `pushed_through` is counted in `dismissed`, `handed_off` in `resolved`, and `pending`,
`asked` and `unknown` in `unresolved`. A notice is not a dismissal and feeds no total.
`went_elsewhere` is counted in neither `resolved` nor `unresolved`, because it is inferred from
what the Hub did not see: a session that ended, or whose later hooks failed open, looks the same
as one that moved on, and `coverage.gaps` says so. `dismissed` is `null` when no hook session was
seen in the window, because only a hook can report an edit. An outcome is what the Hub saw, not
why the agent did it, and an outcome that cannot be determined is reported as such, never as a
favourable class.

`overhead` reports, for the sessions active in the window and in total:
- `hub_calls`: the hook calls that reached the Hub.
- The characters of text in the reply as sent, split into session-start context (including the
  session ID line and required rules), refusal text, and other text (notices, advice, requests).
  Each distinct text is counted once, in UTF-16 units, as Claude Code limits them.
- `client`: what the hook client measured itself. That is calls, total and longest latency in
  milliseconds, calls that timed out, and calls that failed open.

A call that timed out never reached the Hub, so the client keeps its counts in small files per
agent session and sends them with the next call that does (`x-agentscm-hook-report`); no extra
request is made. A call takes waiting files by renaming them first, so two hooks of one session
running at once never send the same counts. Each report carries an id. When the call that carried
it is not answered, the Hub may still have received it (a relayed request outlives the hook), so
the client sends the same report again, with the same id, until a call is answered. The Hub counts
each id once per session.

Recording overhead never adds a store call to a hook. The Hub adds each call to an in-memory total
and writes the totals 30 seconds after the first call since the last write, in one background
store call. Only tokens the store has verified get a total of their own. A token not yet verified
waits in a small table until the next write checks it, so made-up tokens cannot crowd out real
sessions. `per_session` lists up to 50 sessions by agent and counts, without session or token
identities. A session counts as a whole when it was active in the window, including its calls
outside it.

Coverage stays explicit. Both are recorded per session, so a single-Work query reports
`availability: "not_scoped_to_work"`.
- `sessions_without_client_report` counts sessions whose client sent no measurement: an older
  client, a client whose calls all failed before a token was found, or one that never got a call
  through after a measured one. Their latency, timeouts and fail-open answers are unknown, and
  `coverage.gaps` says so.
- Rows are kept 30 days and at most 1,000 per token of each kind. The cap is per token because
  session IDs come from the client, and a cap per repository would let one token push out everyone
  else's rows.
- `partial` is true, with a gap, when the window reaches past the 30 days, a token's cap pruned
  rows inside the window, or more rows matched than were read. For overhead it is also true when
  this Hub process dropped calls of this repository inside the window because its memory tables
  were full, or holds calls of this repository from the window that are overdue for writing. Calls
  of the last minute are on their way to the store and do not make a read partial. A Hub restart
  loses totals not yet written, and forgets which report ids it counted, so a report resent
  across a restart can be counted twice.
- A reply of "coordination unknown" from the Hub itself arrives as an answer, so the client does
  not count it as failed open; `/v1/hub/health` counts those per process.

Nothing is converted into time, money or a percentage.

Reads default to seven days, accept a maximum 90-day window, and return bounded evidence.
MCP lists a default of 20 evidence rows and permits at most 50; repeat `since`, `until`, `work_id` when used, and
`next_after` as `after` to continue. An evidence cursor is bound to the assessment; changed
evidence can require a fresh first page. Inspect `coverage.gaps`, `coverage.omitted`, and
`next_after` before assuming completeness. Amounts use decimal strings for integer micros
and large token totals. Currency, accounting basis, provider and model stay separate; no
currency conversion or inferred savings is performed. Structured request usage is deduplicated
where a usable identity exists. Cumulative session totals are not added to request totals;
missing identity, contradictory reports and incomplete collection are reported as gaps.

## Duplicate starts

A lead `agentscm_start` that names a forge issue whose unfinished Work another person leads is
recorded when the Hub does not let it in: the claim is held (`claim_held`,
`stale_takeover_required`, or `generation_changed` for an old `takeover_generation`), or a
`preflight` that is not clear refused it (`work_preflight_required`). It is recorded once per
session and Work, with a `duplicate_start` activity event. The row keeps the asking token, session
and person, the holder's Work and person, and the time.

Nothing is recorded for a helper or reviewer join, a start by `work_id`, a takeover that went
through, a start on Work its lead offered with a handover checkpoint, or a start on Work you own
or lead: two agents of one person are not someone else. Files
another person's Work declared are counted as contested edits, not here. Reads (overview, search,
preflight and planned issues) record nothing, so an agent that read about the other Work and
stopped is not counted.

`issues.duplicate_starts` counts the window's rows by what the Hub saw the asking person do next:

| Outcome | What the Hub observed |
|---|---|
| `proceeded` | Within 24 hours of the start, the asking person joined or led the holder's Work, created Work of their own linked to the issue, or one of their agents drew a contested-edit refusal or notice on a file a hold of the holder's token kept. |
| `pending` | None of these yet, and the 24 hours are not over. It stays pending for the whole 24 hours, even if the holder finishes sooner. |
| `stood_down` | None of these within the 24 hours. |
| `unknown` | The holder's Work record is gone. |

`observed` is the sum of the four. The outcome is worked out when the metrics are read, not
stored. `stood_down` is inferred from what the Hub did not see: a session that ended, a hook that
failed open, or work done without Work or hooks looks the same. It is not counted as resolved.
None of these counts feed `resolved`, `unresolved` or `dismissed`, and none is turned into time
saved or a claim that a duplicate was avoided. `coverage.gaps` always says that reads are not
recorded, and says what stood down means when the window has one.

Rows are kept 30 days and at most 1,000 per token, and `partial` is set as for contested edits. A
single-Work query reports `availability: "not_scoped_to_work"`, because a row names the holder's
Work, not the asker's. A Hub older than this block leaves `issues.duplicate_starts` out: read that
as not recorded, not as zero.

## Contribution receipts in agent context

`agentscm_contribution_receipt` explicitly reads one Work's compact receipt. Receipt settings
are separate from optional completion-usage reporting. Existing sessions default to
`requested_only`; opt in through the Hub MCP, including its local stdio proxy:

```json
{"name":"agentscm_receipt_settings","arguments":{"session":"my-session","mode":"automatic"}}
```

Use the same explicit session as your Work calls, or omit `session` everywhere to use the
current authenticated agent session. `requested_only` permits explicit retrieval and suppresses
automatic announcements; `disabled` also suppresses explicit reads. These choices apply to this
token and session. Preferences expire after 30 days and are deleted with the token. At most
32 session preferences can be retained per token; creating another at capacity is refused,
while an existing preference can be changed. Session names are validated at runtime and
cannot exceed 255 bytes. These choices do not turn on transcript collection or change a
workflow gate.

In automatic mode, supported substantive paths append a receipt after a reported `finish`
checkpoint, source-bound `revision_tests` or `hosted_ci` skill evidence, and source-bound
`human_approval` skill evidence (the pre-merge phase). Skill evidence must match the current
session's Work, declared branch and checkpoint commit. These reports remain self-reported.
An ordinary stop or a plain `agentscm_done` release does not announce successful completion;
`agentscm_done` with `outcome: complete` does. Native
hooks, the separate native task MCP and other clients do not acquire receipt support merely
because completion reporting is enabled.

A later `agentscm_work_catchup` page can append a distinct provider-observed merge update
for a previously announced Work in that session. The association requires the exact declared
checkpoint commit matching the provider's observed head. A branch name or caller-authored
“merged” report is insufficient. The same receipt identity advances to a new evidence version;
merge never implies acceptance, deployment, production health, or passing pending checks.
Explicit retrieval remains repeatable. Automatic announcements are deduplicated by token,
session, Work generation, receipt phase and assessment version. A reported finish can produce
a completion receipt even when the preceding pre-merge receipt had the same metric version. Delivery is best effort: an announcement
recorded before a lost response can require explicit retrieval.

The preview has at most six rows and three evidence references, under 2 KiB text and 8 KiB
structured data. Full evidence remains available through bounded `agentscm_value_metrics`
reads. The receipt window starts at Work creation or 90 days before the read, whichever is
later, and identifies that window. Automatic merge tracking retains at most 32 Works per
session and 128 per token, with 30-day expiry; dedupe retains at most 64 phase/version announcement identities per Work
generation with the same expiry and deletion on token/Work removal. Older or evicted Work
can still be explicitly read where authorized. Capture or storage errors leave the primary
operation unchanged and report unknown receipt coverage. No receipt posts a PR comment,
sends an external message, polls a provider, or adds a merge gate.

## Optional completion reporting

Automatic completion observations are **off by default**, independently for each saved checkout
connection. From a connected checkout:

```sh
agentscm client reporting status
agentscm client reporting on
agentscm client reporting off
```

Pass `--root /path/to/clone` to select another checkout. After changing this setting, a running
local MCP proxy stops on its next request without forwarding it; restart the proxy before
continuing. Native hooks read the setting on each invocation.

When enabled, the managed native session-end hook and the Hub MCP `agentscm_done` call attempt
to record one observation for the caller's current work. A reported `finish` checkpoint through
the opted-in Hub HTTP or MCP transport also records it. Retries and repeated completion signals
for the same token, session, work item and work generation share one observation. This is not
one report per tool call. A new work generation can produce another observation.

The observation contains the registered agent label, `other` phase, `unknown` outcome,
`unknown` provider/model, and no token counts, cost or source commit. A stopped session does not
prove success. No prompt, response or transcript is read to fill missing values. Explicit usage
reports remain available when the toggle is off and may coexist with a completion observation;
the observation count is not an attempt count.

A completion-only record changes usage coverage from `unknown` to `partial`, adds one missing-cost
observation and an unknown-provider/model token group. It supplies no measured tokens or cost.
The current summary does not separately count completion-only records, so do not use observation
or coverage counts to compare measured usage between clients with different reporting settings.

Reporting adds no agent-facing messages. Errors do not block releasing or finishing work and
are logged by the Hub without report contents. It is best effort: crashes, missing completion
calls, no matching work, expired credentials or storage limits can leave gaps. It does not
automatically scan or backfill old sessions. An opted-in retry of an earlier finish can create
the missing observation while that same generation is still finished. Switching reporting off
does not delete retained observations.

All Hub MCP clients, including Grok and custom orchestrators, can use this path by calling
`agentscm_done` or submitting a finish checkpoint. Direct HTTP/MCP callers opt in with exactly
one `x-agentscm-completion-reporting: true` request header; absent or other values are off.
The local proxy supplies it from the checkout setting. The header applies to completion calls
on `/mcp`, `/v1/hub/hook/{agent}` and `/v1/hub/standup/checkpoint`. It is a caller preference,
not a repository-wide enforcement policy. The separate native workspace task MCP is unaffected.

## Reporting usage

An agent can send the same strict usage record through the Hub's `agentscm_work_usage` MCP tool
(including through the local client proxy) or the universal
`agentscm work usage --request FILE|-` CLI command. The separate native workspace task MCP does not
expose Hub work tools. This is the provider-neutral path for Claude Code, Codex CLI, Gemini CLI,
Cursor, Copilot, any other Hub MCP client, and Grok when it is configured as a Hub MCP client.
AgentSCM does not claim automatic session-usage capture for those clients.

```json
{
  "work_id": "wrk_example",
  "observation_id": "attempt-3",
  "generation": 1,
  "session": "agent-session",
  "phase": "implementation",
  "outcome": "succeeded",
  "agent": "codex",
  "provider": "unknown",
  "model": "unknown",
  "usage_basis": "reported",
  "source_commit": null,
  "input_tokens": 1200,
  "output_tokens": 300,
  "measurement_scope": "request",
  "cost": null
}
```

Use the literal label `unknown` when the client cannot determine its agent, provider or model identity. Do not infer a provider from the agent name: an agent client and its model provider are separate fields. Token counts may be omitted independently. Missing tokens or cost mean unknown, not zero.

Provider-backed clients may also include cache-read and cache-creation input counts, action or
attempt identities, and native provider request or event identities. These are optional facts and
are omitted from legacy canonical records when absent. `measurement_scope` is either `request` or
`session_cumulative`. Insight token and cost totals include request-scoped and legacy records but
exclude cumulative session records, which are retained only as separate evidence. This prevents a
provider's final total from being added to the request records that contributed to it.

The Hub keeps the four token counts apart as they were reported. It does not add cached input to
`input_tokens` or take it out, and it does not know whether a reporter's `input_tokens` already
contains its cached input: that is the reporter's convention. For a client that caches, most input
can be cached input, so read the cache sums beside `input_tokens` before judging how much was read.

`usage_basis` is `reported` for a client-supplied record. `provider_result` means the caller says
the counts came from a structured provider result; the Hub does not verify that claim. A cost, when
present, has a three-letter currency, an integer `amount_micros`, a caller-supplied basis label, and
one of these kinds:

- `reported_charge`: a charge reported by the caller. It is not a verified bill.
- `list_price_estimate`: an estimate under the named rate or price basis.

The Hub does not calculate cost from token counts. It groups costs only when kind, currency, basis, usage basis, agent, provider and model all match.

Each numeric token or cost field is between zero and 1,000,000,000,000 inclusive. Each work item
retains at most 500 immutable usage observations, also subject to the bounded work-payload capacity for the full repository scope
(provider, host, repository name and stable repository ID). Reaching either limit rejects a new observation; existing observations are
not overwritten. `observation_id` makes an exact retry by the same author idempotent. Reusing it
with different content or from another author is rejected. For unfinished work, the author must be
a current participant in the named session and generation. After a reported finish, only the
current lead can report against that same generation. Records are scoped to the authenticated
repository. Stored observations retain the session label and author token ID reference for authorization and retry handling, not the token secret. Insight responses and the [stored report read](#reading-stored-usage-reports) omit these identifiers and do not contain prompts, responses or
transcript contents.

## Reading insights

`GET /v1/hub/standup/insights` accepts `since`, `until`, `limit`, `after` and `through`. The
default cohort is work created in the previous seven days, the maximum window is 90 days, and
`limit` cannot exceed 20. A continuation must repeat the exact `since`, `until` and `through`
returned with the first page and pass its opaque `next_after` as `after`. Those values fix cohort
membership and ordering. They do not freeze each item's metrics: every page projects retained state
at that response's `as_of`. Counts and sums displayed for a page are page-only unless the response
explicitly says otherwise.

`GET /v1/hub/standup/work/{work_id}/insights` returns the bounded detail for one work item. The equivalent read tools are `agentscm_work_insights` for a page and `agentscm_work_insight` for one item. To read the reports behind a work item's usage summary, see [reading stored usage reports](#reading-stored-usage-reports).

Duration fields can be `null`. If history is missing, truncated or crosses a retention gap, all timing and coordination-signal fields are unknown. Otherwise, blocked, paused, handover and finished duration are wall time in the reported state. Active duration is counted only through the stored last check-in plus the 15-minute active window; later time is unknown. These values are derived from retained reports and do not measure actual keyboard, tool or model execution time. Failed attempts are shown only when present in retained usage reports.

Both endpoints are read-only. They return projection evidence and never approve, qualify, accept, publish, merge or land work.

The list projection returns at most 20 token groups and 20 cost groups per work item; one-item
detail returns at most 500 of each. Totals and known-report counts still cover all retained reports.
`groups_complete: false` means only the grouped breakdown was truncated.

A group's `agent` is the label the report gave, with one exception: a generated label that names a
host is shown without the host name, as in [other owners' agents](standup.md#other-owners-agents),
because every reader of the repository sees these groups. Groups that differ only by that host name
are added together.

The usage summary and each token group carry four sums: `input_tokens`, `output_tokens`,
`cache_read_input_tokens` and `cache_creation_input_tokens`. Each has a count of the reports that
gave it: `input_known_observations`, `output_known_observations`,
`cache_read_input_known_observations` and `cache_creation_input_known_observations`. The same rules
hold for all four. A sum is `null` when no counted report gave that count, and is never zero for
that reason; a reported zero is known and is counted. Cumulative session reports are in no sum and
no count. Each sum covers at most 500 reports of at most 1,000,000,000,000 each, so it stays an
exact JSON integer. A Hub older than the cache sums leaves the four cache fields out: read that as
not reported by the Hub, not as zero.

## Reading stored usage reports

`GET /v1/hub/standup/work/{work_id}/usage` returns the usage reports stored for one work item,
oldest first, for the same readers as the insight read: any token of the repository, read-only
included. The read tool is `agentscm_work_usage_reports`; it is listed with the full tool set
(`/mcp?tools=all`) and to read-only tokens, and is callable either way. The read changes nothing.

`limit` is 1 to 100 and defaults to 100. `after` is the number of reports to skip, 0 to 500. The
reply is `{version, as_of, work_id, items, next_after}`. Pass `next_after` as `after` to read the
next page; it is `null` on the last page. Reports are immutable and a work item keeps at most 500,
so five pages read everything. A report stored while you page can appear on two pages if it sorts
before your position; none is skipped. Tell reports apart by `id`.

Each item carries what the report measured and the labels it gave: `received_at`, `generation`,
`phase`, `outcome`, `agent`, `provider`, `model`, `usage_basis`, `source_commit`, the four token
counts, `measurement_scope` and `cost`. An unknown count or cost is `null`, never zero. Cumulative
session reports are listed with `measurement_scope: "session_cumulative"`; they are the reports the
insight totals leave out, so do not add them to the others.

The read leaves out the session, the author token reference and every identifier the reporter
supplied: the observation ID, action and attempt IDs, and the provider request and event IDs.
`id` is a digest of the work item and the observation ID, the same one that
[value-metrics](#recorded-contribution-projection) evidence uses for a report, so it is stable
across reads without revealing the observation ID. A generated agent label that names a host is
shown without the host name, as in [other owners' agents](standup.md#other-owners-agents).

`publications_complete` describes lookup coverage of retained records, not a complete check
history. It is false when the stable repository identity is unavailable, evidence limits omit
records, or a publication has a pull request whose checks cannot be looked up without a forge
connection. A disconnected repository can still show its stored publications.
`commits_truncated` means there are more than 16 distinct valid commit claims or more than 256 retained versions. See `history_retention_gap` for compacted history.
Only stored checks matching the publication’s exact commit are returned; an empty list does not
mean that commit was never checked.

## Recorded contribution projection

The All repositories page, `agentscm_value_metrics` MCP tool and contribution receipts use
`agentscm.observed.v1`. Repository tokens read only their repository. The signed-in web route
`GET /v1/hub/session/value-metrics` checks current repository roles before aggregating; the
repository route is `GET /v1/hub/standup/value-metrics`. Both accept `since`, `until`, `work_id`,
`limit` (1–50, default 20) and an assessment-bound `after` cursor. The web route also accepts
`repository`. Windows are at most 90 days, default seven days. Repeat the window and scope
when paging; changed evidence can require restarting pagination.

A browser `work_id` filter is resolved only among repositories the signed-in person can read
now. Other authorized repositories are skipped only when they lack that Work; unknown and
inaccessible Work IDs both return the same not-found response. Store and authorization failures
still propagate. A repository token continues to reject a Work outside its repository scope.

Context counts identify retained catch-up responses prepared by the Hub and explicit delivery
acknowledgements. They do not establish transport delivery, application or ordering before edits.
Responses are scoped to the requesting token, or to the signed-in person's own structured
catch-up deliveries. Session and token identities are not returned. Replayed source events count
once per subscriber/token, and acknowledgement is combined across retained deliveries.

The supported `pr-gate` evidence path records review-independence refusals. A subsequent admitted
independent-review record resolves the same source/skill/configuration episode. If the reviewer
has no participating Work session, applicability retains the original refusal's Work and generation
together; it does not combine the original Work with an absent reviewer generation. These are
admission observations, not bugs prevented or independent verification. Configured skill gates
retain pending/current observations with source, bundle digest and parameter identity; changed
source or current configuration makes retained evidence stale. The projection checks the linked
Work's current generation and checkpoint commits on every read, including checkpoint-only source
changes with no new skill evidence. A missing current commit makes applicability unknown. A Work association requires the
producer's current participating session and an explicit matching checkpoint commit. A branch
name alone does not establish an association. Current actionable states remain visible even if
first recorded before the contribution window.

Recorded resources preserve provider/model/category and charge versus reference-price bases.
Token and money totals are decimal strings. Session-cumulative reports are excluded from request
totals; cache subsets remain separate. Equal provider/request identities are conservatively
collapsed across Works and owners. Compatible partial reports enrich missing token/cache/cost
components; contradictory known components remain unknown. Charges and reference-price bases
remain separate for the same request. Existing collectors do not supply an account/endpoint
namespace, so that uncertainty remains visible. Work allocation additionally uses a separate
all-retained-history provider/request-to-Work membership proof, including reports before the
selected window. It looks up only stable request identities in the authorized target Work's own
bounded period usage (at most 500 reports), using the same repository scope on the inner target
selection and outer history. An expression index supports provider/request lookup. At most
2,001 matching history rows are read; unrelated requests do not exhaust that proof. Target
identities are deduplicated before lookup, so compatible duplicate reports cannot multiply
membership. Matching-history reads stop once the incompleteness threshold is reached; no
particular overflow subset is used to allocate resources. If relevant
matching history exceeds the cap, that Work's request resources stay unallocated; uniqueness is
never inferred from a capped sample. Only explicit request-scoped reports with stable
request identities enter additive totals; unknown-scope, cumulative and unidentified reports remain
unallocated observations linked in the evidence preview. Shared requests are
unallocated and excluded from individual Work resource totals. `unallocated_usage_records`
counts retained report rows excluded from the selected scope's additive totals. For a shared
request, an individual Work counts only its own excluded reports; the repository projection
counts the request once and does not classify those included reports as unallocated. Missing usage, costs and
AgentSCM's token overhead remain unknown. Work observation coverage does not measure token-volume or
all-provider coverage, and these measurements establish no causal savings.

A requested Work is loaded explicitly, independently of the aggregate Work cap. Aggregate Work
metadata reads the eligible cohort plus Work IDs referenced by the bounded observation, usage
and context-delivery reads, and Works whose checkpoint commits match retained provider-merge
heads. The scoped union is limited to Works created by `until`, newest first
with a 2,000-Work cap. Unrelated old finished Works do not consume that cap; referenced aged
finished Works remain available for source applicability. Period usage, context deliveries and
merge observations are read newest first within the selected window. Satisfied pre-window
follow-ups are omitted before a missing-metadata rewrite can turn them into Unknown. Real old
pending, unresolved and stale exceptions remain inspectable; missing metadata for those retained
bindings still makes coverage incomplete. Aggregate merge evidence receives a Work attribution
only when the candidate facts are complete and exactly one Work declares the matching source.

`observed_works` counts only in-window usage, supported-skill admission/refusal observations
and context responses. The eligible-ID
cohort contains metadata-backed Works created by `until` that are currently unfinished, updated
in the window, have period usage, or were actually observed through in-window usage, admission/refusal observations
or context. Thus a fresh context return can bring an aged finished Work into both period counts.
Old actionable exceptions do not inflate these period Work counts. Explicit metadata does not
make a Work eligible before its creation. Missing/capped metadata makes population unknown;
these are retained Work counts, not all-history or provider-request denominators.

Source scans and detail previews are bounded. Coverage reports scan omissions, usage-cap
rejections, retention gaps and unavailable observations. Catch-up delivery records can expire
24 hours after their last update; longer windows explicitly flag potentially missing context
returns/acknowledgements, even when the remaining source histories are complete. Existing
count-based pruning also records a durable, bounded per-repository timestamp range in the same
transaction as the actual deletion. Windows overlapping that range disclose that context records
were pruned, including pruning within 24 hours.
Observation capture-failure diagnostics are scoped to the stable repository identity, so another
authorized token and the signed-in owner's view see the same failure warning. If the failing token's
scope cannot be resolved, the process reports conservative unknown capture coverage for every
projection. These diagnostics are process-local and cannot establish capture completeness across
Hub restarts; usage-cap rejections and delivery-pruning ranges are retained durably. The projection cache is keyed by
already-authorized source facts and evidence watermark; authentication and role checks still
run on a hit. Supported-skill evidence remains self-reported, and artifact existence has not been
independently verified by this projection. Supported producers do not capture issue dismissals,
false-trigger outcomes, failed follow-up outcomes or cancelled follow-up outcomes. Those four
counters are `null` and displayed as Not captured, rather than observed zero. Native workflow and other adapter outcomes are not
substituted for those supported-skill observations.

Contribution receipt announcement records are token/session/Work/generation scoped. The Hub
retains at most 32 remembered Works per session, 128 per token and 64 announcement versions per
Work generation; unavailable capacity is reported. Records expire after 30 days and follow Work
and credential deletion. A provider-observed merge is a distinct observation associated through
an explicitly recorded source commit; it does not establish acceptance, deployment or health.

## Digest: stale Work and repeated collisions

The digest answers two questions for one person over a period of days: what did my agents leave
unfinished, and where do agents in this repository keep colliding. It is a read-only projection
of records the Hub already keeps. Reading it writes nothing.

`GET /v1/hub/standup/digest` accepts `days` (1 to 31, default 7) and answers for the token's
person and repository. Any token of the repository can read it, read-only included.
`agentscm digest show [--days N] [--json]` prints the same as text or as the JSON reply. The
reply has `schema_version: 1`; clients ignore fields they do not know.

**Your stale Work** (`stale`) lists the unfinished Work the reader owns whose lead agent last
checked in three days ago or earlier, longest quiet first. For Work that was paused because its
session ended, the time is the last check-in before that pause. Each item has its id, title,
branch, state, `quiet_since`, and:

- `merged`: `observed` when a pull request from its branch merged after its last check-in, `none`
  when the Hub read the forge's pull requests back to before the Work was created and saw no
  merge, and `unknown` otherwise (no branch, blocked Work, or the forge was not read that far).
  `unknown` is never shown as "not merged".
- `can_release` and `can_complete`: whether the owner, acting as a person, may make that
  transition now, with the `generation` and `updated_seq` such a transition is fenced by.

At most 10 items are listed; `total` counts all that were found, and `complete: false` says the
person has more unfinished Work than one digest reads (200 items). Other people's Work is never
in this list, so the digest shows no Work title the reader does not own.

**Collisions** (`collisions`) cover the whole repository. A collision is one refused edit
(`edit_denied`) or one repeated edit (`edit_repeated`). The editing side is the agent that made
the edit. The holding side of a refusal is whoever announced the intents the refusal named; a
repeat has the holders of the same session's latest earlier refusal over one of its paths. When
neither names anyone, the hand-over request the refusal filed does.

- `hot_paths`: paths grouped by their first two segments (`src/hub/slack.rs` counts under
  `src/hub`), with how many edits were refused and how many repeated, most first, at most 10.
  `typical_gap_seconds` is the median time between the holder taking the path and the other
  agent's edit, over the collisions where the Hub knows when the holder took it (`gap_samples`
  of them); it is absent when it knows none. A short gap means two agents started on the same
  files at about the same time; a long one means an old hold was still in the way.
- `pairs`: two people whose agents collided on one exact path three or more times, with the path
  and the count, most first, at most 10. `includes_you` marks the pairs the reader is in. One
  person's own two agents colliding is counted in `hot_paths` and is not a pair.
- `unattributed`: collisions whose holding side could not be named, which no pair counts.

Paths and logins in collisions are what every reader of the repository already sees on the
activity page.

**Missing history is unknown, not zero.** `collisions.complete` is true only when the counts
cover the whole period. Otherwise `known_from` is the time the counts start at and `limits` says
why: `event_history_starts_in_period` (the Hub's oldest retained event is inside the period, as
on a new or restored Hub), `collision_history_truncated` (the period holds more than the 10,000
activity events one digest reads), `event_history_empty`, `collision_event_unreadable` or
`collision_event_paths_truncated` (an event named more than 32 paths). The text form says the
same in a sentence.

Titles, branch names, paths and logins are other people's text: each is cut to a bounded length
and stripped of control and invisible characters before it enters the reply.

### Weekly delivery

The Hub can send each person their digest once a week as a private Slack message; see
[the Slack adapter](slack.md#weekly-digest). Delivery has two switches, both stored by the Hub
and keyed by the forge's repository id:

- The repository's switch is off until a repository admin (GitHub `admin`, GitLab maintainer or
  owner) or a Hub admin turns it on: `agentscm digest enable` or `disable`, which call
  `PUT /v1/hub/standup/digest/settings` with `{enabled}` using a token that is not read-only.
  The Hub checks the forge role when the call is made. On a Hub with no forge connection for
  the repository only a Hub admin can change it.
- Each person can switch their own off and on again: `agentscm digest opt-out` or `opt-in`,
  which call `PUT /v1/hub/standup/digest/settings/me` with `{opted_out}`.

`agentscm digest settings` and `GET /v1/hub/standup/digest/settings` answer
`{repository, enabled, opted_out, delivering}` for the token's person, where `delivering` is
`enabled` and not `opted_out`. A token minted without a forge repository id is never sent a
digest. Reading the digest needs neither switch.
