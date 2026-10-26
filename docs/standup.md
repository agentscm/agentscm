# Asynchronous agent stand-up

> **Alpha: not for production.** The API and stored schema may change without migration. Stand-up records are coordination data supplied by agents; they are not proof that code ran, passed review or landed.

The Hub keeps durable engineering work separate from short-lived sessions and file intents. Before starting a task, an agent can retrieve:

- the current unfinished work for the token's repository, including old, stale and unknown claims;
- an ordered, bounded change history since this agent token and subscriber/session last acknowledged a briefing; and
- blockers, decisions, next actions, participants, dependencies and related work reported at checkpoints.

This gives agents working at different times a shared starting point without treating a new chat or session as a new engineering goal. It also works for repositories that stay on plain Git. Native source workflows remain a separate system with their own revisions, execution evidence, approvals and landing checks.

## Work identity and claims

Every work item has a stable `wrk_…` identifier. New work can use a caller-supplied request key for durable retries, or link to an issue in the token's configured GitHub or GitLab repository. The forge remains authoritative for issue content and state; AgentSCM owns the claim and checkpoint history. It does not mirror issue descriptions or publish checkpoint text to the forge.

A work item has one lead by default. Other agents can join as helpers or reviewers. A claim records the human owner, agent, session, declared repository paths, branch, worktree label and next action. Dependencies and related work use explicit `wrk_…` identifiers; AgentSCM does not merge similar free-text tasks automatically.

The claim is coordination state. It does not create a Git checkout, lock a file, stop an old agent process or replace source-control conflict checks. A disconnected or expired lead becomes `stale`; a projection failure becomes `unknown`. Both remain claimed. Takeover is an explicit generation-checked operation, so agents must not interpret either status as available work.

To resume a paused task as the same agent token, start the explicit work ID with the observed `takeover_generation`. The new generation binds the session and fences the previous session. An ordinary retry does not silently change paused work back to active. Existing blockers remain reported until explicitly resolved. After a lead changes, helpers from an older generation must explicitly rejoin before their managed edit checks pass. A finish releases the linked live file intent in the same transaction; a linked finished or fenced session cannot regain paths by falling back to legacy intent advice.

## Checkpoints and handover

The current lead reports concise checkpoints for progress, decisions, blockers, pauses, completion and handover. Helpers and reviewers remain visible participants but cannot mutate the work item. An active blocker remains visible through later progress, decisions and pauses until the lead sends an explicit blocker resolution. A checkpoint can update the declared scope, branch and worktree label, and include a next action plus bounded references to:

- a repository-relative path and optional line;
- a 40- or 64-character commit identifier and optional repository-relative path; or
- another AgentSCM work identifier;
- a declared captured-event identity (`captured_event` with `session_id` and `event_id`); or
- an exact earlier version of the same Work (`prior_work_version` with `work_id` and `event_seq`).

Captured-event references are caller-declared metadata. The Hub does not fetch, retain or verify the
referenced conversation body, and the reference does not establish human authorship or approval.
Prior-Work-version references let a checkpoint explicitly correct or supersede an exact retained
version of the same Work. The Hub checks that identity when recording the checkpoint, but later
compaction can make the historical snapshot unavailable; the reference does not promise retention.
The Work title, scope and existing checkpoint history continue to carry the intent semantics.

References do not accept arbitrary URLs. Forge links shown by the Hub are constructed only for supported canonical GitHub or GitLab hosts. Checkpoint summaries and next actions are self-reports. A `finish` checkpoint records what the agent reported; it does not establish execution, review, acceptance, merge or native landing.

Use a fresh operation identifier for a new checkpoint. Retry the same operation identifier only with the exact same fields after an uncertain transport result. The response separates the committed operation receipt from the current work item. An exact retry can confirm an earlier checkpoint after takeover, while the returned item shows the current lead and generation. This receipt grants no ownership; new checkpoints from the old generation are rejected.

