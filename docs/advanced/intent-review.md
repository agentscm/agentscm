---
title: "Intent review"
---

# Intent review

The existing coding agent runs a selected library skill with its normal authorized tools. AgentSCM retains the contract, task leases, findings, correction budget and candidate identities in its workflow journal. Reports are advisory: they do not qualify native admission or approve a merge.

[Intent review skill](../../skills/intent-review/SKILL.md) compares a human request with the current candidate and evidence. It is meant to run inside the implementing agent, which can inspect source, retrieve evidence, run checks, and make an authorized repair. It does not require a separate reviewer agent, grant additional access, or replace required code reviews and human approvals.

The procedure keeps human requests and explicit amendments separate from the agent's plan and claims. It checks the working tree as well as `HEAD`, and records initial findings before a bounded correction and fresh reassessment. Missing or stale evidence stays unknown. AgentSCM derives the overall outcome from criterion findings; it does not accept an aggregate model pass.

## Run from an existing agent

1. Import the intent-review skill, or another reviewed skill, in the local skill library. Publish a workflow with one standalone **Skill / rule** step. Host execution currently rejects prerequisites, multiple steps, triggers and follow-on workflows instead of bypassing them.
2. Give the implementing agent the published workflow version, step ID and existing Work ID. It extracts a compact contract from the human messages already accessible to it. Preserve exact excerpts and their provenance; label reconstructed or unavailable context explicitly. Keep the agent's plan separate.
3. The agent sends JSON to `agentscm intent-review`, claims the returned task, reads its pinned skill package, and uses its existing tools to inspect files and run relevant checks.
4. It reports every scoped criterion as `supported`, `mismatch` or `unknown`. A mismatch includes the required corrective outcome and recheck. The agent reserves a correction before editing, reports the changed candidate, then claims a fresh assessment.
5. Open the Work in the local page launched by `agentscm open` to inspect intent, coverage, findings and attempts. A browser user can record a scoped dispute or exception with a reason; neither becomes an evaluator pass.

The CLI reads one bounded JSON request from standard input and prints one JSON response. It defaults to the same private store used by `agentscm open`; `--store PATH` selects a different local store and `--root PATH` selects the Git checkout. A store inside the checkout must be ignored by Git. The checkout must have a supported Git origin. No provider account or tool permissions are provisioned.

```sh
agentscm intent-review < intent-request.json
```

`status` and `stop` requests need only the Work ID:

```json
{"operation":"stop","work_id":"wrk_example"}
```

`stop` returns the next cooperative action. It does not install a host Stop hook or stop an agent that ignores the protocol. The first host path uses the CLI through the agent's shell tool; it adds no MCP endpoint or unattended launcher. The [skill protocol](../../skills/intent-review/SKILL.md) provides request examples.

## Results and continuation

Claims return the pinned package, contract, current source and retained supplied evidence. Reports bind the attempt, session and returned fence. Successful assessment reports require every scoped criterion exactly once. Successful correction reports have no assessment and must capture a different candidate. The next operation is a fresh assessment, including after the last allowed correction.

Retrieved or generated evidence is supplied as bounded text with an ID, origin and coverage on start, claim or report. Use a new ID when evidence changes. The host hashes and retains these excerpts, binds them to its captured candidate, and labels them **caller asserted**. It does not establish that supplied command output actually came from an executed test. `host_source` identifies the captured source; it is not a test result. Citation validation proves a retained reference, not the truth of the finding.

Evidence investigation after an unknown can supply new evidence with the next assessment claim. A heartbeat extends the live lease using the same attempt/session/fence; it does not reset the root budget. Cancellation, expired leases, ambiguous delivery and repeated unsuccessful fixes remain visible.

Recovery, amendment and disposition require a cited human authorization and the CLI's `--human-authority` option, or the selected-checkout browser grant. These are trusted-local operations: the option and supplied author label are not independent proof that a human approved them. An agent must have actual user authorization before invoking them. Disposition also supplies the displayed `expected_subject_digest`; a changed candidate is rejected.

An amendment replay can conflict after the first request succeeded because its contract binding has changed. Inspect `status` and compare the retained contract/version before continuing; do not invent another operation ID to repeat an uncertain amendment.

## Scope and limits

The root Work retains the complete final contract. A checkpoint or delegated assignment is a child Work with a subset of criteria and all inherited constraints. Children share the root's budget; child success never implies integrated parent success. After an amendment, stale children need a fresh assignment under the same root. Contract history and prior findings remain inspectable.

The initial profile allows at most two correction rounds, a bounded invocation count, and at most 24 hours of wall time. Reserve enough invocations for the assessment following each repair. Restarting a session or creating a child does not replenish the budget. These limits control tracked invocations, not hidden tool actions by an agent that does not cooperate.

Capture includes tracked and nonignored untracked working bytes. The assessment's source scope may explicitly exclude ignored build products and index state, with a reason tied to a human request reference. Missing required source, submodules, unreadable or unsupported content remains a coverage gap. A changed request, scope, candidate or retained evidence invalidates the earlier result. Requested and observed model names remain caller-reported metadata; the host does not inspect the provider's actual model configuration.

Declare authorized exclusions at start, including for a clean candidate that a correction may make dirty. The scope of an existing Work cannot be changed. Without an applicable exclusion, a declared capture omission keeps the result unknown even when all criterion findings are supported. The Stop response includes the retained `assessed_source` and `source_exclusions`; extra test evidence cannot resolve a source omission. Unresolved scope requires human clarification.

Compact human excerpts and supplied evidence stay in the local store and are not sent to the Hub by this adapter. Only retain material authorized for the local review. This path does not automatically import historical transcripts, establish complete chat coverage, or implement provider-side deletion. Existing capture restrictions and human authorization still apply.

## Synthetic example

The cases in [`tests/fixtures/intent-review/`](../../tests/fixtures/intent-review/) use a request for per-account rate limiting. The initial example limits by client IP, while its tests cover only one account from one IP and pass. The expected assessment identifies the mismatch despite the green tests. The corrected example verifies that requests from two client IPs share one account limit while another account receives its own allowance. A second valid implementation uses a different limiting algorithm with the same account-level behavior. The amendment example changes the requested limit through an explicit human message and marks results for the prior contract stale.

These are synthetic inputs and expected outcomes for later host qualification. They do not claim that an agent ran the skill, performed the repair, or passed an end-to-end test. A scripted or hand-authored fixture result is not evidence of live agent execution.

## Import boundary

Importing a skill does not launch an agent. Parsing an `npx skills add` descriptor locates pinned source bytes; it does not execute `npx` or grant imported instructions authority. The existing productive prompt adapter retains its no-tool restrictions, and native qualification remains a separate path. See [workflow library](workflow-library.md) and [agent-owned workflow tasks](agent-workflow-tasks.md).
