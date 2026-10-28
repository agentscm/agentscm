---
title: "Verified local Git snapshot export"
---

# Verified local Git snapshot export

> **Experimental (alpha).** This part of AgentSCM is not part of the main path, may change without migration, and has had limited testing. See the [advanced index](README.md).

The trusted local operator can project an exact, already integrated native revision into a separately owned bare Git mirror. GitHub remains authoritative during the shadow workflow. This bridge creates local objects and updates one local ref. It never pushes, resets or cleans a checkout, moves native targets, imports remote changes, runs candidate commands or turns Git CI into trusted native check evidence.

New mirrors automatically use export profile `agentscm-git-snapshot/v2`, preserving portable ASCII paths and regular `100644` files, executable `100755` files and valid in-tree symbolic links as `120000` blobs containing the exact target bytes. Binary and empty files are supported. Links are never materialized or followed; dangling in-tree links are supported. There is no profile-selection flag.

Existing mirrors retain `agentscm-git-snapshot/v1` behavior: regular files only, with executable and symlink entries refused as `unsupported entry kind`. Their retained commit/retry identities are unchanged. To export entry kinds from an older installation, create a new mirror; existing mirrors are not silently upgraded.

Native revisions do not represent submodules; capture rejects them or records its explicit exclusions. Both profiles reconstruct the entire tree, so removed files stay removed. Export does not restore excluded entries. `.git` and the native reserved namespaces are rejected on export. The limit is 64 MiB per file, 128 MiB total, fewer than 100,000 files plus directories, and at most 128 directory levels. Executable entries preserve the Git executable kind, not arbitrary Unix permission bits. There is no full-history or evidence-semantic migration. Git reconciliation and offline workspace backup retain their separate entry-kind limitations; v2 export does not extend them.

## Commands

Initialize native data first. Export commands open only an existing current-schema authority in read-only mode; they never migrate it or revoke approvals. Only `export` opens the existing native object directory; inspection does not initialize missing source storage. Cooperative access locks and SQLite coordination files remain local filesystem metadata. Keep the mirror outside the native data directory and any source checkout. The initializer rejects destinations nested under an existing Git checkout, bare repository or native workspace/data directory. The mirror destination must not exist; an existing directory is rejected even if empty. An interrupted initialization is left for inspection and is never automatically adopted. Select a trusted absolute Git executable, defaulting to `/usr/bin/git`.

```sh
agentscm --data-dir ./native-data git-export init --destination ./owned-mirror.git --target main
```

The mirror owns only `refs/heads/agentscm` and is bound to the native repository identity, native target and canonical destination path. It is SHA-1 only. Moving a mirror, changing its config or adding alternate object stores requires a separately designed migration; the exporter rejects it.

To retain existing local Git ancestry, supply both seed flags when creating a new mirror. `--seed-commit` must be one full lowercase 40-digit SHA-1 commit OID; names, tags, abbreviations and SHA-256 objects are rejected. `--seed-repository` is local scope only: initialization reads stored Git objects for that exact commit and its complete ancestry, without reading refs or worktree files, changing the source repository, configuring alternates or using the network. A dirty source worktree is allowed because only the stored commit closure is copied. Shallow repositories and graft metadata are rejected before the destination is created.

```sh
agentscm --data-dir ./native-data git-export init --destination ./owned-mirror.git --target main \
  --seed-repository ./local-checkout --seed-commit FULL_SHA1_COMMIT
```

Seed qualification and copying are bounded to 100,000 reachable commit, tree and blob objects, 64 MiB per raw object and 128 MiB total raw object bytes. Each object is read with `cat-file`, written with `hash-object --no-filters`, and required to reproduce its source OID; the retained commit/tree closure is then read back. This per-object subprocess work can be slow, and each subprocess has a 60-second deadline; there is no whole-initialization deadline or progress guarantee. Failure can leave a partially initialized, unowned destination for inspection. It is never automatically adopted; choose a new destination after resolving the failure.

