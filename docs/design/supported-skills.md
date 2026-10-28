---
title: "Design: AgentSCM supported skills"
---

# Design: AgentSCM supported skills

> **Status: design, with phase 2 built.** The phase 2 slice in section 6 (`pr-gate` v1, the lock
> file, `agentscm skill`, the Hub runs API and MCP tools, the `agentscm skill stop` Stop hook and the
> Hub Skills page) is implemented and **Experimental** in the [roadmap](../roadmap.md). Not built:
> the `SessionStart` recap line (2.4), the `abandoned` run state, and everything section 6 lists
> under "Not in phase 2", which stays **Proposed**. The inventory in section 1 describes the code at
> commit `325f862`.

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

### 2.1 A general-purpose catalog

Supported skills are a library any repository can browse and adopt a few at a time into its own
workflow. A first-party skill carries nothing specific to one repository: CI labels, merge methods,
build parallelism and resource limits are parameters with general defaults, which a repository sets
in its own lock file. AgentSCM's own repository adopts `pr-gate` with its own values (the
`ci-extended` label, squash merges); a repository-specific AgentSCM workflow and a commercial or
infrastructure-as-code offering can come later through the skill sources in 2.5, without changing
the schema or the lock format.

### 2.2 A supported skill is a template manifest, version 2

A supported skill is a directory with the same layout as a reusable workflow template:

```text
src/skill_workflow/supported/pr_gate/v1/SKILL.md      # instructions for the agent (advice)
src/skill_workflow/supported/pr_gate/v1/skill.json    # the declarative unit (checked)
src/skill_workflow/supported/pr_gate/v1/runner.json   # optional command recipe for native steps
```

`skill.json` is a workflow template manifest with `version: 2`. Version 2 keeps every version 1
field and rule (strict JSON, bounded size, the path is the version, subject bindings come from the
Change, never from the manifest) and adds these sections:

