# Trusted automated checks

> **Experimental (alpha).** This part of AgentSCM is not part of the main path, may change without migration, and has had limited testing. See the [advanced index](README.md).

AgentSCM records check attempts and evidence against an exact native source and review subject. Check success is evidence about one execution. It is separate from human approval, installed policy sufficiency and integration authority.

## Built-in source-path check

The built-in `source_paths/v1` check reads named paths from one immutable native revision and compares their SHA-256 digests with a canonical recipe. AgentSCM itself performs the reads and writes the result to its SQLite authority. Candidate source cannot submit a successful result through this interface.

Each attempt binds all of these values:

- immutable repository authority ID;
- stable Change ID and current Change version;
- ReviewContext digest and source revision;
- canonical recipe digest;
- declared toolchain and environment digests;
- limits and check kind.

The built-in check binds the declared toolchain and environment identities. It does not execute or prove an arbitrary compiler, test toolchain or environment. It is useful source integrity and availability evidence, not a substitute for tests or model review.

Recipes are capped at 256 paths, 64 MiB of observed source, 64 KiB of result text and five minutes. Operators may choose lower values. The current in-process timeout is checked between native source reads; it cannot interrupt one read. One read can still allocate the source engine's existing 64 MiB maximum file size before the lower aggregate recipe limit is evaluated. A hard process deadline and smaller memory envelope require the isolated external worker described below.

Attempts move through explicit states:

```text
pending -> running -> success | fail | incomplete
   |          |
   +----------+-> cancelled
```

`fail` means the check completed and its predicate was false. Missing required paths and content mismatches fail. `incomplete` means AgentSCM cannot claim a result, including a read error, resource limit, timeout or uncertain interrupted execution. Reopening the authority does not change `running`: another process may still own that run. An operator must explicitly recover it to `incomplete` or cancel it using the observed run generation. Compare-and-swap generation checks prevent a late finisher from replacing that terminal recovery.

Attempt IDs are idempotency keys. Reusing an ID with identical input returns its durable record; different input is rejected. Canonical request and evidence digests detect altered bindings and stored-byte corruption. The controller alone derives terminal evidence from observations. These controls do not prevent a trusted local owner or a process with write access to `state.sqlite` from tampering with the authority.

## Recipe and context

A recipe has fixed controller ceilings and at most 256 unique portable source paths:

```json
{
  "version": 1,
  "expectations": [
    {
      "path": "src/lib.rs",
      "sha256": "LOWERCASE_SHA256"
    }
  ],
  "limits": {
    "max_paths": 32,
    "max_total_bytes": 16777216,
    "max_output_bytes": 4096,
    "timeout_ms": 30000
  }
}
```

`check-recipe RECIPE.json` returns the digest of canonical recipe bytes. Ordering expectations differently does not change this digest.

To require this check, the ReviewContext must include:

```json
{
  "recipe": {"state": "digest", "sha256": "RECIPE_SHA256"},
  "toolchain": {"state": "digest", "sha256": "TOOLCHAIN_SHA256"},
  "environment": {"state": "digest", "sha256": "ENVIRONMENT_SHA256"},
  "required_evidence": [
    {
      "identity": "source_paths/v1",
      "digest": {"state": "digest", "sha256": "RECIPE_SHA256"}
    }
  ],
  "required_capabilities": [
    "agentscm.review-context/v1",
    "agentscm.checks.source-paths/v1"
  ]
}
```

The required-evidence digest names the recipe, not a future outcome. AgentSCM rejects missing, unknown or mismatched recipe/evidence identities. A context requesting the capability cannot be approved or integrated until the authority transaction finds exact current `success` evidence for every binding above. Updating the context generation invalidates the earlier success even when the source revision and recipe are unchanged. Contexts that do not request this capability retain the current trusted-human approval behavior; there is no general installed-policy evaluator yet. An independently installed [workflow requirement](claude-code-workflows.md) additionally requires a current whole-workflow receipt, even if a Change omits the command capability.

## CLI flow

Create a pending attempt with `check-source-paths REQUEST.json`:

