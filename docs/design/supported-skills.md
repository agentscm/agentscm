# Design: AgentSCM supported skills

> **Status: design proposal (phase 1).** Nothing in this document beyond the inventory in section 1
> is implemented. Every new capability here is **Proposed** in the [roadmap](../roadmap.md). The
> inventory describes the code at commit `325f862`.

A *supported skill* is a workflow that a repository (later, a workspace) adopts and that its
agents must follow during ordinary development: how a task becomes a branch, which evidence a pull
request needs before merge, what happens after merge, and how parallel agents share a machine. The
first skill is the pull-request gate that an orchestrating session ran by hand across six AgentSCM
pull requests on 2026-10-24:

- a branch per task;
- an independent review before merge, with its findings fixed first;
- hosted CI through the `ci-extended` label, merging only when green, squash only, with stacked
  pull requests retargeted;
- a regression pass on the final `main` after merging;
- a shared build slot, disk budget, ports and Docker containers between parallel agents,
  coordinated through hand-written `/tmp/agentscm-capacity-*.txt` notes.

Four gaps showed up while doing that:

1. Intents cover files, not shared resources (CPU slots, disk, ports, containers). The disk filled
   twice.
2. Nothing told the other sessions "main just went red after the last merge, and you inherit it".
3. There is no orchestrator view across sessions.
4. Collision detection exists, but nothing ran it, so conflicts between parallel pull requests were
   found by hand with `git merge-tree`.

The design goal is to turn that loop into a declared, versioned unit that agents discover without
being told, that AgentSCM enforces at the choke points it already has, and that the Hub shows. It
extends workflow templates and skill workflows; it does not add a second workflow engine.

## 1. Inventory: what exists today

The column that matters is the last one. **Blocks** means an AgentSCM component refuses to
proceed. **Advises** means it tells an agent or a person, who may ignore it.

### Defining a workflow

