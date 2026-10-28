# Community workspace pilot

> **Experimental, single-server alpha.** This opt-in workspace adds member and settings management and shared execution of existing native workflows. It is intended for a small trusted team. It does not provide managed hosting, automatic failover or a workflow marketplace. Laptop source/context sync and a remote Claude Code Stop hook are separately opt-in; see the [laptop-to-hosted guide](hosted-laptop-workflows.md).

The workspace reuses Hub GitHub/GitLab identities, repository-scoped agent
tokens, the existing independent team roles, and the native workflow engine.
The existing read-only token dashboard and coordination APIs remain available.
Native execution and landing controls require Unix.

## Selected pilot boundary

The selected first deployment is a supervised native Hub process on one trusted Unix host. It uses one SQLite Hub database, one workspace database and its derived execution queue, one writable native authority/source/workflow directory per repository, task-only agent execution and same-host trusted-local collectors. Attached Git work remains Git-authoritative: publish the exact qualified candidate, then use deliberate provider review and merge under the provider's protections. Native approval and landing remain a separate opt-in workflow. The service binds to loopback behind an authenticated HTTPS deployment boundary. See [deployment](../deployment.md#selected-local-community-workspace-pilot) for the exact process shape.

This is not a hosted or two-machine qualification. Agents may use authenticated HTTP, workspace MCP or the `agent-task` CLI, but the task-only profile does not transfer source or authority state. The collector uses the same host's exact native data directory. Coordination and task discovery do not require Docker or a server-side model provider; a particular admitted command or review task still requires the runtime pinned by its installed collector profile.

Server execution is optional and separate. Adding a `profiles` entry starts the serial server worker and adds its Docker/socket/Claude dependencies. Adding a `source_profiles` entry also requires a matching server execution profile; source transfer cannot be attached to a task-only profile.

### Feature and qualification matrix

| Pilot behavior | Implemented and current evidence | Qualification gap |
| --- | --- | --- |
| Task-only Hub startup with no server command/model dispatcher | `agent_only_http_and_workspace_mcp_expose_the_same_side_effect_free_requirements`, `public_cli_uses_the_real_authenticated_task_route_without_server_execution` and `agent_owned_transport_failure_revision_success_and_native_landing_are_end_to_end` in `tests/hub_workspace_execution.rs` assert that an `agent_task_profiles`-only service does not enable server execution. A same-host actual-browser check with the built UI and synthetic OAuth observed a signed-in session, repository and exact current profile/Change/revision, a bounded empty runs page, Members/Settings navigation and no Start workflow control. | Automated tests use an in-process Hub and synthetic identities. The browser observation used installed Chromium and synthetic OAuth, not live OAuth, a vendor agent client, model execution or a supervised long-running host. |
| Authenticated HTTP, workspace MCP and public CLI access to the same task facade | The first two tests above cover HTTP/MCP parity and the real `agentscm` CLI against the authenticated route. `synthetic_mcp_protocol_client_discovers_and_reads_tasks_without_a_vendor_hook` covers MCP initialize, discovery, instructions, one reference, qualification and token revocation. The shared lifecycle fetches compact instructions and a selected immutable reference, rejects stale task/subject/schema identities plus cross-repository and revoked reads, and proves those reads do not mutate native state. | The MCP clients are hand-written synthetic protocol traffic, not a live Claude, Codex, editor or desktop client. Live OAuth is not exercised. |
| Failure, revised immutable source, current pass, deliberate approval and native landing | `agent_owned_transport_failure_revision_success_and_native_landing_are_end_to_end` retains the native lifecycle: task-only profile, enrolled collector, transport/recovery cases, failed command, fresh revision, current imported receipt, explicit approval/integration and later origin revocation. The shared scenario uses distinct reviewer and integrator identities. | Real CLI and in-process HTTP/MCP with fake Docker and synthetic identities; no real container runtime, live provider/model or physical two-machine qualification. Native landing does not establish Git merge protection. |
| Failed collection remains nonqualifying | The same end-to-end test asserts that a successful `agent-task collect` process can retain/upload a finished failed check and that `qualification` remains false with `collector_not_passed`. | Operators still need a readiness check; process exit alone cannot be monitored as success. |
| Role, repository, read-only and current-authorization enforcement | `agent_only_http_and_workspace_mcp_expose_the_same_side_effect_free_requirements` covers read-only-token refusal. `http_native_actions_require_explicit_human_roles_exact_subject_and_native_evidence` covers independent human roles and exact native evidence. `requests_bind_exact_registered_subject_and_are_durably_idempotent` in `src/hub/workspace_execution/tests.rs` rejects another repository on the server queue. The shared lifecycle also rejects a token bound to another repository on the task-only reference path, proves reviewer approval and native integrator landing permissions, refuses each human's other action and both actions through the contributor agent token, and covers revocation before retry/finalization and revocation of a previously successful origin before approval. | These use synthetic identities rather than live forge OAuth. |
| Restart and no automatic replay | `restart_marks_queued_and_running_unknown_and_never_replays` in `src/hub/workspace_execution/tests.rs` covers the optional server queue and its exclusive lease. The joined cold-restore lifecycle additionally enables a separate server/source profile, retains a queued run, injects the documented durable `importing` source-admission boundary, and verifies on actual reopen and a second restart that the run is `Unknown` with its exact subject and no receipt, while the transfer is `Unknown` with its exact request/input identities and no admitted subject; neither is replayed. The task-only lifecycle separately retains exact recovery identities, fences late heartbeat/result traffic and keeps fake-runtime effects, native task state and receipts unchanged until explicitly fresh work. | The source `importing` row is injected durable recovery state, not evidence of an actual SIGKILL at that instruction boundary. No process-kill acceptance run covers Hub, task claim, collector reservation/upload and native admission together. Task-only attempts retain their separate typed status/recovery state. |
| Two Git worktrees, target advancement and current imported evidence | `two_git_worktrees_exercise_shared_candidate_branch_publication` extends the same lifecycle helper with linked Git worktrees, distinct contributors, disjoint captures, target refresh, current imported qualification, pinned preparation/branch transport, bound fake-GitHub PR publication and status, lost replies, concurrent retries, legacy bypass refusal and revocation. `two_git_worktrees_require_independent_exact_human_merge` adds explicit synthetic human review and exact-head/status/baseline merge guards. Both preserve the other contributor's work. | Real CLI, Git objects/worktrees and bare remote; explicitly synthetic HTTPS-identity transport adapter, fake Docker and fake GitHub. No live TLS, provider protections, model runtime, physical two-machine or cold-restore qualification is established by this joined scenario. |
| Persistent recovery | `two_git_worktrees_cold_restore_at_same_paths` exercises the fully stopped, synthetic same-host filesystem procedure at stable absolute paths, including actual attached Git, native, Hub, collector and publication state plus the separately enabled optional server queue/source-transfer state, omission preflight, later-denial reconciliation and a second restart. | Native data backup/restore still refuses nonempty attached stores. Online or relocated restore, live runtimes/providers and an actual process-kill boundary across all retained states remain unqualified. |