Existing v1 unseeded mirrors retain ownership format version 1; existing v1 seeded mirrors use ownership format version 2. New v2 mirrors use ownership format version 3, with or without a seed. All seeded mirrors durably retain the seed object format, commit and tree. Older binaries reject v2 mirrors. Neither ownership nor journal format numbers are the export profile version. New mirrors start with export journal schema version 1. The first explicit external check-result record upgrades that journal transactionally to schema version 2; ordinary export, status and lookup operations do not migrate it. This exporter accepts both journal versions, while older binaries reject version 2. Older binaries that know only ownership version 1 also reject a seeded mirror. The seed source may be removed after successful initialization because the copied closure is self-contained in the owned mirror.

Write a fixed request outside source repositories. `event_sequence` selects an actual retained native integration or initialization event. For an unseeded mirror, the first `expected_oid` is `null`; for a seeded mirror, it must be the retained seed commit. Each later export must name the previous mirrored commit. The timestamp is Unix seconds in UTC and is fixed for both author and committer.

```json
{
  "event_sequence": 1,
  "expected_oid": null,
  "metadata": {
    "name": "Local operator",
    "email": "operator@example.invalid",
    "timestamp": 1780000000,
    "message": "Export reviewed native snapshot"
  }
}
```

```sh
agentscm --data-dir ./native-data git-export export --destination ./owned-mirror.git export.json
agentscm --data-dir ./native-data git-export status --destination ./owned-mirror.git --event 1
agentscm --data-dir ./native-data git-export lookup --destination ./owned-mirror.git --revision NATIVE_REVISION
agentscm --data-dir ./native-data git-export record-check --destination ./owned-mirror.git check.json
agentscm --data-dir ./native-data git-export checks --destination ./owned-mirror.git --event 1
```

