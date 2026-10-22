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

The coordination hub (`agentscm hub serve`) is a separate server with its
own routes, including the agent MCP endpoint `POST /mcp` and the hook endpoints
`POST /v1/hub/hook/{agent}`; see the [hub guide](hub.md#routes) and
[agent sessions](hub.md#agent-sessions-mcp-and-hook-endpoints).

The [local workspace snapshot timeline](workspace-timeline.md) is exposed through the native
`workspace` CLI and a separate local stdio `workspace-client mcp` process. It adds no route to
`agentscm serve` or the Hub, and it adds no HTTP path to this API or its OpenAPI surface. Capture
and fork are local mutations and are not available over HTTP.

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

## Hub publication forge check

This is the Hub, not `serve`. `GET /v1/hub/repos/{repo}/publications/{id}` adds a
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

Workflow and collision responses carry `nosniff`, no referrer, `no-store` and a
restrictive content-security policy; the revision, file, diff and `/health`
responses do not. Every read takes one of the `--max-concurrency` work
slots and keeps it through response transmission; cancellation does not release
it while native work is still running. A busy service returns 503. Malformed or
over-limit input returns 400, missing records 404 and conflicts 409. Workflow and
collision reads that fail unexpectedly return a sanitized 500 with the detail in
the local service log.

`serve` opens the source engine and initializes a missing data directory, but
it does not create or migrate the authority database (`state.sqlite`); run
a CLI command that opens it after upgrading, or `/v1/workflow` returns an
error. The routes do not establish a read-only mount or hostile-host isolation guarantee.
Authenticated sessions, approval and landing over HTTP remain unimplemented.
