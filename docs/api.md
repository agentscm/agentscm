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
| `GET /v1/workflow/inspection?limit=4096` | Recorded before/after bytes of the workflow's subject |
| `GET /v1/collisions` | Git collision report (off unless `--collisions-repo` is set) |
| `GET /v1/local/skill-workflow-library?change=…&library=…` | `skill-workflow library inspect` output for one template manifest |
| `GET /v1/local/skill-workflows/{run_id}/handoff` | `skill-workflow handoff` output for one retained run |
| `GET /v1/local/workspace/timeline?limit=25&before=…` | `workspace timeline` page (off unless `--timeline-workspace` is set) |
| `GET /v1/local/workspace/timeline/{id}` | `workspace entry` view |
| `GET /v1/local/workspace/diff?left=…&right=…&limit=100&after=…` | `workspace diff` page |

The coordination hub (`agentscm hub serve`) is a separate server with its
own routes, including the agent MCP endpoint `POST /mcp` and the hook endpoints
`POST /v1/hub/hook/{agent}`; see the [hub guide](hub.md#routes) and
[agent sessions](hub.md#agent-sessions-mcp-and-hook-endpoints).

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

Its repository-authenticated stand-up routes also include `POST /v1/hub/standup/usage`,
`GET /v1/hub/standup/insights` and `GET /v1/hub/standup/work/{work_id}/insights`.
The first stores one idempotent partial usage report; the GET routes return bounded
work projections. See [work insights](work-insights.md) for pagination and evidence limits.

Completion calls on `/mcp`, `/v1/hub/hook/{agent}` and `/v1/hub/standup/checkpoint`
accept the optional `x-agentscm-completion-reporting: true` header. Exactly one literal `true`
enables a best-effort, idempotent completion observation; the default is off. It adds no response
fields and does not alter completion success. See [completion reporting](work-insights.md#optional-completion-reporting)
for the local toggle and the distinction between completion metadata and measured usage.

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

Preparation, branch, forge, both recovery routes and `/publication/mcp` read the request body under the Hub's 30-second request bound but may process for up to 120 seconds, because one operation can run several Git transport commands. Past that bound the operation keeps running: the route returns `202` with `{"state":"still_running","follow_up":...}` naming `status`, `branch/recover` or the same route to repeat with the same request IDs, and `/publication/mcp` returns a JSON-RPC error naming the matching status or recovery tool. `POST .../publication/status` keeps the 30-second bound.

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