To push an exported commit to GitHub as a branch and pull request, use [`agentscm hub publish`](../hub.md#publishing-a-revision): it reads the mirror read-only and the hub does the forge calls, so forge credentials never reach this machine. The mirror must be seeded from the commit the forge already has.

`Mirror` provides the same operations as a library. `Mirror::initialize` creates an unseeded mirror. `Mirror::initialize_with_seed(executable, destination, native_repository, target, seed_repository, seed_commit)` creates a seeded mirror with the same qualification rules as the CLI. The destination remains outside native data and source checkouts. The native authority and source engine must belong to the intended local repository.

## Journal and retries

`agentscm-mirror.json` records local ownership and, for a seeded mirror, the durable seed commit/tree mapping. `agentscm-export.sqlite` is the durable metadata sidecar with application ID `1095190616`. It retains the exact event, fixed request, prior expected Git OID, native repository and revision, Git tree and commit, state and last error. Journal version 2 additionally retains at most 256 immutable external observations. Every retained mapping consumer also enforces the stored-payload byte limits below; those limits can be reached before the record-count limit. `agentscm-export.lock` serializes exporters and observation writers through a kernel file lock that releases on process exit. A shared `local_recovery::AccessGuard` lease is retained from before journal/Git writes until after the mirror journal closes; this lets cooperating backup tooling reject active mirror handles. Older binaries, raw Git/SQLite tools and editors still require physical shutdown by the operator. These local files are operator-controlled records, not authentication or independent execution evidence.

States are `pending` after the fixed request is saved, `verified` after the complete tree and bytes have been read back, `mirrored` after the compare-and-swap succeeds, or `error` after a failed attempt. A failure returns an error; inspect the separate status command for the retained record. `current_oid` and `ref_matches` describe the currently observed Git ref separately from the historical outcome. `object_error` reports a current connectivity failure without hiding the retained record. Status checks commit/tree identity and reachable-object presence; it does not collect fresh native byte evidence. A mirrored record can later become historical or disagree with the ref.

Retry the identical request after an interruption or uncertain reply. Metadata cannot be changed for the same event. Even for a previously mirrored request, the exporter rebuilds the same tree and commit, verifies every supported path/mode and byte, and compares the mapping with any saved identity. If the ref already equals that commit, it reconciles the acknowledgement. Otherwise it atomically compares the ref with the fixed expected OID before moving it. A different ref, a changed request or an unfinished earlier export blocks advancement. Re-verifying a successful mirrored record never rewrites its historical journal state or ref, including when verification fails. Only the latest retained request may be retried; a historical request after a newer request or a previously mirrored event whose ref moved fails; inspect status and the newer journal records. Never delete a record or reset the ref to make a stale request succeed. If native integration succeeded but export failed, the native integration remains successful and export remains unfinished.

Git runs with a cleared environment, disabled global/system configuration, templates, hooks, lazy fetch, replace objects, automatic maintenance and signing. The mirror's local config must match initialization. Plumbing writes raw blobs/trees/commits without an index, checkout or filters; the reference update uses `--no-deref` and expected-OID comparison. Explicit symbolic-ref inspection rejects dangling symbolic refs as well as resolved ones. Arbitrary concurrent structural filesystem mutation is outside this trusted local profile. The entire tree listing and every blob are read back before the ref update. Loose-object and reference fsync are requested. A power failure still depends on the filesystem and Git's durability guarantees; no host power-loss qualification is claimed.

Each export also checks the expected parent and new Git commit closure for missing objects, without fetching. This includes copied seed ancestry. Seed intake is limited to 100,000 reachable objects. A combined exported lineage is limited to 200,001 reachable objects, enough for the largest supported seed plus one maximum-profile native snapshot commit and its root tree; later history can still reach this finite ceiling. Missing or corrupted current snapshot objects are rebuilt or cause a failure; a broken historical or seed object blocks advancement. `status` still returns the retained export record and reports the present connectivity failure in `object_error`.

The journal grows with exported events. Lookup and advancement stop beyond 100,000 retained events or 8 MiB of selected mapping payloads. They stream SQL keys and lazily select each mapping within the remaining budget in one SQLite snapshot; exhaustion returns an error without inserting a request, moving a ref or returning a partial lookup. Each Git subprocess has a 60-second deadline and bounded output. There is no whole-export latency guarantee; this implementation writes and reads each file individually and can be slow for large trees.

## Backup and evidence boundaries

Preserve the complete mirror, journal, ownership record and exact native state together in a quiesced backup. The external mirror must be an explicit attachment to native backup tooling. Restoring native state does not rewind Git; inspect the actual ref and reconcile pending exports before resuming. A restored or copied owner file is not proof that an old writer was stopped. The seed preserves Git ancestry only: it does not import that history into native revisions, create native integration events, or provide capture-mode support for historical Git trees. No remote publication or live cutover is authorized by this command.

`git-export lookup --revision NATIVE_REVISION` returns the retained export records for that exact native revision. A mirrored record supplies the exact Git commit and tree corresponding to its retained native integration event. To retain a local check observation, run the diagnostic against that exact commit, then submit a bounded request such as `{"event_sequence":1,"result_id":"local-fsck-1","tested_oid":"0123456789012345678901234567890123456789","outcome":"passed","details":"git fsck --no-reflogs --no-dangling TESTED_OID"}`. `result_id` is constrained ASCII up to 128 bytes, `details` is optional and at most 4,096 bytes, and the complete request is at most 16 KiB. The tested OID must equal the commit in an existing mirrored event mapping. An identical result-ID retry is idempotent; changing any field for that ID is rejected. Historical mirrored commits remain eligible after later exports because binding uses the retained event mapping rather than the current branch.

Before using any retained mapping, the exporter validates the retained mapping's SQL event key, embedded event and request sequences, owned repository/target/profile, supported legacy or current event shape, native identities, state and commit/tree/error consistency. Pending records require neither OID and no error; Verified and Mirrored records require both valid OIDs and no error; Error records retain a bounded error and either both OIDs or neither, preserving real fixed-request retries. Every prior Mirrored mapping must have an exact commit/tree and matching fixed parent before lineage advances; an absent ref and corrupt commit:null cannot restart the lineage. Result association additionally requires Mirrored state. These checks precede new export insertion, ref movement and result-schema migration or insertion. A persisted result is checked against its SQL result identity/event key, fixed unverified provenance and underlying historical mapping on both lookup and retry. Lookups use one read transaction so those records come from a coherent journal snapshot. The retained mapping profile must equal the owned mirror profile, including for status, reverse lookup, fixed-request retries and external result association. These are structural consistency checks, not authentication or native admission.

All retained mapping reads, including export retries, prior lineage, reverse lookup, status and result association, use the same 64 KiB per-row bounded reader and structural validator; stored result reads are limited to 32 KiB per row. Payloads are selected with an atomic stored-length check before decoding, rather than read in full and sliced. A result operation has an 8 MiB cumulative selected-payload budget, including underlying mapping reads; listing charges repeated mappings each time they are read. SQL result identities are limited to 128 bytes before selection. A listing that exceeds a byte or 256-record limit returns an error without a partial list. A collection can exceed the byte budget before reaching 256 records; an event filter helps only when it selects a smaller subset within the budget. The limits do not delete records, upgrade their provenance or change the journal schema.

For a local diagnostic, first obtain the exact commit from `lookup` and replace `TESTED_OID` below with that full 40-character OID. Replace `/absolute/path/to/owned-mirror.git` and `/usr/bin/git` with the intended owned mirror and trusted absolute Git executable. Run the diagnostic with the exporter's static environment and configuration isolation, record its actual exit result, and retain that operator observation with `record-check`:

```sh
env -i \
  GIT_CONFIG_NOSYSTEM=1 \
  GIT_CONFIG_GLOBAL=/dev/null \
  GIT_ATTR_NOSYSTEM=1 \
  GIT_TERMINAL_PROMPT=0 \
  HOME=/nonexistent \
  LC_ALL=C \
  /usr/bin/git \
  -C /absolute/path/to/owned-mirror.git \
  --no-replace-objects \
  --no-lazy-fetch \
  --no-optional-locks \
  --literal-pathspecs \
  -c core.hooksPath=/dev/null \
  -c core.fsmonitor=false \
  -c commit.gpgSign=false \
  -c gc.auto=0 \
  -c maintenance.auto=false \
  fsck --no-reflogs --no-dangling TESTED_OID
```

This diagnostic does not use the exporter's write-specific fsync configuration because it performs no intended writes. The retained record has `record_kind` `external_check_result` and provenance `local_operator_unverified`. It proves only that the operator declared this outcome for the exact retained mapping; AgentSCM does not observe or authenticate the command invocation or its exit status. It does not modify current export status or become native check evidence, an approval, a review context, or protected-target admission. Actual CI systems need their own authenticated result collection; native evidence remains in the native authority's separate check lifecycle.

Quiesced recovery attachments preserve a version 2 mirror journal, including its unverified external observations, alongside the owned Git objects and metadata. Their provenance and evidence limits remain unchanged after backup and restore.

Synthetic tests cover complete trees with binary bytes, empty files and complete deletion to an empty tree across serial integrations; fixed-request reopen; an actual process `SIGKILL` while paused before the ref compare-and-swap; a modeled lost acknowledgement after the ref update; ref drift; ownership/config rejection; disabled hooks; unsupported paths; and writer-lock contention. The pre-CAS process-kill scenario is authored in the black-box tests under `tests/`; the post-CAS lost-reply boundary is modeled by retained state rather than process termination. Host power loss and process-kill during seeded initialization remain unobserved. Test authoring alone does not establish that tests were run or that a real shadow run was observed.

Synthetic v2 fixtures cover seeded and unseeded mirrors, exact mixed-kind trees, mode-only changes, regular-to-link changes, link retargeting, deletions to an empty tree, reopen/retry, historical result association, profile-mismatch rejection and quiesced mirror attachment backup/restore. These fixtures do not establish live forge publication, full repository dogfooding, process-kill coverage for every v2 boundary or workspace symlink backup support.
