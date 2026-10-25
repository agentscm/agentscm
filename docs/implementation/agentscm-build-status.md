# Change record implementation status

> **Local validation recorded 24 October 2026:** M0 and the M1 capture/receipt/handoff slice are implemented and locally validated within the limits below. A first M6 slice (opt-in shared conversations) is implemented and tested locally; the remainder of M6 and M2–M5 and M7–M8 remain separate work.

## Baseline and contracts

The change-record work extends the existing durable Work model. It does not replace Work ownership, human approval, protected integration, or forge state.

- Shared change-record schema: version 1.
- Hub storage migration: paired SQLite and PostgreSQL named migration `change_records`. Its upgrade, restore, and parity checks remain part of final validation.
- Shared records contain bounded typed event and action facts plus opaque artifact identities. Conversation bodies, tool arguments, command output, captured source bytes, and manifests remain in an operator-chosen private local spool, except the redacted conversation bodies of a checkout that opted into `shared-conversation` mode (see the M6 slice below).
- Execution outcome, verification result, source applicability, assurance, evidence availability, coverage, drift, freshness, and decision state remain separate fields.
- A configured exact recipe result is `caller_asserted`. The Hub is not an independent recipe registry or approval authority.
- An agent-authored Work decision is projected as a proposal. Human approval remains separate and unknown until an authoritative human decision exists.

## Completed design slice: M0

M0 established the versioned capture and receipt contracts, finite input and storage boundaries, source and artifact identities, privacy boundary, migration ownership, and consistent HTTP, CLI, MCP, and UI projection shape. Unsupported or missing evidence is represented explicitly rather than converted into success.

## Implemented slice: M1 — action capture and receipt

The M1 slice includes:

- configure-once Claude Code hooks that resolve the current authorized Work and generation for each invocation;
- a private bounded spool with append-before-delivery behavior, explicit gaps, retry-safe identities, quarantine, compaction, and sanitized handling when no exact Work target is available;
- a v4 private-spool identity that binds the checkout, Hub repository/worktree namespace and local
  lock inodes. Operational settings may change only on the same retained identity, capacities may
  stay equal or increase, and disabling hooks preserves the lineage. A changed or legacy identity
  is refused when a guarded identity check observes it rather than treated as an empty replacement.
  Identity-preserving offline path moves are supported; live moves, restores and replacements are
  unsupported and are not described as continuously contained;
- bounded read-only spool inspection and local export with a 64 MiB exact-hash limit per source
  file. An exported or copied spool has no capture authority and is not resumable. The minimum configured action lifecycle requires 48 records and
  65,536 total bytes; this floor qualifies the declared minimal lifecycle rather than arbitrary
  retained history;
- Git and native source bindings, including private snapshots for permitted dirty-worktree content and explicit omissions;
- ordinary native action receipts whose verification result remains unknown when the host does not provide an explicit result;
- registered direct-argv recipes that observe a supervised child process exit, preserve bounded output locally, and emit one configured verification receipt without duplicating the matching native Bash action;
- bounded event and action ingestion in SQLite and PostgreSQL;
- declared captured-event and retained prior-version provenance on existing Work intent/decision
  records, without treating a declaration as proof of content or human approval;
- bounded observed setup fields with explicit unknown components, plus opt-in session-bound
  provider usage through existing Work Insights; request totals exclude session-cumulative records;
- one concise change-record projection used by authenticated HTTP, `change-capture show`, the read-only MCP tool, and the Hub UI;
- a transcript-free handoff containing Work intent, action-bound source, latest action, retained agent proposal, known gaps, and next action. An execution receipt alone does not establish that its source was tested.

This slice is Unix-focused. The Claude Code adapter covers a defined set of visible lifecycle events; Codex exposes a narrower event set and is not claimed as equivalent coverage. Neither path is described as full transcript capture. Protected independent execution, human approval, and forge readiness are outside M1. Shared conversation retention is not part of M1; its first slice is recorded under M6 below.

M1 deliberately does not switch an already bound checkout to a different spool identity. Read-only
inspection and export preserve diagnosis, and verified reboot reconciliation can close a retained
configured-recipe attempt without executing it. An uncertain attempt for a different Work remains
blocked and is not reassociated with the current Work.

## Validation status

The Rust and final UI gates used source-bundle SHA-256
`45a1ef88e5c897dd2fa16e6a388bf0ddc478c56f282a5c8c6921feb49ff2509b`,
based on `fc24a7169a007a1c2b7225ba70264ab86c010d35`, before the final nonfunctional
documentation and trailing-blank-line cleanup. Container and native evidence was reused after
comparing production inputs; subsequent changes are that whitespace-only UI cleanup, documentation
and a corrected second-Work test fixture. Earlier handoff
exercise evidence was retained only after comparing the unchanged projection inputs.

- **Rust and stores:** formatting, strict all-target Clippy, 2,564 nextest cases and the separate
  doctest command passed on macOS arm64 with local PostgreSQL and S3 services supplied. The
  default suite leaves 52 cases skipped unexecuted; those are not reported as passes.
- **UI:** dependency installation, lint, build and notice checks passed; the final UI source
  cleanup removes only one trailing blank line. The final live run passed 188 unit/integration cases and 75 browser cases
  across Chromium, Firefox, WebKit, mobile Chrome and mobile Safari, with no skips.
