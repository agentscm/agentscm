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
- another AgentSCM work identifier.

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

The dashboard uses authenticated read-only GETs. It never acknowledges a request or briefing and never changes an assignment. Each bounded section states its completeness; request pages retain their fixed upper watermark, and Change history retains the briefing cursor tuple. Stale and unknown claims remain assigned. Finished work carries no assignment claim and does not imply tests, review or merge.

## Before work and after an overlap

1. Read `agentscm_work_overview` (or `agentscm work overview`) before planning. It shows unfinished assignments, declared overlaps, and reported outcomes in the last 24 hours by default. `recent_since` selects another starting time. A compacted history is explicitly incomplete.
2. Read the session's briefing and coordination inbox. Retrieve every page before relying on coverage. Reconnect with the same subscriber/session; acknowledge only deliberately received data.
3. Start with a stable work identity, explicit session and intended paths. HTTP/CLI and MCP starts return a pre-claim overview in `context`. The database serializes scope admission with the claim, so simultaneous different work identities cannot both claim the same declared scope unnoticed. Empty scope is unknown coverage; declare scope before editing.
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

The default is a dry run. Inspect its result before repeating the command with `--apply`. The cutoff is a Unix timestamp and must be at least 24 hours old. Compaction refuses while an unexpired, unacknowledged briefing depends on the repository's history. It preserves current work, recent operation receipts and the historical baseline needed for retained snapshots. Later briefings explicitly report a retention gap when their acknowledgement boundary predates removed history.

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

Continue with every cursor field returned by the previous page. `--work-done=true` is significant even when there is no `--work-after` value: it says the current-work snapshot is exhausted while event pages continue.

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
| Hub dashboard | Read-only current-work, recent-outcome, directed-request, detected-overlap, catch-up and work-detail inspection. It cannot mutate or acknowledge. |
| CLI | Agent-authenticated overview, inbox, request acknowledgement/reply/resolution/withdrawal, start, checkpoint, briefing and briefing acknowledgement against the Hub. Work drill-down is available through the HTTP API and dashboard. |
| MCP | Agent-authenticated stand-up tools expose the same bounded records. Calls depend on the model following instructions; MCP cannot intercept arbitrary work. |
| Native hooks | A supported session-start event can read and render a bounded briefing. A safely linked end event can pause active work if its observed checkpoint is still current. Hooks do not create a goal, acknowledge a briefing, infer decisions or meaningful progress, or claim accepted/landed completion. Existing edit events continue to announce file intent and warn about overlaps. |
| Forge links | A work item can refer to an issue in its configured GitHub or GitLab repository. No automatic issue creation, update, comment or two-way mirror is performed. |
| Native workflows | Work references can describe coordination around a workflow, but checkpoints do not become workflow evidence or landing authority. |
| Qualification | Protocol and local storage behavior are tested. Real-client, live two-machine, live-forge and PostgreSQL deployment qualification are still separate work. |

The Hub only knows about agents and checkpoints that reach it. Offline edits, skipped MCP calls, missing end hooks and disconnected agents can leave incomplete or stale records. Preserve source work and inspect current repository state before any takeover or resume.
