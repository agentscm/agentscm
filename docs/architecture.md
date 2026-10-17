# Native source-control architecture

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See the [README](../README.md) for status and the [changelog](../CHANGELOG.md) for known limits.

AgentSCM is a native Rust source-control system for concurrent human and agent work. Git and the forge remain fully supported. Native source operations (checkpoints, reviews, landings, merge sessions) do not invoke Git, but collisions, snapshot capture, reconcile and export run the Git executable on the host. Collisions require Git 2.43 or newer; snapshot capture and reconcile check for 2.45.0 or newer; export has no version check of its own, so use 2.45 or newer throughout. The design targets incremental content-addressed source storage, selective reads, isolated workspaces, local offline work, conditional integration, automated review, audit evidence, privacy and self-hosting.

## Invariants
- Immutable source identities; a small edit changes only bounded paths in a persistent manifest. Reading a revision never replays all preceding edits.
- Admission publishes durable source before a revision can be integrated. Protected target, exact-subject authorization consumption and journal update form one transaction. Changed context or policy invalidates approval. Retries against the intact authority return the original durable outcome. Recovery from an older backup cannot promise outcomes it no longer retains.
- Tenant/repository authorization applies to cache hits and misses. Signatures, execution, coverage, retention and policy sufficiency are distinct facts.
- Untrusted workers cannot hold authority or signing credentials. A container alone is not a hostile-code security guarantee.
- Required retention roots and publication/backup leases precede collection. Missing, expired or incomplete evidence never becomes successful verification.
- Rust is required for first-party engine/server/CLI/sync/policy/verifier/review orchestration. Established databases and external model runtimes remain dependencies.

## Alpha status
The current version is a local native alpha: immutable blobs, persistent manifest trie, incremental revisions, selective reads/diff, a SQLite integration authority, bound local human approvals, durable journal/idempotency, CLI, read-only source HTTP API and container packaging. The local owner is trusted; no multi-tenant or production network-security claim is made. No garbage collector runs yet, so objects persist until the operator removes the store. The current object encoding and API are experimental; a stable interoperable protocol requires conformance and migration work that is not yet done (see the [roadmap](roadmap.md)).

The multi-user [coordination hub](hub.md) is a separate alpha service with its own database (SQLite or PostgreSQL), forge sign-in, agent tokens, intents and an append-only event log. It is not yet joined to the native ledger.

## Review context
A stable Change stores identity and a Change-version-checked current-context pointer. Immutable ReviewContext records alone own intent, acceptance criteria, scope, exclusions, omissions and declared evidence boundaries. Source revisions do not include review context. Registration permits structurally valid incomplete contexts for inspection; approval and integration reject them. Both operations transactionally check repository, current change/context, target/base and policy bindings. New operations use context semantics version 2; migrated historical events remain version 1.

Context digests attest to canonical bytes, not execution truth. The local owner can still supply all administrative claims. Independent check collection, model execution and authorization are subsequent required work. See [review context format](advanced/review-context.md).

## Local workflow continuation

Ordinary filesystem workspaces use a separate durable journal and local generation counter. Checkpoints publish captured bytes; undo/recovery preserve divergent user edits. Offline bundles (see [offline sync](advanced/sync.md)) exchange exact source objects, qualify their closure and publish revisions only after their dependencies. They transfer no authority. Controller-built-in source-path checks bind immutable recipes and exact current review subjects, and a requesting context cannot be approved or integrated without matching successful evidence. These are trusted-local primitives; full command/model execution, merge convergence and authenticated transfer remain required.

Immutable reads also accept bounded controller-supplied encoded-object streams
through a shared [source reader](advanced/source-reader.md). Authorization, manifest
membership and retention belong to that controller; source parsing and complete
closure qualification do not confer those permissions.

Durable [local merge sessions](advanced/merge.md) retain all best bases, deterministic
conflicts and exact current resolutions. Root-last R2 publication qualifies
actual candidate closure before a terminal outcome; opaque completion receipts
bind separate staged workspace application. Local journal references are not
collector-visible retention pins or protected admission authority.

## Planned capabilities
The following are not yet implemented; see the [roadmap](roadmap.md). Operational automated-review adapter with independent trusted check collection, full offline sync/workspace lifecycle, privacy/retention/GC and leases, authenticated multi-user APIs, PostgreSQL/object-store authority and caches, Git history import and a verified round trip back to Git, backup/failover and measured scale qualification remain planned. Advanced semantic merge, transparent virtual filesystems, cross-region active-active writes and blockchain anchoring are optional extensions.

## Deployment
Distribute an OCI container (the supported install) and, for contributors, a native binary. A local/small-team single writer uses SQLite on a durable local volume. Never multiply SQLite authority containers over a shared directory. Later workers/read caches scale separately from fenced PostgreSQL authority. Keep candidate execution separate from controller credentials; no mounted host container socket by default. Containers simplify installation, not database consistency.
