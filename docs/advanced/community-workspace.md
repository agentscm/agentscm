# Community workspace pilot

> **Experimental, single-server alpha.** This opt-in workspace adds member and settings management and shared execution of existing native workflows. It is intended for a small trusted team. It does not provide managed hosting, automatic failover or a workflow marketplace. Laptop source/context sync and a remote Claude Code Stop hook are separately opt-in; see the [laptop-to-hosted guide](hosted-laptop-workflows.md).

The workspace reuses Hub GitHub/GitLab identities, repository-scoped agent
tokens, the existing independent team roles, and the native workflow engine.
The existing read-only token dashboard and coordination APIs remain available.
Native execution and landing controls require Unix.

## Selected pilot boundary

The selected first deployment is a supervised native Hub process on one trusted Unix host. It uses one SQLite Hub database, one workspace database and its derived execution queue, one writable native authority/source/workflow directory per repository, task-only agent execution, same-host trusted-local collectors and explicit native approval/landing. The service binds to loopback behind an authenticated HTTPS deployment boundary. See [deployment](../deployment.md#selected-local-community-workspace-pilot) for the exact process shape.

This is not a hosted or two-machine qualification. Agents may use authenticated HTTP, workspace MCP or the `agent-task` CLI, but the task-only profile does not transfer source or authority state. The collector uses the same host's exact native data directory. Coordination and task discovery do not require Docker or a server-side model provider; a particular admitted command or review task still requires the runtime pinned by its installed collector profile.

Server execution is optional and separate. Adding a `profiles` entry starts the serial server worker and adds its Docker/socket/Claude dependencies. Adding a `source_profiles` entry also requires a matching server execution profile; source transfer cannot be attached to a task-only profile.

### Feature and qualification matrix

| Pilot behavior | Implemented and current evidence | Qualification gap |
| --- | --- | --- |
| Task-only Hub startup with no server command/model dispatcher | `agent_only_http_and_workspace_mcp_expose_the_same_side_effect_free_requirements`, `public_cli_uses_the_real_authenticated_task_route_without_server_execution` and `agent_owned_transport_failure_revision_success_and_native_landing_are_end_to_end` in `tests/hub_workspace_execution.rs` assert that an `agent_task_profiles`-only service does not enable server execution. | Tests use an in-process Hub and synthetic identities. No supervised long-running host has been qualified. |
| Authenticated HTTP, workspace MCP and public CLI access to the same task facade | The first two tests above cover HTTP/MCP parity and the real `agentscm` CLI against the authenticated route. `synthetic_mcp_protocol_client_discovers_and_reads_tasks_without_a_vendor_hook` covers MCP initialize, discovery, instructions, one reference, qualification and token revocation. | The MCP client is hand-written synthetic protocol traffic, not a live Claude, Codex, editor or desktop client. Live OAuth is not exercised. |
| Failure, revised immutable source, current pass, deliberate approval and native landing | `agent_owned_transport_failure_revision_success_and_native_landing_are_end_to_end` covers a task-only profile, one enrolled collector actor, transport/recovery cases, a failed command, a fresh revision, a qualifying imported receipt, explicit approval/integration and later origin revocation. It invokes the real `agentscm` CLI but uses fake Docker and in-process HTTP/MCP. | It is not a two-contributor worktree scenario, physical two-machine run, real container-runtime qualification or live provider/model run. It does not qualify concurrent edits across two worktrees. |
| Failed collection remains nonqualifying | The same end-to-end test asserts that a successful `agent-task collect` process can retain/upload a finished failed check and that `qualification` remains false with `collector_not_passed`. | Operators still need a readiness check; process exit alone cannot be monitored as success. |
| Role, repository, read-only and current-authorization enforcement | `agent_only_http_and_workspace_mcp_expose_the_same_side_effect_free_requirements` covers read-only-token refusal. `http_native_actions_require_explicit_human_roles_exact_subject_and_native_evidence` covers independent human roles and exact native evidence. `requests_bind_exact_registered_subject_and_are_durably_idempotent` in `src/hub/workspace_execution/tests.rs` rejects another repository on the server queue. The end-to-end test covers revocation before retry/finalization and revocation of a previously successful origin before approval. | These are focused synthetic tests, not one frozen two-contributor daily-use scenario against live forge identities; the cross-repository assertion is on the optional server queue rather than the task-only collector path. |
| Restart and no automatic replay | `restart_marks_queued_and_running_unknown_and_never_replays` in `src/hub/workspace_execution/tests.rs` covers the optional server queue and its exclusive lease. `changed_action_same_request_conflicts_and_reconciliation_fences_late_work` in `src/skill_workflow/agent_tasks.rs` covers fenced late work and reopening retained task state without replay. | No process-kill acceptance run covers Hub, task claim, collector reservation/upload and native admission together. Task-only attempts do not use the server queue's `Unknown` row, so operators must inspect their typed status/recovery state separately. |
| Two labeled native workspaces and hosted source/server execution | `two_laptops_get_failure_fix_feedback_and_exact_success_without_native_authority` in `tests/hosted_laptop_workflow.rs` covers two native workspaces/laptop labels and server execution/source transfer. | These are not Git worktrees, task-only collectors, physical laptops or real vendor clients. This is adjacent evidence, not qualification of the selected pilot. |
| Persistent recovery | Hub-only cold backup has a separate synthetic container check, and native recovery has typed primitives. | A consistent combined Hub/workspace/queue/native/collector restore is not yet specified or exercised. Nonempty attached native stores are explicitly refused by current data backup/restore. |

## Start with an existing Hub and native repository

Configure forge sign-in as described in [the Hub guide](../hub.md). The
operator selects the workspace and execution configuration files at startup:

```sh
export AGENTSCM_WORKSPACE_FILE=/etc/agentscm/workspace.json
export AGENTSCM_WORKSPACE_EXECUTION_FILE=/etc/agentscm/execution.json
agentscm hub serve
```

Keep the existing Hub database, objects and secrets on persistent storage. The
workspace file contains no provider credentials:

```json
{
  "workspace_id": "example-team",
  "name": "Example team",
  "database": "/srv/agentscm/workspace.sqlite",
  "initial_owner": {
    "provider": "github",
    "host": "github.com",
    "user_id": 12345
  }
}
```

Use the owner's actual numeric forge ID. The bootstrap runs only for an empty
workspace; restarting or changing the configured owner cannot restore revoked
permissions. The initial owner receives explicit administrator, reader and
contributor roles. Administrators manage grants but cannot promote themselves.
For a two-person setup, the owner can add a second administrator, who can then
grant any additional reviewer/integrator roles to the first person.

Open `/ui/#workspace` and sign in through the configured forge. Workspace
membership is an additional access requirement. Browser operations use a
15-minute Secure, HttpOnly session cookie and an anti-forgery token, rather
than a durable agent key in browser storage. Sessions and revocations persist
in the workspace database. Role changes end the affected person's sessions;
sign in again afterward. Use HTTPS for remote browsers. Some browsers refuse
Secure cookies over plain HTTP even on loopback.

The workspace can be used for members/settings without enabling execution.
Execution configuration without workspace configuration is refused.

## Permissions

Roles are independent. Assign combinations deliberately; administrator does
not imply permission to execute, approve or land.

| Role | Workspace capability |
| --- | --- |
| Reader | Inspect permitted repositories, subjects and runs |
| Contributor | Request and cancel workflows in permitted repositories |
| Reviewer | Explicitly approve an exact qualifying native revision |
| Integrator | Explicitly land an approved, current native revision |
| Administrator | Manage members and workspace settings |

Members are selected by provider, host and numeric forge ID. Repository grants
are explicit; an empty list grants no repository access. `*` deliberately
grants all repositories configured in this workspace. Last-administrator
protection also applies to the legacy Hub disable endpoint.

Existing Hub write-agent tokens can request/cancel execution only within their
repository and their owner's current workspace grant. They cannot manage
members/settings, approve or integrate. Read-only tokens cannot mutate.
Revocation is checked at requests, before dispatch, and before finalization.
Losing forge membership does not automatically revoke an already-issued Hub
token; revoke it or disable the person explicitly.

Settings changes carry the generation shown by the server and a request ID.
Concurrent edits return a conflict. After an uncertain network reply, retry
the same ID with the same body; changing the body requires a fresh request.
Membership, settings and native actions retain audit records without secrets.
The browser keeps unresolved operation IDs and non-secret intent fields in
session storage for that person and workspace. Reloading the same tab preserves
explicit retry; closing the tab discards this local recovery journal. Server
records remain retained. No operation is automatically replayed.

## Register a task-only profile

First prepare the native source, current Change context, installed target workflow, executor enrollment and immutable collector profile using the [Claude Code workflow guide](claude-code-workflows.md) and [agent-owned task contract](agent-workflow-tasks.md). For the selected pilot, register only the exact native repository ID and installed requirement digest in the operator-owned execution file:

```json
{
  "version": 1,
  "agent_task_profiles": [{
    "repository_id": "example/widgets",
    "profile_id": "local-tasks",
    "native_repository": "0123456789abcdef0123456789abcdef",
    "data_dir": "/srv/agentscm/widgets",
    "target": "main",
    "requirement_digest": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
  }]
}
```

Version 1 is strict: unknown fields or another version refuse startup. A task-only entry has exactly `repository_id`, `profile_id`, `native_repository`, absolute `data_dir`, `target`, `requirement_digest` and optional `publish_forge_status`. It does not select Docker, a socket, Claude Code or a provider. The Hub validates the configured native store and current requirement at startup, and no server execution worker starts when `profiles` is empty.

The example identities/digests are synthetic and must be replaced with installed values. The collector must use this same authoritative native store on the trusted host; the task-only facade does not bootstrap an independent store. Pin and record the exact binary and configuration bytes as described in [deployment](../deployment.md#compatible-artifact-and-configuration-pinning).

### Optional server execution

To opt into the existing server dispatcher, add a `profiles` entry with its runtime paths:

```json
{
  "version": 1,
  "profiles": [{
    "repository_id": "example/widgets",
    "profile_id": "claude-tests",
    "native_repository": "0123456789abcdef0123456789abcdef",
    "data_dir": "/srv/agentscm/widgets",
    "target": "main",
    "requirement_digest": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "docker": "/usr/bin/docker",
    "docker_socket": "/run/docker.sock",
    "claude": "/usr/local/bin/claude"
  }]
}
```

These example identities/digests must be replaced with the actual installed values. Server profiles pin the permitted native store, target, requirement and operator-selected executables. Requests cannot choose host paths, sockets, provider endpoints, model credentials or a replacement skill/recipe. Candidate commands still use the existing isolated runner; the Docker socket belongs to the trusted controller and is not mounted into candidate containers. This optional profile starts the server execution worker and therefore requires its Docker/socket/Claude paths to be available and separately qualified.

By default, the server must already have the immutable source and current Change context. To enable bounded source/context exchange from laptops, add an operator-pinned source profile as described in the [laptop-to-hosted guide](hosted-laptop-workflows.md). A source profile is valid only when a matching server `profiles` entry exists; it is not available with only `agent_task_profiles`. Git history import remains a separate operation through the existing source/Git bridges.

## Run, inspect, approve and land

In Workspace, select a repository/profile and a current Change. The server
returns the exact revision, Change version, context digest and installed
requirement digest. Start preserves that subject. Tests/review execute through
the existing native engine, with current authorization checked again at each
dispatch. Failure findings remain inspectable through the run/step view.

Fixes require new immutable source and an updated current context. A prior green
run cannot qualify different bytes, policy, target/base or context. The queue
serializes execution in this first slice; two people can submit work while
another run is active. The settings page shows capacity one.

Approval and native landing are separate, deliberate browser actions. The
native authority independently rechecks the installed workflow receipt,
current exact subject, approval and target/base in its transaction. Neither a
green historical row nor a client-provided success field grants admission.
Inspect the source/context before approving. A different integrator can use
the approval ID returned to the reviewer. These controls do not protect raw
Git pushes or forge merges; configure their server protections separately.

Runs retain queued/running/succeeded/failed/cancelled/stale/unknown states.
Cancellation and revocation fence new passing receipts. Stopping an external
process or container can require reconciliation; uncertain results are not
automatically retried. After a restart, retained queued/running requests become
Unknown and are not redispatched. Inspect native evidence and cleanup custody,
then explicitly prepare fresh work. An execution-queue lease refuses a second
server against the same queue; multiple replicas are unsupported.

## Persistent state and credentials

Treat the following as one deployment inventory. Record the owner, mode, filesystem/storage class, backup inclusion, exact configured path and configuration or artifact digest without placing secret values in the inventory.

| State | Selected local location and rule | Sensitivity / current behavior |
| --- | --- | --- |
| Hub database | `AGENTSCM_DATABASE_URL=sqlite:///srv/agentscm/hub/hub.sqlite` | Persistent writer state. Include SQLite sidecars in any consistent snapshot. General schema and token behavior are in the [Hub guide](../hub.md). |
| Hub objects and secrets | The fixed `--objects` and `--secrets` directories | Objects contain retained Hub payloads. Secrets contain forge application material and bootstrap setup state; keep owner-private. |
| Workspace startup configuration | Exact files selected by `AGENTSCM_WORKSPACE_FILE` and `AGENTSCM_WORKSPACE_EXECUTION_FILE` | Configuration is trusted startup input. Retain reviewed bytes and SHA-256, but do not put credentials in either JSON file. |
| Workspace database | The absolute `database` path in `workspace.json` | Persistent members, settings, browser sessions, revocations and audit records. An old copy can restore old grants. |
| Execution queue and lease | Derived by replacing the workspace database extension with `execution.sqlite`; `/srv/agentscm/workspace.sqlite` therefore uses `/srv/agentscm/workspace.execution.sqlite` and its lock file | Persistent server-run/source-transfer queue. Active server runs reopen as `Unknown`; the lease refuses a second local service but is not HA fencing. Task-only attempts live in the native workflow journal instead. |
| Native authority/source/workflow store | Each exact absolute `data_dir` in the execution configuration | Preserve the whole directory, including immutable source objects, authority state, workflow journals/receipts and installed enrollment/profile records. Git export is not a backup of approvals or workflow evidence. One writer only. |
| Trusted-local collector state | One owner-private state directory outside candidate source | Contains durable execution reservations and retained signed transfers. Reopening must not rerun external work. Preserve it with the matching native store and configuration. |
| Agent token and collector signing key | Separate absolute owner-private files outside candidate source | Token access is repository-scoped; the raw Ed25519 key is not stored by the native authority. Loss can prevent retained-transfer reuse; compromise requires revocation and a new enrollment epoch. |
| Collector runtime/provider state | Pinned Docker executable/socket or Claude Code executable plus the operator account's provider authentication | Not stored in workspace JSON and not transferred to candidate containers. Provider credentials stay outside candidate source and require separate secure handling. |
| Supervisor and reverse-proxy state | Reviewed unit/environment/proxy configuration and TLS material | Record non-secret bytes/digests. Keep environment credential files and TLS private keys secret. The proxy is part of the exposure boundary, not native admission. |

Ordinary working directories and Git worktrees are not authority or evidence backups. Preserve user edits separately, but do not restore them over an immutable native store or collector journal and infer that the retained subject is current.

## Operations and recovery

Keep exactly one writable native authority per repository directory and one workspace execution service. Use the supervised process and HTTPS boundary described in [deployment](../deployment.md). Persistent local storage is required; an ephemeral container filesystem is insufficient.

The combined pilot is not yet restore-qualified. The Hub-only cold-copy procedure in the [Hub container guide](../hub.md#running-in-containers) covers that Hub volume, not an atomic Hub/workspace/queue/native/collector recovery point. AgentSCM has no online backup, cross-store snapshot coordinator, automatic failover or external revocation ledger. A filesystem copy must not be taken while any Hub, native CLI, collector or other writer is active unless the selected storage mechanism proves a consistent snapshot across the complete inventory.

Current native recovery also refuses data backup/restore when the store has retained attachments, because attachment journals cannot be copied as unmanaged files. The attached-checkout roadmap separately marks nonempty-journal backup/restore unsupported. Operators must not work around that refusal with an undocumented file copy or claim that Git/native exports preserve the missing typed state.

The choice between a fully cold, quiesced combined restore and a typed attached-state recovery is pending. A later phase must select one, specify exact snapshot and restore order, missing-object/config/credential failure checks, isolation of old writers, revocation reconciliation, credential rotation, and offline qualification before reopening access. Until that procedure is implemented and exercised, retain backups according to the inventory but describe recovery as unverified; do not promise restoration of a landable subject.

After any interruption, inspect task status/recovery and authenticated `qualification`. A server-executed queued/running request becomes `Unknown` and is never automatically redispatched. A task-only collector reservation or transfer keeps its typed retained state and likewise must not be rerun merely because the Hub restarted. Old backups may restore revoked members, tokens, executor registrations or origin grants, so access must remain closed until later revocations are reapplied and exposed credentials are rotated.

Before calling the selected pilot qualified for daily use, the remaining acceptance run must use two real forge identities and two actual contributor worktrees, exercise failure → fix → current pass → deliberate review/approval → native landing, and cover repository isolation, read-only refusal, role separation, mid-run revocation, concurrent edits, target advancement, stale evidence, process interruption and the chosen restore. Repeat the supported agent paths and vendor-neutral MCP path separately. Report same-host synthetic, real runtime, two-machine, live-forge and hosted results as different evidence classes.

Workspace APIs use `/v1/hub/workspace`: `login`, `session`, `members`,
`settings`, `repositories`, and repository-scoped `profiles/{profile}/subjects`,
`runs`, `runs/{run}/cancel`, `runs/{run}/workflow`, `profiles/{profile}/approve`
and `profiles/{profile}/integrate`. Lists are bounded and cursor-paged. The
broader Community Edition management work is tracked in
[issue #20](https://github.com/counterbranch/agentscm/issues/20).
