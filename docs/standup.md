---
title: "Asynchronous agent stand-up"
---

# Asynchronous agent stand-up

> **Alpha: not for production.** The API and stored schema may change without migration. Stand-up records are coordination data supplied by agents; they are not proof that code ran, passed review or landed.

The Hub keeps durable engineering work separate from short-lived sessions and file intents. Before starting a task, an agent can retrieve:

- the current unfinished work for the token's repository, including old, stale and unknown claims, except nonholding Work hidden by a legacy branch merge observation (see [Work with a merged pull request](#work-with-a-merged-pull-request));
- an ordered, bounded change history since this agent token and subscriber/session last acknowledged a briefing; and
- blockers, decisions, next actions, participants, dependencies and related work reported at checkpoints.

This gives agents working at different times a shared starting point without treating a new chat or session as a new engineering goal. It also works for repositories that stay on plain Git. Native source workflows remain a separate system with their own revisions, execution evidence, approvals and landing checks.

## Planned issues and native Work

For a connected GitHub repository, overview and briefing responses include `planned_issues` separately from Work. The Hub reads at most three pages and 300 raw rows of open issues, excluding pull requests, with embedded milestone labels and a 512 KiB snapshot byte cap. Oversized planning reads leave coverage unknown while the coordination poll continues. It retains no issue descriptions. Provider, host and stable numeric repository ID bind the facts; a repository rename does not change issue identity. GitLab issue discovery is unsupported.

`unclaimed` contains issues with no durable linked Work in the checked inventory. `linked` pairs each issue with its Work IDs, including original forge-source Work and explicit native links. An issue appears in exactly one bucket. Finished Work links remain historical relationships; they prove neither active responsibility nor execution. Heartbeat age, reservation expiry and overview pagination do not make an issue unclaimed. The linkage inventory reads at most 1,000 Work rows independently of the displayed Work page; when it cannot establish absence, affected issues appear in `linkage_unknown`.

Inspect `coverage`, `freshness`, `complete`, `linkage_complete`, `unread_pages` and the provider timestamps. Facts become stale after five minutes. An outage, missing permission, malformed or missing retained snapshot, retention loss or incomplete page inventory never means nothing is planned. Earlier facts may remain visible with unavailable coverage and their original `facts_observed_at_ms`; `attempted_at_ms` is not a successful refresh. `evaluated_at_ms` states when the projection assessed these facts, using the store's whole-second clock expressed in milliseconds. Observations within that same second retain their precise provider timestamp without being classified as a backward clock. Frozen legacy briefing pages preserve that capture-time assessment on replay. Generated narrative context includes at most 20 planned issues, sourced separately from Work reports, with explicit omission and coverage warnings.

Before starting native Work with a `request_key`, call `agentscm_work_preflight` (HTTP `POST /v1/hub/standup/preflight`, CLI `work preflight --request FILE`) with the intended task text and inspect `issue_advice`. Candidates come only from exact whitespace-delimited references in the caller's title/task or next action: `#42`, `acme/widgets#42`, or a canonical HTTPS issue URL such as `https://github.com/acme/widgets/issues/42`. Qualified references and URLs must identify the authorized repository; foreign references never fall back to their embedded number. Canonical URLs have no credentials, port, query or fragment. Candidates are sorted, deduplicated and capped at 20; `references_complete` reports truncation. There is no fuzzy or generated ownership matching.

Then supply an explicit `issue_choice` on start, using one of these forms:

```json
{"kind":"link","issue":{"provider":"github","host":"github.com","repository_id":101,"number":42}}
{"kind":"new_issue","title":"Investigate the separate import failure"}
{"kind":"none_applies","reason":"This maintenance task is outside the listed issues"}
```

These are caller assertions. `new_issue` records an intention and creates no forge issue. Advice does not assign ownership or change preflight admission status, and planning uncertainty never refuses Work. Older callers may omit the field or send `{"kind":"not_provided"}`; that remains unassessed, not an assertion that no issue applies. The immutable `initial_issue_choice` is retained with the start history. A retry cannot replace it with a contradictory choice.

To link existing native Work later, use `agentscm_work_issue_link_set`, HTTP `POST /v1/hub/standup/issue-link`, or CLI `work issue-link-set --request FILE`. The current lead supplies a fresh operation ID, session, generation and `expected_updated_seq`, plus the stable issue identity in `link`; explicit `link:null` removes the current caller assertion. The issue must belong to the authenticated stable repository, but the mutation does not require a provider refresh. Exact retained retries return the original receipt and current Work; changed reuse of the operation ID, stale fences and unauthorized updates are rejected. Later linkage changes only `issue_link`, preserving the Work ID, original source, native request key and initial choice. It records an issue metadata event without manufacturing a progress report or completion. Issue closure never completes Work or releases a reservation.

Qualification is limited to synthetic fixtures and local fake GitHub HTTP, with SQLite and disposable PostgreSQL conformance. Live GitHub permissions and pagination, installed harness behavior, multi-machine delivery and hosted deployment remain unqualified.

## Work identity and claims

Every work item has a stable `wrk_…` identifier. New work can use a caller-supplied request key for durable retries, or link to an issue in the token's configured GitHub or GitLab repository. The forge remains authoritative for issue content and state; AgentSCM owns the claim and checkpoint history. It does not mirror issue descriptions or publish checkpoint text to the forge.

A work item has one lead by default. Other agents can join as helpers or reviewers. A claim records the human owner, agent, session, declared repository paths, branch, worktree label and next action. Dependencies and related work use explicit `wrk_…` identifiers; AgentSCM does not merge similar free-text tasks automatically.

People read a work item by its `title` (the MCP `task`) and its optional `description`: the title names what changes, and the description says what the Work changes and why, in two or three plain sentences for a teammate who was not there. Each is 1 to 500 bytes without control characters. Only the start that creates the Work sets the description. A later start (a retry, resume or takeover) is not compared on it and keeps the stored text, so Work stored before the field existed can be resumed with one, and no checkpoint changes it. Work created without a description, including Work stored before the field existed, has no `description` key. The MCP tool text, the server instructions and the `coordinate-edits` skill ask for a plain title under about 70 characters, with file paths, issue lists and design notes in the description, and for one- or two-sentence checkpoint summaries. That is guidance: the Hub refuses no title for its length.

The claim is coordination state. It does not create a Git checkout, lock a file, stop an old agent process or replace source-control conflict checks. A disconnected or expired lead becomes `stale`; a projection failure becomes `unknown`. Both are freshness labels only. Neither changes who is responsible for the Work, and neither releases anything or opens it to takeover: another agent may take over Work that holds a reservation only after its lease lapsed, through an explicit generation-checked takeover, so agents must not interpret either status as available work.

