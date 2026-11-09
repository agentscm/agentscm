---
title: "Hub local-validation summaries"
---

# Hub local-validation summaries

The opt-in Hub mode retains exact signed validation evidence and maintains one informational summary comment per GitHub pull request through the connected forge identity. Every configured requirement has its own result, commit, freshness and omissions. Independent verification of the archive publishes the statuses; the rendered comment grants no authority. Self-hosted Hubs have the same implementation and enrollment rules; there is no hosted-service entitlement or paywall.

This is an alpha, Unix-only, canonical github.com profile. Synthetic runtime tests cover PAT and App-owned summary identities; actual installation permissions and live operation remain unqualified. GitLab, Forgejo, GitHub Enterprise and combined merge-group validation are unsupported here. Synthetic Git, forge, HTTP and database tests cover the protocol; a live multi-requirement, multi-revision publish → update → fetch → verify → status lifecycle has not been qualified. Do not interpret a green product PR as live forge or Hub container end-to-end qualification.

## Choose one publication mode

The [standalone local-validation guide](local-validation.md) describes the existing `run`, `show`, `verify`, `post` and Actions `reconcile` path. It remains available explicitly: `post` still emits one comment per envelope. Hub mode uses `submit` and the Hub worker instead. An unavailable Hub leaves a retained pending submission and a nonzero command result; it never automatically falls back to `post` or Actions.

Choose one canonical Hub and one connected App identity for each repository. Independent Hubs do not share publication journals and cannot coordinate one comment. The default local/self-hosted Hub can run on a laptop, but sleeping or disconnecting that laptop suspends reconciliation and submission availability. Use an operator-managed persistent host when the team needs continuous verification. Self-hosting requires durable database and object storage, backups and service availability; there is no automatic failover.

Activation is deliberate and repository-scoped; installing or upgrading the binary does not enroll a repository. Preserve existing issuer enrollment in the reviewed default-branch `.github/local-validation/registry.json`, along with recipe, policies and optional `checks.json`. A Hub login or upload token does not enroll a signing key. Signed worker assertions do not independently prove command execution.

