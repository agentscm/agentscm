# Agentic SCM specification, v0.2 (draft)

**Status of this document:** draft for discussion. It describes a protocol and
data model that other tools could implement on top of Git, and labels each part
against the code in this repository:

- **Implemented** — present in this repository's code and tests. Native ledger
  rows are the trusted-local, single-operator profile; hub rows are a
  multi-user service that is not production-ready. Not a scale claim.
- **Partial** — some of the behavior exists; the gap is named.
- **Proposed** — specified here; no implementation in this repository.

The [roadmap](../roadmap.md) additionally marks some Implemented rows
**Experimental** (isolated command checks, forge observation and GitHub
publication, which are exercised only against fakes or a narrow environment).
AgentSCM is alpha software, not for production.

Version 0.2 adds the coordination hub, declared intents with notices, isolated
container checks and an optional path to cutover (section 7). Like 0.1, it is a
conceptual specification: it fixes records, rules and states, not byte
formats. This repository's native encoding and API are
experimental ([architecture](../architecture.md)); versioned wire formats,
hashing rules and conformance test vectors are **Proposed**.

The key words MUST, SHOULD and MAY are used as in RFC 2119. Plain-English
definitions of every term are in [concepts](../concepts.md). Nothing here has
been measured at tens or hundreds of concurrent agents.

## 1. Problem statement

Status: context, not a feature.

Coding agents produce many concurrent lines of work against one repository.
Git and the forges built on it (GitHub, GitLab) were designed for people, and
they do a lot well: immutable content-addressed commits, cheap local branches,
offline work, atomic compare-and-swap on refs (`git update-ref <ref> <new> <old>`,
`git push --force-with-lease`), merge queues and a large tool ecosystem. This
specification keeps all of that. It targets four gaps that grow with the number
of concurrent writers:

