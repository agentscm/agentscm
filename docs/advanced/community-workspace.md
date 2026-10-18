# Community workspace pilot

> **Experimental, single-server alpha.** This opt-in workspace adds member and
> settings management and shared execution of existing native workflows. It is
> intended for a small trusted team. It does not provide managed hosting,
> automatic failover, a workflow marketplace or source synchronization.

The workspace reuses Hub GitHub/GitLab identities, repository-scoped agent
tokens, the existing independent team roles, and the native workflow engine.
The existing read-only token dashboard and coordination APIs remain available.
Native execution and landing controls require Unix.

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

## Register an execution profile

First prepare the native source, Change context and installed target workflow
using the [Claude Code workflow guide](claude-code-workflows.md). Then select
the exact native repository ID and installed requirement digest in an
operator-owned execution file:

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

These example identities/digests must be replaced with the actual installed
values. Profiles pin the permitted native store, target, requirement and
operator-selected executables. Requests cannot choose host paths, sockets,
provider endpoints, model credentials or a replacement skill/recipe. Candidate
commands still use the existing isolated runner; the Docker socket belongs to
the trusted controller and is not mounted into candidate containers.

The server must already have the immutable source and current Change context.
This API does not upload laptop files, import Git history, checkpoint a remote
working directory or automatically refresh a stale context. Continue using
the existing native CLI and source/Git bridges for those operations.

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

## Operations and recovery

Keep exactly one writable native authority per repository directory and one
workspace execution service. Use a supervised process behind an authenticated
HTTPS deployment as described in [deployment](../deployment.md). A persistent
volume is required; an ephemeral container filesystem is insufficient.

Back up the Hub database, objects/secrets, workspace metadata, derived
`workspace.execution.sqlite` queue database and each native authority/source/
workflow store consistently. Stop writers before a filesystem copy unless the
backup method supplies a consistent snapshot. Preserve database sidecars as
required by the chosen snapshot method; do not copy a live SQLite main file
alone. Test restoration offline before use. An old backup can contain old
grants: reconcile later revocations and rotate affected credentials before
reopening access. This slice does not implement automatic failover or an
external revocation ledger.

Before daily use, verify two real forge identities, repository isolation,
read-only refusal, revocation during execution, restart/no replay and backup
restoration on the selected host. Local synthetic OAuth/transport tests do not
establish live forge login, runtime availability or hosted deployment.

Workspace APIs use `/v1/hub/workspace`: `login`, `session`, `members`,
`settings`, `repositories`, and repository-scoped `profiles/{profile}/subjects`,
`runs`, `runs/{run}/cancel`, `runs/{run}/workflow`, `profiles/{profile}/approve`
and `profiles/{profile}/integrate`. Lists are bounded and cursor-paged. The
broader Community Edition management work is tracked in
[issue #20](https://github.com/counterbranch/agentscm/issues/20).
