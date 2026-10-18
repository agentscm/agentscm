# Advanced and experimental

> **Experimental (alpha).** Everything in this section is outside the main path (collision warnings, work intents and the hub), may change without migration, and has had limited testing. See the [README](../../README.md) for status and the [changelog](../../CHANGELOG.md) for known limits.

AgentSCM also contains an immutable ledger for exact revisions, their review contexts, approvals and protected-branch moves, plus bridges to and from Git. These parts work on a local, single-operator install and are described here for people who want to try them.

## Native ledger

- [Review contexts](review-context.md): the brief a revision is reviewed against
- [Checks](checks.md), [evidence](evidence.md), the [portable evidence verifier](evidence-verifier.md) and [installed policy](policy.md)
- [Workspaces](workspace.md), [merge sessions](merge.md), [offline sync](sync.md), [source history](source-history.md) and the [source reader](source-reader.md)
- [Native CLI reference](../reference/cli.md)
- [Source-bound skill workflows](skill-workflows.md): compile a bounded local check graph against an immutable revision and packaged skill; inspect, resume or cancel its recorded run

## Git bridges (shadow workflow)

Capture a Git commit, work and land natively, reconcile with Git and export to a local mirror you own. Nothing is pushed.

- [Git snapshot capture](git-snapshot.md), [Git reconciliation](git-reconcile.md) and [Git export](git-export.md)

## Isolated command checks

- [Isolated command checks](execution.md): run a declared command against an exact revision in a locked-down container. Not a sandbox for arbitrary hostile code.

## Hub features that are experimental

These live in the [hub guide](../hub.md) and are labelled there:

- GitHub publishing (`agentscm hub publish`) and GitLab sync: tested only against in-process fake servers.
- The PostgreSQL profile (`postgres` in `compose.hub.yaml`) and S3-compatible object storage.