| Piece | What it is | Versioning | Blocks or advises |
|---|---|---|---|
| Skill workflow definition (`src/skill_workflow/contracts.rs`) | Strict JSON: `source{repository, revision, policy_epoch}`, `skill{entrypoint, resources}`, 1–32 `steps` with `needs`, `required` and an action: `source_paths`, `command`, `claude_code_review`, `require_capability`. | Compiled to a `plan_digest` over the definition and the exact skill bytes (`compiler.rs` `compile`, `verify_current`). Bound to one native revision and policy epoch. | Structure, bounds, DAG and exact-byte digests are machine-checked. `SKILL.md` prose is input, never authority. |
| Workflow template (`src/skill_workflow/library.rs`, #64) | The same graph without subject bindings: `version: 1`, `name`, `workflow_version`, `skill`, `steps`. `command` names a runner; the Change supplies repository, revision, epoch and context at `instantiate`. | The file path is the version (`skills/checks/v1/workflow.json`); rendered as `NAME:vN`. No mutable catalog, no `latest`, no registry. | Same checks as above. `library inspect` returns `"admission":"not_assessed"`. |
| `skills/native-workflow/SKILL.md` | The only repository skill: how to author a `source_paths` definition. | Path in the repository. | Advice to the authoring agent. |
| Installed target requirement (`src/authority/workflows.rs`, `WorkflowRequirement`) | Per native target: a normalized definition and skill digest in `workflow_requirements`; receipts in `workflow_receipts`. Installed with `skill-workflow require` or `library configure`. | Immutable per target; moving a target from v1 to v2 is refused. The template path and SHA are printed but not persisted. | **Blocks** (see enforcement). |

### Distributing it to agents

| Piece | What it does | Blocks or advises |
|---|---|---|
| `agentscm init-repo` (`src/init_cli/targets.rs`) | Writes a marked block (`<!-- agentscm:begin -->`) into `AGENTS.md`, `.github/copilot-instructions.md` and `.cursor/rules/agentscm.mdc`, plus `@AGENTS.md` pointers in `CLAUDE.md` and `GEMINI.md`. The block covers coordination MCP tools and agent-task operations. It does not mention skill workflows or templates. | Advises. Agents read it on a best-effort basis. |
| `agentscm client setup --agent …` | Installs hooks and MCP config per client. | Installs the enforcing hooks below. |
| Hub MCP (`src/hub/mcp.rs`) | Coordination (`agentscm_start`, `_check`, `_done`, `_handoff`, `_ask`, `agentscm_coordination_*`), stand-up (`agentscm_work_*`), `agentscm_change_record`, `agentscm_setup_status`, and read-only `agentscm_workflow_status`. Workspace task facade: 16 `agentscm_task_*` tools. Attached publication: 6 tools. | Advises, except where a tool is the path to a blocking gate. |
| Local workflow MCP (`src/skill_workflow_mcp.rs`) | `agentscm_workflow_library_inspect`, `agentscm_workflow_handoff`; `_library_configure` with `--allow-configure`; `agentscm_workflow_check` with `--allow-execution`. | `check` can produce the receipt that unblocks a gate. |

### Enforcing it

| Choke point | Blocks or advises | Condition |
|---|---|---|
| Local Stop hook: `claude-stop`, `codex-stop` (`src/agent_stop.rs`), installed by `skill-workflow install-stop` | **Blocks finishing** | Returns `decision: block` unless the `check` result has a receipt and the workspace did not change during the run. At most 8 consecutive continuations per agent, session, workspace and change, then it stops with a reason. Opt-in; live-qualified only in Claude Code print mode and `codex exec`. |
| Agent-task Stop: `task-stop`, `task-codex-stop` (`src/agent_task_stop.rs`) | **Blocks finishing**, read-only | Passes only if the facade qualification is current, the workspace is clean and the base matches. Fixture-tested only. |
| Pre-edit hook `POST /v1/hub/hook/{agent}` | **Refuses once** | The first contested edit per session and file, when another session holds that exact file through hooks or MCP. A repeat passes. At most 3 refusals per 600 s. Claude Code, Codex and Copilot `PreToolUse`; Cursor gets notes only. Hook failures never refuse. |
| Native `approve`, `integrate`, `initialize-target` (`src/authority.rs` → `workflows::require_current_success`) | **Blocks** | The target has an installed requirement and no receipt matches the exact revision and review context. With no requirement installed, admission passes. |
| Forge status `agentscm/workflow/<target>` (`src/hub/forge_check.rs`), also on GitHub merge-queue groups (#91) | Advises, **blocks only if the operator requires it** | Posted `success` only when the pinned native store qualifies the exact candidate and the head (or group tree) matches. AgentSCM never edits branch protection or rulesets. |
| `agentscm/coordination` status | Advises | Always `success`; documented as never to be required. |
| `agentscm/local-validation` (`docs/advanced/local-validation.md`) | Advises unless required | Signed local attestation verified by a repository workflow. No Rust admission path reads it. |

### Observing it

| Piece | What it observes | Blocks or advises |
|---|---|---|
| Change records (#95, `docs/change-capture.md`) | Bounded Claude Code session facts and source-bound action receipts (for example a managed test recipe bound to its source and setup). Execution outcome, verification, assurance and coverage are separate fields. | Observes only; capture failure never blocks the session. |
| Collisions (`docs/collisions.md`) | Read-only snapshot of branches and worktrees touching the same files. | Advises. Runs only when someone invokes it (CLI, an operator's own hook, `GET /v1/collisions`). Nothing runs it automatically. |
| Stand-up work (`docs/standup.md`) | Claims, checkpoints, blockers and handovers per work item. | Advises; check-ins are self-reports. |
| External CI and deployment outcomes (`docs/hub.md`) | GitHub Actions push runs and deployments for a landed publication's merge commit. | Observes; GitHub only, polled, fake-tested. |
| Hub UI | Now, Work, Activity, Workflows (skill-workflow runs), Change record, Landed, Templates, Timeline, Workspace, Setup, Health. | Read-only. |

The unmerged branch `feat/hub-ui-new-feature-pages` is one commit (`45b8b1e`) that already landed
on `main` as #72 and #73; `Landed.tsx`, `Templates.tsx` and `Timeline.tsx` are byte-identical. It
adds nothing `main` lacks and can be closed. Its worktree holds uncommitted `ui-next/` design
experiments that this design does not depend on.

### What this means for the PR gate

Today's loop ran on Git and GitHub, not on a native target. On that path:

- **Nothing blocks.** The native gates apply only to native targets with an installed requirement.
  The Stop hook blocks on a native workflow receipt, which a Git pull request does not have. The
  forge status blocks only for native candidate publications and only when an operator requires it.
- **Everything else was advice in a prompt.** Branch per task, independent review, fixing findings,
  `ci-extended`, squash only, retargeting, the post-merge pass and the capacity notes all lived in
  the orchestrator's instructions.

## 2. The library shape

### 2.1 A supported skill is a template manifest, version 2

A supported skill is a directory with the same layout as a reusable workflow template:

```text
skills/supported/pr-gate/v1/SKILL.md      # instructions for the agent (advice)
skills/supported/pr-gate/v1/skill.json    # the declarative unit (checked)
skills/supported/pr-gate/v1/runner.json   # optional command recipe for native steps
```

`skill.json` is a workflow template manifest with `version: 2`. Version 2 keeps every version 1
field and rule (strict JSON, bounded size, the path is the version, subject bindings come from the
Change, never from the manifest) and adds three sections:

- **`subjects`**: what a run is about: `git_pull_request` (a branch and pull request on a forge,
  identified by repository, number and head SHA) and/or `native_change` (an existing Change and
  target).
- **`roles`**: who may produce evidence: `author` (any session that pushed to the subject),
  `reviewer` (a session or person distinct from every author session for the reviewed head),
  `maintainer` (a person with write access who approves or merges).
- **`gates`**: an ordered list of named states. Each gate lists the evidence it needs, which role
  produces it, how strongly it must be established, and where it is enforced.

The existing `steps` stay exactly as they are: executable checks (source paths, command, Claude Code
review) that a gate can cite as evidence for native subjects. A version 1 manifest is a skill with
no gates.

```json
{
  "version": 2,
  "name": "pr-gate",
  "workflow_version": 1,
  "skill": { "entrypoint": "skills/supported/pr-gate/v1/SKILL.md", "resources": [] },
  "subjects": ["git_pull_request", "native_change"],
  "roles": ["author", "reviewer", "maintainer"],
  "steps": [],
  "gates": [
    { "id": "branch",   "evidence": [{ "kind": "task_branch" }],                          "by": "author",     "min_assurance": "observed",      "enforce": ["advise"] },
    { "id": "tests",    "evidence": [{ "kind": "revision_tests", "bind": "head" }],        "by": "author",     "min_assurance": "self_reported", "enforce": ["stop"] },
    { "id": "review",   "evidence": [{ "kind": "independent_review", "bind": "head" }],    "by": "reviewer",   "min_assurance": "self_reported", "enforce": ["stop"] },
    { "id": "fix",      "evidence": [{ "kind": "findings_resolved", "bind": "head" }],     "by": "author",     "min_assurance": "self_reported", "enforce": ["stop"] },
    { "id": "ci",       "evidence": [{ "kind": "hosted_ci", "label": "ci-extended", "bind": "head" }], "by": "author", "min_assurance": "observed", "enforce": ["forge_status"] },
    { "id": "approval", "evidence": [{ "kind": "human_approval", "bind": "head" }],        "by": "maintainer", "min_assurance": "observed",      "enforce": ["forge_status"] },
    { "id": "merge",    "evidence": [{ "kind": "merged", "method": "squash" }],            "by": "maintainer", "min_assurance": "observed",      "enforce": ["advise"] },
    { "id": "post_merge", "evidence": [{ "kind": "post_merge_check", "on": "target_tip" }], "by": "author",    "min_assurance": "self_reported", "enforce": ["signal"] }
  ]
}
```

**Evidence kinds** reuse what AgentSCM already records wherever it can:

| Kind | Satisfied by | Existing source |
|---|---|---|
| `task_branch` | The subject's head branch is not the target and was created for one work item | Forge sync, stand-up claim |
| `revision_tests` | A test result bound to the exact head SHA | Change-record action receipt for a managed test recipe (#95); a native `command` step receipt; or a self-report naming the command and SHA |
| `independent_review` | A review bound to the exact head SHA by a reviewer distinct from every author session, with a findings list | `claude_code_review` step receipt (native); or a recorded review (new) |
| `findings_resolved` | Every blocking finding of the latest review is fixed at a later head or dismissed by a maintainer with a reason | Recorded findings (new) |
| `hosted_ci` | The forge reports the required checks green on the exact head SHA | Forge status and check reads (the Hub already reads commit statuses for forge checks) |
| `human_approval` | A maintainer approval on the exact head SHA | Forge review state; native `approve` |
| `merged` | The pull request merged with the declared method | Forge sync; landed outcomes |
| `post_merge_check` | A regression pass on the target tip that includes the merge | Change-record receipt on the merge commit; external CI outcomes on the merge commit |
| `workflow_receipt` | A native step receipt for the exact revision and context | `workflow_receipts` (unchanged) |

**Assurance** is a separate field on every piece of evidence, never folded into success:
`receipt` (an AgentSCM controller produced it for exact bytes), `observed` (the Hub read it from
the forge or a store), `self_reported` (an agent said so through MCP or the CLI). A gate states the
minimum it accepts. The Hub shows assurance next to every piece of evidence so a self-reported
review never looks like a controller receipt.

### 2.2 Adoption and pinning

A repository opts in with a committed lock file reviewed like any other change:

```json
// .agentscm/skills.lock.json
{
  "version": 1,
  "skills": [
    { "name": "pr-gate", "workflow_version": 1,
      "path": "skills/supported/pr-gate/v1/skill.json",
      "sha256": "…" }
  ]
}
```

- **The pin is the path plus the SHA-256 of the manifest bytes**, the same identity a
  `WorkflowTemplateReference` already carries. Upgrading is a reviewed edit to the lock file that
  points at `v2/skill.json`; the v1 file is never rewritten.
- **First-party skills ship in the binary.** The AgentSCM release embeds `skills/supported/*`, so
  the Hub can evaluate a pinned skill without reading the repository. The lock file's SHA must match
  the embedded bytes; a mismatch is shown as `unknown skill version`, never silently resolved to
  another version.
- **Authority for adoption** is the lock file on the target's default branch. In phase 2 the Hub
  learns it from the checkout through client setup and labels it `reported by checkout`; reading it
  from the forge with the existing connection is a later step.
- **Workspace-level adoption** (one skill required for every repository in a Community workspace)
  is a later step: a workspace setting that lists pins, shown alongside the repository's own.

### 2.3 Discovery by agents

An agent should not need the orchestrator's prompt to know the rules.

- **Instruction files.** When `.agentscm/skills.lock.json` exists, `init-repo` adds a short
  *Supported skills* section inside its existing marked block: each adopted skill and version, one
  line on what it requires, and the MCP tools to call. The block points at the skill's `SKILL.md`
  for the procedure instead of copying it.
- **MCP.** Two Hub tools:
  - `agentscm_skill_status {repo, pull_request | branch}` → the run's current gate, the evidence
    each gate has (with assurance), what is missing and the next action for this session's role.
  - `agentscm_skill_evidence {repo, pull_request, gate, kind, head_sha, …}` → records
    self-reported evidence (a test command and result, a review with findings, a finding marked
    fixed). The Hub binds it to the head SHA it names and refuses a SHA that is not the current
    head.
- **Hooks.** The existing `SessionStart` recap adds one line per open run for the session's branch
  ("pr-gate v1 on #41: at *review*; needs an independent review of `a1b2c3d`"). The Stop hook
  enforces (below).

### 2.4 Enforcement at existing choke points

Each gate's `enforce` field names one of four choke points. No new blocking mechanism is added.

| `enforce` | Choke point (existing) | Behavior |
|---|---|---|
| `stop` | The local Stop hook (`src/agent_stop.rs` shape, new `skill-stop` mode) | Blocks the author session from finishing while the run sits at an **agent-actionable** gate (tests, review, fix) without sufficient evidence for the current head. The block reason is the next action from `agentscm_skill_status`. Gates that wait on others (CI, approval) never block; the hook lets the session stop with a handover note. The 8-continuation cap applies unchanged. |
| `forge_status` | A Hub-posted commit status `agentscm/skill/<name>` on the pull request head, using the forge-check posting journal | `pending` until every pre-merge gate has sufficient evidence for the exact head SHA, then `success`. A new push resets it. It blocks merging only when an operator requires the context in a ruleset; AgentSCM never edits protections. |
| `native` | Installed target requirement → `approve` / `integrate` | For `native_change` subjects, gates that cite `workflow_receipt` compile to an ordinary template instantiation and requirement. This path already blocks. |
| `advise`, `signal` | Instruction files, MCP status, Hub UI; `signal` also notifies other sessions (section 4.2) | Shown and recorded, never blocking. |

What stays advice in every version of this design, and the docs will say so: branch per task,
squash only (a forge repository setting AgentSCM can read and flag, not change), retargeting
stacked pull requests, and anything a session reports about itself. Self-reported evidence can
satisfy a gate only when the gate says `min_assurance: self_reported`; the Hub never upgrades it.

### 2.5 Third-party skills (later)

A third-party skill is the same directory, vendored into the repository at a versioned path and
pinned in the lock file by path and SHA. `agentscm skills add <git-url>@<commit>:<path>` would copy
it, compute the SHA and write the lock entry for review. The Hub evaluates a vendored skill from the
manifest bytes the checkout reports with the matching SHA, and labels it `third-party`. There is
no remote registry, no `latest` and no automatic update, consistent with the template library. A
third-party skill can only compose the evidence kinds and choke points above; it cannot add a new
blocking mechanism or claim stronger assurance than its evidence has.

## 3. The first skills

### 3.1 (a) `pr-gate` v1

**Subject:** one pull request (or one native Change). **Roles:** author session, reviewer
session, maintainer.

| Gate | Evidence | Assurance | Enforced | Notes |
|---|---|---|---|---|
| branch | `task_branch` | observed | advise | Flag pushes to the target and branches shared by two work items. |
| tests | `revision_tests` on head | self-reported (phase 2); receipt when a managed recipe or native step exists | stop | The command and its result for the exact head SHA. A new push invalidates it. |
| review | `independent_review` on head | self-reported; receipt for `claude_code_review` | stop | Reviewer session must differ from every author session that pushed to the head. Records findings with IDs and `blocking` flags. |
| fix | `findings_resolved` | self-reported | stop | Each blocking finding is fixed at a later head (and the review is re-run on that head) or dismissed by a maintainer. |
| ci | `hosted_ci` with label `ci-extended` | observed | forge status | Green on the exact head. If the label is missing the next action is "add `ci-extended`" (a maintainer action for same-repository pull requests). |
| approval | `human_approval` on head | observed | forge status | Forge approval or native `approve`. |
| merge | `merged` with method `squash` | observed | advise | When a stacked parent merges, the next action for the child is "retarget to `main`". |
| post_merge | `post_merge_check` on target tip | self-reported or observed | signal | Hands off to `post-merge` (c). A failure raises the main-red signal. |

Ordering: gates are evaluated in order; evidence bound to an older head is shown as `stale`, not
deleted. A run is *complete* when `post_merge` has evidence, *abandoned* when the pull request
closes unmerged.

### 3.2 (b) `parallel-work` v1

**Subject:** a set of sessions working on one repository at the same time.

| Gate | Evidence | Enforced | Today's equivalent |
|---|---|---|---|
| claim | A stand-up claim and an intent for the files the session will edit | advise (the pre-edit hook already refuses the first contested edit) | `agentscm_start` |
| resources | A resource claim for each heavy resource (build slot, disk budget, port, container) before using it | advise in v1; `stop` once resource claims exist (4.1) | `/tmp/agentscm-capacity-*.txt` |
| collisions | A collision check against every open sibling branch before opening or updating a pull request, with each overlap acknowledged or coordinated | stop (blocks finishing with an unchecked open pull request) | `git merge-tree` by hand |
| release | Intents and resource claims released, build artifacts cleaned | stop (blocks finishing while this session holds a resource claim) | `cargo clean`, deleting the note |

The collisions gate makes collision detection run without anyone remembering to: the Stop hook
calls the existing `agentscm collisions` scan for the session's branch and records the result as
evidence.

### 3.3 (c) `post-merge` v1

**Subject:** one merge commit on the target.

| Gate | Evidence | Enforced |
|---|---|---|
| tip | The current target tip includes the merge (later merges are fine) | advise |
| regression | The repository gate (for AgentSCM: fmt, clippy, nextest, doctests) on that tip, bound to its SHA | signal |
| verdict | Green → the target is marked green at that SHA. Red → the main-red signal names the first red SHA, the merges since the last green SHA and who ran the check | signal |

The orchestrator ran this by hand after every merge. As a skill, the session that merged owns it,
and any session can take it over through a stand-up handover.

## 4. Product gaps (each Proposed)

### 4.1 Resource claims as a kind of intent

An intent today names files. A resource claim names a shared, finite thing on one host or a
shared service: `build_slot` (count), `disk` (bytes, with a floor such as "stop under 8 GB free"),
`port` (number), `container` (name). It has an owner session, a host, an expiry and a release, and
it goes through the same `agentscm_start` / `_done` lifecycle and the same notices. Over-subscription
produces a notice like a file overlap; the Stop hook can block finishing while a claim is held
(`parallel-work` release gate). A *protected* resource list (for example containers and ports a
session must never touch) is a claim held by a person with no expiry. Claims stay advice: AgentSCM
does not control `cargo` or Docker.

### 4.2 "Main went red, you inherit it"

A repository-scoped signal raised by a `post-merge` red verdict (or by an observed failing push run
on the target from external CI outcomes). It records the first red SHA, the last green SHA and the
merges between them. Every session whose branch is based at or after the red SHA sees it in its
`SessionStart` recap, in `agentscm_skill_status` and as a stand-up notice; the Hub shows a banner.
It clears when a later `post-merge` run is green. It is information, not a merge freeze; a
repository that wants a freeze can make `pr-gate`'s forge status depend on it in a later version.

### 4.3 Orchestrator view across sessions

One Hub page that joins what already exists per session: stand-up claims, intents, resource claims,
skill runs and their current gates, collision results between open branches and the main-red
signal. It answers "what is every session doing, what is each one waiting on, and what will collide
or exhaust a shared resource next?" without opening each session. It stays read-only like the rest
of the Hub.

## 5. Hub UI

New section **Skills** in the rail's *Verify* group, next to Workflows and Landed. Hash routes:
`#skills` (library), `#skills/runs` (runs), `#skills/runs/<repo>/<pr>` (one run). It reuses `Box`,
`Pill`, `Empty`, `Notice`, `Path`, `Ago` and `Landed.tsx`'s `Track` step component, follows the
Semantic Field Rule (every state color has a text label) and keeps to GET reads.

### 5.1 Library

```text
┌ Skills ──────────────────────────────────────────────────────────────────────────────┐
│ Which workflows do agents in this repository have to follow?                           │
│ [ acme/widgets ▾ ]                                       Adoption: reported by checkout │ ← lock file source, never implied verified
│                                                                                        │
│ ┌ Adopted ─────────────────────────────────────────────────────────────────── 2 ─────┐ │
│ │ pr-gate            v1   sha256:3f9a…   [first-party]  [Enforced: stop, forge status]│ │
│ │ Branch → tests → independent review → fix → hosted CI → approval → merge → check     │ │
│ │ 3 open runs · 1 waiting on review · last completed 2 h ago            View runs →    │ │
│ │ ──────────────────────────────────────────────────────────────────────────────────── │ │
│ │ parallel-work      v1   sha256:a71c…   [first-party]  [Enforced: stop]              │ │
│ │ Claims, resource claims and collision checks between sessions                        │ │
│ └──────────────────────────────────────────────────────────────────────────────────────┘ │
│ ┌ Available ───────────────────────────────────────────────────────────────── 1 ─────┐ │
│ │ post-merge         v1   [first-party]                                              │ │
│ │ Regression pass on the target tip after each merge             How to adopt (copy) │ │ ← copies the lock entry; the page never writes
│ └──────────────────────────────────────────────────────────────────────────────────────┘ │
│ ┌ Needs attention ─────────────────────────────────────────────────────────────────── ┐ │
│ │ [unknown version] pr-gate sha256:0b2e… is pinned but this hub ships v1 (3f9a…).      │ │
│ └──────────────────────────────────────────────────────────────────────────────────────┘ │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

### 5.2 Runs (per pull request or session)

```text
┌ Skill runs ─ pr-gate v1 ───────────────────────────────────────────────────────────────┐
│ [ Open ] [ Completed ] [ Abandoned ]                       Search runs on this page …   │
│                                                                                        │
│ #41  Add retry budget to sync worker     claude · session 75a8   head a1b2c3d          │
│   ●branch ─ ●tests ─ ◐review ─ ○fix ─ ○ci ─ ○approval ─ ○merge ─ ○check                 │ ← Track; ● done ◐ current ○ not reached
│   [At review]  Needs an independent review of a1b2c3d by another session.              │ ← next action leads (Ownership First)
│                                                                                        │
│ #40  Cache parsed manifests              codex · session 3c6f   head 9e8d7f6          │
│   ●branch ─ ●tests ─ ●review ─ ●fix ─ ◐ci ─ ○approval ─ ○merge ─ ○check                 │
│   [Waiting on CI]  ci-extended green on 4 of 6 checks.                                 │
│                                                                                        │
│ #38  Tighten upload limits               claude · session 37ad  merged as 55af0e1     │
│   ●… ●merge ─ ✕check                                                                    │
│   [Main red]  Regression pass failed on main at 55af0e1.         Inherited by 2 sessions│ ← gap 4.2 surfaces here
└────────────────────────────────────────────────────────────────────────────────────────┘
```

### 5.3 One run

```text
┌ #41 · pr-gate v1 · acme/widgets ───────────────────────────────────────────────────────┐
│ Lead: claude (session 75a8, ada)       Reviewer: —        Head a1b2c3d (pushed 12 min)  │
│ ┌ Next action ──────────────┬ Blocking finish? ─────────┬ Last evidence ─────────────┐ │
│ │ Independent review of     │ Yes: Stop hook blocks the │ tests passed on a1b2c3d     │ │
│ │ a1b2c3d by another session│ author until review exists│ 12 min ago (self-reported)  │ │
│ └───────────────────────────┴───────────────────────────┴─────────────────────────────┘ │
│ Gate        State          Evidence                                   Assurance         │
│ branch      [Done]         feat/sync-retry-budget from main           observed          │
│ tests       [Done]         cargo nextest run … exit 0 on a1b2c3d      self-reported     │ ← assurance column always visible
│ review      [Current]      —                                                            │
│ fix         [Not reached]                                                               │
│ ci          [Not reached]  label ci-extended: absent                                     │
│ approval    [Not reached]                                                               │
│ merge       [Not reached]  method: squash                                                │
│ check       [Not reached]                                                               │
│ Stale evidence (older heads) ▸                                                          │
│ Technical details ▸                       (manifest path, sha256, run id, raw events)   │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

### 5.4 Where the gaps would appear

- **Resource claims (4.1):** a *Resources* box on the Now page beside contested files: each claim,
  its owner session, host and expiry, with over-subscription as a warning field with text.
- **Main red (4.2):** a page-level `Notice` on Now, Work and Skills, and the `[Main red]` field on
  affected runs, naming the first red SHA and the merges since the last green one.
- **Orchestrator view (4.3):** a new *Sessions* section in the *Coordinate* group: one row per
  session with its claim, current skill gate, resource claims and collision count, sorted by what
  it is waiting on.

## 6. Phase 2: what gets built (after approval)

The smallest end-to-end slice of `pr-gate` on the Git pull-request path, which is the path
AgentSCM's own pull requests use:

1. **Definition.** `src/skill_workflow/supported.rs`: template manifest version 2 (`subjects`,
   `roles`, `gates`, evidence kinds, assurance), strict parsing and bounds reusing `library.rs`;
   a pure `evaluate(manifest, evidence, head_sha) -> RunView` (current gate, per-gate state, stale
   evidence, next action). `skills/supported/pr-gate/v1/{SKILL.md, skill.json}` embedded in the
   binary. Unit tests for parsing, refusal of unknown fields and every gate transition, including a
   new push invalidating head-bound evidence and a reviewer equal to an author being refused.
2. **Adoption and discovery.** `.agentscm/skills.lock.json` parsing; `client setup` reports the
   lock file to the Hub (`reported by checkout`); `init-repo` adds the *Supported skills* section
   when a lock file exists. Tests for both.
3. **Hub storage and API.** `hub_skill_runs` and `hub_skill_evidence` (SQLite and PostgreSQL, with
   conformance tests); `GET /v1/hub/skills` (library: shipped, adopted, version) and
   `GET /v1/hub/skills/runs` / `…/runs/{repo}/{pr}`; MCP `agentscm_skill_status` and
   `agentscm_skill_evidence` (self-reported; refuses a non-current head). Route and MCP tests.
4. **Enforcement.** A `skill-stop` Stop-hook mode reusing `agent_stop.rs`: blocks the author
   session at tests, review or fix without evidence for the current head; never blocks on CI or
   approval; keeps the 8-continuation cap. Tests with fixture hook payloads, matching the existing
   Stop tests.
5. **Hub UI.** The Skills library page and the runs list and run detail, mock endpoints in
   `ui/dev/mock.ts`, vitest unit tests, and Playwright specs for both pages (desktop and mobile).
6. **Dogfood.** Adopt `pr-gate` v1 in this repository's lock file and run AgentSCM's next pull
   requests through it, recording which gates were self-reported.

**Not in phase 2** (later, each Proposed): the `agentscm/skill/<name>` forge status; reading
`hosted_ci`, `human_approval` and `merged` from the forge as `observed` (phase 2 records them as
self-reported with links); `parallel-work`, `post-merge`, resource claims, the main-red signal and
the orchestrator view; workspace-level adoption; third-party skills and `agentscm skills add`;
native-subject compilation to target requirements.

Validation for phase 2: `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`,
the affected nextest binaries, and the UI vitest and Playwright suites for the new pages.

## 7. Open questions for the maintainer

1. Should a self-reported independent review be enough to pass the `review` gate in v1, or should
   v1 require the `claude_code_review` receipt and stay native-only until the forge status lands?
   This design chooses self-reported plus a visible assurance label, so the Git path works now.
2. Is the Stop hook the right first enforcement point, given it is opt-in and live-qualified only in
   print and exec modes? The alternative is to build the forge status first, which blocks merges
   for everyone but has never run against live GitHub.
3. Should adoption authority come from the default branch through the forge connection from day one
   instead of `reported by checkout`?