Every new checkpoint includes `expected_updated_seq` from the work item the caller inspected. If another checkpoint changed that item in the meantime, the new operation is rejected instead of overwriting newer progress, blockers or completion. Automatic session-end pauses use this same guard. Retained exact retries are resolved before this version check.

## Briefings and acknowledgement

The first briefing page fixes an upper event watermark. Continue with the exact returned event cursor, work cursor, work-complete flag and watermark. This keyset traversal keeps concurrent appends out of the current briefing and prevents an exhausted current-work snapshot from restarting while event pages remain. The change interval is exclusive of `from` and inclusive of `through`.

The response contains two independent sections:

- `items`: a page from the current unfinished-work snapshot. Work created before the acknowledgement boundary still appears here.
- `events`: ordered changes after the previous acknowledged cursor and no later than the fixed watermark.

`retention_gap: true` means some requested history is no longer retained. Do not turn that gap into an empty or complete history claim. A writable agent briefing has a delivery identifier. Acknowledge it only after every page was delivered successfully; acknowledgement advances only that token and subscriber/session's boundary for the repository. Repeat `subscriber` on each continuation; the delivery records which subscriber an acknowledgement advances. Separate sessions cannot consume each other's recap when they use distinct subscriber identities. Omitting `subscriber` on the HTTP API keeps a token-level compatibility cursor. MCP and supported startup hooks resolve the current session automatically. A read-only token receives authorized inspection data without a delivery identifier and cannot acknowledge or mutate work.

The Work dashboard separates **Working now**, **Recently finished** and **Needs coordination**. Working now preserves the progress lanes and identifies the lead, participants and next action. Recently finished uses a server-reported time window, independent of acknowledgement, and includes actual reported completion and decision checkpoints. Needs coordination shows durable directed requests, per-lead receipt and reply state, and recorded resolutions. Detected scope overlaps are listed separately: detection alone is not delivery or resolution.

The dashboard reads current Work without changing assignments or acknowledging coordination requests or agent briefings. Its separate personal catch-up panel advances only the signed-in person’s review position after an explicit action. Each bounded section states its completeness; request pages retain their fixed upper watermark, and Change history retains the briefing cursor tuple. Stale and unknown claims remain assigned. Finished work carries no assignment claim and does not imply tests, review or merge.

### Since you last checked

Hub Work and the inline MCP Work app show a personal return-to-work delivery above
current assignments. It combines retained Work/checkpoint and coordination events
with immutable merge observations. Human review state belongs to the authenticated
person and repository, so it survives token rotation and is shared across that
person’s devices. A verified repository ID keeps that history and readership position
through a repository rename; unbound repositories retain name-based isolation.
Agent subscribers retain separate token/session positions.
Opening a page, login, polling, dismissal and startup never mark records reviewed.
The personal position is an authenticated owner’s readership assertion. An owner’s
bearer credentials, including read-only tokens, can advance it; the protocol does
not attest human presence, understanding or approval. Agents use their separate
namespaced subscribers and must not select or acknowledge `human` without the
owner’s explicit review action. Acknowledged delivery receipts retain the acting
token and delivery client within the bounded receipt lifetime; they do not
establish human presence.

`GET /v1/hub/standup/catchup` freezes a delivery with separate Work-event and
merge-observation high-waters. Continue with the exact delivery and both returned
cursor pairs; pages never chase new events beyond that fixed window. Load all
pages before explicitly posting the delivery and expected starting cursors to
`POST /v1/hub/standup/catchup/ack`. A concurrent acknowledgement cannot move either
position backwards. Retrying the same completed acknowledgement is idempotent;
a stale position reports a conflict and the client reloads current state.
Read-only tokens can maintain their own readership state and trigger bounded,
idempotent projection of trusted stored publications into the shared merge feed.
They cannot supply merge observations or mutate Work, publications or the forge. Agent Work briefing acknowledgements remain separate.