The joined Git scenario uses a server-owned publication binding separate from client manifest JSON. The binding pins actor, profile, attachment, configuration, prepare and branch operations. Current actor/origin grants and executor qualification are checked before dispatch and before successful forge status after provider reads. Legacy CLI preparation and unbound forge-status reads remain native-only and fail closed for imported receipts. Durable PR attempts recover by observation; a missing PR never grants another create. Authorization at dispatch cannot roll back an effect that raced revocation.

Existing `attached_joined_https_candidate_uses_existing_hub_and_continues_same_pr` in `tests/hub_publish.rs` separately exercises actual loopback HTTPS and fake GitHub with native evidence. Existing GitLab tests qualify the shared candidate publisher/status machinery with native evidence; the joined imported-receipt scenario is fake-GitHub only. No live forge merge is authorized or exercised. The selected recovery procedure is complete stopped-service, same-host filesystem restore at stable paths, exercised by the synthetic joined scenario below. This does not expand the native backup API or qualify live runtimes, providers or an actual process-kill boundary across the combined task, collector, server-queue and source-admission state. Browser qualification is recorded separately from this automated scenario.

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

The qualified recovery profile is deliberately narrow: a fully cold restore on the same Unix host to the same absolute paths, using a synthetic fixture and local fake transports. It is not online backup, relocation, high availability, a live-forge result or real-runtime qualification. The native backup/recovery API still refuses retained attachments; this operator filesystem procedure does not change that API or make a Git export a backup.

Use this order and keep public ingress closed until the last step:

1. Record the frozen configuration bytes and digests, absolute paths, owners, modes and file types for every item in the inventory above. Include the Hub database, objects and secrets; workspace database; execution queue and source-transfer state; the complete native authority/source/task/workflow/attachment directory; Git common directory, linked-worktree administrative directories, indexes, refs and objects plus the worktree bytes; owned candidate mirror and its journals; collector database, reservations and transfers; and separately protected token, signing-key, forge credential, CA, supervisor and proxy files. Runtime sockets are excluded and recreated. Preserve SQLite sidecars; never delete a WAL to make a copy appear usable.
2. Stop ingress and every Hub, workspace execution, native CLI, collector, mirror/publication and Git writer. Wait for explicitly owned children to exit, with a finite timeout that fails the operation rather than permitting an unsafe copy. Abort and await listener tasks. Release all database, authority, engine, workspace, execution-service, mirror, socket, collector and CLI handles.
3. For each inventoried SQLite database, open the existing database, run `PRAGMA wal_checkpoint(TRUNCATE)`, require a result with zero busy pages and all log pages checkpointed, then close it. A missing database is a refusal, not permission to initialize an empty replacement.
4. Copy the bounded inventory to private backup storage and write a stable-path manifest containing path, file type, mode, size and digest. Verify that every required object, configuration and credential is present and that the manifest contains no secret values. External provider effects, fake-forge counters and the later-revocation ledger are deliberately outside this backup.
5. Before restore, isolate the stopped old roots without modifying the backup. Preflight the manifest, paths, types, modes, digests, required source objects, mirror metadata, collector journal, configuration, credentials and revocation ledger before invoking constructors that could initialize missing state. Restore only to the recorded absolute paths, recreate runtime sockets, and keep ingress closed.
6. An older backup cannot know revocations made after its snapshot. Reapply the independent denial ledger in maintenance mode: disable the affected member at the current workspace generation with a fresh request ID, revoke each agent token, and revoke each workflow executor at the next current policy epoch. If the ledger, configuration, required objects or credentials are missing, or an executor epoch cannot be mapped unambiguously, do not reopen access. Rotate credentials whose confidentiality may have been exposed.
7. Reopen without public ingress and inspect task status/recovery, source transfers, collector reservations, publication recovery, attachment state and authenticated qualification. Keep uncertain task, run, transfer, push and PR outcomes observational; do not automatically retry them. Confirm retained task/request/attempt/transfer/candidate/publication identifiers and prove that execution, probe, push and PR-create counters did not increase during restore or refused retries. An old browser cookie must remain invalid independently of token denial, and an otherwise valid request from a revoked executor must be refused. Regranting membership must not revive that old session.
8. Perform an ordinary second stop/start and repeat the denial and retained-state observations. Only then reopen ingress.