Before activation, update the existing GitHub App to grant at least **Issues: read** and subscribe to **issue_comment**, including created, edited and deleted events. Existing installations need their owners to approve added permissions manually; a new App manifest does not update those installations. GitHub documents the permission requirement for [issue_comment](https://docs.github.com/en/webhooks/webhook-events-and-payloads#issue_comment). Retain the existing Contents read, Pull requests write and Commit statuses write permissions used to fetch source, manage the summary and publish verification results. App permission approval and webhook delivery remain operator checks.

A Hub administrator enables the connected repository through the authenticated human-session API: `POST /v1/hub/admin/repos/{repository}/validation-summary` with JSON `{"enabled":true}`. The selected repository's current connection owns this setting; it is not a client-selected signing authority. Setting `enabled` to false stops new intake and verification eligibility while retaining history and publication recovery state.

Stop the legacy Actions verifier/status writer and all standalone comment publishers before cleanup. Update branch protection/rulesets to require the intended Hub App integration identity and each configured status context; the former GitHub Actions identity is a different writer even when the context text matches. Disabling an old workflow, changing required checks, and updating App permissions are operator-owned changes, not automatic rollout actions. Keep the old requirements enforced until the replacement is deliberately qualified; do not remove protection merely to obtain a green result.

## Submit from existing wrappers

`tools/attest-pr.sh` and consumer wrappers that call `local-validation post` still use standalone publication. Enabling Hub mode without changing those calls creates a second writer. Update the wrapper's publication step explicitly; keep its trusted-configuration pinning, clean-source checks, service setup and signing logic. Do not use `post` as an error handler for `submit`.

For example, after the existing `run` step writes its JSON result to `$result_file` and retains the evidence in the absolute owner-private `$state` directory:

```sh
envelope=$(jq -er .envelope_sha256 "$result_file")
"$bin" local-validation submit \
  --state-dir "$state" --attestation "$envelope" \
  --hub-url "$hub_url" --repository-id "$hub_repository_id" \
  --proposal "$pr" --token-file "$hub_token_file"
```

The Hub token file must be absolute and owner-private and contain a writable repository agent token for the exact connected repository. It is distinct from the signing key and GitHub cleanup token. Repeat `--attestation` to upload several retained requirements together, up to 32 entries. The command retains a canonical bundle and pending destination record before transport, then a matching durable receipt. Retry the same command after an uncertain submission; identical canonical bytes for the same repository and proposal return the same receipt/generation. The receipt means `received_unverified`, never that validation passed. A failed validation can be retained and submitted as evidence of failure; each wrapper should preserve its chosen command exit behavior.

## Archive and clean up legacy comments

First stop every standalone writer, including other people's machines and scheduled workflows. GitHub comment deletion has no atomic body comparison: a re-read narrows the race but cannot protect an edit made between the read and DELETE. Keep writers stopped throughout migration and keep a backup of local migration state.

Archive without deletion first, using an absolute owner-private state directory and token files:

```sh
agentscm local-validation migrate \
  --state-dir "$migration_state" --github-repo acme/widget --proposal 123 \
  --github-token-file "$github_token_file" --hub-token-file "$hub_token_file" \
  --hub-url "$hub_url" --repository-id "$hub_repository_id"
```

The command reads the canonical GitHub repository and pins its default-branch commit before fetching `.github/local-validation/registry.json` (override with `--registry-path` for a different trusted relative path). It discovers the complete bounded PR comment inventory before doing cleanup. It parses legacy v1 carriers, checks exact envelope/record commitments, verifies the signature and current issuer trust against that pinned registry, checks the local-validation requirement identity and issuer's allowed check kind, and checks canonical repository binding. It archives valid historical or expired evidence unchanged; migration never sets a passing status or certifies that historical source is current. The Hub verifier must independently assess the current head, context and complete requirement set.

For each eligible legacy comment it creates a deterministic single-pair bundle, obtains a matching durable receipt, and reads the exact bundle back from the authorized Hub generation. **Every eligible comment must finish archive/readback before any deletion.** Local private `migration-*.json` files retain the source comment ID, author, body digest, pinned registry commit/digest, destination and Hub receipt. This provenance is unsigned migration metadata, not issuer authority, and is not embedded in the signed payload or automatically uploaded as a separate provenance object. Retain/export it with your audit records. The original envelope and record files remain byte-for-byte intact locally and in the Hub carrier.

Rerun the same command with `--delete-legacy` to request cleanup. Only comments belonging to the GitHub token's authenticated author can be deleted. A mixed-author run archives all eligible evidence, cleans up its own author’s comments, and reports any remaining comments as incomplete. Other authors must perform their own migration/cleanup; granting a maintainer broader GitHub deletion rights does not change this rule. Each body and author is re-read before deletion. A changed body refuses cleanup; after an uncertain delete reply, exact 404 readback confirms absence. A retry rediscovering an already deleted comment does not recreate it or resubmit its evidence. The local journal records `delete_attempting` before transport and `deleted_or_absent` after confirmation. If the process dies before recording confirmation, retain that journal and verify the exact comment and archived generation before treating cleanup as complete.

Malformed carriers, forged signatures, unauthorized/revoked issuers, incomplete pagination and refused/unavailable archives fail explicitly. They remain unresolved; there is no arbitrary marker-based deletion. Archive-only output reports the count of legacy comments remaining. Cleanup rechecks discovery and fails if any first-line legacy marker remains; ordinary discussion quoting a marker is ignored. A canonical Hub summary uses a different marker and is left alone. No migration result promises one comment while unresolved legacy comments remain. If a malformed/untrusted legacy artifact requires manual disposition, preserve it separately and record the operator decision; it cannot be turned into valid signed evidence by migration.

## Wire contract, verification and history

The JSON carrier is `{"version":1,"entries":[{"envelope_base64":"…","record_base64":"…"}]}`. Base64 preserves exact independently signed bytes. Canonical carrier ordering sorts by exact envelope digest and refuses duplicate envelope digests. The carrier is limited to 2 MiB and 32 pairs; individual evidence/record parser limits also apply. Carrier validation checks repository and record commitments, not execution truth or issuer authority.

| Interface | Contract |
| --- | --- |
| `POST /v1/hub/repos/{repository}/validation-evidence/{proposal}` | Writable repository agent authentication; enabled exact repository/connection; canonical durable object before transactional receipt; identical bundle digest reuses its receipt. |
| `GET /v1/hub/repos/{repository}/validation-evidence/{proposal}?after=0&limit=100` | Authorized read token; ascending generations; follow `next_after` until null, including all pages. Limits are 1–100; `after` is a nonnegative generation. |
| `GET /v1/hub/repos/{repository}/validation-evidence/{proposal}/{generation}` | Authorized exact-generation read; returns the carrier. Missing generation is 404; missing/corrupt backing object fails as unavailable, never passing evidence. |
| `POST /v1/hub/admin/repos/{repository}/validation-summary` | Admin human session; JSON `{"enabled":true}` or `{"enabled":false}`; no agent-token administration. |

A receipt binds repository, proposal, monotonically increasing generation, bundle SHA-256, receiving user/time and `received_unverified` state. The Hub persists exact carrier objects under the `hub-validation` namespace and retains SQL receipt history. The summary links the authorized history API and recent digests rather than embedding all historical envelopes. Neither comment display nor GitHub edit history is the evidence store. SQL receipt possession alone does not establish backing-object availability.

The verifier reads trusted checks, recipe, policies and registry from the default branch, fetches source, and checks each requirement independently. It evaluates signature, current issuer authorization, recipe/policy/registry binding, source commit/tree/base context, required commands, coverage and freshness. Missing requirements remain pending; stale evidence cannot become a current-head pass. Each trusted policy retains the standalone `exempt_paths` behavior: a nonempty change consisting entirely of exempt paths, with no verifier trust-input changes, succeeds as “not required” without signed evidence. The summary identifies the observed head for that source-only decision. Shared heads cannot receive exemptions; recipe command-path selection remains separate. Unavailable signed archives or receipt inventories are shown explicitly and leave evidence-dependent requirements pending; they do not suppress an independently established source-only exemption. Applicable attempts are selected by signed issue time, then envelope digest as a deterministic tie-breaker, independently per requirement. Arrival generation and comment order do not select a winner; a later-arriving older attempt cannot replace a newer applicable result. Preserve accurately synchronized signing-host clocks; there is no global run-order oracle across identities.

The controller's durable publication journal fences concurrent workers through compare-and-swap and retains all prior status slots for invalidation. It reserves before writing, invalidates prior slots, writes each verdict, then publishes the summary as the final operation marker. The comment's App identity and journal-owned ID must match; a forgeable marker alone is never adopted. Identical completed content avoids a needless edit; definite deletion permits recreation only after complete owned discovery. Webhook-driven and periodic reconciliation use retained evidence, so edits and deleted presentation do not create validation authority.

## Uncertain writes, retention and operations

An uncertain status/comment mutation leaves the publication journal in `Attempting`; a lease expiry or process restart does not authorize a resend. Exact final-operation-marker readback can prove completion of the preceding status sequence. An ambiguous mutation with unproved completion remains blocked in `Attempting`, without another mutation or automatic resend. Definite read/source failures and recovered unsent `Preparing` reservations attempt pending invalidation of retained status slots in the same reconciliation sweep; they do not reset an uncertain attempt. Unsent invalidation reservations retry at most three times when evidence fences change; exhausting that bound reports failure and may leave prior statuses unchanged. API refusal, ambiguous replies or unresolved `Attempting` state may also prevent invalidation of an old success. This is an explicit availability/security limit, not a guaranteed fail-closed remote status under all forge failures.

There is no public publication-journal inspection/reset or force-recovery CLI/API. Operators should inspect Hub logs, GitHub deliveries, the actual comment and required statuses, preserve the database and archive objects, and repair permissions/connectivity before letting normal reconciliation recover. Do not delete journal rows, blindly retry uncertain writes, or adopt a comment by marker to unblock a check. A stuck attempt without proving readback/invalidation needs maintainer investigation; this guide does not invent a runnable reset command.

Retention is indefinite until explicit repository administration/removal; there is no automatic TTL or validation archive collector, and conversation retention does not apply. Export all paginated receipts and exact-generation carriers while authorized, and preserve the local migration provenance files separately. Back up the whole Hub SQL database, including `hub_validation_summary_config`, `hub_validation_proposals`, `hub_validation_evidence`, `hub_validation_publication`, repository/connection/user/token authorization bindings and SQL schema-migration metadata. Local migration provenance is stored in the private client state directory, not a Hub SQL provenance table; back up those files separately. Back up the complete immutable object backend, including every `hub-validation` object; a SQL-only dump or Git backup cannot reconstruct signed bytes. Use a consistent database/object backup boundary and verify restoration by authorized exact-generation readback. Restoring older snapshots can lose receipts and uncertain-write recovery authority; do not resume publication until the operator has reconciled that loss. No production backup/restore, retention-scale or failover qualification is claimed.

The controller bounds a complete proposal inventory to 1,000 receipts and 64 MiB decoded evidence, retains at most 512 status slots, reads at most 30 comment pages and three open-pull pages, and refuses incomplete discovery. Trusted checks are limited to 20. Source fetch has a 300-second deadline and bounded subprocess output, but no total fetched-disk quota. History remains exportable in pages when the verification bound is exceeded; exceeding it does not authorize partial passing verification. Increase/decompose requirements or address retention explicitly through reviewed product changes rather than silently dropping history. Synthetic fixtures cover concurrency, lost replies, stale source, ownership, unavailable evidence and migration refusal; live provider behavior and production capacity remain unverified.
