---
title: "Authenticated hub reads for skill workflows"
---

# Authenticated hub reads for skill workflows

> **Experimental (alpha).** The hub can expose retained native skill workflow status only when
> its operator explicitly maps a hub repository to an existing native store. The mapping grants
> read access; it does not grant execution, resumption, cancellation, approval or admission.

The hub is unconfigured by default. Existing hub features continue to work, while workflow reads
return an unavailable envelope. To enable them, set `AGENTSCM_WORKFLOW_BINDINGS_FILE` before
starting the hub. Its value names an existing regular JSON file no larger than 64 KiB:

```json
{
  "version": 2,
  "bindings": [
    {
      "hub_repository": "acme/widgets",
      "hub_host": "github.com",
      "hub_repository_id": 12345,
      "native_repository": "0123456789abcdef0123456789abcdef",
      "data_dir": "/operator/selected/existing/native-store"
    }
  ]
}
```

The file is strict: unknown fields, unsupported versions, invalid identities, duplicate hub
repository identities, more than 32 bindings and missing native stores stop hub startup.
`hub_host` is the lowercase forge hostname used by the Hub (including its port when configured),
without a scheme or path. `hub_repository_id` is the forge's positive numeric repository ID,
not the Hub's `rep_` row ID. Confirm both against the connected repository and its forge before
adding a binding. `hub_repository` remains a validated display label; the mapping key is the
host and stable repository ID. Renames retain the mapping, while a different host or ID does not
select it, even when the display name matches. Duplicate host/ID pairs are refused across labels.
`data_dir` comes only from this startup file. Requests cannot supply a path or URL. Startup
checks that the current
native authority identity equals `native_repository` and that the native root, authority database,
access lock and object directory already exist. It does not create a native store or workflow
journal.

Version 1 configuration files are refused at startup because a display name alone cannot pin
a forge repository. To migrate, stop the Hub, retain a private copy of the old operator file,
confirm the host and stable forge repository ID for every binding, add `hub_host` and
`hub_repository_id`, set the file's top-level `version` to `2`, and restart the Hub. Check the
intended repository's status/list reads and a separate repository's unavailable response before
relying on the mapping. Native stores and workflow journals need no migration. The HTTP/MCP
response envelope remains version 1.

For rollback without native workflow reads, unset `AGENTSCM_WORKFLOW_BINDINGS_FILE` and restart
the Hub; other Hub features continue to work and workflow reads become unavailable. Keep the
version 2 file for a later restart. Do not restore name-only bindings to work around identity
validation. If rolling back the Hub binary, leave this optional feature disabled until the binary
supports host/ID-scoped bindings.

An authenticated agent or read-only token can request:

```text
GET /v1/hub/skill-workflows
GET /v1/hub/skill-workflows/{run_id}
```

The run route accepts no query fields. The collection route accepts only an optional `before`
cursor (a previous page's `next_cursor`) and an optional `q` search text of at most 200 bytes.
The hub resolves the token's forge repository scope through the same host/stable-ID helper used
by other repository reads. A legacy token without an ID resolves the connected repository on its
owner's host; if none is connected, it cannot select a version 2 binding. Before
each journal read it reopens the native authority and compares its identity with the configured
identity. A missing, unreadable or replaced authority returns an unavailable envelope without a
retained projection. The retained run must also name the configured native identity.

The collection route returns at most 16 summaries per page, newest-created first, with a
`next_cursor` for older runs. Each summary contains only `run_id`, workflow `name`, creation
`sequence`, immutable source `revision` and retained `state`; it does not invent an update time
or project a run's steps. The hub's Workflows page lists these runs and lets the reader find
another run by name, state, revision or ID without knowing its identifier. A configured native
store with no workflow journal returns an empty list without creating one.

An available response has the same projection used by the trusted-local native read service:

```json
{
  "version": 1,
  "availability": "available",
  "projection": {
    "version": 1,
    "run": {},
    "present_applicability": {}
  }
}
```

An unconfigured repository is a normal 200 response:

```json
{
  "version": 1,
  "availability": "unavailable",
  "reason": "native_workflow_not_configured"
}
```

Other unavailable reasons distinguish an unavailable or replaced operator binding without exposing
filesystem paths. A missing run returns 404, malformed input returns 400, missing authentication
returns 401, and exhausted or failed hub work returns the hub's existing generic 503 response.
Historical run and step states remain observations. `present_applicability` is a separate current
check, and neither field is an admission or safe-to-merge decision.

The hub checks the configured authority immediately before the journal read and reopens it after
projection before returning retained data. This detects an authority replacement observed at those
checks; it is not an atomic snapshot against a hostile local operator or filesystem.

The stateless MCP endpoint advertises `agentscm_workflow_status` with strict
`{"run_id":"..."}` arguments. Its text content is the same versioned JSON envelope. The existing
`?repo=` mismatch guard still applies. This tool accepts read-only tokens and does not start or
heartbeat a coordination session. Read-only tokens may also use MCP initialization, ping and tool
discovery so a normal client can reach it; their tool list contains only the hub's read tools,
including `agentscm_workflow_status`. Ordinary agent tokens also see the coordination and work
tools, which retain their existing write-token behavior.

The join performs reads only. Native workflow execution and lifecycle changes remain local CLI
operations, and the hub has no workflow run, resume, cancel, approval or admission endpoint.

Read-only access makes no workflow-row, execution, approval or integration changes and does not
initialize an absent journal or native store. SQLite may create or update WAL/SHM sidecars for
existing databases, including the native authority. Reads can fail if required sidecar housekeeping
is unavailable. This is logical read-only access, rather than a zero-filesystem-write guarantee.
