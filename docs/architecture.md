---
title: "Native source-control architecture"
---

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

The multi-user [coordination hub](hub.md) has its own database (SQLite or PostgreSQL), forge sign-in, agent tokens, intents and an event log. General Hub events are append-only; [stand-up work events](standup.md#storage-maintenance) support explicit operator compaction with retained-history boundaries. The opt-in [Community workspace](advanced/community-workspace.md) adds SQLite membership, settings and a durable execution queue. Operator-owned profiles join permitted Hub repositories to selected native stores and installed workflow requirements. This is a single-server trusted-team pilot, not a general multi-tenant native API.

## Review context
A stable Change stores identity and a Change-version-checked current-context pointer. Immutable ReviewContext records alone own intent, acceptance criteria, scope, exclusions, omissions and declared evidence boundaries. Source revisions do not include review context. Registration permits structurally valid incomplete contexts for inspection; approval and integration reject them. Both operations transactionally check repository, current change/context, target/base and policy bindings. New operations use context semantics version 2; migrated historical events remain version 1.

Context digests attest to canonical bytes, not execution truth. The local owner can still supply all administrative claims. Built-in source-path and isolated command collectors can provide evidence for a requesting current context. A bounded Claude Code adapter supports the local workflow loop; authenticated portable execution evidence and broader policy integration remain required. See [review context format](advanced/review-context.md).

## Local workflow continuation

Ordinary filesystem workspaces use a separate durable journal and local generation counter. A [task-scoped workspace](advanced/task-workspaces.md) can project explicit paths from the full immutable base and expand them durably; checkpoints overlay only that scope, so unmaterialized base paths remain unchanged. Omitting scope retains full-workspace behavior. Checkpoints publish complete revisions; undo/recovery preserve divergent user edits. This is local materialization control, not a partial-source identity, virtual filesystem or permission boundary.

Offline bundles (see [offline sync](advanced/sync.md)) exchange exact source objects, qualify their closure and publish revisions only after their dependencies. They transfer no authority. Controller-built-in source-path checks bind immutable recipes and exact current review subjects, and a requesting context cannot be approved or integrated without matching successful evidence. These are trusted-local primitives. The isolated [command collector](advanced/execution.md) runs declared recipes with durable attempts and explicit recovery; a context requesting its capability needs successful evidence for that exact subject before approval or integration. Model execution, authenticated transfer and unattended service recovery remain incomplete.

Immutable reads also accept bounded controller-supplied encoded-object streams
through a shared [source reader](advanced/source-reader.md). Authorization, manifest
membership and retention belong to that controller; source parsing and complete
closure qualification do not confer those permissions.

The local CLI exposes bounded manifest listing, stat, read, diff and history for
workspace-free exploration. The experimental team profile separately exposes
authenticated paged reads from retained publications. Neither read surface creates
an authenticated remote task or workspace mutation lifecycle.

Durable [local merge sessions](advanced/merge.md) retain all best bases, deterministic
conflicts and exact current resolutions. Root-last R2 publication qualifies
actual candidate closure before a terminal outcome; opaque completion receipts
bind separate staged workspace application. Local journal references are not
collector-visible retention pins or protected admission authority.

## Skill workflows

The local [skill workflow runner](advanced/skill-workflows.md) compiles a bounded graph from an
immutable `SKILL.md` package and explicit resources. It runs source-path checks and, on Unix,
pinned command recipes through the existing container collector. Its journal retains progress;
command attempts fence concurrent claims, cancellation and recovery. Claude Code review uses
the finite review controller with durable dispatch reservations and no candidate tools. A run
remains an observation; an installed target requirement additionally needs a controller-finalized
exact-subject receipt in the authority database before approval or native landing.

The native API and operator-mapped hub/MCP/dashboard inspect retained status and current binding.
The Unix `agent-task` CLI uses that same authenticated facade for typed task operations and uses
the existing trusted-local collector for an exact server-derived descriptor. It requires the
authoritative immutable native source and context in its selected local data directory; task-only
profiles do not bootstrap source to an independent collector machine.
The [Claude Code Stop loop](advanced/claude-code-workflows.md) is an opt-in local trigger; the
authority independently enforces installed native target requirements. The existing file-edit
hooks retain contention-only behavior. The optional Community workspace exposes authenticated
queue/cancel, explicit human approval and native landing through operator-pinned profiles.
Current membership and repository permissions are checked at request, dispatch and finalization;
the native authority independently enforces exact-subject admission. Source upload, remote
checkpoint/context management, other agent adapters and automatic failover remain planned.

## Planned capabilities
The following are not yet implemented; see the [roadmap](roadmap.md). Broader agent/model adapters, general authenticated multi-user native source APIs, PostgreSQL/object-store authority and caches, full offline sync/workspace lifecycle, privacy/retention/GC and distributed leases, Git history import and live authority cutover, automatic recovery/failover and measured scale qualification remain planned. Bounded local source exchange, merge, Git export, shared workflow execution and offline recovery primitives already exist; they do not complete those operational capabilities. Advanced semantic merge, transparent virtual filesystems, cross-region active-active writes and blockchain anchoring are optional extensions.

## Deployment
Install a published binary through `npx`, the release install script or a manual download, or run a versioned OCI container; see [install options](install.md). Contributors can build from source. A local/small-team single writer uses SQLite on a durable local volume. Never multiply SQLite authority containers over a shared directory. Later workers/read caches scale separately from fenced PostgreSQL authority. Keep candidate execution separate from controller credentials; no mounted host container socket by default. Containers simplify installation, not database consistency.