- **`summary`** and **`applicability`**: when to use the skill, in a form an agent can act on
  without a human prompt: `triggers` (moments such as "opening a pull request", "before merging a
  pull request", "parallel sessions on one repository", "after a merge to the default branch"),
  `prerequisites` (what must already be true) and `not_for` (when not to follow it).
- **`parameters`**: named, typed values with defaults (`string`, `bool`, `integer`, optional
  `enum`) that gates and next actions read, for example `ci_label` or `merge_method`. A repository
  overrides them per pin; the manifest bytes, and so the pin, stay the same.

- **`subjects`**: what a run is about: `git_pull_request` (a branch and pull request on a forge,
  identified by repository, number and head SHA) and/or `native_change` (an existing Change and
  target).
- **`roles`**: who may produce evidence: `author` (any session that pushed to the subject),
  `reviewer` (a session or person distinct from every author session for the reviewed head),
  `maintainer` (a person with write access who approves or merges).
- **`gates`**: an ordered list of named states. Each gate lists the evidence it needs, which role
  produces it, how strongly it must be established, and where it is enforced. Each gate also has a
  required `plain` string (1 to 200 bytes): one plain-English line saying what happens. The Hub
  library, `agentscm skill show` and the MCP skill list print a walkthrough generated from the
  manifest alone (the `plain` line plus who does it, the evidence, the minimum assurance and
  whether the step blocks or only warns), so the wording has a single source.

The existing `steps` stay exactly as they are: executable checks (source paths, command, Claude Code
review) that a gate can cite as evidence for native subjects. A version 1 manifest is a skill with
no gates.

```json
{
  "version": 2,
  "name": "pr-gate",
  "workflow_version": 1,
  "summary": "Evidence-gated pull requests: tests, independent review, fixes, CI and approval before merge, then a post-merge check.",
  "skill": { "entrypoint": "src/skill_workflow/supported/pr_gate/v1/SKILL.md", "resources": [] },
  "applicability": {
    "triggers": ["starting a task that will become a pull request", "opening or updating a pull request", "before merging a pull request", "after a pull request merges"],
    "prerequisites": ["a Git repository with pull requests on GitHub or GitLab", "the AgentSCM hub connected for this repository"],
    "not_for": ["direct commits to the default branch", "exploratory work that will not be merged"]
  },
  "parameters": [
    { "name": "ci_label", "type": "string", "default": null, "description": "Label that triggers the CI to wait for; null means whatever checks the repository requires." },
    { "name": "merge_method", "type": "string", "enum": ["any", "merge", "squash", "rebase"], "default": "any", "description": "Required merge method." },
    { "name": "retarget_stacked", "type": "bool", "default": true, "description": "Remind authors to retarget dependent pull requests after a merge." }
  ],
  "subjects": ["git_pull_request"],
  "roles": ["author", "reviewer", "maintainer"],
  "steps": [],
  "gates": [
    { "id": "branch",   "evidence": [{ "kind": "task_branch" }],                          "by": "author",     "min_assurance": "self_reported", "enforce": ["advise"] },
    { "id": "tests",    "evidence": [{ "kind": "revision_tests", }],        "by": "author",     "min_assurance": "self_reported", "enforce": ["stop"] },
    { "id": "review",   "evidence": [{ "kind": "independent_review", }],    "by": "reviewer",   "min_assurance": "self_reported", "enforce": ["stop"] },
    { "id": "fix",      "evidence": [{ "kind": "findings_resolved", }],     "by": "author",     "min_assurance": "self_reported", "enforce": ["stop"] },
    { "id": "ci",       "evidence": [{ "kind": "hosted_ci", "label_param": "ci_label" }], "by": "author", "min_assurance": "self_reported", "enforce": ["forge_status"] },
    { "id": "approval", "evidence": [{ "kind": "human_approval", }],        "by": "maintainer", "min_assurance": "self_reported", "enforce": ["forge_status"] },
    { "id": "merge",    "evidence": [{ "kind": "merged", "method_param": "merge_method" }], "by": "maintainer", "min_assurance": "self_reported", "enforce": ["advise"] },
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
review never looks like a controller receipt. In `pr-gate` v1 every gate accepts self-reported
evidence (decision 1 in section 7); a later version raises `ci`, `approval` and `merge` to
`observed` once the Hub reads them from the forge.

### 2.3 Adoption and pinning

A repository browses the catalog (`agentscm skill list`, `agentscm skill show NAME`, MCP
`agentscm_skill_list`, the Hub Skills page) and adopts a skill with `agentscm skill add NAME
[--param k=v]`, which writes a pin into a committed lock file reviewed like any other change.
`agentscm skill remove NAME` deletes the pin. The Hub page stays read-only: it shows the exact
command to copy. The lock lives at the repository root as `agentscm-skills.lock.json`, not under
`.agentscm/`, because that directory is AgentSCM's ignored local data directory and Git snapshot
capture excludes it.

```json
// agentscm-skills.lock.json
{
  "version": 1,
  "skills": [
    { "name": "pr-gate", "workflow_version": 1, "sha256": "…",
      "source": { "kind": "first_party" },
      "params": { "ci_label": "ci-extended", "merge_method": "squash" } }
  ]
}
```

- **The pin is the name, version and SHA-256 of the manifest bytes**, the identity a
  `WorkflowTemplateReference` already carries. Upgrading is a reviewed edit to the lock file that
  names v2; the v1 bytes are never rewritten.
- **Parameters** are overrides validated against the manifest's declared parameters; unknown names
  or wrong types make the pin `invalid_params`. Effective values (defaults overlaid by the pin) are
  shown wherever a run is shown.
- **First-party skills ship in the binary.** The AgentSCM release embeds `skill.json` and `SKILL.md` from
  `src/skill_workflow/supported/` (inside the crate so the Docker build context has them), so the Hub
  can evaluate a pinned skill without reading the repository. The lock file's `sha256` is a bundle
  digest, SHA-256 over `agentscm-supported-skill/v1\0` and the length-prefixed bytes of both files, so
  it must match the embedded manifest and procedure; a mismatch is shown as `unknown_version`, never silently resolved to
  another version.
- **Authority for adoption** is the lock file on the default branch (decision 3). When the Hub has a
  forge connection for the repository it reads the lock file from the default branch; otherwise it
  uses the lock file the checkout reports and labels it `reported by checkout`. With a forge connection a checkout's
   report is refused (409 `forge_authoritative`), so a local lock changes nothing until it is on the
   default branch.
- **Workspace-level adoption** (one skill required for every repository in a Community workspace)
  is a later step: a workspace setting that lists pins, shown alongside the repository's own.

### 2.4 Discovery by agents

An agent should not need the orchestrator's prompt to know the rules.

- **Instruction files.** When `agentscm-skills.lock.json` exists, `init-repo` adds a short
  *Supported skills* section inside its existing marked block: each adopted skill and version, its
  triggers ("Follow when: …"), its `not_for` line, and the commands and MCP tools to call. The block points at
  `agentscm skill show NAME` (and MCP `agentscm_skill_list`) for the procedure instead of copying it
  or naming a repository path. The hub library entry carries the procedure text.
- **MCP.** Three Hub tools:
  - `agentscm_skill_list {repo}` → the catalog and this repository's pins with applicability
    (triggers, prerequisites, not_for) and effective parameters, so an agent decides on its own when
    a skill applies.
  - `agentscm_skill_status {repo, pull_request | branch}` → the run's current gate, the evidence
    each gate has (with assurance), what is missing and the next action for this session's role.
  - `agentscm_skill_evidence {repo, pull_request, gate, kind, head_sha, …}` → records
    self-reported evidence (a test command and result, a review with findings, a finding marked
    fixed). The Hub binds it to the head SHA it names and refuses a SHA that is not the current
    head.
- **Hooks.** The existing `SessionStart` recap adds one line per open run for the session's branch
  ("pr-gate v1 on #41: at *review*; needs an independent review of `a1b2c3d`"). The Stop hook
  enforces (below).

### 2.5 Enforcement at existing choke points

Each gate's `enforce` field names one of four choke points. No new blocking mechanism is added.

| `enforce` | Choke point (existing) | Behavior |
|---|---|---|
| `stop` | The opt-in local Stop hook (`src/agent_stop.rs` shape, `agentscm skill stop`; it blocks only where the user installed it) | Blocks the author session from finishing while the run sits at an **agent-actionable** gate (tests, review, fix) without sufficient evidence for the current head. The block reason is the next action from `agentscm_skill_status`. Gates that wait on others (CI, approval) never block; the hook lets the session stop with a handover note. The 8-continuation cap applies unchanged. |
| `forge_status` | A Hub-posted commit status `agentscm/skill/<name>` on the pull request head, using the forge-check posting journal | `pending` until every pre-merge gate has sufficient evidence for the exact head SHA, then `success`. A new push resets it. It blocks merging only when an operator requires the context in a ruleset; AgentSCM never edits protections. |
| `native` | Installed target requirement → `approve` / `integrate` | **Planned.** For `native_change` subjects, gates that cite `workflow_receipt` would compile to an ordinary template instantiation and requirement, which the existing native path already enforces; nothing compiles skill gates to it yet. |
| `advise`, `signal` | Instruction files, MCP status, Hub UI; `signal` also notifies other sessions (section 4.2) | Shown and recorded, never blocking. |

What stays advice in every version of this design, and the docs will say so: branch per task,
the merge method (a forge repository setting AgentSCM can read and flag, not change), retargeting
stacked pull requests, and anything a session reports about itself. Self-reported evidence can
satisfy a gate only when the gate says `min_assurance: self_reported`; the Hub never upgrades it.

### 2.6 Skill sources: the extension point

A pin's `source` says where the manifest bytes come from. Phase 2 resolves only `first_party`
(embedded in the binary). The other kinds are reserved in the lock format now, so adding them later
changes neither the schema nor existing lock files; until then they parse and resolve to
`unsupported_source`:

- `repo_local {path}`: a skill directory inside the repository itself, such as AgentSCM's own
  internal workflow. The Hub evaluates it from the bytes the checkout or the default branch provides
  with the matching SHA.
- `third_party {url, commit, path}`: a skill vendored from another Git repository at an exact
  commit; `agentscm skill add <git-url>@<commit>:<path>` would copy it, compute the SHA and write
  the pin for review.
- `private {registry, …}`: a skill from an organization's or a commercial catalog (for example an
  infrastructure-as-code offering), fetched by exact SHA.

Every non-first-party skill is labelled with its source in the CLI, MCP and Hub. There is
no remote registry, no `latest` and no automatic update, consistent with the template library. A
third-party skill can only compose the evidence kinds and choke points above; it cannot add a new
blocking mechanism or claim stronger assurance than its evidence has.

## 3. The first skills

### 3.1 (a) `pr-gate` v1

**Subject:** one pull request on any GitHub or GitLab repository, with any CI. **Roles:** author
session, reviewer session, maintainer. **Parameters:** `ci_label` (default none), `merge_method`
(default `any`), `retarget_stacked` (default true). **Follow when:** starting a task that will become
a pull request, opening or updating one, before merging, after it merges. **Not for:** direct
commits to the default branch or exploratory work that will not be merged. **Known limit:** the Stop hook gates work on a branch; a detached checkout is not gated, and
the hook lets that session finish.

| Gate | Evidence | Assurance | Enforced | Notes |
|---|---|---|---|---|
| branch | `task_branch` | self-reported (phase 2); observed later | advise | Flag pushes to the target and branches shared by two work items. |
| tests | `revision_tests` on head | self-reported (phase 2); receipt when a managed recipe or native step exists | stop | The command and its result for the exact head SHA. A new push invalidates it. |
| review | `independent_review` on head | self-reported; receipt for `claude_code_review` | stop | The hub refuses a review recorded with the same agent token and session as an author; it cannot tell two sessions apart beyond that. Records findings with IDs and `blocking` flags. |
| fix | `findings_resolved` | self-reported | stop | Each blocking finding is fixed at a later head (and the review is re-run on that head) or dismissed by a maintainer. |
| ci | `hosted_ci` | self-reported (phase 2); observed later | forge status | Green on the exact head. With `ci_label` set, the next action names the label to add; with the default (null) it waits for whatever checks the repository requires. |
| approval | `human_approval` on head | self-reported (phase 2); observed later | forge status | Forge approval or native `approve`. |
| merge | `merged` | self-reported (phase 2); observed later | advise | Uses `merge_method` (default `any`). With `retarget_stacked` (default true), the next action reminds the author to retarget dependent pull requests to the default branch. |
| post_merge | `post_merge_check` on target tip | self-reported or observed | signal | Hands off to `post-merge` (c). A failure raises the main-red signal. |

Ordering: gates are evaluated in order; evidence bound to an older head is shown as `stale`, not
deleted. A run is *complete* when `post_merge` has evidence, *abandoned* when the pull request
closes unmerged.

### 3.2 (b) `parallel-work` v1

**Subject:** a set of sessions working on one repository at the same time. **Parameters:**
`build_slots`, `disk_floor_gib`, `protected_ports` and `protected_containers`, all empty by default;
a repository or machine sets its own values (AgentSCM's own would be 4 slots, an 8 GiB floor and its
named services).

| Gate | Evidence | Enforced | Today's equivalent | `plain` |
|---|---|---|---|---|
| claim | A stand-up claim and an intent for the files the session will edit | advise (the pre-edit hook already refuses the first contested edit) | `agentscm_start` | The session says which files it will edit before editing them. |
| resources | A resource claim for each heavy resource (build slot, disk budget, port, container) before using it | advise in v1; `stop` once resource claims exist (4.1) | `/tmp/agentscm-capacity-*.txt` | The session reserves each heavy resource before using it. |
| collisions | A collision check against every open sibling branch before opening or updating a pull request, with each overlap acknowledged or coordinated | stop (blocks finishing with an unchecked open pull request) | `git merge-tree` by hand | The session checks for overlap with every other open branch before opening a pull request. |
| release | Intents and resource claims released, build artifacts cleaned | stop (blocks finishing while this session holds a resource claim) | `cargo clean`, deleting the note | The session gives back its claims and cleans up what it built. |

The collisions gate makes collision detection run without anyone remembering to: the Stop hook
calls the existing `agentscm collisions` scan for the session's branch and records the result as
evidence.

### 3.3 (c) `post-merge` v1

**Subject:** one merge commit on the target.

| Gate | Evidence | Enforced | `plain` |
|---|---|---|---|
| tip | The current target tip includes the merge (later merges are fine) | advise | The target branch already contains this merge. |
| regression | The repository gate (for AgentSCM: fmt, clippy, nextest, doctests) on that tip, bound to its SHA | signal | The full repository check runs on the merged tip. |
| verdict | Green → the target is marked green at that SHA. Red → the main-red signal names the first red SHA, the merges since the last green SHA and who ran the check | signal | The target is marked green, or red with the first bad commit named. |

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
   `summary`, `applicability` and `parameters`; a pure `evaluate(manifest, evidence, head_sha) ->
   RunView` (current gate, per-gate state, stale evidence, next action from effective parameters). `src/skill_workflow/supported/pr_gate/v1/{SKILL.md, skill.json}` embedded in the
   binary. Unit tests for parsing, refusal of unknown fields and every gate transition, including a
   new push invalidating head-bound evidence and a reviewer equal to an author being refused.
2. **Catalog, adoption and discovery.** `agentscm-skills.lock.json` with `source` and `params`;
   `agentscm skill list | show | add | remove` to browse and adopt; the checkout reports the lock
   file to the Hub (`reported by checkout`), and the Hub reads it from the default branch when a
   forge connection exists; `init-repo` adds the *Supported skills* section with triggers when a
   lock file exists. Tests for each.
3. **Hub storage and API.** `hub_skill_runs` and `hub_skill_evidence` (SQLite and PostgreSQL, with
   conformance tests); `GET /v1/hub/skills` (library: shipped, adopted, version) and
   `GET /v1/hub/skills/runs` / `…/runs/{repo}/{pr}`; MCP `agentscm_skill_list`, `agentscm_skill_status` and
   `agentscm_skill_evidence` (self-reported; refuses a non-current head). Route and MCP tests.
4. **Enforcement.** A `skill-stop` Stop-hook mode reusing `agent_stop.rs`: blocks the author
   session at tests, review or fix without evidence for the current head; never blocks on CI or
   approval; keeps the 8-continuation cap. Tests with fixture hook payloads, matching the existing
   Stop tests.
5. **Hub UI.** The Skills library page (browse with applicability, copyable adopt and remove
   commands, effective parameters) and the runs list and run detail, mock endpoints in
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

## 7. Decisions

The maintainer settled the open questions on 2026-10-24 before phase 2:

1. **Review evidence.** A self-reported independent review passes the `review` gate in v1. Every
   gate that passed on self-reported evidence carries a visible `self-reported` assurance label in
   the CLI, the MCP output and the Hub UI. A `claude_code_review` receipt remains the stronger
   form when it exists.
2. **Enforcement order.** The Stop hook (`skill-stop`) is the first enforcement point. The
   `agentscm/skill/<name>` forge status comes next; #91 already posts the workflow status on
   GitHub merge queue groups, so it can reuse that posting journal.
3. **Adoption authority.** When the Hub has a forge connection for the repository, it reads
   `agentscm-skills.lock.json` from the default branch. Otherwise it uses the lock file the
   checkout reports and labels it `reported by checkout`.