The CLI exposes `work catchup` (optional `--request` query JSON) and
`work catchup-ack --request` (exact acknowledgement JSON). MCP exposes
`agentscm_work_catchup` and `agentscm_work_catchup_ack`. The HTTP/CLI default subscriber is `human`; MCP and supported agent startup paths
select namespaced independent agent subscribers. Browser/MCP App instances supply
an opaque `delivery_client` so two devices or tabs page independently while sharing
the person’s review position. With browser Web Locks, Hub tabs retain a bounded set
of client slots across reloads and acquire exclusive ownership before using a stored
client; a copied busy client is replaced. Browsers without that facility use a
volatile client. MCP App instances also keep independent volatile clients. Reuse
one client identity throughout a delivery. Idle
deliveries expire after 24 hours without moving the review position; an expired
continuation must reload. Recent acknowledgement receipts are retained for retry.
Delivery storage is bounded per person and repository: at most 128 active windows
and 128 MiB of retained page payload. One agent token is limited to 16 windows
and 32 MiB; agents together are limited to 96 windows and 96 MiB, preserving
capacity for human review. Human windows are capped at 32. Byte limits apply to
continuation pages as well as new windows; capacity errors leave review positions
unchanged.

Merge observations retain an exact repository, PR/MR number, head/merge commits,
typed original source, changed paths and ingestion sequence. The sequence records
when AgentSCM first retained the observation; an optional provider occurrence time
is displayed separately. Publication observations and ordinary forge observations
deduplicate on repository, proposal number and merge commit. Display titles are bounded and
strip control and invisible characters consistently with the clients; exact source
identities are never rewritten. Unsupported identity characters leave source
coverage unavailable rather than inserting an unreadable delivery. A provider record is
read through the configured authenticated GitHub/GitLab transport, never accepted
from an unverified webhook payload. The sync also reads a bounded recent closed
proposal page and re-reads new merge candidates. Source status records unread
pages, deferred observations and failures. Source-quality metadata refreshes on
each page without changing the delivery’s event watermarks. This is observed coverage, not a claim
of complete forge history. No model call generates the recap.

Retention gaps, unavailable sources and partial pages remain visible. An empty
retained delivery does not establish that nothing happened outside that coverage.
Current Work and existing coordination warnings stay visible alongside it. No
catch-up acknowledgement closes Work, changes a claim or authorizes an edit.

### Agent-generated catch-up narrative

The Work dashboard can show an optional agent-generated narrative above the retained
catch-up feed. Reading it requests or reuses an ephemeral synthesis for the same
bounded human subscriber window. Each idle cached variant is retained for up to
30 minutes. A visible ready view polls every 30 seconds and keeps reusing the same
variant while its source facts remain unchanged. Variant capacity is bounded,
reserves room for the general human briefing and protects active generation leases.
Reading, refreshing or generating the narrative never acknowledges the catch-up
delivery, moves an agent briefing cursor, changes Work or authorizes an edit.

The Hub does not launch a model provider. An eligible connected agent claims a
pending window with `agentscm_briefing_prepare`, retrieves the bounded source facts,
generates the structured narrative and submits it through
`agentscm_briefing_submit`.
The dashboard can request task-specific implications through its optional task
field; it sends the task only when the person submits the form. Connected agents
discover and prepare those pending requests through the same bounded workflow.
Concurrent snapshot requests for the same owner and repository fail fast while
one capture is running; other owners use separate capture gates. Temporary
unavailability is retried while the dashboard is visible. A claim lasts five minutes. Two expired attempts pause the job for ten minutes,
then make it eligible again; polling does not extend that cooldown. Failed views
keep checking for recovery. New source facts supersede the old job, including any
active lease, so the agent must prepare again rather than submit stale prose.
Frequent coverage changes can therefore cause repeated supersession. Agents retry
a temporarily busy prepare or unchanged submission at most twice; a busy response
does not consume their existing lease.
Agent-facing results frame the narrative as untrusted JSON data.
The ephemeral cache has 64 slots across the Hub. If every slot holds a general
briefing or active lease, another request can be temporarily unavailable even in
a different owner namespace. This is a bounded availability limit, not shared
access to another owner’s facts.
There is no unattended provider worker; generation happens only when an eligible
connected agent follows that workflow.
Startup and MCP instructions tell eligible agents how to prepare, generate and
submit that result. A successful finish checkpoint also offers a fresh general
briefing while its agent is still present, even if nobody has opened the dashboard.
Progress checkpoints deliver existing requests without starting repeated synthesis. If no eligible agent is connected, the narrative remains
pending and the retained catch-up feed stays available. Generation failures,
incomplete source windows and warnings remain visible and cannot be read as an
empty result, authorization or a conflict all-clear.

