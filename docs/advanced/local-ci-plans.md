---
title: "Immutable local CI plans"
---

# Immutable local CI plans

The experimental Rust `local_ci` library defines a shared, command-first contract for exact-source local CI requirements, immutable plans and reported coverage. Existing repository scripts can be declared with explicit argv, working directory and environment without Actions YAML, `act`, Docker or native source migration. Git and native source identities use the same resolver.

This is a contract implementation with synthetic and temporary-Git conformance tests. It adds no job executor, signing service, result endpoint or forge admission. Device enrollment now exists separately ([trusted local CI devices](local-ci-devices.md)). The command and Actions adapter inputs are read-only views over the same resolved inventory; they do not dispatch jobs or establish runtime qualification. Controlled execution, optional pinned `act` support and device/evidence verification remain separate milestone work ([#270](https://github.com/agentscm/agentscm/issues/270), [#271](https://github.com/agentscm/agentscm/issues/271), [#272](https://github.com/agentscm/agentscm/issues/272), [#273](https://github.com/agentscm/agentscm/issues/273)).

## Trusted requirements and exact subjects

`Requirements` version 1 declares a repository, positive policy epoch, exact workflow-byte commitments, explicit job/matrix cells and permitted omissions. Each cell binds argv, repository-relative working directory, environment, resource requirements, runner profile digest, platform, architecture, toolchain digest, service definitions, dependency identities and required capabilities. Cells are already expanded: this contract does not evaluate YAML expressions, discover jobs or expand a matrix. A controller must establish the complete required inventory before resolution; an adapter cannot independently shrink it.

The controller selects a `ConfigBinding` independently of the candidate: repository-scoped immutable source, configuration path and SHA-256 of the raw configuration bytes. `TrustedRequirements::load` verifies those bytes and every declared workflow at that source. A configuration file whose bytes happen to match a hash has no authority unless the caller authorized that source selection.

`PlanInputs` supplies the independently observed subject, explicit event inputs, selected omissions and controller-qualified capabilities. A `pr_head` subject binds the exact head and base; a `merge_result` also binds the separately observed result and executes against that result. Git identities include both commit and tree; native identities include repository and revision. Mixed source kinds and repositories are rejected. The reader checks source membership, while the controller remains responsible for authorization and for establishing that a merge result has the intended ancestry and event meaning.

Resolution copies the entire trusted inventory into `ImmutablePlan`, including any omitted cells. Each selected omission must match a trusted `(job, cell, reason)` allowance exactly. Required cells must have resolved dependencies and all required capabilities in the controller's qualified set. Missing, observed-only, unavailable or unknown dependency identities block resolution. The capability set is a controller declaration for the selected profile, not discovered worker truth or a runtime qualification result. Actual per-profile resource admission and isolation enforcement belong to execution.

Every declared workflow must have the reviewed byte digest at the execution source as well. Candidate edits to requirements cannot remove jobs or services from the independently selected configuration. Candidate edits to a bound workflow or reviewed script are refused until review results in an explicitly selected new trusted configuration revision with matching commitments. Hooks are optional triggers and are never discovered or executed by this contract.

The plan binds the configuration source and raw digest, canonical requirements digest and policy epoch, exact subject, workflow commitments, event inputs, cells and omissions. Different commits with the same tree still produce different plans. Changed base, merge result, policy, workflow, dependency, event or runner inputs require a different plan. There is no inferred cross-revision evidence reuse.

## Immutable source access

`SourceReader` is a controller-owned interface for exact revision verification and bounded file reads. Implementations must enforce repository authorization, revision membership and byte limits; an implementation that ignores source identities cannot establish those facts. Native integration can supply this interface directly without changing Git adoption.

The Unix `GitSourceReader` reads only exact local objects. It verifies commit/tree bindings and accepts regular or executable blobs, refusing symlinks and submodules for configuration/workflow files. Each Git subprocess has a 30-second deadline and an output bound. Reads disable replacement refs, graft files, lazy network fetch, optional locks, hooks and fsmonitor, clear the inherited environment, use a private home and disable system/global Git configuration. No checkout, clean filter or candidate command runs. The authorized checkout, repository label and Git executable on the controller's `PATH` remain trusted inputs. Missing objects and interrupted or oversized reads fail; the reader does not fetch them.

## Version 1 bytes and bounds

Use the types' `from_slice`, `validate`, `canonical_bytes` and `digest` methods. `from_slice` rejects malformed or trailing JSON, duplicate object keys recursively, unknown fields and unsupported versions. Direct generic serde decoding alone does not provide the recursive duplicate-map check or semantic validation.

Canonical bytes are compact UTF-8 JSON produced by the declared version 1 Rust schema with serde_json escaping, no insignificant whitespace and no trailing newline. Struct fields retain declaration order; optional `reviewed_script` is serialized as `null` when absent. String maps use ascending key order. This is a schema-specific format, not a claim of RFC 8785 JSON canonicalization. The [schema implementation](../../src/local_ci/mod.rs) defines nested field order; [frozen fixtures](../../tests/fixtures/local-ci/README.md) and integration golden vectors pin its output.

| Document | Top-level canonical field order |
|---|---|
| Requirements | `version`, `repository`, `policy_epoch`, `workflows`, `cells`, `permitted_omissions` |
| Plan | `version`, `subject`, `configuration`, `policy_epoch`, `requirements_sha256`, `workflows`, `event_inputs`, `cells`, `omissions` |
| Coverage | `version`, `plan_sha256`, `cells` |

Workflows sort by path; cells sort by `(job, id)`; services sort by ID; dependencies sort by identity; capabilities sort lexically; omissions sort by `(job, cell)`. Coverage observations sort by `(job, cell)` and service ID. Duplicates are rejected before sorting. Argv order is preserved. Workflow/configuration commitments hash exact file bytes, including whitespace, rather than parsed content. Resource numbers are integer JSON values; digests are lowercase hexadecimal SHA-256.

For document digests, compute SHA-256 over the ASCII domain below, one zero byte, then the canonical document bytes:

| Identity | Domain before the zero byte |
|---|---|
| Requirements/policy | `agentscm.local-ci.requirements.v1` |
| Immutable plan | `agentscm.local-ci.plan.v1` |
| Coverage report | `agentscm.local-ci.coverage.v1` |

The configuration's raw SHA-256 and canonical requirements digest are intentionally distinct. A plan artifact also uses ordinary SHA-256 of canonical plan bytes, which differs from the plan's domain-separated identity.

Documents are limited to 256 KiB and each workflow file to 64 KiB. Requirements and plans need 1–32 workflows and 1–128 cells; at most 128 omissions are allowed. Each cell permits at most 32 services, dependencies and capabilities, and each string map at most 32 entries. Argv has 1–64 items. Text/path values are at most 2,048 bytes and NUL-free; identifiers reuse the portable-evidence 128-byte ASCII identifier rules. Paths reject absolute paths, traversal, empty components, control characters, backslashes, colons and reserved source-control namespaces; `.` is allowed only as a working directory. The recursive parser also caps each JSON array and object at 256 entries. Resource declarations require positive values bounded at 24 hours, 256,000 CPU millis and 2^50 bytes each for memory/disk. These are schema ceilings, not claims that a runner can supply them.

## Independent coverage assessment and evidence mapping

`verify_coverage` reloads the currently trusted configuration and recomputes the expected plan from independently supplied current inputs. It never uses submitted plan fields as expected authority. The submitted canonical plan and report plan digest must match that recomputed plan. Supplying the report's subject, omissions or capability claims back as trusted inputs would defeat this boundary and is unsupported.

Reports declare exact cell and service identities with `success`, `failed`, `skipped`, `cancelled`, `unavailable`, `incomplete` or `unknown` outcomes. Missing or extra cells/services and observations for omitted cells make the inventory incomplete; duplicates are invalid. `inventory_complete` describes structural coverage independently of outcome. Complete successful cells with all required services successful produce a declared `Pass`; a completed failed cell produces `Fail`. Skipped/uncertain cells or any unsuccessful required service produce `Incomplete`, even if all identities are present. An exit status alone cannot establish these observations or their truth.

`ImmutablePlan::evidence_subject` maps into existing portable `CheckSubject`: context is the plan digest, recipe is the canonical requirements digest, and runner/environment identities are domain-separated commitments over canonical plan inputs. Native execution uses its exact revision; Git execution uses `agentscm.local-ci.git-source.v1` plus zero byte and the serialized Git source identity. Runner and environment domains are `agentscm.local-ci.runners.v1` and `agentscm.local-ci.environment.v1`, respectively. The caller independently supplies the current change ID/version. `evidence_artifact` supplies canonical plan bytes and an existing `ArtifactRecord` named `local-ci-plan`. The profile uses `command/v1` and requirement ID `local-ci-plan/v1`; it creates no parallel envelope or signing authority.

A coverage result assesses declarations only. Signature integrity, device authorization/revocation, freshness, execution basis, artifact availability and policy sufficiency must still be verified separately. A valid signature on false worker claims would not prove execution. These helpers do not accept submissions into the Hub or authorize a forge status, native landing or merge.

## Adoption shapes and legacy migration

The synthetic fixtures were designed after inspecting existing local-validation scripts at frozen adopter revisions. They contain invented repositories, runner/dependency identities and workflow bytes; no adopter code is copied and no fixture executes a script or starts a device.

Spicy at `0224fa55c8f161854fa9ace7753fce0ebaacb0cf` declares a one-hour Bash pre-push script with Bun/Node dependencies and configurable worker count. Its script can return a cached success for 24 hours across commits using an index-based key that excludes its own script and selected Markdown. That exit zero does not establish fresh execution for the new exact-commit plan. Adoption must qualify or remove that internal reuse before treating it as fresh local CI evidence; the synthetic reviewed-script fixture does not qualify the real cache.

RNQC at `27ca9a067e4e7c5721fb0b213418b8e247d14967` declares separate two-hour `bun run e2e:ios` and `bun run e2e:android` commands. The inspected runner provisions its own simulator/emulator and artifacts and cleans owned resources. iOS requires Xcode/simctl; Android requires SDK/JDK/emulator tooling. The mobile fixture therefore retains two explicit cells and different host requirements. It does not establish that either runtime, cleanup behavior or a generic container substitute is qualified.

Existing [local-validation recipes, records and verification](local-validation.md) retain their original formats and assurance. Migration requires an explicit new trusted requirements configuration, independently selected source/subject/event/profile inputs, a newly resolved plan and exact coverage observations. Existing signatures or legacy successful records are not automatically converted to this profile. Git use does not require importing history or migrating source to the native store. No real Spicy/RNQC job, pinned `act` run, mobile runtime, device lifecycle or live Hub verification has been qualified by these conformance fixtures.
