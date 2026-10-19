# Agent-owned workflow task contract

> **Agreed contract, not an execution API.** This page fixes the phase-one schema and trust boundaries for issue #29. Task endpoints, executor enrollment/revocation operations, dispatch, signature verification and admission changes are not implemented. Existing server-executed skill workflows, controller evidence and native admission behavior remain unchanged.

The contract lets an existing coding agent describe one installed workflow task at a time, perform work locally and return bounded assertions or observations without creating a second workflow engine. The existing journal remains the durable scheduler and recovery record. The authority remains the only component that can finalize a successful exact-subject receipt and independently rechecks current evidence at approval and native landing.

The Rust types live in `skill_workflow/task_contracts.rs`. Version 1 uses capability `agentscm.agent-workflow-tasks/v1`, strict JSON decoding, bounded collections and domain-separated SHA-256 identities. Unknown fields and unsupported versions fail.

## Identity and requests

`WorkflowTaskSubject` binds repository, execution profile, protected target, immutable revision, Change and context, policy epoch, installed requirement, compiled plan, immutable skill and workflow step. `WorkflowTask` adds a logical task ID, required/optional graph status and a compact instruction entrypoint. Every offered task, including an optional step, has at least one criterion. A changed bound input produces a different task digest.

`WorkflowTaskAttempt` represents one fresh execution. It binds the task ID and digest, fresh attempt ID, immutable `claim_request_id`, immutable execution operation ID, authenticated owner and agent, executor registration and a positive bounded lease with monotonic fence. `validate_for_claim` binds the attempt to the exact claim `request_id` and limits the issued lease interval to `requested_lease_ms`. A timeout, expired lease or disconnected client does not prove that external execution stopped. A replacement gets a new fence; old-fence mutations cannot finish it.

Every mutation has a separate idempotency `request_id`. `TaskClaimRequest` defines a usable claim shape, including the new execution identity and requested bounded lease. `TaskMutationRequest` covers heartbeat, cancellation and authorized reconciliation and binds the exact task and attempt digests, execution identity, actor, executor and fence. Its canonical request digest is domain-separated by action. `TaskResultAssertion` likewise has its own result request identity. A future journal must bind each request ID to its exact canonical bytes: an identical retry returns the retained outcome, while the same ID with changed task, attempt, actor, fence, action, reason or result conflicts.

`TaskStatusRequest` is separate. It has no idempotency key, consumes no mutation identity and is always a side-effect-free exact-attempt read. Recovery-envelope reads are side effect free too.

## Instructions and named references

`InstructionEntrypoint` contains capability and schema version, workflow and policy digests, every criterion, named references and one permitted workflow progression. Required criteria stay inline; progressive loading cannot hide an obligation. Reference reads are permitted to an authenticated actor that may view or has claimed the task, independently of that progression field.

A named reference contains only its name, expected SHA-256 and canonical relative path. Paths are ASCII, use `/` as the only separator and reject empty, `.` or `..` components, leading `/`, backslashes, control characters (including NUL and DEL) and `:` drive/prefix forms. `ReferenceFetchRequest` supplies the exact task ID and digest, name, expected digest/schema and authenticated actor. The service must load the stored canonical task and derive repository, immutable revision and path; caller data is never source permission and cannot select an arbitrary path. Candidate-authored instructions remain data and cannot relax operator policy.

`ReferenceFetchResponse` preserves `available`, `missing`, `denied`, `uncertain` and `limit` as distinct bounded outcomes. Available content is bounded UTF-8 whose SHA-256 must equal both the stored reference and request commitment. There is no partial-success fallback.

`InstructionEntrypointResponse` returns an entrypoint or an explicit schema mismatch. A mismatch is not an empty task and cannot fall back to mutable client help.

## Trust and criterion results

| Trust class | Meaning | May directly satisfy admission? |
|---|---|---|
| `agent_assertion` | Authenticated agent statement. | No. |
| `registered_executor_observation` | Observation attributed to an enrolled local executor. | Only for a criterion accepting enrolled-local evidence, after later registry, revocation, signature and exact-binding checks. |
| `independent_controller_evidence` | Existing controller-owned collection and retention path. | Only through existing controller finalization and exact-current admission checks. |

`TaskResultAssertion` carries one unique row for every task criterion, with a bounded outcome, findings and evidence commitments. Counts and completeness are derived, never submitted. Task-aware validation rejects duplicate, unknown or omitted criteria, wrong task/attempt bindings and a pass with no eligible required criterion. A mixed task uses `not_claimed` for controller-only rows; a registered executor cannot claim those rows. Agent assertions remain nonqualifying regardless of their row outcomes.