### Source and scope preflight

`POST /v1/hub/standup/preflight`, CLI `work preflight --request`, and MCP
`agentscm_work_preflight` inspect a proposed Work start without claiming it.
The request uses the same exact Work resolver and normalized scope as start, plus
an optional immutable source revision, fixed merge window and deliberately checked
merge identities. Admission repeats the relevant check in the same transaction as
the Work claim, so a changed merge watermark cannot silently use an old check.
Titles and similar branch names never prove that two tasks are equivalent. An exact
retained source head is checked independently of the caller’s chosen merge window;
an empty window cannot hide that duplicate signal.

The result distinguishes `review_required`, `unknown` and
`clear_within_checked_window`. The last status is limited to the declared comparisons
and caller-reported source relationship; it is not authenticated Git ancestry. It reports existing subject Work, scope overlaps and
exact merge identities; it is not a general conflict-free or semantic-duplication
guarantee. Git ancestry is a caller-reported local source fact: report `source_relation_checked` only
after inspecting it, and `in_source` for each exact checked merge. For Git,
`git merge-base --is-ancestor MERGE_COMMIT SOURCE_REVISION` distinguishes ancestry
from a definite non-ancestor; missing objects or command errors are unknown.
A different SHA alone is not proof of staleness. Missing objects, missing scope, unavailable
coverage or an unverified source relationship remain unknown. Current claim,
generation and deliberate handover rules continue to apply. An explicit stale or
incomplete preflight refuses a new lead claim with `work_preflight_required` and
its typed evidence. A new-subject start with an active scope overlap instead records the existing
durable coordination request and returns the nonclear preflight without acquiring
a competing claim. Repeating that start reuses the request; inspecting preflight
alone does not notify or mutate other Work. An explicit retry/resume of already-owned Work still passes
through the existing identity and generation checks. Applicable Work retained under
a former repository name requires explicit review rather than a false clear result.
Legacy starts without
preflight evidence retain compatibility admission and return unknown readiness. Native startup hooks
and MCP start show a bounded catch-up preview without creating a delivery or
acknowledgement. A preview cannot be continued or acknowledged: call the explicit
catch-up tool to begin a fixed-window delivery, read every page and deliberately
acknowledge that agent subscriber. Opening new agent sessions therefore does not
consume delivery quotas or pin retained history. MCP is advisory and cannot
intercept arbitrary edits outside a supported hook.

## Before work and after an overlap

1. Read `agentscm_work_overview` (or `agentscm work overview`) before planning. It shows unfinished assignments, declared overlaps, and reported outcomes in the last 24 hours by default. `recent_since` selects another starting time. A compacted history is explicitly incomplete.
2. Read the personal/agent catch-up delivery, session briefing and coordination inbox. Retrieve every page before relying on coverage. Reconnect with the same subscriber/session; acknowledge only deliberately received data.
3. Inspect the proposed source and scope with preflight, then start with a stable work identity, explicit session, intended paths and the same checked merge watermark. HTTP/CLI and MCP starts return a pre-claim overview in `context`. The database serializes scope admission with the claim, so simultaneous different work identities cannot both claim the same declared scope unnoticed. Empty scope is unknown coverage; declare scope before editing.
4. A conflicting lead start creates an idempotent durable coordination request, addressed to the affected current leads, without creating a competing assignment or announcing paths. A conflicting scope checkpoint also records a request while leaving the work unchanged. Exact files and `directory/**` scopes use component-aware overlap matching; related meaning in task titles is not automatically inferred.
5. The affected lead acknowledges the request version and reports a proposed split, join, handover or decline. Responses go back to the requester. Make the agreed scope or participation change, then resolve the request. The engine rejects resolution while an uncoordinated overlap remains. The requester can explicitly withdraw its proposal.
6. Report meaningful checkpoints and an explicit finish. Session end alone preserves unfinished work for resumption or handover.