```json
{
  "attempt_id": "source-check-1",
  "change": {"repository": "REPOSITORY_ID", "id": "change-1"},
  "change_version": 1,
  "context_digest": "CONTEXT_SHA256",
  "revision": "REVISION_SHA256",
  "recipe": {
    "version": 1,
    "expectations": [
      {"path": "src/lib.rs", "sha256": "LOWERCASE_SHA256"}
    ],
    "limits": {
      "max_paths": 32,
      "max_total_bytes": 16777216,
      "max_output_bytes": 4096,
      "timeout_ms": 30000
    }
  }
}
```

Then use:

```sh
agentscm --data-dir ./agentscm-data check-run source-check-1 0
agentscm --data-dir ./agentscm-data check-show source-check-1
agentscm --data-dir ./agentscm-data check-evidence EVIDENCE_SHA256
```

`check-cancel ATTEMPT_ID EXPECTED_GENERATION REASON` moves a pending or running attempt to `cancelled`. `check-recover ATTEMPT_ID EXPECTED_GENERATION REASON` moves only a running attempt to `incomplete`. Running the built-in check is synchronous. A successful run normally advances generation `0` to `1` while running and to `2` when terminal evidence is stored.

## Isolated command checks

The separate [isolated command checks](execution.md) (an OCI collector) implement canonical command recipes, exact subjects, read-only source snapshots, bounded Docker supervision and controller-derived outcomes. The durable `command/v1` check connects it to the authority and CLI (`check-command`, `check-run-command`, `check-command-show`; see [execution](execution.md)); it has no HTTP route. Its synthetic tests do not qualify hostile execution. The built-in source-path capability remains unchanged and cannot stand in for command execution.

## Required next enforcement

A separate finite model review library (`src/reviewer`) adds an explicit loopback HTTP transport, controller-observed source exposure, conservative reservation seams and finding lineage. Its fixtures do not qualify a real model or hard provider limits, and no durable attempt/admission integration is supplied. Its records remain unauthenticated observations with unverified qualification.

Arbitrary tests and candidate code require an external trusted collector and a qualified isolation boundary. The controller must materialize the exact revision read-only, then start a pinned process/container profile with disposable scratch space, no authority or evidence database, no signing or provider credentials, no inherited secrets, no host/container socket and no network by default. It must fix the user, capabilities, syscall policy, process count, CPU, memory, filesystem, time and output/artifact bounds. The collector observes exit and resource facts outside the worker, hashes bounded output and writes evidence. A worker JSON file is never accepted as trusted evidence.

Qualification must use malicious fixtures that attempt authority and credential access, socket and network access, escape, process/resource exhaustion, output flooding, fabricated evidence, approval, policy changes and integration. The current built-in check does not establish this boundary. Local container service availability was not sufficient to run this qualification for this version.

Automated review also needs one real, approved model/runtime adapter, which does not exist yet. The planned narrow interface is:

```text
ModelAdapter::assess(
    request: ModelRequest,
    source: &mut BoundedSource,
    budget: ModelBudget,
) -> ModelOutcome
```

`ModelRequest` binds the same check subject, allowed source paths and one installed provider/model scope. `BoundedSource` enforces and records file, read and byte coverage. `ModelBudget` fixes attempts, wall time, source reads/bytes, prompt bytes, input/output tokens and cost micros when a pinned price schedule or provider control makes that monetary cap provable. There is no automatic provider fallback. `ModelOutcome` records requested and observed provider/model identities, usage, stop reason, findings, coverage and unresolved scope. Unknown identity, partial capture or exceeded/unprovable budget is incomplete. A model returns an assessment only and cannot issue trusted execution evidence or authorization. No model call or cloud credential is part of this version.

Full completion requires a qualified isolated test command, one bounded real model-adapter run, named policy evaluation distinct from evidence truth, atomic admission revalidation, privacy/provider controls, authenticated portable evidence, retention/deletion, PostgreSQL/object-store equivalence and the committed recovery/fencing tests. Until those pass, AgentSCM remains a trusted-local alpha and makes no hostile-worker or production automated-review claim.