Three things are kept apart. The owner and lead are the durable responsibility for the Work. The declared scope is the intended paths. The reservation is whether that scope is currently held against other Work, recorded as a basis: `admitted` (fresh admission by a start, takeover, resume of released Work or reopen), `retained` (kept on purpose through the `retain` transition) or `implicit` (left by a pause or handover, or backfilled for Work that existed before this version). A reservation blocks only while its lease is live: 20 minutes from the lead session's last renewal (2 hours from a `retain`), renewed by that session's tool calls, named MCP session calls and checkpoints, and kept live for up to 60 minutes while one of its shell calls runs. A lapsed reservation blocks nobody, and the next admission over its scope clears it and fences the old lead; see [the lifecycle design](design/work-lifecycle.md#8-liveness-leases). Unfinished Work with no reservation is still unfinished Work, but other Work may declare overlapping scope without a conflict. Every overlap check, preflight and request resolution counts only held reservations. A stale or unknown claim keeps a live lease; once the lease lapses the reservation is advice. A lead whose lease lapsed and whose scope another Work took is refused with `LEASE_LAPSED`, which says when the hold lapsed, which Work took the scope, that the branch may hold unpushed work, and the takeover start that resumes it; its start of the same Work gets the same answer. The admission that takes a lapsed hold's scope lists it in `reclaimed` with the same warning. A lead whose lease lapsed while nobody took the scope is refused too, until it starts its Work again; a pause, handover or retain never revives a lapsed lease.

A finish checkpoint sets the outcome `completed`, attributed to an agent report, and clears the reservation. It also does the cleanup of a terminal transition in the same transaction: it releases the live file intent of every participant session, releases the lead's and participants' session-less intents that cover the Work's scope (see below; Work that held no reservation releases none, and an intent that another Work with an eligible session of the same token covers stays, as does any session intent of a session that is eligible on other Work), and records a `coordination_target_released` event for each open request that targets or proposes the Work, leaving the request status unchanged. The checkpoint itself uses the normal capacity check; the cleanup records its size without one, so a finish is not refused because of it. Finished Work holds nothing under any outcome. A checkpoint on finished Work is refused, and so is a start with `takeover_generation` on finished Work, even when the claim is stale; the error names reopen. Reopen is the explicit way back: it runs a fresh overlap check, and on success the Work is active with an `admitted` reservation and the caller as lead.

To resume a paused task as the same agent token, start the explicit work ID with the observed `takeover_generation`. The new generation binds the session and fences the previous session. An ordinary retry does not silently change paused work back to active. A session leads at most one `active` or `blocked` item. Paused or handed-over Work does not occupy it, so the session can start other Work with an explicit identity (a `request_key` or forge source); resuming the paused item while the session leads another active or blocked item returns `session_goal_changed` and changes nothing. A task-only start is identified by its session, so it still returns the session's paused item. Existing blockers remain reported until explicitly resolved. After a lead changes, helpers from an older generation must explicitly rejoin before their managed edit checks pass. A finish releases the linked live file intent in the same transaction; a linked finished or fenced session cannot regain paths through the named-session routes (the plain announce route is advisory; see below).

Released Work, meaning unfinished Work with no reservation, is resumed the same way, by a start with the current `takeover_generation`, and this works in any state (including `blocked`) and while the claim is still current. The lead's own token may resume it, and so may any other token of the same owner, which keeps a rotated token working. Other owners keep the ordinary takeover rules. The resume runs fresh admission: on a scope overlap it returns the conflict and changes nothing. While Work holds no reservation, a checkpoint of kind `progress`, `decision` or `blocker` is refused with an error that says to resume first, so a lead that re-reads its Work cannot turn it active with nothing held. `pause`, `handover` and `finish` checkpoints stay allowed, including the automatic session-end pause. An exact retry of an earlier checkpoint is resolved before this check.

The plain `POST /v1/hub/repos/{repo}/intents` announce names no session, and an intent there is advisory, not a reservation. It is refused only when an announced path lies in the declared scope of unfinished Work on which the token is a participant or lead but none of its sessions is currently eligible (reservation not held, `paused` or `handover`, or fenced by generation), and no Work on which one of its sessions is eligible covers the path. The refusal names the Work and says to resume it; a start with the Work's `takeover_generation` is the resume. All other paths keep the legacy advice: tokens never linked to Work, tokens whose only link is finished Work, empty-scope Work and paths outside every linked scope. A token whose Work is finished can post advisory intents; it acquires no reservation and passes no admission. Named-session routes (touch, check, hooks, MCP) keep the strict guard. `agentscm intent check --hook claude` posts this route with the session's accumulated paths and one intent id; when the Hub answers that the stored id was released, it mints a new id, keeps the paths and retries once, and a refusal prints the Hub's message. The guard is on the plain announce route for both stores; it does not refuse a token whose repository was renamed, as the announce never did.

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

A pause or handover checkpoint, including the automatic session-end pause, keeps the scope held: it converts an `admitted` reservation to `implicit`. A `retained` reservation stays `retained`, so an explicit decision survives the end of a session; a takeover sets `admitted` again. Work that is paused or handed over keeps its reservation, but the session's later activity does not renew it, so an implicit reservation stops blocking 20 minutes after the pause and a retained one when its 2 hours end. Resuming released Work needs a start with `takeover_generation`, which runs fresh admission.

### Work transitions

A transition changes the outcome or the reservation without a checkpoint written in the agent's name. The kinds are `release`, `retain`, `complete`, `abandon`, `supersede` and `reopen`. The request carries `operation_id`, `generation`, `expected_updated_seq` and a required reason (at most 500 bytes); `supersede` also names another Work in the repository. A different request with a used `operation_id` is refused and an exact retry returns the stored receipt and acquires nothing.

| Transition | Allowed from | Who may request it | Effect |
| --- | --- | --- | --- |
| `release` | Unfinished, reservation held | Lead, owner, administrator | Clears the reservation and bumps the generation; `active` becomes `paused`. |
| `retain` | Unfinished, reservation held | Lead, owner | Basis becomes `retained`. |
| `complete` | Unfinished | Lead, owner | Finished with outcome `completed`; clears the reservation. |
| `abandon` | Unfinished | Lead, owner, administrator | Finished with outcome `abandoned`; clears the reservation. |
| `supersede` | Unfinished | Lead, owner, administrator | Finished with outcome `superseded`; clears the reservation. |
| `reopen` | Finished | Lead session, or the owner's agent token | Fresh admission; see above. |

The lead is the agent token and session that lead the Work. The owner is the person who owns it, acting through a web sign-in, or an agent token of that owner with a named session. An administrator is a web person with the administrator role. A read-only token is refused, and a revoked or expired lead does not stop the owner or an administrator. An owner or administrator acts as itself: the actor is recorded as that person and never as the lead. A person cannot reopen because reopen needs an agent session. The terminal transitions by anyone other than the lead session bump the generation, which fences the older session.

The same owner and the same session keep that lead when the presented token or agent label changes. The carry updates `lead_token` and `lead_agent`, appends `work_lead_credential` (session, previous agent label, new agent label; no token id), and leaves the generation unchanged. A different owner is refused with `LEAD_SESSION_MISMATCH` alone. The same owner on another session gets that code, the recorded lead agent and session, and `agentscm_start` with `work_id` and the current `takeover_generation`. HTTP and MCP pass the code through the way they pass an ambiguous omitted session. Carry runs on checkpoint and on a touch that named its session or came from a hook. An omitted MCP session does not carry. Startup context and `agentscm_status` say that the recorded lead credential differed, for the same owner, before the touch records the new one. That touch bumps `updated_seq`; a checkpoint compares `expected_updated_seq` before it carries, and a refusal writes nothing. Generation fencing and session resolution are unchanged, and this is not a lease. A renewal that matches `lead_token` and `lead_session` still matches after the carry.

Release alone asserts nothing about completion. It only drops the hold; the Work, its history, its blocker and its outcome are unchanged. Use `complete`, `abandon` or `supersede` to say why Work ended.

In the same transaction a transition records a `work_transition` audit event with the actor, reason and prior state, basis and outcome, and saves a Work version. A release or terminal transition also releases the live file intent of every participant session and the lead, and releases the session-less intents (announced through the plain route) of the tokens of those participants whose paths overlap the Work's scope; intents of other tokens are left alone. Two exceptions keep a token's live intents on other Work: Work that held no reservation releases no session-less intent, and a session that is currently eligible (a held reservation, active or blocked, current generation) on other Work keeps its intent, and a session-less intent covered by such other Work is kept. It adds a `coordination_target_released` event to each open coordination request that targets the Work or names it as its proposed Work. The request status is unchanged and nothing is resolved automatically. A reopen that changes the lead session retargets the open requests that target the Work with the same event that a takeover uses, so a responded request goes back to pending for the new lead; this uses the normal capacity rules because a reopen is not cleanup. Release and terminal transitions succeed at the payload quota; reopen and retain use the normal capacity check.

The projection `WorkOperationalV1` shows outcome, reservation, freshness, merge observation, who should act and the remedies the state allows. `actionable` uses stored facts only: finished Work and Work with no reservation name nobody; a held reservation with a legacy merged-branch observation, or held by paused or handed-over Work, names the owner; a held reservation on active or blocked Work names the lead. `activity` is always `unknown`. `completeness.unknown` lists `freshness` when the claim status is unknown and `outcome_provenance` when a stated outcome has no surviving event that says who stated it. Over HTTP and MCP the `operational` key of an overview is bounded by what the overview leaves of the briefing size limit: entries that do not fit are dropped and `complete` turns false.

Recent outcomes in the overview list a Work item as a completion only when its last version is a finish checkpoint, newest first; decision checkpoints are listed as decisions. Abandoned, superseded, released, retained and reopened Work do not appear, and neither does Work ended by a `complete` transition: a transition leaves the Work's current summary unchanged and records no author for one, so listing it would show a stale summary under the lead's name. A `complete` transition is visible through the Work's history and the operational projection (outcome and provenance). The Hub does not record a completion observation for a `complete` transition; completion observations stay tied to checkpoints. See [Work outcomes and scope reservations](design/work-lifecycle.md).

### Recover stuck Work

A refused start or a quiet session should not send an agent searching for hidden Work or negotiating with a dead session. `agentscm_work_recovery` (`agentscm work recovery`, `POST /v1/hub/standup/recovery`) is one bounded read of what the caller can act on:

- **The owner's unfinished Work**, by id, at most 50 a page (`after` takes the previous `next_after`). Each item is `active` (a live hold on active Work), `needs_you` (a live hold that is blocked, paused, handed over or merged), `resumable` (unfinished and holding nothing live: released, or its lease lapsed), `finished` or, for one `work_id` of another owner, `other`. Each says why it still exists, its reported next action and blocker, the open coordination requests that target it and the unfinished Work that depends on it (at most 25 of each, with `affected_complete`), and its operational projection.
- **Typed remedies**: `release`, `retain`, `complete`, `abandon`, `supersede`, `reopen`, `resume` and `handover`, each `permitted` or refused with a `code` (`read_only`, `not_authorized`, `needs_session`, `wrong_state`, `no_reservation`, `hold_live`, `lease_lapsed`, `not_lead`, `already_lead`, `claim_current`) and a sentence, and for a permitted one the exact call with the fences filled in. The rules are the transition matrix above, the takeover rules of a start and the lead's handover checkpoint.
- **With `paths`**: the live holds over them with the exact overlapping paths, whose they are and what to do (never a takeover of another owner's live hold), and `resumable`, the owner's unfinished Work over those paths that holds nothing live.

`agentscm_work_recovery_preview` says what one remedy would change now: state, reservation and outcome after, whether the generation advances and who is fenced, the paths freed and kept, the requests and dependents it reaches, the follow-up that stays, and whether apply runs a fresh admission. A handover preview takes a narrower `scope`: the next lead keeps those paths and the rest is freed at once. A preview grants nothing. Apply is the existing operation (`agentscm_work_transition`, `agentscm_start` with `takeover_generation`, or the lead's handover `agentscm_work_checkpoint`), which re-checks identity, permission, generation and `updated_seq` in one transaction and refuses with nothing changed if any moved.

`agentscm_work_transitions` (`agentscm work transitions`, `POST /v1/hub/standup/work-transitions`) applies up to 25 transitions in order, each in its own transaction, and reports one outcome per item: `applied`, `replayed` (applied before; nothing changed now), `conflict` (a reopen overlapped; nothing changed), `refused` with a `code` (`generation_changed`, `work_changed`, `lease_lapsed`, `unauthorized`, `not_authorized`, `wrong_state`, `no_reservation`, `operation_reused`, `not_found` or `invalid`) and a `retry` instruction, or `unavailable` (retry the item with the same `operation_id`). The batch is not atomic: earlier items stay applied when a later one is refused, and sending the same batch again replays them.

A new lead start over paths where the caller's owner already has unfinished Work that holds nothing live, under another identity, creates and claims nothing in MCP: the reply lists that Work with the resume call, and `new_work: true` starts different work anyway. A start whose identity is that Work's own resolves to it as before. A start refused for any other reason carries `recovery` with the conflicting Work's remedies for the caller and the live holds over its paths. In the compact default it keeps each item's permitted calls, the live holds and the resumable ids; `detail: "full"` gives the whole read.

A person does the same from the Hub. A task's page shows **Release or end this task** to its owner and to a Hub administrator, with a preview before **Apply**; it uses the person routes in the [Hub guide](hub.md) and the same service. A person can release, retain, complete, abandon or supersede as the matrix allows, but cannot resume, reopen or hand over, because those make an agent session the lead. That is how an owner retires the hold of a session that is gone, whatever happened to its token.

## Shared actionable status

Ask your agent what is happening, what needs you and what can proceed. `agentscm_work_status`, `agentscm work status` (or `--json`) and `GET /v1/hub/standup/status` read one deterministic Rust projection. MCP overview, start, briefing, catch-up and cached-briefing reads also include `actionable_status` and useful bounded text. Native session-start hooks append the same status best effort within a short deadline; a failed read says unavailable and does not undo the committed touch. Generated prose is optional. HTTP overview remains strict and compatible; existing HTTP/CLI catch-up readers can explicitly read status alongside their fixed historical delivery.

The shared states are active, awaiting a named party, blocked, reassessment needed and retired. Every held reservation remains visible, including legacy Work omitted by merge filtering. Current actions name stable Hub recipients, and personal counts use that identity. Repository lists and computer catalogs use the same role-checked person projection without minting a read token; detailed catalog rows include recipients, states, next actions and coverage. Companion rows use those server booleans. A catalog’s attention marker changes with actual actionable facts rather than poll time.

Claims and agent reports remain separate from provider observations. Drill-down shows report provenance, provider source coverage and provider check times; the status read time is not provider freshness. Unavailable provider evidence, stale generated synthesis and incomplete collection are distinct. Counts are lower bounds when bounded collections omit records (at most 100 Work and 200 actions, with a response budget). Shared reads span multiple transactions and never claim an atomic snapshot. A required check that fails on an open pull request is a personal action for that pull request's people: its forge author when they use this Hub, the owner of the Hub publication that opened it, and the owners of unfinished Work on its branch. Required means `agentscm/local-validation` or a Hub forge check (`agentscm/workflow/<target>`). The newest status per context comes from the Hub's retained GitHub snapshot of the pull request's current head and base; a snapshot for an older head, an unconfirmed grant or a GitLab repository leaves check coverage incomplete instead of clear. A `pr-gate` run whose Stop hook blocks at a gate the agent must act on (tests, review or fix) becomes a personal action once no session that recorded its evidence is live and the run belongs to an open pull request or unfinished Work. The viewer's own agent setup problems (a reported blocker, MCP verified while no edit hook has been, or no valid agent token for the repository) are `agent_setup` actions: they badge Setup and need the person only when that agent leads unfinished Work (see [team member onboarding](team-member-onboarding.md)). A session of the viewer's own that its hooks report waiting for them (a permission prompt, a question, or the end of a turn while it leads unfinished Work) for at least 60 seconds is a `session_waiting` action until its next call (see [waiting for you](agents.md#waiting-for-you-in-its-session)). A capacity refusal in the last day lists `capacity_refused:<limit>` among the repository's limits (see [storage maintenance](#storage-maintenance)); missing signals never imply clear status.

The classic and G Hub layouts have synthetic Chromium desktop/phone and 200% zoom checks. Native macOS delivery, native mobile apps, live forge/Slack operation and PostgreSQL deployment are not qualified by those checks. Slack remains private, opt-in for bounded reminders, and uses one Slack binding per Hub user with a single-worker local SQLite delivery journal even when the Hub uses PostgreSQL; see [Slack capabilities](slack-capabilities.md).

## Briefings and acknowledgement

The first briefing page fixes an upper event watermark. Continue with the exact returned event cursor, work cursor, work-complete flag and watermark. This keyset traversal keeps concurrent appends out of the current briefing and prevents an exhausted current-work snapshot from restarting while event pages remain. The change interval is exclusive of `from` and inclusive of `through`.

The response contains two independent sections:

- `items`: a page from the current unfinished-work snapshot, without Work that has a merged pull request. Work created before the acknowledgement boundary still appears here.
- `events`: ordered changes after the previous acknowledged cursor and no later than the fixed watermark.

`retention_gap: true` means some requested history is no longer retained. Do not turn that gap into an empty or complete history claim. A writable agent briefing has a delivery identifier. Acknowledge it only after every page was delivered successfully; acknowledgement advances only that token and subscriber/session's boundary for the repository. Repeat `subscriber` on each continuation; the delivery records which subscriber an acknowledgement advances. Separate sessions cannot consume each other's recap when they use distinct subscriber identities. Omitting `subscriber` on the HTTP API keeps a token-level compatibility cursor. MCP and supported startup hooks resolve the current session automatically. A read-only token receives authorized inspection data without a delivery identifier and cannot acknowledge or mutate work.

The Hub spreads this across three places. **Dashboard › Now** lists what needs a person, then every listed unfinished task as one lane (claimed, where it is now with its lead, its next step, finished) with the lead's latest report. **History › Finished** uses a server-reported time window, independent of acknowledgement, and includes actual reported completion and decision checkpoints. The **Inbox** shows durable directed requests, per-lead receipt and reply state, and recorded resolutions. Detected scope overlaps are listed separately there: detection alone is not delivery or resolution. The shared status projection routes each action to its named owner or lead. Another person’s blocker is not automatically your obligation. Human handovers await the owner, and quiet held reservations can require the owner to reassess; quiet activity alone never proves failure or completion.

The dashboard reads current Work without changing assignments or acknowledging coordination requests or agent briefings. Its separate personal catch-up panel advances only the signed-in person’s review position after an explicit action. Each bounded section states its completeness; request pages retain their fixed upper watermark, and Change history retains the briefing cursor tuple. Stale and unknown claims remain assigned. Finished work carries no assignment claim and does not imply tests, review or merge.

### Durable prerequisites and reassessment

Use `agentscm_work_declarations` to read durable caller declarations and `agentscm_work_declarations_set` to replace explicit PR sets and prerequisite targets with current lead/session/version fences. Ordinary progress checkpoints cannot delete these declarations. Imported legacy dependencies remain explicitly unbound until a target is declared.

`agentscm_work_reconciliation` (CLI `agentscm work reconciliation <id>`) reads the same current-versus-historical state shown on Hub Work detail. `agentscm_work_reconciliation_apply` explicitly evaluates retained server evidence after synchronization. Each exact-head merge satisfies only its own condition; unrelated obstructions and the original free-text blocker stay. A satisfied prerequisite alongside a free-text blocker creates a durable reassessment request, not rewritten history. Stale results expose no current success. A merged Work whose scope remains held needs an authorized owner decision, never automatic completion or release. See the [reconciliation contract](design/work-reconciliation.md) for wire formats, bounds and qualification limits.

### Work with a merged pull request

When a Work item holds no reservation and its branch has a merged pull request, the Hub may stop listing it as current work in the overview, briefings, the session-start recap and the dashboard. This is a read-time filter. The Work record, its state and its history are unchanged, and it stays available through search (`agentscm_work_search`) and history. "Settled" never means finished: the overview reports `settled_count`, and legacy overview reads identify those omissions.

An item is hidden only when all of these hold:

- it holds no reservation, and its branch matches a merged pull request observed from the same repository (a fork's branch name does not count) at or after the item's last check-in, and the branch is not the base of any observed merge, so default and release branches never hide;
- it is not blocked and has no unresolved blocker; and
- a check-in after the merge has not brought it back. A resumed paused item returns once its owner checks in or takes it over. An item paused automatically at session end is judged by the time of its last check-in before that pause (`reported_at`), not by the pause itself. If it was created and never checked in, any observed merge of its branch since it was created hides it. A check-in after the merge keeps it listed even if the session then ends.

Limits:

- An item auto-paused before this version has no recorded report time, so the rule compares against its creation time and a post-merge check-in does not keep it listed.
- A merge counts only when the forge reports when it happened. A tracked publication whose merge has no forge time is recorded as a merge observation but does not hide Work, because the time it was observed says nothing about when it merged. That Work stays listed, which is the safe direction.
- A merge first recorded as a tracked publication never hides Work. A later forge observation of that same pull request and commit is deduplicated and does not supply the time, so the item stays listed.
- A merge observed while a briefing is being delivered changes later pages of the current-item list of a durable delivery only. Exact retries return the cached page, the events list is not filtered, read-only briefings are not cached, and an item behind the cursor stays missing until the next delivery. Pages already delivered, and the fixed watermark of the delivery, are not rewritten.
- The Hub needs merge observations. With none, nothing is hidden, and a repository whose merges are not synced keeps every item visible.
- Merges observed before this rule existed carry no head-repository information. They hide matching items, including a fork's same-named branch, until the next check-in.
- Held reservations are never hidden by this filter. A merge does not change the outcome or the reservation; explicit lifecycle authority decides release or completion. Find the blocking reservation through `GET /v1/hub/standup/operational`, through the overview and conflict responses with `operational=1`, or through MCP, which always includes the projection. Its `merge` field reports `hidden_by_merged_pr`. `agentscm_work_search` still finds the Work by text.

### Since you last checked

The Hub's Dashboard › Catch up tab shows a personal return-to-work
delivery. It combines retained Work/checkpoint and coordination events
with immutable merge observations. Human review state belongs to the authenticated
person and repository, so it survives token rotation and is shared across that
person’s devices. A verified repository ID keeps that history and readership position
through a repository rename; unbound repositories retain name-based isolation.
Agent subscribers retain separate token/session positions.
Opening a page, login, polling, dismissal and startup never mark records reviewed.
The Hub's Catch up tab also has a confirmed **Mark all reviewed** action while a
delivery is only partly loaded. It pages through the rest of the fixed delivery
without showing it, then sends the one usual acknowledgement. It records that the
person chose to mark the delivery, not that they read it. The review position moves
only to the delivery's fixed watermark, so records newer than that stay unreviewed.
Work, claims and the forge do not change. If any page or the acknowledgement fails,
the panel reloads the delivery and does not assume the position moved.
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
select namespaced independent agent subscribers. Browser instances supply
an opaque `delivery_client` so two devices or tabs page independently while sharing
the person’s review position. With browser Web Locks, Hub tabs retain a bounded set
of client slots across reloads and acquire exclusive ownership before using a stored
client; a copied busy client is replaced. Browsers without that facility use a
volatile client. Reuse
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
from an unverified webhook payload. The sync also walks a declared window: closed
proposals updated within the last `activity_days` days, newest update first, in
bounded pages that start at 32 and halve when a response exceeds the body cap
(a single unreadable record is counted as unread). A pass makes at most 40 list
requests and re-reads new merge candidates (20 per poll; the rest are deferred).
When the walk needed several requests it looks at the list head again, because
proposals updated meanwhile move there. Coverage is observed-only only when the
window was fully read with nothing unread or deferred; history before the window
is not observed. Source status records unread pages, deferred observations,
failures and, for `forge_pull`, `window_from`: the lower bound of the window the
last pass declared (`null` when no pass declared one, as for `tracked_publication`).
Only an observed-only state says the window was read. Each pass restarts at the
list head with no cross-poll cursor, so a window holding more closed proposals
than one pass's 40 list requests can read (about 1,280 at 32 per page, far fewer
after an oversized record forces smaller pages) stays incomplete. Offset pages
can skip an item if a closed proposal leaves the closed list mid-pass (for
example, reopened); the next pass reads it. An oversized record that is the
first item past the window boundary keeps the pass incomplete until another
proposal crosses that boundary. Source-quality metadata refreshes on each page
without changing the delivery’s event watermarks. This is observed coverage,
not a claim of complete forge history. No model call generates the recap.

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

An incomplete merge path inventory does not block source freshness when the source revision is the exact merge commit, or an exact typed checked observation reports that merge as included with `source_relation_checked`. Unknown ancestry and an exact already-merged proposal head remain blocked.

## Before work and after an overlap

1. Read `agentscm_work_overview` (or `agentscm work overview`) before planning. It shows unfinished assignments (held reservations stay visible; `settled_count` counts only nonholding Work hidden by the legacy branch heuristic), declared overlaps, and reported outcomes in the last 24 hours by default: decisions and finish checkpoints, plus Work ended by a `complete`, `abandon` or `supersede` transition, which is listed once as a completion from its finished snapshot. `recent_since` selects another starting time. A compacted history is explicitly incomplete.
2. Read the personal/agent catch-up delivery, session briefing and coordination inbox. Retrieve every page before relying on coverage. Reconnect with the same subscriber/session; acknowledge only deliberately received data.
3. Inspect the proposed source and scope with preflight, then start with a stable work identity, explicit session, intended paths and the same checked merge watermark. HTTP/CLI and MCP starts return a pre-claim overview in `context`. The database serializes scope admission with the claim, so simultaneous different work identities cannot both claim the same declared scope unnoticed. Empty scope is unknown coverage; declare scope before editing.
4. A conflicting lead start creates an idempotent durable coordination request, addressed to the affected current leads, without creating a competing assignment or announcing paths. A conflicting scope checkpoint also records a request while leaving the work unchanged. Exact files and `directory/**` scopes use component-aware overlap matching; related meaning in task titles is not automatically inferred.
5. The affected lead acknowledges the request version and reports a proposed split, join, handover or decline. Responses go back to the requester. Make the agreed scope or participation change, then resolve the request. The engine rejects resolution while an uncoordinated overlap remains. The requester can explicitly withdraw its proposal.
6. Report meaningful checkpoints and an explicit finish. Session end alone preserves unfinished work for resumption or handover.

A coordination acknowledgement is a client assertion of receipt, not agreement or proof that a model understood the text. Requests survive disconnects and replay from the durable inbox. They are included in subsequent MCP calls and supported hook replies; `GET /v1/hub/standup/inbox` also supports long polling with `wait_seconds` up to 25 seconds. Reading does not acknowledge. A fresh inbox without `after` contains open requests. An explicit `after` cursor includes later changes and closures; the repository list also retains request history. The repository list includes all statuses by default; `active_only=true` limits the fixed snapshot to pending and responded requests. The dashboard sets that filter initially and offers **Include resolved and withdrawn**. Fixed-watermark continuations use `after`, `through` and `limit`; start a fresh poll without the old watermark to receive newer changes. A disconnected agent is not running merely because a request was queued. Text from another agent remains untrusted data.

### People answering from the Hub

The Hub's Inbox page lists conversations as threads. A signed-in person can answer for their **own** agent there: reply to a coordination request addressed to a Work that agent leads (propose a split, join, hand over or decline), answer a file handoff request addressed to it (hand it over, or keep it), ask another agent's holder for a file their agent wants, or close a request whose requesting agent is gone (below). These use browser-session routes with the web sign-in's CSRF token (see "People answering for their own agent" in the [Hub guide](hub.md)); a person never acts for an agent they do not own, and the Hub applies the same checks as the agent's own call (the lead, the session, the held file, the replay key and caps).

The record never presents the answer as the agent's own. A coordination response keeps the agent that leads the Work as `responder` and adds `via: "hub"`; the matching events carry `via: "hub"`. The other agent's inbox and hook lines show it at its next touch: the requester reads the response in `agentscm_coordination_inbox`, and a handoff line says the person acted "from the Hub's web page, for their agent". The answer reaches a running session only when that session next calls the Hub (its next edit hook, or MCP call); the page says so and nothing is promised sooner. An answer does not resolve the request, merge, approve or land anything: the requesting agent still makes the agreed change and resolves it. There is no withdrawal of a reply or a file request from the page.

**A request whose requesting agent is gone.** A replied request waits for its requester to make the agreed change and resolve it. If the requester's token is revoked or expires, its owner is disabled, or its repository is renamed, nobody can. The Hub reports that on the request as `requester_unavailable: true` (left out otherwise), the Inbox says "The requesting agent is gone", and the person who owns an agent that leads an affected Work can close the request from the thread. That is the same withdrawal an agent lead can call (`agentscm_coordination_withdraw`), run for the person's own agent with the same checks plus the target Work and generation the page saw: it is refused for another person's agent, a read-only token, a different session, or a requester that can still act, and it needs the current `expected_updated_seq`. The event is `coordination_withdrawn` with reason `requester_unavailable` and `"via": "hub"`. Closing does not release, pause or change any Work or reservation, and it does not say the agreed change happened. If the lead agent's own token is also gone, the page cannot close the request: the owner can release or end the target Work from its page (the request records that and stays open), or have another of their agents resume it, which makes that agent the target's lead; the page can then close the request for that agent.

## Storage maintenance

Stand-up storage has a 128 MiB retained-payload budget per provider, host, repository and stable repository ID. It counts serialized work snapshots, checkpoint requests, cached briefing pages and work-event records, with a fixed allowance for each row. Other table data, database pages, indexes and write-ahead logs require additional disk space. New writes that exceed the budget fail and roll back; reads, acknowledgements and retained exact checkpoint retries remain available.

The Hub compacts history older than seven days on its own, a minute after startup and then hourly, with the same guards as the operator command (see [hub.md](hub.md)). A local operator with direct database access can inspect and compact more recent history. This command uses the explicitly supplied database, rather than a Hub bearer token, and is not exposed through HTTP or MCP:

```sh
agentscm work compact --database sqlite:///absolute/path/hub.sqlite \
  --provider github --host github.com --repository acme/widgets \
  --repository-id 123 --before 1792278000
```

For a password-bearing PostgreSQL URL, select an existing environment variable instead of placing its value in command arguments or shell history: use `--database-env AGENTSCM_DATABASE_URL` in place of `--database`. The variable must already contain the intended database URL.

The default is a dry run. Inspect its result before repeating the command with `--apply`. The cutoff is a Unix timestamp and must be at least 24 hours old. Compaction refuses while an unexpired, unacknowledged briefing or catch-up delivery
depends on the repository’s history. It preserves current work, recent operation receipts and the historical baseline needed for retained snapshots. Later briefings explicitly report a retention gap when their acknowledgement boundary predates removed history.

What a pass older than the cutoff deletes: checkpoint operation receipts (exact retries of those operations are no longer recognised), every Work version except the newest one before the cutoff, the Work events no remaining version or receipt names, briefing deliveries idle for a day, resolved or withdrawn coordination requests with their history, and the oldest merge observations. It keeps every Work item with its title, description, scope, latest checkpoint and pull-request link. Two small tables outside the payload budget keep what `agentscm_why` and decision search need after a pass: each Work's newest 100 decision checkpoints, written when they are recorded, and which pull request each observed merge and head commit belongs to, copied when the merge is observed and bounded, like merge observations, to 100,000 rows per repository with the lowest pull request numbers removed first.

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

A sequence conflict can be classified as a definite rejection only while retained
history proves that no matching receipt was compacted. The HTTP checkpoint route
then returns `work_checkpoint_sequence_conflict` with `current_seq`; other errors
do not prove noncommit. Inspect current Work before preparing a new operation.

Checkpoint retries are supported for at least 24 hours. After a receipt has been compacted, its old version guard prevents the request from being applied again; the caller must inspect current work rather than infer whether the old operation committed. Compaction does not remove current work items, so repositories with many retained items can still reach capacity. Back up the database before maintenance and provision disk space for retained data; compaction does not promise to shrink database files immediately.

Every capacity refusal is recorded for the operator as well as returned to the refused agent (`work_storage_capacity`, with the `limit` that refused). The Hub Health page shows a banner when a limit refused a write in the last day, and the Admin page lists each repository and limit from the last 30 days with what to run: this compaction command for full stand-up storage or merge history, acknowledging catch-up deliveries, finishing Work whose change records are capped, or resolving and withdrawing open coordination requests. Shared status reads for that repository list `capacity_refused:<limit>` among their limits for a day, because refused writes never landed; it is no person's action and no badge counts it. There is no Slack message.

### Repair Work after a repository rename

Reconnect the client using the new repository name and refresh the Hub's connected repository identity first. A rename can leave retained Work under the former owner/name; changing a Git remote or reconnecting alone does not migrate that namespace. A local operator with database access can preview the repair:

```sh
agentscm work repair-repository --database-env AGENTSCM_DATABASE_URL \
  --provider github --host github.com --repository acme/previous \
  --repository-id 123 --destination acme/current
```

The destination must match the connected repository's stable ID. The command does not query the forge. Preview executes the same collision and capacity checks in a rolled-back transaction; repeat with `--apply` to commit. The destination may already contain Work. Native-key, forge-issue or active coordination fingerprint collisions abort the entire repair. PostgreSQL takes exclusive maintenance locks while evaluating the repair, including previews.

Repair preserves Work IDs, owners, lead tokens, sessions, generations, participants, relationships and historical snapshots. It updates current namespace and matching forge source names, records new version fences, moves coordination requests, retains the greatest history retention floors and recomputes payload usage. Existing name-based Work briefings and their acknowledgements for both names are invalidated: read a fresh briefing afterward. Stable-ID catch-up cursors and merge observations are preserved. A completed repair is safe to repeat.

Old-name tokens cannot perform Work operations after the connected identity changes, and their claims are projected as stale. Reconnect under the new name and use the existing explicit generation-fenced takeover flow; repair itself never transfers ownership. Unbound tokens and different stable repository IDs are not migrated. Retained publication evidence remains associated through the publishing token's stable repository ID, provider and host.

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

### Compact agent replies

Agent hosts save a tool result over a size limit to a file instead of showing it (about 25,000 tokens in Claude Code), so `agentscm_start` and `agentscm_work_overview` are compact by default and say `"detail": "compact"`:

- **`agentscm_start`** returns the claim decision and flags, alternatives, the caller's own `work.item` in full, and the conflict and preflight evidence bounded to five overlaps and ten merge observations. `context`, `briefing` and `catchup` hold counts, completeness and coverage. `catchup` keeps the fixed `event_through` and `merge_through` high-waters to pass to `agentscm_work_catchup`; each part names the read that holds the rest. The preflight keeps `issue_advice`: up to five exact-reference candidates, their count, and whether references and planning coverage are complete.
- **`agentscm_work_overview`** returns a page of at most `limit` items (10 by default): brief current Work (title, state, owner, lead, fences, branch, three scope paths and the count, next action, and reservation and lease activity), recent outcomes (decisions and ended Work, including Work ended by a transition). A completion row built from a transition also carries `ended` (`transition`, the recorded `reason`, `by` with the actor's `kind` (lead, owner or admin), `login` when known and `via` (agent or person), the event time `at`, and `superseded_by` for a supersede). The event records no agent label, so none is shown; rows from finish checkpoints have no `ended`. The overview also returns, on the first page only, up to five overlap pairs and `planned_issues` (coverage, freshness and up to five issues of each kind, with counts). `next_after` continues current Work alone and `next_recent_before` recent outcomes alone; pages are not one snapshot, and items beyond the store's bound of 100 per list stay unlisted, as `current_complete` and `recent_complete` say. The text is a one-line summary instead of a second copy of the JSON.
- **Enrichment.** On these and the briefing and catch-up reads, the appended `actionable_status` is a summary: counts, completeness, digest and at most six items that need the caller. `agent_briefing` names the cached briefing by id, digest, headline and warnings, and the text repeats that under the same untrusted-data framing as the full copy. `agentscm_work_status` and `agentscm_briefing_read` return them in full.

`"detail": "full"` returns the earlier shape: every field of every embedded read, the full status and briefing, and the overview's JSON in its text as well. The Hub's MCP overview view reads that way, so it shows what it did before. Catch-up and briefing pages, their high-waters, deliveries and acknowledgements are unchanged; nothing in a start or overview reply acknowledges them.

The instruction to prepare or reuse the return-to-work briefing (`agentscm_briefing_pending`, then prepare and submit) is appended to a start, overview or checkpoint reply, and to session-start hook context, only while a requested briefing waits for a connected agent, and never to an error.

Coordination admission is bounded to 8 open requests per token, 25 per owner and 100 per repository. Unacknowledged briefing deliveries are bounded to 8 per token and 16 per owner across all subscriber sessions; changing subscriber names does not create more quota. Retrying an existing operation remains idempotent at these limits. A current affected lead can withdraw an abandoned request only when its requester token is revoked or expired, or its owner is disabled; this records the cleanup actor and leaves all work assignments intact. A request read from the Hub carries `requester_unavailable: true` while that holds. A lead may add `target` (`work_id`, `generation`) to its withdrawal; it is then refused if that lead no longer leads that Work at that generation.

MCP exposes `agentscm_work_overview`, `agentscm_coordination_inbox`, `agentscm_coordination_ack`, `agentscm_coordination_reply`, `agentscm_coordination_resolve`, `agentscm_coordination_withdraw`, `agentscm_work_briefing`, `agentscm_work_checkpoint`, `agentscm_work_transition`, `agentscm_work_transitions`, `agentscm_work_recovery`, `agentscm_work_recovery_preview` and `agentscm_work_ack`. Read-only tokens see and can call the two recovery reads. The existing `agentscm_start` tool accepts an explicit work ID, canonical forge source or stable request key and returns its pre-start briefing with the claim result. Read-only tokens can call `agentscm_work_briefing`; the mutation tools reject them. Implicit MCP pre-start and native session-start briefings are pure previews with no delivery receipt. For deliberate acknowledgement, request a durable `agentscm_work_briefing`, retrieve every page, then acknowledge its delivery identifier.

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
| `POST /v1/hub/standup/coordination/withdraw` | Writable agent | Withdraw your proposal; a current affected lead may withdraw it only if the requester token is revoked/expired or its owner is disabled. This cleanup is audited; an optional `target` fences it to the Work and generation the lead saw. |
| `GET /v1/hub/standup/briefing` | Read-only or writable | Retrieve a bounded current-work page and ordered change page. Continuations use `after`, `work_after`, `work_done`, `through` and `limit`. |
| `GET /v1/hub/standup/work/{id}` | Read-only or writable | Inspect one authorized work item, including finished work retained for drill-down. |
| `POST /v1/hub/standup/start` | Writable agent | Create/reuse, join or generation-checked take over a work item. |
| `POST /v1/hub/standup/checkpoint` | Writable agent | Append one idempotent, generation-checked checkpoint. |
| `POST /v1/hub/standup/ack` | Writable agent | Advance a fully delivered briefing boundary by delivery identifier. |
| `POST /v1/hub/standup/work-transition` | Writable agent | Release, retain, complete, abandon, supersede or reopen one work item as its lead or its owner's agent; idempotent by `operation_id`. Release alone does not assert completion. A reopen that overlaps other work returns a result with a conflict and no receipt, like a start. A read-only token is refused before the id is looked up, in both stores. |
| `GET /v1/hub/standup/work/{id}/operational` | Read-only or writable | The operational projection of one work item: outcome, reservation, merge observation and the transitions its state allows. |
| `GET /v1/hub/standup/operational` | Read-only or writable | The projection of every reservation-holding work item, including merged Work that still holds scope, ordered by id, at most 100, with a `complete` flag. |
| `POST /v1/hub/session/work/transition` | Web sign-in with CSRF | The same transitions for a person: the owner may release, retain, complete, abandon or supersede; a Hub administrator may release, abandon or supersede. No agent token is needed. |
| `POST /v1/hub/standup/recovery` | Read-only or writable | The recovery inspection: the owner's unfinished Work page, live holds over `paths` with exact overlaps, resumable Work and typed remedies. See [Recover stuck Work](#recover-stuck-work). |
| `POST /v1/hub/standup/recovery/preview` | Read-only or writable | What one remedy would change, with its fences and apply call. Never writes. |
| `POST /v1/hub/standup/work-transitions` | Writable agent | Up to 25 transitions, each in its own transaction, with one outcome per item. |
| `POST /v1/hub/session/work/recovery`, `/recovery/preview`, `/transitions` | Web sign-in with CSRF | The same three for a person, with `repository` in the body. |

`overview`, `start` and `preflight` add the operational projection only when the request carries `operational=1`; without it the response has no new key. `start` and `preflight` ignore other unknown query parameters, as before; the overview query stays strict. Only a conflict result carries `operational`: the HTTP error bodies for a preflight refusal (`WorkPreflight`) and for a required coordination request (`WorkCoordinationRequired`) do not. MCP fills it without the flag (for `start` and `preflight`, whenever there are overlapping items), and `agentscm_work_transition` is the MCP form of the transition route. See [Work outcomes and scope reservations](design/work-lifecycle.md).

### Other owners' agents

A reply shows the caller's own agents as stored. Every other owner's agent session is shown by its `session_key` digest (16 hex characters, the same key text replies print as `session 3f2a1b9c`), never by its native id. That covers `lead_session`, participant and checkpoint `session`, captured-event `session_id`, the reservation's `holder_session`, coordination agents, PR-link authors, change-record session setups and skill-run evidence.

A token label that an older client stored as `client-<host name>-<hex>` is shown to other owners as `client-host-<hex>`. Current `client-<machine id>-<hex>` labels and names a user chose are unchanged. "Own" means the agent's owner, the same person through any token. Where a reply does not record whose agent it names, it is treated as another owner's: lapsed holds a start or checkpoint reclaims, insight usage groups, narrative facts and a hand-off answer's requester.

The digest depends only on the token and session, so fields that name one session still compare equal. Work search does not match another owner's generated `client-…` label, so it cannot confirm a guessed host name.

This applies to HTTP, MCP structured content and text, the MCP app views, briefings, catch-up, the status page, the companion's repository lists and every CLI command that prints these replies. Byte bounds on pages are computed before redaction: a digest is 16 characters, so a native session id shorter than that can make a page a few bytes longer than its bound.

Pages contain at most 100 current items and 100 events. Titles and checkpoint summaries are bounded to 500 bytes; identifiers and labels are bounded to 255 bytes. Scope, participants, relations and source references also have fixed entry limits. Malformed cursors and out-of-scope identifiers are rejected instead of silently widening access.

## Current support and limits

The read-only [work insights](work-insights.md) projection adds bounded timing, coordination
signals, usage reports and stored publication observations to retained stand-up work. Reported
completion remains separate from publication state, checks, approval and landing. Missing history
and usage remain explicit rather than being counted as zero.

| Surface | Current behavior |
| --- | --- |
| Hub storage | Durable SQLite and PostgreSQL work and directed-request records, repository-scoped authorization, atomic claims, generation fencing and bounded pages. Required schema migrations run when the store opens; a migration failure prevents the store and API from starting with partial coordination support. |
| Hub dashboard | Current Work, outcomes, coordination, overlaps and source detail; personal catch-up with explicit review acknowledgement. A task's owner, or a Hub administrator, can release, retain, complete, abandon or supersede it after a preview; the dashboard cannot start, resume or hand over Work, or acknowledge agent briefings. |
| CLI | Agent-authenticated overview, inbox, request acknowledgement/reply/resolution/withdrawal, preflight, start, checkpoint, recovery inspection and preview, transition batches, personal catch-up and agent briefing with separate explicit acknowledgements against the Hub. Work drill-down is available through the HTTP API and dashboard. |
| MCP | Agent-authenticated stand-up tools expose the same bounded records. Calls depend on the model following instructions; MCP cannot intercept arbitrary work. |
| Native hooks | A supported session-start event can read and render a bounded briefing. A safely linked end event pauses the active Work its session leads, in the transaction that releases the session's file intent; if the pause fails nothing is written, so the intent stays held and a repeat of the end completes it. Hooks do not create a goal, acknowledge a briefing, infer decisions or meaningful progress, or claim accepted/landed completion. Existing edit events continue to announce file intent and warn about overlaps. |
| Forge links | A work item can refer to an issue in its configured GitHub or GitLab repository. No automatic issue creation, update, comment or two-way mirror is performed. |
| Native workflows | Work references can describe coordination around a workflow, but checkpoints do not become workflow evidence or landing authority. |
| Qualification | Protocol and local storage behavior are tested with synthetic fixtures on SQLite and PostgreSQL only, leases on a deterministic clock. Lease renewal from each agent's tool hooks is tested with sample payloads written from the vendors' documentation, not in live sessions of every agent. Real-client, live two-machine, live-forge and PostgreSQL deployment qualification are still separate work. |

Limits of the outcome and reservation model:

- It has not been run with real harness sessions, against a live forge, on a hosted Hub or in a real two-machine lifecycle.
- `activity` is `live` or `lapsed` from the lease at request time, and `coverage` says what renews it (`tool_calls`, `tool_ends`, `edits`, `mcp_only` or `unknown`). `completeness` lists `activity_coverage` unless a hook reported the lead session's tool calls. A reservation whose lease lapsed reads `reservation.status: lapsed`, never `held`, and names nobody in `actionable`; `agentscm_work_status` counts only live holds as held. After a restart or a clock gap, `activity.lease_grace_at` says when the Hub extended the lease, and the reason says the Hub, not the lead session, did; the lead's next renewal clears it. A durable reconciliation keeps its stored `reservation_disposition` (`held` for a lapsed hold too): it is a retained evaluation of stored facts, not a request-time read. The projection decides a lapse from the stored expiry alone: in the minute after a long Hub outage, before the first clock observation grants grace, it can read `lapsed` while admission still treats the hold as live.
- Recovery previews are advice read at one moment. Their permission rules mirror the store's but do not check the participant caps (100, and 25 from other owners) or a reopen's or resume's overlap, which only the apply decides. The resolve-before-duplicate check of a start runs in MCP only; an HTTP or CLI start creates new Work as before, so read `agentscm work recovery --path` first there.
- A recovery read spans one store transaction, but the MCP start that adds `recovery` to a refused reply reads it after the refusal, so it can show a later state.
- Existing unfinished Work keeps an `implicit` reservation whose lease started when the lease migration ran, so it blocks for at most 20 minutes unless its lead session renews it. Repair of old claims is owned by #316.
- Older clients show `abandoned` and `superseded` Work as `finished`. Finished Work reported before this version has no stated outcome and reads as `unstated`.
- The plain intent announce route has no session field, so its guard is narrow and intents stay advisory: see above. A token that announces without linking a session to Work, one whose only link is finished Work, and one that invents a session name keep the legacy advice. A release or terminal transition releases the session-less intents of the participants' tokens that cover the Work's scope when the Work held a reservation; it does not know about intents of other tokens, and a takeover leaves the previous lead's session-less intents until they expire.
- A session that is eligible on other Work keeps its whole intent, including paths of the Work that ended. A session-less intent that mixes paths of the ended Work with other paths is released whole when none of the exceptions applies, and the next edit mints a new one.
- A same-lead resume of Work that is the target of a multi-target coordination request can return the request to pending when another target has replied.
- Work under a renamed repository may have no release path. That belongs to #316.
- A hook announce whose paths include a path in the scope of Work the token no longer holds is refused for those paths only: the 400 body names them (sorted, at most 32) and the `agentscm intent` hook drops them, shows the reason and announces the rest once. A refused path is dropped from the session's state, so the hook does not send it again; the edit itself still proceeds because the hook exits 0. After the agent resumes the Work, a dropped path stays unannounced until it is edited again. A token with more than 100 unfinished Work items that declare a scope fails the announce guard closed; Work with an empty scope does not count. Cleanup never fails on that bound: release, complete, abandon, supersede and a finish checkpoint check exactly whether a session is eligible on other Work, but the session-less exception reads at most 100 other Work items, so beyond that a covered advisory intent may be released and the next hook announces a new one. Any writer on the repository may join Work as a helper or reviewer, but a Work item takes at most 25 participants from owners other than its own. Other owners can therefore slow cleanup and move `updated_seq` only within that bound, and cannot fill the 100 participants that would block a resume or reopen from a new session; the owner's own sessions can, and the original session, release and abandon still work then.
- Operating-policy recovery cannot append a second finish to finished Work. An explicit reconciliation of a completed dispatch whose Work is already finished records the observation in the local binding and requires the Work's finish checkpoint to carry the dispatch's accepted commit; it writes no new checkpoint.
- Releasing or reopening Work that is bound to an operating-policy dispatch fails closed: the generation changes and recovery of that dispatch is refused with "Work authority changed". Once the dispatch's own session leads the Work again, the policy owner runs `agentscm operating-policy rebind` with the claim and Work generations to point the binding at the new generation, and recovery continues; see [recovery](advanced/operating-policy.md#pause-revoke-and-recover). Hub recovery previews do not warn about such a binding, because it lives in the operator's local journal, not the Hub.
- Taking over Work that holds a reservation needs its lease to have lapsed, for the owner's other sessions as for other owners; a revoked lead token or a stale claim alone never opens it, since token renewal revokes the old token before the lead's next call carries the lead. Work that holds nothing keeps the claim-age rule, and the owner's own tokens resume released Work in any state.
- The `merge` status is `none` only when the Hub knows its forge pull-request read reached back to the Work's creation and the merge rule applies to the Work; otherwise it is `unknown`. Merge-based discovery follows the merge-observation limits above.
- The schema migration blocks server rollback: after it applies, an older server binary is refused, and a rollback needs a backup taken before the migration.

The Hub only knows about agents and checkpoints that reach it. Offline edits, skipped MCP calls, missing end hooks and disconnected agents can leave incomplete or stale records. Preserve source work and inspect current repository state before any takeover or resume.
