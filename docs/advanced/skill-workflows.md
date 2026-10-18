# Source-bound skill workflows

> **Experimental (alpha).** This local workflow profile has limited testing and its CLI,
> schema and storage may change without migration. It runs against an existing native AgentSCM
> data directory owned by the local operator.

A skill workflow binds a small directed graph of built-in checks to one immutable native source
revision and one packaged `SKILL.md`. The package may include explicitly named resources below
the skill directory. AgentSCM preserves those bytes and includes their identity in the compiled
plan; it does not interpret the skill prose as authority or as a successful check result.

The initial profile implements one executable action: `source_paths`. It reads exact files from
the selected native revision and compares their bytes with declared lowercase SHA-256 digests.
This establishes source identity for those paths. It does not execute the source or demonstrate
application behavior, permissions, deployment safety, model review, command isolation, hosted
execution, or user-interface behavior.

`require_capability` records an operation the profile cannot perform. A required unavailable
capability blocks the run. An optional unavailable capability remains visible as an omission and
does not become a success; after all required work succeeds, the run reports
`completed_with_omissions`.

## Definition

Definitions are strict JSON: unknown fields are rejected. The repository ID comes from the
read-only native authority in `--data-dir`; the revision, policy epoch, skill paths, dependency
graph and source expectations must all validate before a plan is produced.

```json
{
  "version": 1,
  "name": "verify-native-source",
  "source": {
    "repository": "0123456789abcdef0123456789abcdef",
    "revision": "0000000000000000000000000000000000000000000000000000000000000000",
    "policy_epoch": 0
  },
  "skill": {
    "entrypoint": "skills/native-workflow/SKILL.md",
    "resources": []
  },
  "steps": [
    {
      "id": "verify-guide",
      "needs": [],
      "required": true,
      "action": {
        "kind": "source_paths",
        "expectations": [
          {
            "path": "docs/advanced/skill-workflows.md",
            "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
          }
        ]
      }
    }
  ]
}
```

The entrypoint must end in the exact filename `SKILL.md`. Resource paths must be regular files
within its parent directory. The entrypoint is included automatically. AgentSCM reads every
declared skill file from the immutable revision, so it does not follow host filesystem paths.

Workflows contain 1 to 32 steps. Each step may name at most 32 dependencies, and a
`source_paths` action contains 1 to 32 expectations with an 8 MiB aggregate file-content limit and a separate 16 MiB encoded-object read ceiling. Step and
workflow identifiers are bounded canonical ASCII. Dependencies must exist, be unique and form an
acyclic graph. The compiler uses a stable topological order.

## Local CLI

Use the same existing native data directory that contains the source revision and authority:

```sh
agentscm --data-dir ./agentscm-data skill-workflow plan workflow.json
agentscm --data-dir ./agentscm-data skill-workflow run local-run-1 workflow.json
agentscm --data-dir ./agentscm-data skill-workflow status local-run-1
agentscm --data-dir ./agentscm-data skill-workflow resume local-run-1 --max-steps 1
agentscm --data-dir ./agentscm-data skill-workflow cancel local-run-1
```

`plan` validates the definition and reports its plan and skill identities without running steps.
`run` starts or reopens the named run and advances it by at most 32 steps. `resume` revalidates the
stored definition, native repository, revision, current policy epoch and packaged skill bytes
before continuing. A changed binding stops continuation; historical observations remain recorded.

Run IDs are idempotency keys. Reusing an ID with the same plan returns retained state. Reusing it
for a different plan is a conflict. Each completed step is persisted, so reopening the local data
directory preserves progress. Cancellation is terminal and leaves recorded observations intact.

The JSON status includes `admission: "not_assessed"`. A successful workflow means only that its
implemented source checks matched. It does not approve, integrate, merge, deploy or authorize the
revision. The local owner and controller remain trusted, and this profile does not join hub or
native authority domains.

The compiler rejects more than 63 explicitly selected resources before source access; the automatic
`SKILL.md` makes 64 package entries. Relative package paths are limited to 512 bytes.

A partial `resume --max-steps N` may exit successfully while the JSON state is still `running`.
Inspect the state rather than treating process exit as workflow completion. A status read can
conflict with an active journal writer; retry after that operation finishes.

Local journal consistency checks bind retained definitions, ordered steps and observations. They
detect inconsistent records under the trusted local-owner model; they do not authenticate a
producer or prevent an owner from replacing coherent state. Recovery can finalize already-recorded
terminal steps after a policy change without rerunning source checks. Unfinished work still requires
the current source and policy bindings.

The native service exposes a [read-only run projection](skill-workflow-service.md) with historical
state separate from present source applicability.

For authenticated team access, configure the [hub/MCP read mapping](skill-workflow-hub.md) and use
the [dashboard run lookup](skill-workflow-ui.md). Execution and recovery remain local CLI operations.

Read-only access makes no workflow-row, execution, approval or integration changes and does not
initialize an absent journal or native store. SQLite may create or update WAL/SHM sidecars for
existing databases, including the native authority. Reads can fail if required sidecar housekeeping
is unavailable. This is logical read-only access, rather than a zero-filesystem-write guarantee.

Library callers using `advance(..., 0)` execute no new steps, but may reconcile an already
recorded all-terminal run outcome. The CLI accepts step budgets from 1 to 32.