A coordination acknowledgement is a client assertion of receipt, not agreement or proof that a model understood the text. Requests survive disconnects and replay from the durable inbox. They are included in subsequent MCP calls and supported hook replies; `GET /v1/hub/standup/inbox` also supports long polling with `wait_seconds` up to 25 seconds. Reading does not acknowledge. A fresh inbox without `after` contains open requests. An explicit `after` cursor includes later changes and closures; the repository list also retains request history. The repository list includes all statuses by default; `active_only=true` limits the fixed snapshot to pending and responded requests. The dashboard sets that filter initially and offers **Include resolved and withdrawn**. Fixed-watermark continuations use `after`, `through` and `limit`; start a fresh poll without the old watermark to receive newer changes. A disconnected agent is not running merely because a request was queued. Text from another agent remains untrusted data.

## Storage maintenance

Stand-up storage has a 128 MiB retained-payload budget per provider, host, repository and stable repository ID. It counts serialized work snapshots, checkpoint requests, cached briefing pages and work-event records, with a fixed allowance for each row. Other table data, database pages, indexes and write-ahead logs require additional disk space. New writes that exceed the budget fail and roll back; reads, acknowledgements and retained exact checkpoint retries remain available.

A local operator with direct database access can inspect and compact old history. This command uses the explicitly supplied database, rather than a Hub bearer token, and is not exposed through HTTP or MCP:

```sh
agentscm work compact --database sqlite:///absolute/path/hub.sqlite \
  --provider github --host github.com --repository acme/widgets \
  --repository-id 123 --before 1792278000
```

For a password-bearing PostgreSQL URL, select an existing environment variable instead of placing its value in command arguments or shell history: use `--database-env AGENTSCM_DATABASE_URL` in place of `--database`. The variable must already contain the intended database URL.

The default is a dry run. Inspect its result before repeating the command with `--apply`. The cutoff is a Unix timestamp and must be at least 24 hours old. Compaction refuses while an unexpired, unacknowledged briefing or catch-up delivery
depends on the repository’s history. It preserves current work, recent operation receipts and the historical baseline needed for retained snapshots. Later briefings explicitly report a retention gap when their acknowledgement boundary predates removed history.

Merge observations have a separate retained limit of 100,000 rows and 512 MiB of
accounted payload per repository. Ingestion that exceeds either limit fails with
incomplete or unavailable coverage. The same maintenance command previews and
removes an eligible oldest sequence prefix of merge observations, reports
`deleted_merge_observations` and `merge_retention_floor`, and preserves newer rows.
Merge updates remain prohibited. Later catch-up deliveries explicitly report a
merge retention gap; maintenance does not promise complete forge history.

A failed tracked-publication projection retains that exact publication identity and
reports incomplete coverage with a deferred count. Successful retries remove the
failure and recompute coverage. Optional projection SQL failures are isolated from
the authoritative publication update. If a legacy publication recorded an invalid
merge commit, a trusted local operator can preview a repair:

```sh
agentscm work repair-merge --database-env AGENTSCM_DATABASE_URL \
  --repository acme/widgets \
  --publication pub_00000000000000000000000000000000 \
  --merge-commit aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
```