1. **Ref contention.** A protected branch is one mutable pointer. Every landing
   is a compare-and-swap on it; with N writers, N−1 lose each round, rebase and
   rerun checks. Merge queues ([GitHub](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue),
   [GitLab merge trains](https://docs.gitlab.com/ci/pipelines/merge_trains/))
   make this orderly and run checks speculatively in parallel, but landings
   are still ordered through one ref, and one failed speculation reruns the
   entries queued behind it.
2. **No visibility of in-flight work.** A branch is invisible to others until it
   is pushed from its clone or machine, and nothing in Git records "this agent is about to edit these
   paths". Collisions are discovered at merge time, after the work is done.
   This is not only a large-team problem: two people each running several
   agents can collide.
3. **Review is not bound to exact bytes.** Forges record which commit a
   review was submitted on and, depending on settings, can dismiss approvals
   when new commits arrive. But no forge binds an approval to immutable task
   text, and the merge or squash commit that lands is a new object that no
   reviewer saw. The task statement and acceptance criteria live in mutable
   pull request or issue text.
4. **Mutable state without a ledger.** Branches, PR state and review state are
   mutable. Git's reflog is local and expires. There is no durable, append-only
   record of which exact source, task description and approval moved a branch.

Alternative and Git-compatible systems show the same pressures at much
larger scale: Google's Piper with CitC client workspaces
([Potvin and Levenberg, 2016](https://research.google/pubs/why-google-stores-billions-of-lines-of-code-in-a-single-repository/))
and Meta's [Sapling](https://github.com/facebook/sapling). This
specification takes a narrower route: it layers on an existing Git repository
first, and native authority is an optional later level (section 6).

## 2. Design principles

Status: normative intent. Principles 2–4 and 6 are **Implemented**. 1, 5, 7
and 8 are **Partial**: cutover (section 7) is Proposed; intents exist on the hub
and are announced by `agentscm intent` and a Claude Code hook; see 7 and 8.

1. **Git stays.** A team keeps its Git repository, its forge and its CI, and
   both remain fully supported. The forge remains authoritative unless a team
   chooses an explicit, recorded, reversible cutover for a given target;
   nothing requires one.
2. **Mutable pointers, immutable facts.** Git refs and branches are mutable
   backing state. The AgentSCM ledger is append-only and records immutable
   revisions, review contexts, approvals and events. Pointers move; facts do not
   change.
3. **Bind to exact bytes.** An approval names an exact revision, an exact
   review-context digest, a Change version and a policy epoch; check evidence
   names the revision, context digest, Change version and recipe. Changing any
   of them makes the approval stale, even when the source bytes are the same.
4. **Conditional moves only.** Every move of a protected target states the
   state it expects to replace. A mismatch is a conflict, never a silent
   overwrite.
5. **Announce early, decide late.** Agents publish what they intend to touch
   before they write. Overlap produces a notice, not a lock or a block.
6. **Uncertainty stays visible.** Missing, interrupted or incomplete results are
   reported as `unknown`, `incomplete` or `pending`, never as success.
7. **Retries are safe.** Every mutation SHOULD carry an operation ID.
   Retrying an identical request returns the stored outcome; reusing the ID
   with different input is rejected. Today landing operations (`integrate`,
   `initialize-target`, `prepare-landing`) and the reconcile and export
   journals do this; other commands use caller-chosen IDs or expected versions.
8. **No implicit authority.** A model response, a green check or a signature is
   evidence, not permission. Landing MUST require an explicit approval and
   SHOULD require installed policy. Today an approval plus any required
   built-in or container checks authorizes a landing; policy-gated admission
   is Proposed.

## 3. Model

### 3.1 Two layers

Status: **Partial** — immutability of revisions, review contexts and events is
implemented; lifecycle records are mutable as described below.

| Layer | Holds | Mutability | Example |
|---|---|---|---|
| Backing state (Git) | Commits, trees, branches, tags, forge PRs | Refs move; forge state changes | `refs/heads/main`, PR #12 |
| Ledger (AgentSCM) | Revisions, Changes, review contexts, approvals, events, operations | Append-only; a few explicitly versioned pointers | Integration event 42 |

A conforming ledger MUST NOT rewrite or delete a recorded revision, review
context or event. Other ledger records are mutable lifecycle state and MUST
change only through defined transitions: a target's current revision and a
Change's current context move by compare-and-swap on a version; the policy
epoch only increases; an approval's revoked and consumed flags only go from
unset to set; check attempts and operation journals move through their own
state machines.

This repository keeps two separate stores today, not yet joined: the native
ledger (one local SQLite authority) and the [hub](../hub.md) (SQLite or
PostgreSQL), which holds identities, agent tokens, intents and an event log.

In the native ledger, append-only is enforced by the API, not made
tamper-evident: the ledger is a local SQLite file that its operator controls,
with no hash chain or signatures. The hub's event table refuses updates and
deletes in the database itself (triggers on SQLite and PostgreSQL), which stops
accidental rewrites but not an administrator who controls the database. Tamper
evidence (for example signed receipts or a transparency log) is **Proposed**.

### 3.2 Records

Status: per row.

| Record | Identity | Contents | Status |
|---|---|---|---|
| Repository | Random 128-bit ID fixed at `init` | Scopes Changes, contexts, approvals, targets and events; revisions and blobs are repository-independent and keep their IDs when copied | Implemented |
| Revision | SHA-256 of a canonical native encoding | Ordered parents, persistent path manifest, file bytes as content-addressed blobs | Implemented (entries are regular, executable or symlink; Git capture accepts them, but Git export v2 preserves them; legacy export v1 and reconcile refuse executable and symlink entries; submodules are unsupported) |
| Change | Repository ID + caller-chosen Change ID | Stable identity of one line of work, plus a pointer to its current review context and a version number | Implemented |
| Review context | SHA-256 digest of canonical JSON | Intent, acceptance criteria, scope, exclusions, omissions, dependencies, revision, base, target, policy epoch, required evidence and capabilities | Implemented ([format](../advanced/review-context.md)) |
| Approval | Caller-chosen approval ID | Target, revision, context digest, Change + Change version, policy epoch; single-use | Implemented (local administrative claim, not signed) |
| Target | Name, for example `main` | Current revision and a monotonic target version | Implemented |
| Integration event | Monotonic sequence number | Kind (`integrate`, `initialize`), operation ID, before/after target state, approval, context digest, Change binding | Implemented |
| Operation | Caller-chosen operation ID | Request digest and durable outcome | Implemented |
| Check attempt and evidence | Attempt ID and run generation; evidence digest | Exact subject, recipe, limits, outcome, cleanup custody | Implemented for `source_paths/v1` and isolated `command/v1` container checks (section 4.4); records are unsigned |
| Agent candidate | Native revision ID | Revision derived from a successful isolated run's output, parented on the run's exact subject | Implemented (derived on request and content-addressed, so not stored separately) |
| Hub user | `usr_…`; unique forge provider + host + numeric user ID | Cached login, disabled flag; administrators come from configuration, not the record | Implemented |
| Agent token | `tok_…`; only a digest of the secret is stored | Owner, one repository (with the forge's stable repository ID when minted through sign-in), name, fixed expiry of at most 30 days, revoked flag | Implemented |
| Hub event | `evt_…` and a commit-ordered sequence | Kind, subject, data (most kinds name the acting user or token); never secrets | Implemented (append-only, enforced by the database) |
| Work intent | `int_…` chosen by the client | Task, paths, optional branch/base/machine, expiry; attributed to its token and owner | Implemented on the hub (section 4.3) |
| Overlap notice | Hub event sequence | The announcing agent and owner, their paths, the recipient's paths | Implemented on the hub (section 4.3) |

Git commit IDs and native revision IDs differ. Bridges record the mapping in
their own journals rather than treating either as the other. No record yet maps
a forge pull or merge request to a Change (section 7).

### 3.3 Lifecycle

```text
announce intent → work in isolation → checkpoint revisions → register review context
   → checks/evidence → approve exact subject → conditional land → append event
```

Changing the review context advances the Change version and invalidates
earlier approvals. Advancing the policy epoch invalidates approvals made under
the old epoch. Landing consumes the approval exactly once.

Status: **Implemented** natively for the steps from checkpoint to event (CLI
`checkpoint`, `start`, `context`, `approve`, `integrate`, `events`; a guided
form is `agentscm workflow`). Announcing intent is
**Implemented** on the hub's HTTP API and through `agentscm intent` (section 4.3).
Checks are **Implemented** for the built-in source-path check and isolated
container checks (section 4.4); installed policy assessment
exists as a library that the authority does not yet use as an admission gate
([policy](../advanced/policy.md)).

## 4. Concurrency model for many agents

### 4.1 Conditional target moves

A landing request MUST carry `expected_target`: either `null` (target must be
absent) or `{revision, version}`. The ledger compares it with the current target
and, in one transaction, checks the approval, Change version, context digest
and policy epoch, moves the target, increments its version, consumes the
approval and appends the event. The version number prevents an A→B→A race from
passing a revision-only comparison. For an existing target, the candidate's
first parent MUST equal the expected revision, the review context's base MUST
still match the target, and required checks MUST have current successful
evidence. Today the built-in `source_paths/v1` check and the isolated
`command/v1` check are enforced this way (**Partial**: other declared evidence is
not looked up, and one context cannot require both).

When the target has moved, the loser first brings its work onto the new target
(a native [merge session](../advanced/merge.md), or new checkpoints on top of it). Then
`prepare-landing` seals the result: it builds a landing revision with parents
`[target, tip]` and the tip's tree, rebinds a fresh review context to it and
advances the Change, conditional on the target being unchanged (the immutable
revision is written first, so a failed preparation can leave an unreferenced
object). If the
tip does not descend from the target by first parent, it fails with a conflict
and changes nothing. The new revision needs a new approval.

Status: **Implemented** in the local SQLite authority (`integrate`,
`initialize-target`, `prepare-landing`). **Not implemented:** three-way merge
inside `prepare-landing`, batching or a queue for
many landings on one target, and authenticated multi-writer admission. Landings
on one target are necessarily ordered; this model makes the ordering explicit
and cheap to retry, not parallel.

### 4.2 Exact-revision approval binding

An approval MUST bind repository, Change, Change version, target, revision,
review-context digest and policy epoch. The ledger MUST reject a landing if any
of these differ from current state, if the approval was revoked or already
consumed (an identical retry of the completed operation instead returns its
stored result), or if the context is incomplete (unknown or unavailable required
inputs, capabilities or evidence digests, or declared omissions). An approval for a source tip does not authorize the landing
revision prepared from it.

Status: **Implemented** (local administrative approval). **Proposed:**
authenticated approvers, signed approvals, and policy-derived approval rules.

### 4.3 Declared work intent and early collision notices

There are two complementary signals. **Declared intents** say what an agent is
about to edit, before it edits. **Git collision scans** report what branches
and worktrees have already changed. Both are advisory.

**Declared intents (hub).** An agent SHOULD announce an intent before editing.
On the hub, an agent token announces:

```json
{
  "intent_id": "int_0123456789abcdef0123456789abcdef",
  "task": "Fix token expiry check",
  "branch": "fix-expiry",
  "base": "COMMIT_OR_REVISION",
  "machine": "laptop-2",
  "paths": ["src/auth/**", "docs/auth.md"],
  "ttl_s": 3600
}
```

- The repository is the token's own; the hub never accepts or reveals intents
  for another repository.
- The actor is not a field. The hub attributes every intent to the announcing
  agent token and to the token's owner, a forge identity (provider, host and
  numeric user ID).
- A path is either an exact repository-relative path or a directory spec
  `dir/**` (the descendants of `dir`). Two specs overlap when they are equal or
  one is a directory spec containing the other (a path or a narrower directory
  spec). No other patterns are accepted.
- The reply lists the other live intents in the same repository that overlap
  it (only the same intent ID is excluded), and each holder gets a notice naming the announcing agent, its
  owner, their paths and the holder's paths. Notices are appended to the hub
  event log and read by long-poll (`wait` up to 25 seconds); there is no push.
- Intents expire by the hub's clock (1 second to 24 hours) and can be renewed or
  released. Repeating an identical announce is idempotent and sends no notice.

See [hub](../hub.md#intents-and-notices) for routes, limits and error cases.

Intents are advisory. They are not locks: a conforming tool MUST NOT block an
edit because another intent overlaps it.

**Git collision scans.** `agentscm collisions` (CLI) and `GET /v1/collisions`
(served with `--collisions-repo`) need no ledger or hub. See
[collisions](../collisions.md). Each call:

- enumerates local and remote branches and, with `--worktrees`, worktrees of
  one repository;
- computes the changed paths of each branch head against its merge base with
  the target, and for a worktree its uncommitted changes (staged, unstaged and
  untracked);
- reports **path overlap** between any two lines of work;
- runs `git merge-tree --write-tree` (Git 2.38 or later; the tool requires 2.43)
  on each overlapping pair of committed heads and reports **Git merge
  conflicts** (content, rename/delete, file/directory and others), without
  touching any worktree or ref (it can add unreachable objects to the object
  database and runs any merge drivers the repository configures); a pair that
  includes uncommitted changes is reported as a same-file warning only, since
  there is no commit to merge; and
- prints one JSON document, so agents can consume it. Hook recipes that call it
  before or after edits are documented, not shipped as code.

Without `--refs`, branches whose tip commit is older than `--active-days`
(default 14; 0 for all) are skipped and counted; commit dates are set by
whoever made the commit, so this filter is a convenience, not a guarantee. Both
the CLI and the service refuse more than 200 items. The CLI has no overall
deadline or output budget and never marks a report truncated. The HTTP service
always includes worktrees and uses 14 days; it stops at a 60-second deadline or
a roughly 4 MiB output budget, kills the Git process group on timeout and marks
the report `truncated`.

**Scope of what a scan sees.** It sees only the branches and worktrees present in
the repository it scans. Run `git fetch` first so remote branches are current.
Work in another clone, or on another machine, is visible only once it has been
pushed to a branch the scanned repository can see; unpushed and uncommitted work
elsewhere is invisible to it.

Overlap on the same file is a **warning, not proof of conflict**: two edits to
different functions often merge cleanly. A clean `merge-tree` result is also not
proof of compatibility: edits to different files can still break each other
through APIs, tests or configuration. Notices inform; review and checks decide.
An absent notice is not evidence that nobody else is working on the same code.

Who announced an intent is authenticated; what it declares is not. An agent can
edit paths it never declared, so a missing overlap proves nothing. Intents
reveal tasks, paths and branch names to every token holder of the repository,
so they MUST NOT be shared across repository or tenant boundaries. An intent
outlives a revoked token until it expires, and nobody can release it early.
Comparing every pair of lines of work grows quadratically; a tool SHOULD run
`merge-tree` only on pairs that already overlap by path.

Status: Git collision scans are **Implemented** for local Git repositories.
Declared intents and notices are **Implemented** as the hub's HTTP API and
the `agentscm intent` command with a Claude Code hook (multi-user, alpha).
**Proposed:** comparing intents with Git branches and worktrees (today intents
are compared only with other intents, and collision scans only with Git); push
delivery. Related
pieces: the review context's `scope` field records declared paths (**Implemented**
as data; nothing compares scopes across Changes), and the
issue intake library detects existing work for the same
issue (**Partial**: library only, no CLI).

### 4.4 Isolated checks and agent candidates

A required check MUST run against the exact revision it names, in an
environment the candidate cannot use to reach the ledger, credentials or the
network, and its outcome MUST be derived by the controller, never reported by
the candidate. An interrupted run MUST end as `incomplete`, `cancelled` or
`indeterminate`, never `success`.

The `command/v1` check runs a declared command in a Docker container with no
network, read-only source, bounded scratch, a non-root user, dropped
capabilities and fixed CPU, memory, process, output and time limits. Attempts
are persisted; the run generation fences late finishers; container cleanup
custody is persisted before the container is created; cancel and recovery
commands finish a running attempt only after confirmed removal, except that
`--abandon-unconfirmed` ends it as `indeterminate`, which never admits. A successful attempt's
output can be turned into an **agent candidate**: a new revision whose parent is
the run's exact subject. A candidate needs its own review context and approval
before it can land.

Status: **Implemented** in the trusted-local profile on Unix with Docker
(`check-command`, `check-run-command`, `check-cancel-command`,
`check-recover-command`, `check-command-candidate`; see
[execution](../advanced/execution.md)). A fixed corpus of hostile-boundary canaries runs
in a separate CI workflow and qualifies only that runtime and image. **Not
implemented:** unattended recovery after a controller crash, signed records,
installed-policy integration, an HTTP route.

### 4.5 Reconciliation with Git

While Git is authoritative, the ledger observes it explicitly. Each observation
compares one clean Git commit with the native target and records one of:

| Result | Meaning |
|---|---|
| `synchronized` | Complete file trees match; the baseline advances |
| `git_changed` | Git moved; a native candidate revision is captured for review |
| `native_changed` | Native work needs explicit outbound handling |
| `conflict` | Both moved without convergence; both tips stay available for merge |
| `unknown` | Observation failed or was interrupted; never evidence that nothing happened |

Reconciliation MUST NOT move a native target, create approvals or infer
approval from commit messages.

Status: **Implemented** locally as `git-reconcile` ([details](../advanced/git-reconcile.md)).
**Not implemented:** remote fetch, forge observation, continuous sync, Git
history import.

## 5. Git coexistence modes

Status: per row.

Adoption starts with three modes, shadow, reconcile and export; publish and an
optional cutover are further modes. The forge stays authoritative throughout
unless a team explicitly chooses a cutover for a target.

| Mode | What happens | Writes to Git? | Status |
|---|---|---|---|
| Shadow | Capture a clean Git commit as a native revision; do native work, review and landing alongside Git | No | Implemented (`git-snapshot`, [Git snapshot capture](../advanced/git-snapshot.md)) |
| Reconcile | Repeatedly compare Git and native state; capture Git-side changes as candidates | No (its `prepare-exit` step uses export to write the local mirror) | Implemented (`git-reconcile`) |
| Export | Project a landed native revision into a separately owned local bare mirror with ref compare-and-swap, optionally seeded with existing Git history | Local mirror only; never pushes | Implemented (`git-export`, [details](../advanced/git-export.md)) |
| Observe forge | Record pull/merge requests and branches from GitHub and GitLab; optionally post comments and statuses (opt-in per connection) | Comments and statuses only | Implemented for GitHub and GitLab, tested only against in-process fakes |
| Publish | Push an exported commit to the forge as a new branch and pull/merge request for normal forge review | Remote branch | Implemented for GitHub only (`hub publish`, not yet verified against live GitHub); GitLab merge requests are not implemented |
| Cutover (optional, reversible, per target) | The ledger becomes the landing authority for one target, with Git kept as a projection; a tested way back restores forge authority | Remote, by policy | Proposed |

A cutover is optional and MUST be an explicit, recorded, reversible decision per target. Before it, a
native landing, a local mirror export, a remote branch and an accepted forge
merge are four different outcomes, and tools MUST NOT report one as another.

Snapshot limits today: no submodules, one commit at a time, no history import;
export v2 preserves executable and symlink entries, while legacy export v1 and reconcile refuse them.

## 6. Conformance levels

Status: per row.

| Level | A conforming tool… | Repository status |
|---|---|---|
| **L0 — Observe Git** | Reads Git refs and worktrees; emits collision notices as JSON; never moves refs, edits worktrees or keeps a ledger | Implemented for local Git repositories (`agentscm collisions`, section 4.3); notices are read on demand and not pushed to actors |
| **L1 — Ledger + shadow** | Keeps the append-only ledger of section 3; captures, reconciles and exports Git state; binds approvals to exact subjects; publishes work intents; Git/forge stays authoritative | Partial: the ledger, capture, reconcile and export are implemented for one trusted local operator. The [hub](../hub.md) adds forge sign-in, owner-scoped agent tokens and declared intents for several people, but is not joined to the ledger and has no approval or landing admission. It observes GitHub and GitLab and can publish a branch and pull request to GitHub (fake-tested only) |
| **L2 — Native authority** | Optionally owns protected targets, after an explicit cutover; performs conditional landings; projects to Git for compatibility | Partial: local conditional landing and journal are implemented; authenticated multi-user admission and a cutover procedure are not; remote publication exists for GitHub only through the hub |

Levels are independent claims: L0 needs no ledger, and a ledger tool need not
implement L0. A tool MUST state each level it implements and its profile
(trusted-local, team, hosted), and MUST NOT claim a level from a subset of its
features. This repository implements L0 for local Git repositories and parts of L1 and L2.

## 7. Path to cutover

Status: optional direction; the evidence listed under "Recorded today" is
**Implemented**, everything under "Still needed" is **Proposed**.

Cutover is optional, reversible and per target. Git and the forge remain fully
supported, and nothing here says AgentSCM should displace the forge. A team
that chooses to make the ledger the landing authority for one target, with Git
kept as a mirror, needs evidence for that decision: for a period, every move of
the forge's protected branch should have an exact, explainable ledger record,
and every ledger landing should have an exact Git counterpart. Git remains
fully usable if the team decides against it or reverses it.

**Recorded today.**

- Immutable revisions, review contexts and integration events, with operation
  IDs that make retries safe (section 3).
- Exact export lineage: each export maps a native integration event to the Git
  commit written to the local mirror, with ref compare-and-swap
  ([git export](../advanced/git-export.md)).
- Verified baseline pairs of a Git commit and a native revision whose trees
  matched, and the outcome of every reconcile observation
  ([git reconcile](../advanced/git-reconcile.md)).
- On the hub: forge identities by numeric user ID, the forge's stable
  repository ID on agent tokens minted through sign-in, and an append-only event
  log of sign-ins, token changes, intents and notices, each with a stable ID.
  Most events name the acting user or token; renewals and releases are
  attributable only through the intent's token.
- Isolated check attempts and evidence bound to exact subjects (section 4.4).

**Still needed.**

1. Observe the forge: pull/merge request and branch observation exists for
   GitHub and GitLab (fake-tested); still needed are exact commit SHA ↔
   revision and pull request ↔ Change mappings in the ledger.
2. Publish out: a native Change becomes a forge branch and pull request on
   GitHub; GitLab merge requests are not implemented. Native landing, remote publication, request creation and forge
   acceptance (including squash and rebase) stay distinct outcomes.
3. Join the hub and the native ledger, so authenticated people approve and
   land, not a local operator.
4. A verified round trip: reconciled two-way synchronization with a verified
   exit back to Git,
   including Git history import and file modes, symlinks and submodules.
5. Multi-user landing admission with installed policy as a gate.
6. Tamper evidence for ledger and hub records, and backup and restore for the
   hub.
7. Packaging as one container exists, with an end-to-end test that simulates
   two machines on one host; a test on two real machines is still needed.
8. A recorded, per-target cutover decision and a tested way back to forge
   authority.

## 8. Relationship to the native architecture

Status: context, not a feature.

[The architecture](../architecture.md) defines AgentSCM as a native engine whose
source operations do not invoke Git. This specification does not change that:
L2 is the native engine. L0 and L1 describe how a team gets value while keeping
Git authoritative, which is a complete way to use AgentSCM and not only a
starting point.

## 9. Open questions

Status: open unless marked resolved.

1. **Intent transport.** *Resolved in v0.2:* the hub's HTTP API, scoped to
   one repository per agent token. Git ref namespaces and forge APIs were not
   used.
2. **Collision precision.** Path-level overlap is cheap and noisy; symbol-level
   or test-impact analysis is precise and expensive. What threshold avoids
   alert fatigue for hundreds of agents?
3. **Landing throughput.** Ordered landings on one target are unavoidable. Should
   the spec define batched landings (one conditional move for several approved
   changes) and how approvals bind to the combined result?
4. **ID mapping.** Native revision IDs (SHA-256 over a native encoding) differ
   from Git SHA-1/SHA-256 commit IDs. Should the ledger also key revisions by
   Git tree ID while in shadow mode?
5. **Forge approvals.** Can a forge review be imported as evidence without
   pretending it binds exact bytes?
6. **Agent identity.** *Partly resolved in v0.2:* an actor is a forge
   identity plus an owner-scoped, expiring agent token. Still open: two agents
   under one owner, or one administrator, must not count as independent
   reviewers.
7. **Cutover reversibility.** What is the exit procedure back to forge
   authority, and what history and evidence survive it?

## 10. References

These works are cited for comparison. AgentSCM is an independent project and is not affiliated with, endorsed by, or derived from any of them.

Status: context. Public sources only.

- Potvin and Levenberg, [Why Google Stores Billions of Lines of Code in a Single Repository](https://research.google/pubs/why-google-stores-billions-of-lines-of-code-in-a-single-repository/) (2016) — Piper and CitC.
- [Jujutsu](https://github.com/jj-vcs/jj), [GitButler](https://github.com/gitbutlerapp/gitbutler), [Entire](https://github.com/entireio/cli), [Sapling](https://github.com/facebook/sapling).
- Gerrit [patch sets and Change-Ids](https://gerrit-review.googlesource.com/Documentation/concept-patch-sets.html).
- Git [`merge-tree`](https://git-scm.com/docs/git-merge-tree) and [`update-ref`](https://git-scm.com/docs/git-update-ref).
- [in-toto attestation statement](https://github.com/in-toto/attestation/blob/main/spec/v1/statement.md).