Independent review remains `controller_only`. Controller evidence does not imply hosted model dispatch: the existing pinned local finite review collector owns dispatch binding and retained review records. This phase does not implement or claim authenticated review transfer between machines. Signatures attribute observations; they do not prove execution, independence or truth.

`ExecutorEnrollment::validate_scope_and_binding` checks only structural repository/step scope and exact binding. `ExecutorRevocation::matches_binding` only compares identities. Neither is admission. Later admission must transactionally load the authoritative current enrollment, key and revocation state and recheck current policy; no caller-provided epoch or revocation slice can establish currency.

Enrollment grants no repository-wide execution, extra source access, credentials, receipt creation or landing permission. Current-registry revocation invalidates observations from that registration even when retained earlier; re-enrollment uses a new epoch and does not rehabilitate old evidence. Required failure, incomplete coverage, cancellation and uncertainty remain blocking.

### Observation signing bytes

For a registered observation, clone the validated `TaskResultAssertion`, set `executor_provenance` to `null` (the field is serialized as JSON `null`, not omitted), then serialize using the compact `serde_json::to_vec` encoding specified below, in declared struct-field order: `version`, `task_id`, `task_digest`, `attempt_id`, `attempt_digest`, `request_id`, `execution_operation_id`, `agent`, `executor`, `fence`, `trust_class`, `outcome`, `criteria`, `executor_provenance`. Nested field order is `owner_id`, `agent_id` for `agent`; `registration_id`, `executor_id`, `registration_epoch`, `verification_key_id` for `executor`; and `criterion`, `outcome`, `findings`, `evidence_digests` for each criterion row. Preserve all array order, including findings and evidence digests, and serialize empty arrays as `[]`.

Emit no insignificant whitespace, BOM or trailing newline. Strings use double quotes; escape quotation mark and backslash as `\"` and `\\`, and backspace, tab, newline, form feed and carriage return as `\b`, `\t`, `\n`, `\f`, `\r`. Encode other U+0000–U+001F characters as lowercase `\u00xx` (validation still rejects NUL in findings). Emit every other character directly as UTF-8, including `/`, DEL, non-ASCII characters, emoji, U+2028 and U+2029, without Unicode normalization. Serialize integers as unsigned base-10 digits with no leading zeros, sign, fraction or exponent; retain the full `u64` value without floating-point conversion. Enum strings use their declared snake_case names. This specifies the current struct serialization, not RFC 8785 canonicalization or JSON key sorting. The `observation_serialization_golden_vector` Rust test fixes representative bytes and their digest.

Compute SHA-256 over the ASCII domain `agentscm.agent-workflow-observation.v1` followed by one NUL byte, then the JSON byte length as an unsigned 64-bit big-endian integer, then the exact JSON bytes. Store the digest as 64 lowercase hexadecimal characters. Ed25519 signs the raw 32 digest bytes decoded from that hexadecimal value; the signature is 64 bytes encoded as 128 lowercase hexadecimal characters. Current types validate shape and binding only; they do not perform cryptographic verification.

## Recovery

`TaskRecoveryEnvelope` binds the request and immutable execution identities, exact task and attempt digests and old fence. It reports one of three dispositions:

- `confirmed`: at least one confirmed effect, no unresolved effects, and only inspect/stop;
- `conclusive_rejection`: the current request had no effect, with no confirmed or unresolved effect, permitting inspect or stop in every observed state; identical safe request retry is permitted only for `pending`; or
- `uncertain_effect`: at least one typed unresolved effect, never an identical retry.

`observed_state` independently describes the existing attempt, not an effect of the rejected request. A finished, replaced or cancelled attempt can therefore truthfully accompany a conclusive rejection, but cannot authorize identical retry. Leased, expired and unknown attempts also cannot authorize that retry; the contract provides no safe retry precondition for those states.

Confirmed retained assertion requires a finished attempt. Fence supersession explicitly records the old and strictly greater fence; expiry alone never implies supersession. `prepare_fresh_attempt` is allowed only for `replaced` with confirmed monotonic fence supersession and only unresolved `execution_may_still_run`. That means an explicit authorized new execution with a fresh identity, not automatic replay. `result_acceptance_unknown`, resource custody or any additional unresolved external effect blocks fresh execution and requires inspection, reconciliation or stop.

Late old-fence results remain attributable to the old attempt and cannot erase uncertainty, finish a replacement or satisfy another revision. Recovery contains no executable command, credential or filesystem-path field.

## Deferred implementation

These types add no API, CLI command, MCP tool, queue, catalog, service, registry, signature verifier or admission rule. Existing server execution and legacy read-only MCP behavior remain unchanged. Later adapters must call one service over the existing journal and authority, preserve repository scopes, authorize every transition, and independently recheck native admission. Omitting MCP calls cannot bypass admission, and no MCP contract can promise edit interception or wake a disconnected agent.
