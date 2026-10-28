---
title: "Offline source synchronization"
---

# Offline source synchronization

> **Experimental (alpha).** This part of AgentSCM is not part of the main path, may change without migration, and has had limited testing. See the [advanced index](README.md).

AgentSCM exchanges native source through bounded local files. Run the same sequence in each direction to exchange divergent work:

```sh
agentscm --data-dir sender sync offer offer.json REVISION...
agentscm --data-dir receiver sync request offer.json request.json
agentscm --data-dir sender sync bundle offer.json request.json source.bundle
agentscm --data-dir receiver sync import source.bundle receiver-sync-state
```

Every output path must be new. Files are completed with a no-clobber hard link, so a concurrent writer cannot replace an existing result after a precheck. The sender's offer includes the complete history and source-object closure of the named roots. The receiver requests only objects it does not already hold with a verified digest. The standalone bundle embeds the exact offer and request, followed by the requested encoded objects. Repeating an import with the same bundle and state directory returns the same source result.

This is source transfer only. It preserves the exact encoded objects and therefore their existing SHA-256 object and revision IDs. It does not transfer or modify protected targets, repository identity, Changes, ReviewContexts, approvals, policy, check evidence, integration events, operation outcomes, users, roles, or credentials. The origin repository field is an unauthenticated description. A receiver must create its own review context and satisfy its own current policy before local integration.

## Validation and interruption

The file envelope remains `agentscm-offline-bundle-v1`. An offer whose complete closure contains only `AGSCM-R1\0` revisions uses source profile `agentscm-source-v1`, version 1, with exactly `complete-revision-history`, `exact-object-bytes`, and `root-last-import`. An offer whose closure contains an `AGSCM-L2\0` leaf (an executable or symlink entry) uses `agentscm-source-v3`, version 3, with the four version 2 capabilities followed by `entry-kinds`, whatever its revision versions. A receiver that does not know source profile v3 rejects the offer. A declared profile that disagrees with the closure's entry kinds is rejected, and every symlink target in an imported closure is validated. Team source publication accepts only source profiles v1 and v2, so a closure with executable or symlink entries cannot be published there. An offer whose closure has no L2 leaf and contains any `AGSCM-R2\0` revision uses `agentscm-source-v2`, version 2, with those three capabilities followed by `multi-parent-revisions`. R1 revision bytes and IDs are unchanged, and an R2 closure can include R1 ancestors. Unknown profiles, formats, required capabilities, versions, JSON fields, and profile/object-version mismatches fail explicitly.

Bundle records have no filesystem paths and are never extracted as an archive. Import streams one encoded object at a time into a private transfer directory. It verifies each declared length, object digest and native encoding. After every record is present, qualification walks only the closure reachable from the offered revision roots and requires the offer to equal that closure exactly. Unrelated objects are rejected.

Qualification also checks:

- every root and parent is a supported revision and the complete parent DAG is acyclic;
- R1 revisions have zero or one parent, while R2 revisions have 2 through 8 ordered, unique parents;
- revision tree references and all manifest object types are correct;
- empty branches occur only at namespace roots, while compressed prefixes, branch slots, increasing depths, leaf keys, paths and namespaces agree;
- exact leaves reference blobs;
- exact and portable indexes contain the same paths and exact-leaf links;
- prefix entries and descendant counts equal those derived from the exact index; and
- file/directory and portable-name collisions are absent.

Nothing is installed in the main object store before the complete bundle passes those checks. Import then installs blobs, trees and manifest nodes, followed by non-root revisions in parent-first order across every parent edge. Offered roots are installed last. Every published revision therefore has its full history and source dependencies available, even if the process stops during installation. A durable completion marker is written only after the installed closure is reread and qualified from the destination store. Temporary staging files are not source roots. Retry verifies and reuses intact staged and installed objects; a conflicting digest path fails instead of being overwritten.

`qualify_source` exposes the same complete-closure and root-antichain validation for local callers. Its opaque result describes the exact roots, object descriptors and selected source profile observed during that call. It is point-in-time local validation: it does not retain objects, authenticate a repository, authorize publication, or prove that state remained unchanged. A retention or authority transaction must revalidate its own generations and retained objects atomically.

## Bounds

- 1 to 256 revision roots per offer; roots form an ancestry antichain.
- At most 100,000 distinct objects and 1 GiB of encoded bundle data per exchange.
- Offer and request files are each at most 16 MiB.
- Existing source limits remain: file content is at most 64 MiB and an encoded object at most 64 MiB plus 4 KiB.
- Each closure-qualification pass performs at most 1,000,000 manifest/revision visits. Import runs one pass over the staged bundle and a second pass over the installed destination closure before writing the completion marker.
- Parent-DAG validation, root-antichain validation and parent-first installation ordering are iterative. Each pass examines each applicable revision at most once and each parent edge once, including shared ancestry and diamonds. The antichain and ordering passes are each bounded by the 100,000-object exchange limit.
- Import holds one encoded object at a time, plus at most 64 KiB for streaming equality checks and bounded offer, request, closure, path and traversal metadata. It does not clone a full encoded object during publication.

Exceeding any bound fails the exchange. It never produces a successful shortened offer or incomplete success result. The local profile has no garbage collector, so installed and staged bytes persist until an operator removes the data or transfer state. This is not a retention or backup guarantee.

## File enumeration

`Engine::list_files(revision, cursor, limit)` enumerates one immutable revision's exact file index. Limits are 1 through 4,096 entries. A continuation token binds the revision, exact root and a validated root-to-frontier traversal chain, so resume does not rescan or re-emit earlier leaves. Each page scans at most 100,000 manifest nodes. `next = None` is the only completion signal; a page can be empty and still carry `next` when it consumed only branch nodes.

The token has a checksum for corruption detection, but no secret authenticator in the trusted-local profile. Parsing rejects malformed, oversized, wrong-revision/root, invalid-state and out-of-tree frontier tokens. A caller that deliberately reconstructs a valid in-tree traversal position can choose where it resumes; authenticated network pagination requires server-authenticated cursor state or a keyed token.

Each `FileEntry` contains a path and blob ID. File bytes remain a separate read capped at 64 MiB, which prevents a page from multiplying that allocation by 4,096.

## Required convergence and service work

Importing two heads does not resolve them and never moves a target. Synchronization can carry multi-parent revisions, but it does not create a merge or select merge content. Deterministic three-way merge, durable unresolved conflicts, workspace resolution and abort/recovery remain required before divergent convergence is complete. A new merged revision still requires fresh local review and conditional local integration. No remote approval can authorize that result.

The later authenticated service must authorize the repository on cache hits and misses and key logical objects by trust domain, immutable repository authority ID, source profile and object ID. Before transfer it must create durable read/sync and upload leases. Publication must recheck retained qualification generations and tombstones, atomically install reachable roots with retention, and serialize those roots and leases with garbage collection. PostgreSQL/object-store retry records use repository ID, fenced authority generation and operation ID through their supported retry horizon. This local file transport supplies source framing and qualification behavior; it does not implement authentication, cloud durability, collection, fencing, backup, failover, capacity, or network security.