- **Container:** the unmodified Dockerfile built for Linux arm64 and the repository smoke
  script passed, including persistent server restart, Hub health and embedded UI serving.
- **Native capture:** a real Claude Code interaction retained the actual visible prompt,
  assistant message, two tool calls/results, source/recipe/setup bindings and partial usage.
  Replay, compaction and exclusion of private bodies from shared records were checked. The
  final launcher accidentally supplied 1,336 additional stdin bytes after its intended 704-byte
  prompt. Its original verifier failed; a separate offline audit verified faithful capture of
  the actual 2,040-byte prompt and the remaining assertions without another model call. This
  qualifies the observed interaction, not intended-prompt isolation.
- **Interface parity:** authenticated HTTP, CLI, MCP and the current UI matched for the actual
  retained Work after normalizing only the observation timestamp. Source, proposed decision
  and next action were visible, including at a 390-pixel mobile width without overflow.

The passive-capture measurement used one warm-up pair and eight alternating repetitions of
four identical events per arm: 66 invocations in total, 32 measured invocations per arm, no
nonzero/signalled measurement and no measurement stderr. The invocation ledger contained zero
AgentSCM-initiated model calls. Four-event wall time was 9.532/11.657/13.641 ms
(minimum/median/maximum) for the baseline and 1,005.314/1,175.403/1,313.154 ms with capture.
Measured user/system CPU totals were 0.020/0.043 s and 2.069/3.762 s respectively; reported
block input/output operations were zero for both arms. Maximum RSS was 1,441,792 versus
31,555,584 bytes. The local spool grew from 458 bytes in seven files to 46,856 bytes in 116
files. A post-measurement wrapper self-hash used an obsolete filename and failed; the retained
measurements and byte-identical measurement core were validated offline. The wrapper is not
reported as passing. These shared-host observations establish no production capacity or savings.

One matched synthetic handoff exercise used two fresh recipients without transcript access.
Both identified the exact source, proposed decision and next action, and each completed the
same one-test continuation with zero identification errors and four evidence questions.
Ordinary-handoff identification/completion took 30.310/56.286 s; record-assisted results took
43.208/67.369 s. Timing includes dispatch gaps. Access restrictions were instructional, not OS
isolation. This pair establishes bounded functional evidence, not a speed, quality or cost
improvement or the M8 pilot.

## Remaining milestones

- **M2 — Durable readiness:** extend action-level retry and recovery into work-level budgets, no-progress stopping, reconnect behavior, and consistent readiness across interfaces.
- **M3 — Queue-safe forge:** qualify exact-head and merge-group subjects, App identity, stale and group recovery, protection reporting, and provider contracts before any required-check rollout.
- **M4 — Setup and risk governance:** complete setup provenance, deterministic risk policy, governed recipe provenance, authoritative human decisions, and protected-execution qualification.
- **M5 — Complete handoff and review UX:** add search and drill-down, full timeline results, reproduction guidance, coordination handover, readiness explanations, an MCP App surface, and a second qualified agent adapter.
- **M6 — Shared retention (first slice implemented, rest open):** implemented and tested locally against synthetic fixtures, the in-process Hub on SQLite (PostgreSQL parity tests skip unless a database is configured) and a local object store: an opt-in per-checkout `shared-conversation` capture mode (default `metadata-only`; `change-capture set-mode`, `configure --capture-mode`, and a macOS tray toggle), best-effort client-side redaction with a `redacted` gap, a 1 MiB per-body cap, upload after the Hub accepts the event, reads limited to the Work owner's tokens and Hub admins (403 for other tokens), one global retention period (`hub serve --shared-conversation-retention-days`, default 30) with an hourly expiry sweep, and early deletion by the owner or an admin through the CLI or HTTP. Still open before M6 can be called complete: team-level retention policy beyond that single global setting; admin UI deletion (the Hub UI is read-only, so deletion is CLI or HTTP only); redaction verification on the Hub; export of shared bodies; derivative invalidation; backup and restore qualification; live S3-compatible delete qualification; and qualification of real Claude Code payloads for the `Stop`, `PreCompact` and subagent hooks, which are covered by synthetic fixtures only.
- **M7 — Feedback and outcomes:** add bounded review and CI feedback, capacity policy, accepted-source mapping, revert handling, and complete usage and outcome views that preserve unknowns and abandoned work.
- **M8 — Pilot and release qualification:** establish cross-surface parity, live adapter and forge coverage, upgrade and backup recovery, supported platform coverage, release gates, and an honestly measured transcript-withheld handoff pilot.

## Release gates and unsupported claims

This ledger records local M1 validation. It does not establish deployment or M8 release qualification. Windows, Linux amd64, the default suite's skipped cases, multi-machine operation and equivalent capture across other agent hosts were not qualified by this run. Publication and merge status are recorded by the associated signed commit and pull request; they are not inferred from local test results.

Do not claim complete conversation capture, independent verification, protected execution, equivalent support across agent hosts, merge-queue readiness, deployment readiness, or measured cost or quality improvement from M1. Do not claim that shared conversations are secret-free (redaction is best effort and unverified by the Hub), that a complete shared-transcript lifecycle exists (M6 is only partly implemented), that expiry or deletion removes copies a reader already made, or that the tray toggle has had human native UI acceptance.
