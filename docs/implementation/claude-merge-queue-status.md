---
title: "GitHub merge queue lane status (Claude)"
---

# GitHub merge queue lane status (Claude)

Owner: Claude. Integration owner for shared files: Codex. Codex's ledger: `docs/implementation/agentscm-build-status.md` on `feat/change-record-capture-v1`.

## Baseline and branch

- Base: `f85d4e4a3604f43a82773263e2227f728e638a27`. Branch: `feat/github-merge-queue-checks`.
- Contract: `merge-queue-contract/v1`, in `docs/advanced/github-merge-queue.md#integration-contract`.

## Inventory (baseline gaps, now closed or stated)

- `webhook_work` dropped `merge_group`; the App manifest did not request it. Closed.
- A required `agentscm/workflow/<target>` blocked queued merges. Closed for single-entry groups; multi-entry groups stay pending (core gap below).
- Protection was always `unverified`. The Hub now reports ruleset requirement and status source per group (read-only); classic branch protection is still not read (needs administration access).
- `agentscm/local-validation` remains a separate, worker-asserted status. This lane does not read or change it.

## Shared files this branch changes (integration owner: Codex)

Rebase `feat/change-record-capture-v1` onto this branch once it lands; expect textual conflicts only where both lanes append.

- `src/hub/store.rs`, `store_sqlite.rs`, `store_pg.rs`: eight `merge_group*` methods appended after the forge-check block.
- `src/hub/connection.rs`: `MergeGroup`, `NewMergeGroup`, `validate_group_note`.
- Named migration `merge_groups` (no integer slot). Please do not reuse the name.
- `src/hub/mod.rs`: `HubState::webhook_jobs_pending` (webhook backlog; used by tests as a completion signal).
- `src/hub/sync.rs`: `merge_group` routing, manifest permission and event, `Sync::jwt` and `pending_webhooks` visibility.
- `src/hub/publish.rs`: `forge_check.merge_queue` on the publication view.
- `docs/hub.md`, `docs/api.md`, `docs/advanced/README.md`: pointers to the guide and the API fields.
- `tests/integration/hub_store_conformance.rs`, `tests/support/fake_github.rs`: additive.

## Consumer fields for Codex (UI, MCP, readiness)

`forge_check::merge_group::{group_projection, pull_projections, queue_readiness}` and the publication API's `forge_check.merge_queue` (fields in `docs/api.md`). Do not scrape status descriptions.

## Remaining

- Core: an assessment of a combined group source so multi-entry groups can qualify (fields in the guide's contract). Until then they stay pending.
- Live qualification: GitHub refused a merge queue ruleset on a private Team-plan organization repository (422). Needs a public organization repository or a plan with private merge queues, an installed App and a webhook-reachable Hub.
- Existing GitHub Apps must add `merge_queues: read` and the `merge_group` event; `delivery` readiness reports which is missing.
- Changelog delta for the integration owner: "Hub: the opt-in workflow status is posted on GitHub merge queue groups under exact-tree rules, with delivery readiness and landed outcomes (experimental, fake-tested)."
