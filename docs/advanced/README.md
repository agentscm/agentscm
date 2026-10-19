# Advanced and experimental

> **Experimental (alpha).** Everything in this section is outside the main path (collision warnings, work intents and the hub), may change without migration, and has had limited testing. See the [README](../../README.md) for status and the [changelog](../../CHANGELOG.md) for known limits.

AgentSCM also contains an immutable ledger for exact revisions, their review contexts, approvals and protected-branch moves, plus bridges to and from Git. These parts work on a local, single-operator install and are described here for people who want to try them.

## Native ledger

- [Review contexts](review-context.md): the brief a revision is reviewed against
- [Checks](checks.md), [evidence](evidence.md), the [portable evidence verifier](evidence-verifier.md) and [installed policy](policy.md)
- [Workspaces](workspace.md), [task-scoped source and workspaces](task-workspaces.md), [merge sessions](merge.md), [offline sync](sync.md), [source history](source-history.md) and the [source reader](source-reader.md)
- [Native CLI reference](../reference/cli.md)
- [Source-bound skill workflows](skill-workflows.md): run exact-file checks and explicit packaged container commands against an immutable revision; inspect, resume, cancel or recover the run.
- [Claude Code workflow loop](claude-code-workflows.md): an opt-in local Stop trigger, tests/review, failure feedback and exact-current native landing requirements. Existing edit hooks remain contention-only.
- [Community workspace pilot](community-workspace.md): persistent members/settings, independent roles, shared execution and explicit approval/native landing on one server. The legacy token dashboard and MCP remain inspection-only.

- [Laptop-to-hosted workflows](hosted-laptop-workflows.md): bounded source/context exchange and a remote Claude Code Stop hook under installed server policy.

## Git bridges (shadow workflow)

Capture a Git commit, work and land natively, reconcile with Git and export to a local mirror you own. Explicit local-bare remote exchange is a separate opt-in command.

- [Git snapshot capture](git-snapshot.md), [Git reconciliation](git-reconcile.md), [Git export](git-export.md) and [local Git remote exchange](git-remote.md) and [authenticated network transport](git-remote-network.md)

## Isolated command checks

- [Isolated command checks](execution.md): run a declared command against an exact revision in a locked-down container. Not a sandbox for arbitrary hostile code.

## Hub features that are experimental

These live in the [hub guide](../hub.md) and are labelled there:

- GitHub publishing (`agentscm hub publish`) and GitLab sync: tested only against in-process fake servers.
- The PostgreSQL profile (`postgres` in `compose.hub.yaml`) and S3-compatible object storage.
