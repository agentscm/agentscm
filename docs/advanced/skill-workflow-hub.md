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
  "version": 1,
  "bindings": [
    {
      "hub_repository": "acme/widgets",
      "native_repository": "0123456789abcdef0123456789abcdef",
      "data_dir": "/operator/selected/existing/native-store"
    }
  ]
}
```

The file is strict: unknown fields, unsupported versions, invalid identities, duplicate hub
repositories, more than 32 bindings and missing native stores stop hub startup. `data_dir` comes
only from this startup file. Requests cannot supply a path or URL. Startup checks that the current
native authority identity equals `native_repository` and that the native root, authority database,
access lock and object directory already exist. It does not create a native store or workflow
journal.

An authenticated agent or read-only token can request:

```text
GET /v1/hub/skill-workflows
GET /v1/hub/skill-workflows/{run_id}
```

The run route accepts no query fields. The collection route accepts only an optional `before`
cursor (a previous page's `next_cursor`) and an optional `q` search text of at most 200 bytes.
The hub uses the token's repository as the only mapping key. Before
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