The synthetic joined scenario qualifies this stopped-service ordering, stable-path manifest and reconciliation behavior on one host. It also refuses restore preflight when a required source object, mirror metadata, collector journal, required configuration or protected credential is absent. The selected service remains task-only; a separate explicitly enabled server/source profile fixture retains a queued run and an injected durable source-admission row through the same backup. Actual reopen converts both exact envelopes to `Unknown` without replay, preserves task/collector state and effect counts, and repeats those observations after a second restart. The injected source row is not an actual SIGKILL boundary; existing queue and source-transfer unit boundary tests remain separate evidence.

The executable checkpoint is `two_git_worktrees_cold_restore_at_same_paths` in `tests/hub_workspace_execution.rs`, inside the existing signed-collector/two-worktree/publication lifecycle. It restores the actual local directories, including retained signed transfer, optional server queue/source-transfer rows, both attachments, Git worktree administration, owned mirror and publication journals. The fake provider repository and effect counters remain outside the backup. The fixture uses regular files/directories and local synthetic HTTP/Git transport; custom CA files, a reverse proxy, supervisor configuration, filesystem relocation and operating-system ownership changes are not exercised. Include those configured inputs in an operator inventory, but do not infer their qualification from this fixture.

Run only this checkpoint with `cargo nextest run --locked --test hub_workspace_execution -E 'test(two_git_worktrees_cold_restore_at_same_paths)'`. For a bounded browser inspection of the actual running fixture before its cold checkpoint, build with the UI feature and opt in:

```sh
release_dir=$(mktemp -d)
AGENTSCM_TEST_BROWSER_HOLD_SECONDS=600 AGENTSCM_TEST_BROWSER_RELEASE_FILE="$release_dir/release" cargo test --locked --features ui --test hub_workspace_execution two_git_worktrees_cold_restore_at_same_paths -- --exact --nocapture
```

The hold prints only the local Hub UI URL; use its synthetic OAuth sign-in links. In another shell, create the selected release file to continue, or let the timer expire. The timer is capped at ten minutes; unset/zero means no hold, so normal tests do not pause. The fixture continues through restore and removes its temporary state when the test finishes. A 2026-10-22 same-host walkthrough with the built UI and installed Chromium observed synthetic OAuth sign-in, authenticated session/repository/task-only runs responses, current profile/Change/revision display, Members/Settings navigation and no server Start workflow control. It does not qualify live OAuth, vendor clients, models or remote browsers.

After any interruption, inspect task status/recovery and authenticated `qualification`. A server-executed queued/running request becomes `Unknown` and is never automatically redispatched. A task-only collector reservation or transfer keeps its typed retained state and likewise must not be rerun merely because the Hub restarted. Old backups may restore revoked members, tokens, executor registrations or origin grants, so access must remain closed until later revocations are reapplied and exposed credentials are rotated.

Before calling the selected pilot qualified for daily use, repeat the acceptance scenario with two real forge identities and contributor worktrees: failure → fix → current pass → deliberate provider review/approval → provider-controlled landing. Verify the configured provider protections, repository isolation, read-only refusal, role separation, mid-run revocation, concurrent edits, target advancement, stale evidence, process interruption and cold restore on the selected host. Repeat the supported agent paths and vendor-neutral MCP path separately. Report same-host synthetic, real runtime, two-machine, live-forge and hosted results as different evidence classes.

Workspace APIs use `/v1/hub/workspace`: `login`, `session`, `members`,
`settings`, `repositories`, and repository-scoped `profiles/{profile}/subjects`,
`runs`, `runs/{run}/cancel`, `runs/{run}/workflow`, `profiles/{profile}/approve`
and `profiles/{profile}/integrate`. Lists are bounded and cursor-paged. The
broader Community Edition management work is tracked in
[issue #20](https://github.com/agentscm/agentscm/issues/20).
