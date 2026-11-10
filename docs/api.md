---
title: "Read-only HTTP API"
---

# Read-only HTTP API

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See the [README](../README.md) for status and the [changelog](../CHANGELOG.md) for known limits.

`agentscm serve` exposes a read-only JSON API and `/health`. It has no web interface (the
coordination hub's read-only status page is separate: see [the hub guide](hub.md#status-page)).
The CLI and this API are the supported surface. `GET /v1/collisions` belongs to the main path;
the revision, diff and workflow routes read the experimental native store (see
[advanced](advanced/README.md)).

`serve` binds loopback by default and refuses any other `--bind` address unless you also pass
`--public`. The API is unauthenticated, so a public bind needs an authenticating, TLS-terminating
proxy you operate.

```sh
agentscm --data-dir ./agentscm-data serve --bind 127.0.0.1:7331
curl http://127.0.0.1:7331/health
```

| Route | Returns |
|---|---|
| `GET /health` | `{"status":"ok"}`: process liveness, not a storage-integrity or readiness certificate |
| `GET /v1/revisions/{revision}` | Revision metadata |
| `GET /v1/revisions/{revision}/file?path=…` | File bytes (8 MiB maximum); `X-Agentscm-Entry-Kind: regular\|executable\|symlink` names the entry kind (a symlink's bytes are its target) |
| `GET /v1/diff?left=…&right=…&limit=1000` | Bounded diff (`limit` 1 through 10000). An entry is listed when its blob or kind differs; `before_kind`/`after_kind` (`executable` or `symlink`) appear only for a present non-regular side, so a mode-only change shows one kind field and regular-only entries are unchanged |
| `GET /v1/workflow` | Guided workflow status |
| `GET /v1/workflow/inspection?limit=4096` | The workflow subject's recorded diff. Each entry carries `before_text` or `before_base64`, and `after_text` or `after_base64`: `_text` when that side is valid UTF-8, `_base64` (standard alphabet) otherwise, and neither field for an absent side |
| `GET /v1/collisions` | Git collision report (off unless `--collisions-repo` is set) |
| `GET /v1/local/skill-workflow-library?change=…&library=…` | `skill-workflow library inspect` output for one template manifest |
| `GET /v1/local/skill-workflows?before=…&q=…` | One page of 16 recent runs, `{"runs": [...], "next_cursor": …}`. `before` is the previous `next_cursor` (a positive integer); `q` is the shared search text below and matches run id, workflow name, source revision or state. With `q` at most 256 runs below the cursor are read, so a page can be short while `next_cursor` is set; `searched_to` is then the oldest sequence examined |
| `GET /v1/local/changes?limit=50&after=…` | Current Change pointers by id, `{"changes": [{"id", "version"}], "next_after": id\|null}`. `limit` is 1 through 100; `after` is the previous `next_after`. `next_after` is null on the last page |
| `GET /v1/local/skill-workflows/{run_id}/handoff` | `skill-workflow handoff` output for one retained run |
| `GET /v1/local/workspace/timeline?limit=25&before=…` | `workspace timeline` page (off unless `--timeline-workspace` is set) |
| `GET /v1/local/workspace/timeline/{id}` | `workspace entry` view |
| `GET /v1/local/workspace/diff?left=…&right=…&limit=100&after=…` | `workspace diff` page |

The coordination hub (`agentscm hub serve`) is a separate server with its
own routes, including the agent MCP endpoint `POST /mcp` and the hook endpoints
`POST /v1/hub/hook/{agent}`; see the [hub guide](hub.md#routes) and
[agent sessions](hub.md#agent-sessions-mcp-and-hook-endpoints).

## Hub: people answering for their own agent

Browser-session routes for the Hub's Inbox page; the web sign-in cookie, and for a change its
`x-csrf-token`. Each names one of the signed-in person's own writable agent tokens; another
person's, a read-only or an unknown token is 403 `not_your_agent`. See the
[Hub guide](hub.md) for the bodies and replies.

| Route | Effect |
|---|---|
| `POST /v1/hub/session/coordination/reply` | A coordination reply for the agent that leads the target Work; recorded with `via: "hub"` |
| `POST /v1/hub/session/coordination/withdraw` | Close a request whose requesting agent is gone, for the agent that leads the target Work; recorded with `via: "hub"` |
| `POST /v1/hub/session/handoff/answer` | Hand over or keep a file asked of the person's agent |
| `POST /v1/hub/session/handoff/ask` | Ask the holder of a file to hand it to the person's agent |
| `GET /v1/hub/session/handoffs?repository=…` | The person's 50 newest file handoffs there, either side |

### Hand-off pickups

A person hands a task or a Work item to their own agent with one line, and the Hub shows when an
agent picked it up. These routes take the same web sign-in cookie and, for a POST, `x-csrf-token`;
they name no agent token, and each person sees only their own records.

| Route | Effect |
|---|---|
| `POST /v1/hub/session/pickups` | Body `{"repository", "work_id"?, "title"?}`. Records a hand-off waiting for one hour, after the same repository checks a read-token mint makes. Without `work_id` the reply's `request_key` is the key the line gives `agentscm_start`; with it, `request_key` is `null` and a start that resumes or joins that Work picks it up. `work_id` must be a `wrk_` id (400 otherwise). Each create keeps the person's newest 200 records and prunes any older than seven days. Repository names match without regard to case |
| `POST /v1/hub/session/pickups/abandon` | Body `{"repository", "id"}`. Abandons a waiting hand-off; any other one is returned unchanged. 404 when the person has no such record there |
| `GET /v1/hub/session/pickups?repository=…` | `{"pickups": [...]}`, the person's 50 newest records there, newest first |
| `GET /v1/hub/session/dismissals?repository=…` | `{"dismissals": [...]}`: the person's own dismissed and snoozed attention items there, oldest first, each `{"key", "fingerprint", "until", "at"}`. Snoozes whose `until` has passed are dropped. Requires the same repository checks a read-token mint makes |
| `PUT /v1/hub/session/dismissals` | Body `{"repository", "key", "fingerprint", "until"?}`. Records or replaces the entry for `key`: an opaque item key of 1 to 200 bytes and a state fingerprint of 1 to 128 bytes, neither with control characters. `until` (Unix seconds, within 90 days) snoozes the item; without it the item stays dismissed until the page sees a different fingerprint. Keeps the newest 200 entries per person and repository. Returns the list |
| `DELETE /v1/hub/session/dismissals?repository=…&key=…` | Removes the entry for `key` and returns the list |

Each record is `{"id", "repository", "request_key", "work_id", "title", "state", "created_at",
"expires_at", "abandoned_at", "picked_up"}`. `state` is `waiting`, `picked_up`, `expired` or
`abandoned`. `picked_up` is `null` until an `agentscm_start` (MCP or `POST /v1/hub/standup/start`)
by one of the same person's agent tokens in the same repository, before expiry, carries the
`request_key` or claims the `work_id` without a conflict; it is then `{"agent", "session",
"work_id", "at"}`: the agent token's name, the session the start named, the Work it claimed and
the Unix time. Another person's agent using the same key never picks it up. A start that the
preflight or a conflict refused does not count.

## Hub: Work recovery

The same recovery service for agents and people. A preview never writes; every apply re-checks
identity, permission, generation and `updated_seq` in its own transaction. See
[stand-up](standup.md#recover-stuck-work).

| Route | Caller | Effect |
|---|---|---|
| `POST /v1/hub/standup/recovery` | Agent or read-only bearer token | Body `RecoveryQuery` (`work_id`, `paths`, `after`, `limit`, `session`, all optional): the owner's unfinished Work page, live holds over the paths, resumable Work and typed remedies |
| `POST /v1/hub/standup/recovery/preview` | Agent or read-only bearer token | Body `work_id`, `remedy`, optional `session`, `scope` (handover) and `superseded_by` (supersede): effects, fences and the apply call |
| `POST /v1/hub/standup/work-transitions` | Writable agent token | Body `session` and up to 25 `items`; one outcome per item |
| `POST /v1/hub/session/work/recovery` | Web sign-in with CSRF | The person's inspection in `repository`: their own Work, anyone's as a Hub administrator |
| `POST /v1/hub/session/work/recovery/preview` | Web sign-in with CSRF | The person's preview; `repository`, `work_id`, `remedy`, optional `scope` and `superseded_by` |
| `POST /v1/hub/session/work/transitions` | Web sign-in with CSRF | The person's batch: `repository` and up to 25 `items` |

MCP exposes the same service as `agentscm_work_recovery`, `agentscm_work_recovery_preview` and
`agentscm_work_transitions`.

## Hub PR context

| Route | Token and effect |
|---|---|
| `GET /v1/hub/repos/{repo}/pr-context?number=…&expected_head=…` | Agent or read-only bearer token; retained PR context with explicit coverage, no provider refresh or writes |

`expected_head` is optional. MCP exposes the same service as `agentscm_pr_context`.
See [PR context](advanced/pr-context.md) for revision identity, bounds and unknowns.

| Work relationship route | Contract |
| --- | --- |
| `GET /v1/hub/standup/status` | Repository-scoped reader, read-only tokens allowed; bounded shared states, named recipients, personal actions, reservations, evidence digests and explicit coverage; advisory non-atomic reads, no generated prose required |
| `GET /v1/hub/standup/work/{id}/pr-link` | Repository-scoped reader; optional `expected_updated_seq` and `expected_head` comparisons |
| `POST /v1/hub/standup/pr-link` | Current Work lead with generation/session/sequence fencing; explicit set or `link:null` removal, idempotent operation ID |
| `POST /v1/hub/standup/issue-link` | Current native Work lead; fenced, idempotent set or explicit `link:null` removal of a caller-declared issue relationship |
| `GET /v1/hub/standup/work/{id}/reconciliation` | Repository-scoped reader, read-only tokens allowed; read-only PR evidence and prerequisite report for one Work item, `404` for Work the token cannot read; see [design](design/work-reconciliation.md) |
| `GET /v1/hub/standup/work/{id}/declarations` | Repository-scoped reader; durable v2 caller-declared PR links and prerequisites, not provider facts |
| `POST /v1/hub/standup/declarations` | Current lead; fenced, retry-safe replacement of durable declarations |
| `GET /v1/hub/standup/work/{id}/reconciliation-v2` | Repository-scoped reader; durable current versus retained historical reconciliation with explicit staleness; fresh evidence digest/source coverage remain separate even without a current retained evaluation |
| `POST /v1/hub/standup/reconciliation` | Current lead; fenced server-evidence evaluation, preserves original reports and reservation authority |

MCP exposes `agentscm_work_pr_link` and `agentscm_work_pr_link_set` through the same
Work store. The relationship records caller-reported identity, not provider approval.

### Planned issues and issue linkage

`GET /v1/hub/standup/overview` and `GET /v1/hub/standup/briefing` include `planned_issues`. Its `unclaimed`, `linked` (issue plus `work_ids`) and `linkage_unknown` buckets are distinct from Work reports. Coverage, freshness, `complete`, `linkage_complete`, `unread_pages`, `attempted_at_ms`, `facts_observed_at_ms` and `evaluated_at_ms` keep provider facts, caller links and the capture-time assessment separate. Frozen briefing replays retain that assessment. GitHub discovery is bounded to three pages/300 raw rows; facts are fresh for five minutes. The independent linkage inventory caps at 1,000 Work rows. Unsupported GitLab discovery, missing or stale evidence, outages and permission failures cannot establish an empty plan. See [planned issues](standup.md#planned-issues-and-native-work) for matching syntax, buckets and qualification limits.

`POST /v1/hub/standup/preflight` returns `issue_advice`, independently of its existing admission status and reasons; start returns the advice in its `preflight`. It contains `planning`, at most 20 exact-reference `candidates`, `references_complete` and `choice_provided`. Advice never assigns ownership or refuses admission. Native `WorkStart` accepts optional `issue_choice`: `{"kind":"link","issue":ISSUE_IDENTITY}`, `{"kind":"new_issue","title":"…"}`, `{"kind":"none_applies","reason":"…"}`, or the unassessed compatibility value `{"kind":"not_provided"}`. Omission is also unassessed. This field does not apply to a forge-source start. Naming a new issue creates no forge object.

`POST /v1/hub/standup/issue-link` accepts strict `WorkIssueLinkUpdate` JSON:

```json
{
  "work_id":"wrk_0123456789abcdef0123456789abcdef",
  "operation_id":"link-issue-42",
  "generation":1,
  "session":"lead-session",
  "expected_updated_seq":91,
  "link":{"provider":"github","host":"github.com","repository_id":101,"number":42}
}
```

`link` is required; explicit `null` removes the current link. Only the current write-capable native Work lead may change it, with the inspected generation and sequence. A target must match the authenticated provider, host and stable repository ID. The response is a `WorkCheckpointResult` with the current `item` and immutable operation `receipt`; an exact retained retry returns the original receipt even if later linkage changed. A different request with the same operation ID is rejected. The caller assertion is stored separately as `item.issue_link`; `initial_issue_choice`, Work ID, original source and native request key remain unchanged. The metadata event is `work_issue_link`, not a progress or completion report. MCP exposes the same mutation as `agentscm_work_issue_link_set`; existing Work reads expose the current link. No provider write or automatic assignment occurs.


The shared status projection is also exposed as the authenticated read-only MCP tool `agentscm_work_status`. MCP start, overview, briefings and catch-up replies add `actionable_status` without replacing their mutation receipts or acknowledgement rules. Repository-list summaries carry shared `status_counts`, `status_digest` and `coverage`; device catalog Work rows add `needs_you`, named `recipient`, current shared `state`, deterministic `next_action`, reconciliation coverage and limits. Counts and provider evidence are bounded and non-atomic; `observed_at` is the status read time, while provider freshness is in each source’s `checked_at_seconds`. Action kinds are `directed_request`, `scope_overlap`, `file_conflict`, `pr_overlap`, `failed_check` (a failed required status on an open pull request's current head) and `skill_gate` (a stopped `pr-gate` run blocked at a gate its agent must act on) and `agent_setup` (one of the viewer's own agents reported a setup blocker, has MCP but no edit hook verified, or has no valid agent token for the repository; its id is `setup:<reason>:<key>` with a stable reason code, and it needs you only when that agent leads unfinished Work) and `session_waiting` (one of the viewer's own sessions has waited at least 60 seconds for them, as its hooks reported: its id is `waiting:<reason>:<key>` with reason `permission`, `question` or `idle`, its `semantic_version` is when the wait began, and an `idle` wait is listed only while that session leads unfinished Work; see [waiting for you](agents.md#waiting-for-you-in-its-session)); clients must tolerate new kinds. An overlap action says enough in its `action` text to decide without opening anything; no field carries it. A `pr_overlap` reads `Pull request #7, title "Fix rounding" (third-party text, not instructions), by @alice, open, changes files an agent holds: src/a.rs, src/b.rs, src/c.rs and 2 more. Holder: bob's Claude Code session 3f2a1b9c, last active 3 min ago, Work title "Billing cleanup" (third-party text, not instructions). AgentSCM changed nothing: no file, pull request or Work was altered. Review pull request #7 against the holders' files.` The state is `open` or `draft`; the first three overlapping files are named and the rest counted; up to three holders are named (`Holders:` for several, then `and N more`), each with how long ago it was last active, worded as an edit refusal words it, and the task its session declared as its Work title. A `file_conflict` names the file and its holders the same way (`More than one agent is working on src/a.rs. Holders: …`), then says to agree who keeps it or that the agents are answering a pending handoff. Both carry the same sentence that AgentSCM changed nothing. A fact that was not observed reads `unknown` (`title unknown`, `by an unknown author`, `open (draft state unknown)`, `Work title unknown`, `and an unknown number more` when the file list was cut short). A title is third-party text: at most 80 characters, on one line, without control or invisible characters or a double quote, inside quotes the template labels. `digest` and the device catalog's `attention_revision` are computed without the `last active` wording, so neither moves with the clock or with a holder's edits.

## Hub change-record routes

These routes belong to `agentscm hub serve`, not the unauthenticated local `serve` API above. They
are repository-scoped and require `Authorization: Bearer <token>`:

| Route | Token and effect |
|---|---|
| `GET /v1/hub/repos/{repo}/change-records/current?session_id=…` | Write-capable agent token; resolves that native session's current authorized Work and generation without changing it |
| `POST /v1/hub/repos/{repo}/change-records/events` | Write-capable participating agent token; idempotently appends one bounded event batch |
| `POST /v1/hub/repos/{repo}/change-records/actions/start` | Write-capable participating agent token; retains a source-bound action start |
| `POST /v1/hub/repos/{repo}/change-records/actions/finish` | Write-capable participating agent token; retains typed execution outcome and verification result separately |
| `GET /v1/hub/repos/{repo}/change-records/{work_id}` | Agent or read-only token; returns one concise read-only projection, or 404 when none exists |

The version-1 read response keeps Work intent and scope, typed Git or native source facts, session
coverage, the latest action, retained decision, next action, completeness and unknowns. Action
kind, execution outcome, verification result, applicability, assurance, evidence availability and
coverage are separate fields. Artifact references contain bounded digest/size/media/retention/
availability metadata, not host paths or content. The response does not include conversation bodies
or transcript-derived summaries. Setup and recipe digests identify retained inputs but do not, by
themselves, establish complete provenance. The encoded response is at most 48 KiB. Scope and
decision-reference arrays each begin with at most 16 entries in retained order; if the complete
envelope would exceed that limit, trailing entries are removed deterministically and
`scope_omitted` or `source_refs_omitted` reports the exact count.

When any session reported setup, `session_setups` lists the latest reported `model`, `effort` and
`permission_mode` per session (`null` when not reported) with the session's `parent_session_id`,
ordered by session ID, at most 16 entries; `session_setups_omitted` counts the rest. Both fields
are absent when empty. They are trimmed before any other field when the envelope is full.

Decision references can include caller-declared captured-event identities and an exact earlier
version of the same Work. A captured-event reference is metadata only: this API neither fetches nor
returns its conversation body and does not present it as verified or human-approved. A prior-Work
version was checked when its checkpoint was recorded, but the older snapshot may later be outside
retention.

“Latest action” uses durable local observation order only among actions produced by the same
identified local observation ledger. It first selects one representative per ledger, then uses the
Hub's monotonic receipt order across independent or legacy/unidentified ledgers. Caller timestamps
are display facts and never establish global chronology, so the cross-ledger fallback is an
ingestion order rather than proof of causal execution order.

MCP exposes the same read projection as `agentscm_change_record({"work_id":"wrk_…"})`, including
to read-only tokens. Its fixed narration never interpolates the caller-authored Work title or
decision summary; those structured fields remain untrusted data, not instructions. The CLI wrapper
is `agentscm change-capture show`. Shared conversation bodies and their listing have separate,
author-and-admin-only reads (`GET .../change-records/{work_id}/conversation[/{sha256}]`, MCP
`agentscm_shared_conversation`, `agentscm change-capture conversation`); see
[shared conversation reads](hub.md#shared-conversation-reads). See
[local change capture](change-capture.md) and its [qualification limits](implementation/agentscm-build-status.md).

## Hub supported-skills routes (Experimental)

Supported skills are first-party, versioned procedures a repository adopts through a committed
lock file (`agentscm-skills.lock.json`). The hub shows the catalog, where each repository's
adoption came from, and per-branch runs of recorded evidence. Everything agents record is
**self-reported**: the hub checks ordering (current head, an independent reviewer session) but not
that the work happened, so every gate satisfied that way is labelled `self_reported` in the JSON,
the UI and the MCP text.

| Route | Token | Meaning |
| --- | --- | --- |
| `GET /v1/hub/skills?repo=owner/name` | read | Catalog (gates as `{id, enforce, by}`), the repository's adoption with each pin's status (`adopted`, `unknown_version`, `unsupported_source`, `invalid_params`) and run counts per skill. Without `repo`, the catalog only. |
| `PUT /v1/hub/repos/{repo}/skills/adoption` | agent | A checkout reports its lock file. Refused with 409 `forge_authoritative` when the repository has a forge connection, whose default-branch lock is then the only adoption. Otherwise stored as `reported_by_checkout` unless the hub already read the lock from the forge's default branch, which wins; the reply says which `source` applies and whether this report `applied`. |
| `GET /v1/hub/repos/{repo}/skills/runs?state=open\|completed\|all&limit=` | read | `{runs, truncated}`; limit at most 100 (default 50, newest 100 runs scanned). |
| `GET /v1/hub/repos/{repo}/skills/runs/by-branch?branch=` | read | One run, or 404 `not_found`. |
| `GET /v1/hub/repos/{repo}/skills/runs/{run_id}` | read | One run. |
| `POST /v1/hub/repos/{repo}/skills/evidence` | agent | Records evidence (`skill`, `gate`, `kind`, `head_sha`, `assurance: "self_reported"`, `summary`, `branch`, optional `url`, `pull_request`, `detail`, `session`) and returns the run. |

`{repo}` is one URL-encoded `owner/name` segment. A run is keyed by repository, branch and skill and
keeps at most 256 evidence records. Refusals are 409 with `{error, message}`: `not_adopted` (the
skill is not an `adopted` pin), `stale_head`, `not_independent`, `unknown_gate` and `run_full`;
malformed evidence is 400. Adoption is read from `agentscm-skills.lock.json` on the default branch of
a connected repository about every five minutes (GitHub only reads are exercised by tests; GitLab
reads the file through its raw-file API and accepts JSON files only). A missing or invalid file there
clears the forge adoption.

MCP tools: `agentscm_skill_list {repo}` (catalog with when to follow each skill, prerequisites,
what it is not for, effective parameters and adoption source), `agentscm_skill_status {repo, branch}`
(one line per gate, `[self-reported]` after gates satisfied by agent-recorded evidence, the next
action and whether finishing is blocked) and `agentscm_skill_evidence {repo, skill, gate, kind,
head_sha, summary, branch, url?, pull_request?, detail?, session?}`. The first two are available to
read-only tokens; recording needs an agent token. Each returns structured JSON beside the text.

## Local template and timeline reads

The `/v1/local` routes return exactly the JSON that the matching CLI command prints, without the
CLI's `ok` envelope, so a local status page can show
[reusable workflow templates](advanced/reusable-workflows.md) and the
[workspace snapshot timeline](workspace-timeline.md). They are read-only: only GET is routed (other
methods get 405). Capture, fork, configure, check, resume and cancel stay in the CLI and the local
stdio MCP servers. The Hub has no copy of this local data and serves none of these routes.

No request can choose a data directory or host path. The library route takes a Change ID and a
native manifest path, read at that Change's current revision. The timeline routes read the one
native workspace that the operator names at startup (Unix only), and return 404 without the flag:

```sh
agentscm --data-dir ./agentscm-data serve --timeline-workspace ./work
```

Bounds are the CLI's. Timeline `limit` is 1 through 100 (default 25) and `before` is a positive
entry ID taken from `next_before`. Diff `limit` is 1 through 100 (default 100) and `after` is the
opaque `next` cursor. Unknown or repeated query fields return 400. A timeline response is limited
to 1 MiB and any `/v1/local` response to 4 MiB; a larger one returns 400.

Read the completeness signals before showing a result as complete:

- `next_before` (timeline) or `next` (diff) is present while more pages exist. A diff page can be
  empty and still carry `next`.
- A non-empty `needed_objects` means the comparison could not read a local source object. That page
  is incomplete, not "no changes".
- `coverage.includes_pre_feature_history` is always false. The timeline never covers activity
  before capture began.
- `admission` is always `not_assessed`. Neither inspect nor handoff is an approval.

### Browser access

When `AGENTSCM_SERVE_READ_TOKEN` is set at startup (at least 32 bytes), every `/v1/local` request
needs `Authorization: Bearer TOKEN`. Otherwise the request gets 401 with
`WWW-Authenticate: Bearer`. Without the variable, these routes are unauthenticated like the rest of
this API. The token is a separate local secret, not a Hub token. A page that reaches `serve`
through a same-origin development proxy needs no CORS. A page on another origin needs the operator
to allow that exact origin:

```sh
AGENTSCM_SERVE_READ_TOKEN="$(cat token-file)" agentscm --data-dir ./agentscm-data serve \
  --timeline-workspace ./work --browser-origin http://127.0.0.1:7332
```

`--browser-origin` is repeatable and requires the token. For a listed origin, `/v1/local` replies
carry `Access-Control-Allow-Origin` for GET with the `authorization` header, without credentials,
and an `OPTIONS` preflight gets 204. A preflight from any other origin gets 403 and no CORS headers,
so the browser cannot read the reply. The other routes of this API never send CORS headers.

Every `/v1/local` request must carry a loopback `Host` (`127.0.0.1`, `localhost` or `[::1]`, any
port), or it gets 421. This stops a page whose DNS name was rebound to the loopback address from
reading these routes as its own origin, with or without a token. A proxy in front of a `--public`
bind must forward a loopback `Host`.

Except for the repository explorer's literal source search, list and search parameters share these rules on the Hub and on `serve`. `limit` is an integer in the
endpoint's range, otherwise 400. `q` is trimmed; empty means no filter; more than 200 bytes or any
control character is a 400. Matching is case-insensitive substring, and `%`, `_` and `\` in `q` are
literal. Case-insensitivity is ASCII-only on SQLite (a non-ASCII letter matches at its own case); PostgreSQL
folds by its database locale. Where a record has a number, `q` of `123` or `#123` also matches it exactly. A cursor is the
value the previous page returned (`next_cursor`, `next_before` on activity, `next_after` on local
changes) and `q` applies on every page. Cursors that name a record are checked: publications
`before` must be a publication of the repository, and work search `cursor` must name a work item of
the token's repository with exactly its creation time; otherwise, and for any malformed cursor, the
reply is a 400, never an empty page. Cursors that are plain positions are not checked against a row:
activity `before` (an event `seq`), skill-workflow `before` (a run sequence) and local changes
`after` (a Change id) return the rows after that position, which may be none. Cursors are not
signed.

Its repository-authenticated stand-up routes also include `GET /v1/hub/standup/work?q=…&limit=20&cursor=…`,
which finds work by text, finished work included, in creation order, newest first (limit 1 through 50;
`cursor` is the previous `next_cursor`, `<created_at>:<id>`, built from the immutable creation time,
so an update between pages cannot repeat or skip an item; treat it as opaque). It matches title, branch, owner login,
work id, scope paths, the source issue number and the lead agent label, except another owner's generated `client-…` label, which is masked as described in [other owners' agents](standup.md#other-owners-agents). The reply is `{"items": [{id, title, state,
lead_agent, owner, branch, source_issue, updated_at, created_at}], "next_cursor": cursor|null, "complete": bool}`.
Read-only tokens may call it. Items of `GET /v1/hub/repos/{repo}/publications` also carry the manifest `title`. It also includes `POST /v1/hub/standup/usage`,
`GET /v1/hub/standup/insights` and `GET /v1/hub/standup/work/{work_id}/insights`.
The first stores one idempotent partial usage report; the GET routes return bounded
work projections. See [work insights](work-insights.md) for pagination and evidence limits.

`POST /v1/hub/standup/checkpoint` returns HTTP 409 with
`{"error":"work_checkpoint_sequence_conflict","current_seq":N}` only when the
expected Work sequence is stale and retained history proves that the exact
operation did not commit. It checks exact operation replay and current lead
identity/generation first. Receipt compaction can make a missing operation
ambiguous; that case remains a generic HTTP 400 `invalid_request`, which must not be used
as proof of noncommit. Inspect current Work before constructing any new checkpoint.

Completion calls on `/mcp`, `/v1/hub/hook/{agent}` and `/v1/hub/standup/checkpoint`
accept the optional `x-agentscm-completion-reporting: true` header. Exactly one literal `true`
enables a best-effort, idempotent completion observation; the default is off. It adds no response
fields and does not alter completion success. See [completion reporting](work-insights.md#optional-completion-reporting)
for the local toggle and the distinction between completion metadata and measured usage.

## Local repository explorer

`GET /v1/local/explorer/{summary,tree,file,history,diff,search,evidence,conversation}` reads one
operator-selected Git checkout and optional private capture spool. See the
[repository explorer guide](repository-explorer.md#read-api) for query fields, source identity,
paging and availability semantics. All responses are versioned and bounded to 4 MiB; the total
query string is bounded to 4 KiB. Tree/diff pages allow at most 200 entries; history, search,
evidence and conversation pages allow at most 100.

Explorer startup always requires a local read token and loopback bind. Every read uses the existing
local token/Host guard. A browser `Origin` must match an explicitly configured `--browser-origin`;
an unapproved origin is rejected before reading source or capture, including on a GET. The local
read secret is separate from the Hub credential. These routes never resolve caller-supplied
filesystem roots or arbitrary artifact identities and never expose private conversation through
the Hub.

## Guided workflow observations

The status route accepts no query fields. Inspection accepts only one `limit`
from 1 through 4096; its default is 4096. No HTTP request can select a data
directory, workspace or mirror path. The server supplies the configured data
directory and its paired Engine. These GETs do not create a workflow journal,
retry pending operations, open an old workspace, approve, land or return an
actionable approval token. The working tree is not opened or attested and
admission is not assessed. Completeness is a source-inspection result, not a
claim that a workspace is clean or an approval is current.

The guided journal is bounded to 256 KiB. Inspection retains the CLI's bounds:
4096 changed entries, 128 Engine scan pages, 256 KiB combined before/after bytes
per file, 2 MiB encoded diff entries and 4 MiB total service view.

A successful source-path check establishes declared immutable-file integrity,
not command execution or model review. Operator notes remain unverified claims.
Historical landing, local Git export, remote branch push, PR/MR merge and incoming
reconciliation are distinct. The view does not observe GitHub/GitLab connection
state or establish continuous synchronization.

## Who's touching what

Collision warnings are off by default. To turn them on, name one trusted Git
checkout when starting the service:

```sh
agentscm --data-dir ./agentscm-data serve --collisions-repo /path/to/checkout
curl http://127.0.0.1:7331/v1/collisions
```

`GET /v1/collisions` runs the same read as
`agentscm collisions --git-dir CHECKOUT --worktrees` with `/usr/bin/git`, the
default base (`origin/HEAD`, then `main`) and every branch active in the last 14 days, and returns
the `agentscm-git-collisions/v1` report itself, without the CLI's `ok` envelope.
The route accepts no query fields, so no HTTP request can choose a repository,
ref or base. Without the flag it returns 404.

Each request runs `git status` in every worktree and `git merge-tree` for
overlapping branches. It holds the service's shared work slot, so
`--max-concurrency` still bounds memory; with the default of 1, other reads get
503 until a scan finishes. A scan stops after 60 seconds or before its JSON would
exceed 4 MiB and returns what it found so far, with `truncated` set (see
[collisions](collisions.md)); a truncated report is not "nothing more found".
A checkout without `origin/HEAD` or `main`, or a missing or older-than-2.43 Git,
returns 503 with the detail in the service log; use `agentscm collisions` for
the full, untruncated report. `merge-tree` writes unreferenced objects into the repository (`git gc`
removes them). Repository-configured clean filters and merge drivers run as in a
shell: anyone who can change that repository's Git config, including agents
working in its worktrees, can make these reads run commands as the service's
user. Only configure a repository whose writers you trust that far. See
[collisions](collisions.md) for the profile and its limits. The result is a
warning, never a lock or a native record.

## Workspace attached-candidate publication

With an operator-pinned `attached_publication_profiles` entry, the authenticated workspace Hub exposes `POST /v1/hub/workspace/repositories/{repository}/profiles/{profile}/attachments/{attachment}/publication`. Preparation accepts only `request_id`, `expected_generation`, an exact `RunSubject`, and `CommitMetadata`. `POST .../publication/status` accepts `request_id` and reads retained preparation without asserting current qualification.

`POST .../publication/branch` accepts `request_id`, `prepare_request_id`, `candidate_id`, `expected_generation`, and optional `previous_branch_request_id`. It publishes the retained candidate through the pinned Mirror transport under fresh authorization. `POST .../publication/branch/recover` accepts `request_id`, `prepare_request_id`, and `candidate_id`; it observes a retained attempt without pushing again.

`POST .../publication/forge` accepts `request_id`, `prepare_request_id`, `branch_request_id`, `candidate_id`, `expected_generation`, `title`, `body`, and optional `previous_publication`. The Hub constructs the manifest from the retained candidate and confirmed branch receipt, atomically stores a separate trusted authorization binding, then uses the existing GitHub PR/GitLab MR publisher. No request can choose a path, endpoint, credential, ref or branch preimage. The endpoint must match the connected forge host and repository; a bare local endpoint cannot establish this mapping. `POST .../publication/forge/recover` accepts only `publication_id` and observes an existing attempt. An absent search result after durable `pr_attempting` never authorizes another create. Valid owners can recover historical attempts after the subject becomes stale; recovery grants no new write authority.

The sibling `/publication/mcp` exposes `agentscm_prepare_attached_publication`, `agentscm_attached_publication_status`, `agentscm_publish_attached_branch`, `agentscm_recover_attached_branch`, `agentscm_publish_attached_forge`, and `agentscm_recover_attached_forge`, with the same strict input schemas. Current Contributor membership, writable repository-scoped agent credentials, configured ownership, executor registrations, all imported-origin grants, attachment/source identity and exact native requirements are rechecked at effect boundaries. The trusted binding retains actor digests and configuration digests, not bearer tokens, credential contents or live authorization proof. Bound publications and their unbound successors cannot use legacy run/recover to bypass this guard.

Preparation, branch, forge, both recovery routes and `/publication/mcp` authenticate first, then require the request body within the Hub's 30-second request bound, but may process for up to 120 seconds, because one operation can run several Git transport commands. Each operation runs as its own task, so it keeps running past that bound: the route then returns `202` with `{"state":"still_running","follow_up":...}` naming `status`, `branch/recover` or the same route to repeat with the same request IDs, and `/publication/mcp` returns a JSON-RPC error naming the matching status or recovery tool. `POST .../publication/status` keeps the 30-second bound. On every Hub route a client must send its whole request head within 30 seconds.

Opt-in forge status uses current imported-origin authorization only for server-bound publications; unbound publications retain native-only qualification. Success additionally requires the exact provider repository, head, base baseline, tree and parents. Definite revocation or stale source is nonpassing; unavailable authorization is unknown/error. A failed downgrade retains the possibility that a success remains visible. Dispatch authorization is not an atomic provider transaction: a revocation during a dispatched write may leave an observable effect, and subsequent effects stop without claiming rollback. Neither agent publication nor native approval performs provider review or merge. Synthetic qualification does not establish live provider protection enforcement, SSH, cold restore or UI support.

## Hub publication forge check

This is the Hub, not `serve`. Agent and read-only tokens can read publications; see
[publishing](hub.md#publishing-a-revision) for the list route. `GET /v1/hub/repos/{repo}/publications/{id}` adds a
read-only `forge_check` object when the publication is a candidate with a pull request.
It posts and changes nothing. `mode` is always `git_default`. `provider` is `github`, `gitlab`
(fake-tested only) or `unsupported`. `enabled` is false unless an agent-task profile for the target
sets `publish_forge_status`; then `context`, `state` (the journal's last status state),
`head_sha` and `posting` (`posted`; `uncertain` while a post's reply is unrecorded; or
`refused` after a definite forge refusal) are added, and are null before the first post. `protection` is always `unverified`
and `note` says branch protection, raw pushes and administrator bypass are not
enforced or verified by AgentSCM. A GitLab status on the MR head is not a verified merged-result
pipeline. See [workflow status](hub.md#workflow-status-for-candidate-pull-requests).

On GitHub with the flag on, `forge_check.merge_queue` is added: `readiness` (null until the
first poll after a start; otherwise `target`, `delivery` — `subscribed`, `permission_missing`,
`event_not_subscribed`, `personal_token_unsupported` or `unknown` — `undelivered`, the pull
request numbers of current queue branches no `merge_group` delivery named, or null when the
listing was refused or truncated, and `checked_at`) and `groups`, at most 10 merge queue groups
that queued this pull request, newest first: `kind` (`merge_group`), `context`, `group_sha`,
`base_ref`, `base_sha`, `pr_number`, `pr_head_sha`, `stacked_on`, `lifecycle` (`current` or
`ended`), `ended_reason`, `outcome` (`pending`, `landed`, `merged_reported`, `not_landed` or
`unknown`), `state`, `description`, `binding_digest`, `posting` (`pending` before the first
post, `uncertain`, `posted` or `refused`), `ruleset_requirement`, `status_source`,
`observed_at` and `updated_at`. See [GitHub merge queues](advanced/github-merge-queue.md).

## Hub publication external outcomes

This is the Hub, not `serve`. When the repository is listed in `AGENTSCM_GITHUB_EXTERNAL_RUNS`
and the publication has a merge commit, `GET /v1/hub/repos/{repo}/publications/{id}` adds a
read-only `external` object; otherwise the reply has no such field. It reads the Hub's store
and calls no forge. Fields: `provider` (`github`), `note` (observed after landing, not
evidence for the landing decision), `merge_commit`, `state`, `reconciled_at` (unix seconds of
the last read, or null), `ci` (`run_id`, `attempt`, `branch`, `name`, `outcome`,
`provider_updated_at`, `html_url`) and `deployments` (`run_id`, `environment`, `name`,
`outcome`, `provider_updated_at`, `html_url`). `outcome` is `pending`, `running`,
`succeeded`, `failed`, `cancelled`, `superseded` or `unknown`. `state` is `unobserved` (no
read yet), `incomplete` (the read was cut short, or more than 200 rows of a kind exist) or
`observed`. Under `incomplete` the lists may be non-empty but are partial, and under
`unobserved` they are empty; an empty list is not success unless `state` is `observed`.
`reconciled_at` is the staleness signal; a later failed read leaves the previous value.
`html_url` may be null. At most 200 rows of each kind are shown. Names and links come from the provider and are untrusted.
These outcomes are separate from source approval and from `forge_check`. See
[external CI and deployment outcomes](hub.md#external-ci-and-deployment-outcomes).

## Hub: requests and replies across versions

The Hub and the programs that call it are upgraded separately: the `agentscm` commands, the menu bar app, and the hook and MCP clients on each computer. Either side can be the older one. Two rules keep them working together.

**The Hub refuses what it does not know.** A request with a field the Hub does not have, or with a value outside the set a field takes, gets `400` with `invalid_request` and changes nothing. The Hub never ignores part of a request, because the ignored part could be the part that mattered. A client must not send a field to a Hub that does not have it yet.

**A client ignores what it does not know.** `agentscm` reads a reply that has a field it does not know as if the field were absent. A Hub release can add a field to any JSON reply without a new route or a new schema version.

A later Hub can also add a value to a reply field that takes one of a set of values, such as a Work `state`. A client never reads an unknown value as a known one:

| Client | With a value it does not know |
|---|---|
| A command that prints the reply as JSON (`--json`, `work start`, `work transitions`, `promise`) | Prints the reply as the Hub sent it. `work start` also exits non-zero when `item.state` is unknown, because the claim is not confirmed |
| `work status`, `work recovery` and `skill status` as text | Print the reply as JSON, and on standard error name the field and the value |
| The menu bar app | Shows the Hub's own text for an unknown state, claim status or recipient kind. Its counts do not change |
| A client that acts on the whole reply (managed Work, change capture, agent tasks, the hosted workflow client) | Stops with an error that names the field and the value |

If you write your own client, do the same: ignore a field you do not know, and treat a value you do not know as unknown.

This applies to `agentscm` from the release that adds this section. An `agentscm` installed before it reads replies strictly, and `work status`, `work start` and `work recovery` fail there when a reply has a field they do not know.

## Hub errors

Every error the Hub returns carries the same fields on every surface: HTTP bodies, MCP tool
results, hook notes and the `agentscm` CLI. `error` stays the stable machine code, as before; the
other fields are additive, so a client that reads only `error` keeps working.

| Field | Meaning |
| --- | --- |
| `error` | The stable machine code (MCP: `structuredContent.error.code`, also `kind`). Codes are never renamed or removed. |
| `message` | One sentence saying what happened and what to do. Hub-authored text only. |
| `retryable` | `true` only if the same request can succeed unchanged later. Retry a mutation with the same idempotency key or operation id. |
| `retry_after_seconds` | Present only when `retryable` and the Hub knows a wait. |
| `recovery` | `{action, command?}`. `action` is one of `retry`, `wait`, `sign_in`, `renew_credential`, `reconnect`, `provide_field`, `start_session`, `inspect_work`, `ask_owner`, `update_client`, `contact_operator`, `none`. `command` is a fixed command such as `agentscm client renew`, never built from caller text. |
| `fields` | For input problems: `{name, problem, hint}` where `name` is a request field name and `problem` is `missing`, `unknown`, `wrong_type`, `invalid`, `too_long` or `conflict`. A value the caller sent is never echoed. |
| `correlation_id` | `cor_` plus 32 hex characters, generated by the Hub for this failure, also sent as the `x-agentscm-correlation-id` response header. The Hub's single diagnostic log line for the failure carries the same `ref`, so an operator can find the server-side cause. It is never taken from the caller and never derived from a token, session or user. If the operating system cannot supply entropy the field is omitted. |

Per-code keys that already existed (`current_seq` on `work_checkpoint_sequence_conflict`, the
`overlaps`, `preflight` and `coordination_request` payloads, `limit` on `work_storage_capacity`)
stay where they were. An untyped internal `Invalid` reason is never forwarded over HTTP: the
caller gets `invalid_request` with `fields` when a request field can be named, and the cause goes
to the Hub log with the reference. Session, lead-session and lease refusals are typed
(`session_required`, `lead_session_mismatch`, `lease_lapsed`) and keep their messages. A caller with
no valid token always gets the same `unauthorized` body, and an unknown resource and one the caller
cannot see both answer `not_found`; another owner's session, Work or token never appears in an
error (the viewer rules in [session privacy](hub.md) are applied before the error is built).

**Hooks** stay fail-open: the edit is not blocked, and the note reads
`AgentSCM: coordination unknown (<reason>; <recovery>; ref cor_...)`. Unseen coordination is not
protection. A failed `SessionStart` (start, resume or replay) always shows its note; repeats of the
same failure for ordinary events are shown once per token, session and code. When the Hub is
unreachable the note says so and carries no reference, because none exists. Hooks that Claude
Code, Cursor or others run as permission-only or lifecycle events have no channel for a note; the
reference still travels in the `x-agentscm-correlation-id` header of the hook response.

**Clients** read the same fields with one tolerant decoder. An older Hub's `{"error":"..."}` gets a
fallback message and recovery from the registry. An unknown future code is kept as an opaque
`[a-z0-9_]{1,64}` string, `retryable` follows the HTTP status class (never guessed `true`), and
remote text is bounded and stripped of control characters, terminal escapes and anything shaped
like a credential before it is shown. The workspace task MCP keeps its own
deliberately terse refusals and does not attach a reference.

Registered codes:

| Code | HTTP | Retryable | Recovery action |
| --- | ---: | --- | --- |
| `invalid_request` | 400 | no | `provide_field` |
| `not_found` | 404 | no | `none` |
| `read_only` | 403 | no | `ask_owner` |
| `forbidden` | 403 | no | `ask_owner` |
| `gone` | 410 | no | `inspect_work` |
| `unauthorized` | 401 | no | `sign_in` |
| `wrong_repository` | 403 | no | `reconnect` |
| `busy` | 503 | yes | `retry` |
| `unavailable` | 503 | yes | `retry` |
| `work_storage_capacity` | 503 | no | `contact_operator` |
| `work_scope_overlap` | 409 | no | `inspect_work` |
| `work_coordination_required` | 409 | no | `inspect_work` |
| `work_overlap_incomplete` | 503 | yes | `retry` |
| `work_preflight_required` | 409 | no | `inspect_work` |
| `work_checkpoint_sequence_conflict` | 409 | no | `inspect_work` |
| `session_required` | 400 | no | `start_session` |
| `lead_session_mismatch` | 400 | no | `inspect_work` |
| `lease_lapsed` | 400 | no | `inspect_work` |
| `not_your_agent` | 403 | no | `ask_owner` |
| `not_your_work` | 403 | no | `ask_owner` |
| `login_failed` | 400 | no | `sign_in` |
| `device_flow_unavailable` | 409 | no | `sign_in` |
| `csrf` | 403 | no | `sign_in` |
| `insufficient_role` | 403 | no | `ask_owner` |
| `login_denied` | 403 | no | `sign_in` |
| `not_allowed` | 403 | no | `ask_owner` |
| `not_connected` | 403 | no | `ask_owner` |
| `user_disabled` | 403 | no | `ask_owner` |
| `too_many_logins` | 429 | yes | `wait` |
| `device_revoked` | 403 | no | `sign_in` |
| `promise_conflict` | 409 | no | `inspect_work` |
| `too_many_publications` | 429 | no | `contact_operator` |
| `already_connected` | 409 | no | `none` |
| `not_configured` | 404 | no | `contact_operator` |
| `work_not_found` | 404 | no | `inspect_work` |
| `generation_conflict` | 409 | no | `inspect_work` |
| `idempotency_conflict` | 409 | no | `inspect_work` |
| `last_administrator` | 409 | no | `ask_owner` |
| `member_limit` | 409 | no | `contact_operator` |
| `native_subject_conflict` | 409 | no | `inspect_work` |
| `csrf_required` | 403 | no | `sign_in` |
| `execution_not_configured` | 503 | no | `contact_operator` |
| `expected_json` | 415 | no | `provide_field` |
| `expected_source_bundle` | 415 | no | `provide_field` |
| `source_body_too_large` | 413 | no | `provide_field` |
| `source_body_invalid` | 413 | no | `provide_field` |
| `source_body_timeout` | 408 | yes | `retry` |
| `source_response_invalid` | 500 | no | `contact_operator` |
| `integrity_failure` | 424 | no | `contact_operator` |
| `retry_upload` | 503 | yes | `retry` |
| `no_repository_role` | 403 | no | `ask_owner` |
| `access_check_unavailable` | 503 | yes | `wait` |
| `permission_unavailable` | 503 | yes | `wait` |
| `hub_unreachable` | 503 | yes | `reconnect` |
| `invalid_response` | 502 | no | `update_client` |
| `sign_in_required` | 401 | no | `sign_in` |
| `invalid_or_changed` | 400 | no | `inspect_work` |
| `not_adopted` | 409 | no | `ask_owner` |
| `run_full` | 409 | no | `contact_operator` |
| `unknown_gate` | 409 | no | `provide_field` |
| `stale_head` | 409 | no | `inspect_work` |
| `not_independent` | 409 | no | `ask_owner` |
| `forge_authoritative` | 409 | no | `ask_owner` |
| `method_not_allowed` | 405 | no | `provide_field` |
| `request_too_large` | 413 | no | `provide_field` |
| `request_timeout` | 408 | yes | `retry` |
| `rate_limited` | 429 | yes | `wait` |
| `context_too_large` | 503 | no | `contact_operator` |
| `invalid_params` | 400 | no | `provide_field` |
| `encoding_failed` | 500 | no | `contact_operator` |
| `unsupported_provider` | 409 | no | `update_client` |
| `outbound_disabled` | 409 | no | `ask_owner` |
| `candidate_endpoint_mismatch` | 409 | no | `provide_field` |
| `ambiguous_repository_name` | 409 | no | `provide_field` |
| `candidate_default_unavailable` | 409 | yes | `retry` |
| `unsupported_candidate_default` | 409 | no | `provide_field` |
| `candidate_output_is_default_or_alias` | 409 | no | `provide_field` |
| `publication_conflict` | 409 | no | `inspect_work` |
| `workspace_publication_required` | 409 | no | `update_client` |
| `blobs_missing` | 409 | no | `provide_field` |
| `candidate_required` | 409 | no | `provide_field` |
| `invalid_request_id` | 400 | no | `provide_field` |
| `unknown_repository` | 404 | no | `none` |
| `invalid_expiry` | 400 | no | `provide_field` |
| `enrollment_invalid` | 400 | no | `provide_field` |
| `unknown_device` | 404 | no | `none` |
| `device_exists` | 409 | no | `inspect_work` |
| `stale_generation` | 409 | no | `inspect_work` |
| `request_conflict` | 409 | no | `inspect_work` |
| `sharing_off` | 403 | no | `ask_owner` |
| `duplicate_key` | 409 | no | `provide_field` |
| `owner_role_missing` | 409 | no | `ask_owner` |
| `role_check_unavailable` | 503 | yes | `retry` |
| `unknown_owner` | 404 | no | `ask_owner` |
| `owner_disabled` | 409 | no | `ask_owner` |
| `owner_wrong_forge` | 409 | no | `provide_field` |
| `repository_renamed` | 409 | no | `inspect_work` |


## HTTP boundaries

Workflow, collision and `/v1/local` responses carry `nosniff`, no referrer, `no-store` and a
restrictive content-security policy; the revision, file, diff and `/health`
responses do not. Every read takes one of the `--max-concurrency` work
slots and keeps it through response transmission; cancellation does not release
it while native work is still running. A busy service returns 503. Malformed or
over-limit input returns 400, missing records 404 and conflicts 409. Workflow,
collision and `/v1/local` reads that fail unexpectedly return a sanitized 500 with the detail in
the local service log.

`serve` opens the source engine and initializes a missing data directory, but
it does not create or migrate the authority database (`state.sqlite`); run
a CLI command that opens it after upgrading, or `/v1/workflow` returns an
error. The routes do not establish a read-only mount or hostile-host isolation guarantee.
Authenticated sessions, approval and landing over HTTP remain unimplemented.
