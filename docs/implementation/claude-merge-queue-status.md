# GitHub merge queue lane status (Claude)

Owner: Claude. Integration owner for shared files: Codex. Codex's ledger: `docs/implementation/agentscm-build-status.md` on `feat/change-record-capture-v1`.

## Baseline

- Base: `f85d4e4a3604f43a82773263e2227f728e638a27`. Branch: `feat/github-merge-queue-checks` (worktree `agentscm-merge-queue`).
- No push, PR, merge, protection change or deployment has happened.

## Inventory (baseline gaps)

- Hub workflow status `agentscm/workflow/<target>` (`src/hub/forge_check.rs`) is posted only on pull request heads. `webhook_work` (`src/hub/sync.rs`) drops every event except `pull_request` and `push`; the App manifest subscribes only to those. A required context therefore blocks queued merges.
- Protection is reported as `protection: "unverified"` (`src/hub/publish.rs`); nothing reads rulesets or the status creator.
- `agentscm/local-validation` is a separate verifier-workflow status (`worker_asserted`). This lane does not read or change it, and a green local-validation status satisfies nothing here.

## Shared-file requests to Codex (integration owner)

1. Named migration `merge_groups` (`src/hub/migrations/named/merge_groups.{sqlite,pg}.sql`): one new table `hub_merge_groups`. Named, so it takes no integer slot. Please do not reuse the name.
2. `Store` trait (`src/hub/store.rs`) plus both impls: new methods appended at the end of the forge-check block only (`merge_group`, `observe_merge_group`, `active_merge_groups`, `reserve_merge_group_check`, `complete_merge_group_check`). Delivered as one isolated commit so it can be applied or rebased as a narrow patch.
3. `src/hub/sync.rs` webhook filter and App manifest `default_events`: add `merge_group`. Small, isolated hunk.
4. Projection: `forge_check::merge_group::Projection` (serializable) via `forge_check::merge_group::group_projection(state, repo_id, group_sha)`. Codex wires it into HTTP/UI/MCP; this lane does not edit `publish.rs` views, `http.rs`, UI or MCP.
5. `docs/hub.md` (Codex-owned), in "Workflow status for candidate pull requests": replace the sentence "Merge queue (`merge_group`) is not supported; a required context will block queued merges." with "Merge queue groups get the same context on the group commit, under stricter exact-source rules; see [GitHub merge queues](advanced/github-merge-queue.md)." Add the guide to `docs/README.md`'s index (Codex-owned).

## Contract

`merge-queue-contract/v1`, defined in `docs/advanced/github-merge-queue.md#integration-contract`. Pin the commit that lands this file on `feat/github-merge-queue-checks`.

## Commits (on `f85d4e4`)

1. `1d9a3e7` Add a durable journal for GitHub merge queue groups — the shared-file patch (store trait, both impls, `connection.rs` types, named migration, conformance test). Apply or rebase as one unit.
2. `3c8ad24` Post the Hub workflow status on merge queue groups — `forge_check/merge_group.rs` (owned), `github.rs` reads, poller hook in `forge_check.rs`, `merge_group` routing and manifest in `sync.rs`.
3. `0fb3bce` Test merge queue group checks end to end against the fake GitHub.
4. Docs and this ledger (next commit).

## Validation (local macOS, this branch)

- `cargo clippy --all-targets --locked -- -D warnings`: clean. `cargo fmt --check`: clean.
- `cargo nextest run --locked --lib -E 'test(merge_group) | test(manifest)'`: 17 passed.
- `AGENTSCM_TEST_DATABASE_URL=<local PostgreSQL 18> cargo nextest run --locked -E 'binary(hub_store_conformance)'`: 60 passed, 0 skipped (`merge_group_journal` on both backends).
- Same database: `binary(hub_merge_queue) | binary(hub_forge_check) | binary(hub_gitlab_forge_check) | binary(fake_github_smoke) | test(merge_group) | test(forge_check)`: 102 passed.
- Not run: the full `cargo nextest run --locked`, doctests, S3, container smoke. These are left for the combined gate.
- Live GitHub: not run (no authorized test repository). Fixture-only qualification.

## Remaining for integration / release

- Live qualification steps in the guide; payload field names and status-as-required-check acceptance are unconfirmed live.
- Core: a group-subject assessment so multi-entry groups can qualify (contract in the guide). Until then, multi-entry groups stay pending.
- Codex: wire `group_projection` into HTTP/UI/MCP; apply the `docs/hub.md` / `docs/README.md` patch above; changelog delta: "Hub: the opt-in workflow status is posted on GitHub merge queue groups under exact-tree rules (experimental, fake-tested)."
- Existing GitHub Apps must add `merge_queues: read` and the `merge_group` event by hand.