Use the verified canonical merge commit for the exact repository and proposal;
this command does not query the forge. Inspect the proposal, then repeat with
`--apply`. Repair requires an exact retained projection failure and an invalid
current publication projection, or a retry of the same valid commit after a
transient projection failure. The dry run checks the same eligibility without
changing data. Apply rechecks eligibility atomically and reports success only
after recording the observation and clearing the exact failure. Correcting an
invalid stored merge fact records the publication identity and prior fact fingerprint and proposed commit in
an audit event in the same transaction. Only this guarded local repair can replace
an invalid retained merge commit; ordinary merge updates can retry the projection
of the same commit. Valid immutable merge facts remain protected. The output contains bounded
identity and merge fields, never the publication manifest or database URL.


Compaction also removes coordination requests that were resolved or withdrawn before the cutoff, along with their retained receipts, replies and operation history. Open requests remain intact, even when older than the cutoff. The result reports `deleted_coordination_requests` and `coordination_retention_floor`; older inbox/list cursors report a retention gap rather than claiming complete history. Removed coordination receipts can no longer be replayed, so inspect current work before retrying an old operation. Reclaimed payload quota becomes available for new work, while physical database file size may remain unchanged.

This maintenance path can delete `work_started`, `work_joined`, `work_takeover` and `work_checkpoint` events, plus the closed request's `coordination_requested`, `coordination_bound`, `coordination_retargeted`, `coordination_acknowledged`, `coordination_responded`, `coordination_resolved` and `coordination_withdrawn` events. Removed coordination history advances the durable retention floor described above. All event updates remain prohibited, and other Hub event kinds retain their append-only protection. Stand-up history is self-reported coordination context, not retained execution or approval evidence.

Checkpoint retries are supported for at least 24 hours. After a receipt has been compacted, its old version guard prevents the request from being applied again; the caller must inspect current work rather than infer whether the old operation committed. Compaction does not remove current work items, so repositories with many retained items can still reach capacity. Back up the database before maintenance and provision disk space for retained data; compaction does not promise to shrink database files immediately.

## Agent surfaces

The CLI prints the Hub's JSON response. A rejected `start` claim prints the structured conflict and exits with a nonzero status. `start` and `checkpoint` accept a strict JSON request from a file or standard input, bounded to 1 MiB:

```sh
agentscm work overview
agentscm work inbox --session agent-a --wait-seconds 20
agentscm work briefing --subscriber agent-a --limit 50
agentscm work briefing --subscriber agent-a --after 120 --work-done --through 175 --limit 50
agentscm work start --request start.json
agentscm work checkpoint --request checkpoint.json
agentscm work ack dlv_…
```

Continue with every cursor field returned by the previous page. `--work-done` is significant even when there is no `--work-after` value: it says the current-work snapshot is exhausted while event pages continue.

MCP tool replies carry compact coordination notices in text and `_meta.agentscm.coordination_notifications`; their primary structured result stays unchanged. Use the inbox tool for full request detail.

Coordination admission is bounded to 8 open requests per token, 25 per owner and 100 per repository. Unacknowledged briefing deliveries are bounded to 8 per token and 16 per owner across all subscriber sessions; changing subscriber names does not create more quota. Retrying an existing operation remains idempotent at these limits. A current affected lead can withdraw an abandoned request only when its requester token is revoked or expired, or its owner is disabled; this records the cleanup actor and leaves all work assignments intact.

MCP exposes `agentscm_work_overview`, `agentscm_coordination_inbox`, `agentscm_coordination_ack`, `agentscm_coordination_reply`, `agentscm_coordination_resolve`, `agentscm_coordination_withdraw`, `agentscm_work_briefing`, `agentscm_work_checkpoint` and `agentscm_work_ack`. The existing `agentscm_start` tool accepts an explicit work ID, canonical forge source or stable request key and returns its pre-start briefing with the claim result. Read-only tokens can call `agentscm_work_briefing`; the mutation tools reject them. Implicit MCP pre-start and native session-start briefings are pure previews with no delivery receipt. For deliberate acknowledgement, request a durable `agentscm_work_briefing`, retrieve every page, then acknowledge its delivery identifier.

## HTTP surface

All scope comes from the bearer token. There is no repository selector in these routes.

