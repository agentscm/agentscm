---
title: "Review contexts"
---

# Review contexts

> **Experimental (alpha).** This part of AgentSCM is not part of the main path, may change without migration, and has had limited testing. See the [advanced index](README.md).

A source Revision identifies immutable source. A Change identifies a continuing line of work. A ReviewContext identifies the exact task and declared inputs being approved. Changing criteria or a recipe creates a new context without changing source identity.

Run `agentscm init` and use its `repository` value below. Replace the example revision and digest values with lowercase SHA-256 values. Input identities describe declared inputs; they do not prove those inputs were used or that a check ran.

```json
{
  "version": 1,
  "change": {"repository": "REPOSITORY_ID", "id": "change-1"},
  "change_version": 1,
  "revision": "REVISION_SHA256",
  "base": null,
  "target": "main",
  "policy_epoch": 0,
  "intent": "Add the requested behavior",
  "acceptance_criteria": ["Describe a concrete acceptance condition"],
  "scope": ["src/lib.rs"],
  "exclusions": [],
  "omissions": [],
  "dependencies": [],
  "recipe": {"state": "digest", "sha256": "RECIPE_SHA256"},
  "toolchain": {"state": "digest", "sha256": "TOOLCHAIN_SHA256"},
  "environment": {"state": "digest", "sha256": "ENVIRONMENT_SHA256"},
  "required_evidence": [],
  "required_capabilities": ["agentscm.review-context/v1"]
}
```

For an existing target, `base` is `{"revision":"BASE_SHA256","target_version":1}` with the actual version. Integration still requires the source revision's first parent to be that base. `prepare-landing` rebinds a supplied context to the landing revision it publishes. Context registration is not a source-availability check. The source engine checks the revision during integration.

Use `start context.json` for Change version 1. Use `context EXPECTED_VERSION context.json` to install the next Change version. The input's `change_version` must be exactly one greater than the expected version. Concurrent updates cannot both succeed at the same Change version. Historical contexts remain available by digest; `show REPOSITORY_ID CHANGE_ID` resolves the current pointer and displays that exact context.

Approvals and integrations name `change`, `change_version`, and `context_digest`, alongside their existing target/revision/policy fields. The authority checks that they all describe the same current context. A new Change version invalidates old approvals even if its text later returns to an earlier value. Target movement and policy changes also invalidate stale context bindings. An identical retry of a completed integration returns its original result even after these later changes.

## Missing input is explicit

Recipe, toolchain, environment and evidence identities use one of:

- `{"state":"digest","sha256":"LOWERCASE_SHA256"}`
- `{"state":"unknown"}`
- `{"state":"unavailable","reason":"Describe what is unavailable"}`

A dependency is `{"identity":"dependency-name","state":{"state":"declared"}}`. Its state can instead be `resolved` with `revision`, `observed` with optional `revision` and `evidence_digest`, `unknown`, or `unavailable` with `reason`. These are evidence classes within a declared boundary. An empty list or observed read set does not establish universal dependency completeness.

Required evidence is `{"identity":"check-name","digest":{"state":"digest","sha256":"LOWERCASE_SHA256"}}`. A digest records a claim about an artifact. AgentSCM neither retrieves that artifact nor validates its issuer or execution. Readiness means the required context fields are present and understood; it is not a verification result.

Structurally valid incomplete contexts can be stored and inspected. Unknown/unavailable required inputs, nonempty omissions, and unsupported required capabilities deny approval and integration. The local authority supports `agentscm.review-context/v1` and the built-in `agentscm.checks.source-paths/v1` check. On Unix it also supports `agentscm.checks.command/v1`, an isolated container command check; see [OCI command collector](execution.md). The source-path check verifies declared immutable-file hashes; it does not run tests or models. See [checks and evidence](checks.md). Context readiness alone does not verify arbitrary evidence issuers or execution.

## Canonical encoding and limits

Version 1 uses compact UTF-8 JSON in the declared field order, with the escaping emitted by the pinned `serde_json` implementation. Set-like lists sort bytewise; duplicates and duplicate dependency/evidence identities are rejected. Text is neither trimmed nor Unicode-normalized. The digest is lowercase SHA-256 of these bytes, with no trailing newline. There are no maps or floating-point values. This is an experimental versioned encoding, not a claim of RFC 8785 compatibility. Fixed vectors live in `tests/context_contract.rs`.

Canonical contexts are limited to 64 KiB. Identity strings are bounded portable ASCII; intent is at most 8 KiB, and individual text entries/reasons at most 1 KiB. Collections have count limits before canonicalization. Invalid or oversized input fails explicitly. Context CLI input is also limited to 64 KiB before JSON parsing, including whitespace.

## Migration and operating boundary

Opening a supported older authority upgrades schema v1 through v8 to v9 transactionally. Historical v1 events retain legacy semantics version 1 and no fabricated context. Unconsumed legacy approvals are revoked. Exact previously completed legacy requests can still retrieve their retained result; new requests require a Change binding. New events use semantics version 2. Source object IDs do not change. Keep an independently verified consistent backup before upgrading valuable data; older binaries reject newer schemas and there is no downgrade path.

Each data directory is one trusted local repository authority. Its repository identity persists through reopen and migration; copying the directory copies that identity and does not create a safely fenced replica. Local filesystem access still grants administrative power. This version provides neither multi-tenant authorization nor recovery from a stale database copy. Do not use context readiness as a compliance or production-security badge.