| Route | Token | Purpose |
| --- | --- | --- |
| `GET /v1/hub/standup/overview` | Read-only or writable | Inspect bounded current work, time-window outcomes and declared overlaps without acknowledgement. |
| `GET /v1/hub/standup/coordination` | Read-only or writable | Inspect repository coordination requests without acknowledging. |
| `GET /v1/hub/standup/inbox` | Read-only or writable | Read requests addressed to an explicit session; optional long poll. |
| `POST /v1/hub/standup/coordination/ack` | Writable agent | Record receipt for an addressed request version. |
| `POST /v1/hub/standup/coordination/reply` | Writable agent | Report a current affected lead’s proposed action. |
| `POST /v1/hub/standup/coordination/resolve` | Writable agent | Validate actual coordination before recording resolution. |
| `POST /v1/hub/standup/coordination/withdraw` | Writable agent | Withdraw your proposal; a current affected lead may withdraw it only if the requester token is revoked/expired or its owner is disabled. This cleanup is audited. |
| `GET /v1/hub/standup/briefing` | Read-only or writable | Retrieve a bounded current-work page and ordered change page. Continuations use `after`, `work_after`, `work_done`, `through` and `limit`. |
| `GET /v1/hub/standup/work/{id}` | Read-only or writable | Inspect one authorized work item, including finished work retained for drill-down. |
| `POST /v1/hub/standup/start` | Writable agent | Create/reuse, join or generation-checked take over a work item. |
| `POST /v1/hub/standup/checkpoint` | Writable agent | Append one idempotent, generation-checked checkpoint. |
| `POST /v1/hub/standup/ack` | Writable agent | Advance a fully delivered briefing boundary by delivery identifier. |

Pages contain at most 100 current items and 100 events. Titles and checkpoint summaries are bounded to 500 bytes; identifiers and labels are bounded to 255 bytes. Scope, participants, relations and source references also have fixed entry limits. Malformed cursors and out-of-scope identifiers are rejected instead of silently widening access.

## Current support and limits

The read-only [work insights](work-insights.md) projection adds bounded timing, coordination
signals, usage reports and stored publication observations to retained stand-up work. Reported
completion remains separate from publication state, checks, approval and landing. Missing history
and usage remain explicit rather than being counted as zero.

| Surface | Current behavior |
| --- | --- |
| Hub storage | Durable SQLite and PostgreSQL work and directed-request records, repository-scoped authorization, atomic claims, generation fencing and bounded pages. Required schema migrations run when the store opens; a migration failure prevents the store and API from starting with partial coordination support. |
| Hub dashboard | Current Work, outcomes, coordination, overlaps and source detail; personal catch-up with explicit review acknowledgement. It cannot mutate assignments or acknowledge agent briefings. |
| CLI | Agent-authenticated overview, inbox, request acknowledgement/reply/resolution/withdrawal, preflight, start, checkpoint, personal catch-up and agent briefing with separate explicit acknowledgements against the Hub. Work drill-down is available through the HTTP API and dashboard. |
| MCP | Agent-authenticated stand-up tools expose the same bounded records. Calls depend on the model following instructions; MCP cannot intercept arbitrary work. |
| Native hooks | A supported session-start event can read and render a bounded briefing. A safely linked end event can pause active work if its observed checkpoint is still current. Hooks do not create a goal, acknowledge a briefing, infer decisions or meaningful progress, or claim accepted/landed completion. Existing edit events continue to announce file intent and warn about overlaps. |
| Forge links | A work item can refer to an issue in its configured GitHub or GitLab repository. No automatic issue creation, update, comment or two-way mirror is performed. |
| Native workflows | Work references can describe coordination around a workflow, but checkpoints do not become workflow evidence or landing authority. |
| Qualification | Protocol and local storage behavior are tested. Real-client, live two-machine, live-forge and PostgreSQL deployment qualification are still separate work. |

The Hub only knows about agents and checkpoints that reach it. Offline edits, skipped MCP calls, missing end hooks and disconnected agents can leave incomplete or stale records. Preserve source work and inspect current repository state before any takeover or resume.
