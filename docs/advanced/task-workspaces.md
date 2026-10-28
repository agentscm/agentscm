---
title: "Task-scoped source and workspaces"
---

# Task-scoped source and workspaces

> **Experimental, trusted-local alpha.** This profile uses one local native source store and workspace journal. It does not add a remote task API, a virtual filesystem, directory IAM or a hostile-code boundary.

A task can inspect one immutable revision before creating a working directory, then materialize only the paths it needs into an ordinary filesystem workspace. Later expansion is explicit and durable. Checkpoints still produce complete native revisions: files that were never materialized remain inherited from the full base rather than becoming deletions.

Use this profile when a task has a known starting scope but still needs ordinary editors, compilers and other filesystem tools. Use a full workspace when the repository is already small or its tools require most of the tree.

Agents should choose this at task start, then expand before editing newly needed files. The
[agent source workflow](agent-source-workflow.md) connects instruction loading, native selection,
safe sparse Git worktrees and occasional measured reports. It does not require a Hub screen.

## Explore without a workspace

The local source commands read immutable metadata and bytes directly from `--data-dir`:

```sh
agentscm --data-dir ./agentscm-data files REVISION --limit 1000
agentscm --data-dir ./agentscm-data files REVISION --limit 1000 \
  --prefix crates/example --contains Cargo
agentscm --data-dir ./agentscm-data stat REVISION crates/example/Cargo.toml
agentscm --data-dir ./agentscm-data read REVISION crates/example/Cargo.toml
agentscm --data-dir ./agentscm-data diff BASE REVISION --limit 1000
agentscm --data-dir ./agentscm-data history REVISION --limit 100
```

`files` scans one bounded manifest page, then applies the optional filters to that page. `--prefix PATH` includes the exact path and its descendants; `--contains TEXT` is a literal, case-sensitive path match of 1 to 1024 bytes. The response is metadata only. Filtering can produce an empty `entries` list with a non-null `next`; consume `next` until it is null before concluding that no later path matches. Save the returned JSON cursor string in a file and pass it with `--cursor FILE`.

`stat` resolves exact manifest membership without reading the file blob. `present` means the manifest contains the path and returns its entry metadata; it does not claim that the blob is locally available. `nonexistent` means the complete manifest does not contain that path. Missing source objects, denied reads, indeterminate reads and limits remain errors rather than being reported as absence; failed CLI commands include the typed top-level `error_kind` when standard error is not a terminal. `read` fetches the entry kind and content and therefore can still fail after a successful `stat` if its blob is unavailable.

These commands use the same bounded `SourceReader` as other native reads. The separate experimental team service already offers authenticated paged file, history, diff and relation reads from retained publications. It does not create or mutate these local task workspaces. The hosted laptop workflow transfers a bounded complete source bundle before creating its local workspace; it is a separate profile.

## Start the integrated local task

The guided workflow is the primary local entry when the task will continue through review and landing. Its repeated `--path` values select workspace materialization; `--scope` remains the separate declared review-context scope:

```sh
agentscm --data-dir ./agentscm-data workflow start \
  --change task-1 \
  --workspace ./work \
  --revision REVISION \
  --issue task-1 \
  --intent "Update the example parser" \
  --criterion "The parser accepts the new form" \
  --scope crates/example \
  --path Cargo.toml \
  --path Cargo.lock \
  --path crates/example \
  --recipe RECIPE_SHA256 \
  --toolchain TOOLCHAIN_SHA256 \
  --environment ENVIRONMENT_SHA256
```

The command creates the Change, immutable review context and workspace under one retained guided session. Workspace paths do not silently rewrite the declared review scope, acceptance criteria, exclusions or omissions.

For lower-level composition, start the Change and review context first, then create a workspace from its exact revision:

```sh
agentscm --data-dir ./agentscm-data workspace create ./work CHANGE_ID REVISION \
  --path Cargo.toml \
  --path Cargo.lock \
  --path crates/example
```

Library callers use `Workspace::create_scoped` with the same repeated roots and `Workspace::expand` with an operation ID and expected generation. `Workspace::create` remains the full-workspace API. `WorkspaceState.scope` is `None` for a full workspace and contains the canonical effective roots for a scoped workspace.

Each root selects that exact path and, when it is a directory prefix, all descendants present in the base revision. Roots are canonicalized and redundant descendants are removed. Configuration files, manifests, lockfiles and dependencies selected this way are ordinary editable workspace files; the scope is a source projection, not a read-only dependency mount or permission boundary.

Each scope construction accepts at most 256 roots and 65,536 total path bytes before canonicalization. Workspace schema 4 stores the resulting roots; older binaries that understand only schemas 1 through 3 refuse to open it rather than treating the workspace as full.

Omit every `--path` for the legacy full workspace. `--path .` is the explicit full-tree fallback when creating or expanding a scoped workspace.

If a tool or the task needs more source, expand from the current base with the workspace generation as a compare-and-swap fence:

```sh
agentscm --data-dir ./agentscm-data workspace state ./work
agentscm --data-dir ./agentscm-data workspace expand ./work expand-1 0 \
  --path crates/shared
```

Inside the guided flow, expansion does not change the Change source. Checkpoint at the new workspace generation to bind the new bytes and scope-visible deletions into a complete revision:

```sh
agentscm --data-dir ./agentscm-data workflow checkpoint checkpoint-1 1
```

Expansion adds exact base entries under the requested roots and advances the workspace generation. Existing dirty files already in scope are preserved. Expansion refuses a newly selected local occupant, including one whose bytes happen to equal the base, as well as file/directory collisions, unsupported links and uncertain interrupted operations. Move an out-of-scope occupant aside, expand, then reapply the edit inside the expanded scope. Reusing an operation ID with identical input returns its durable outcome; different input conflicts.

After an interruption, use `workspace recover ROOT --choice complete` or `--choice roll-back`.
Recovery retains installation evidence for each pending expansion entry. An unrelated local
file blocks recovery even when its bytes equal the requested source. Move that occupant aside
and retry; never edit the workspace metadata. Rollback removes proven expansion files while
preserving ancestor directories, so harmless empty directories can remain.
If cleanup fails after the scope transaction commits, the error identifies a durable
outcome with unfinished metadata cleanup. Inspect workspace state and retry the identical
operation; cleanup never reverses the committed scope or edits working files.


`workspace path-state ROOT PATH` does not read the source blob. `materialized` means the path is in scope and exists locally, including an addition or modification; `missing_local` means an in-scope base entry is absent and would be deleted by checkpoint; `unmaterialized` means the base contains the path outside the current scope; `outside_scope_local` means a local occupant exists outside scope; `nonexistent` means neither the base nor the projected working tree contains the path.

`.gitignore`, `.agentscmignore` and private workspace exclusions still control working-tree capture inside the projected scope. They are convenience rules, not task permissions. Tracked root and ancestor `AGENTS.md`, `AGENTS.override.md`, `CLAUDE.md`, `CLAUDE.local.md` and `GEMINI.md` files are also retained with selected paths, alongside the applicable ignore files and generated Cursor/Copilot instructions. Instruction symlinks require their in-tree target chains in the selection; missing targets and cycles block scoped creation/expansion. Inspect effective workspace state/preview and read the applicable instructions before editing. Imported instruction resources or other agent configuration must be explicitly selected when needed; this does not discover imports or grant repository text authority over trusted instructions.

## Checkpoint semantics

`WorkspaceState.scope` is `None` for a full workspace (the JSON state omits `scope`) or the canonical selected roots for a scoped workspace. Status, preview, checkpoint, undo, merge application and recovery operate on that projection.

For a tracked path inside scope, absence from the working directory is an explicit deletion. A tracked path outside scope is unmaterialized and remains inherited unchanged from the full base revision. New files outside scope are ignored and preserved on disk; expand their parent before they can enter a checkpoint. This distinction prevents a sparse directory from deleting the rest of the repository.

Checkpoint overlays the scoped additions, modifications, kind changes and deletions on the full immutable base:

```sh
agentscm --data-dir ./agentscm-data workspace status ./work --limit 1000
agentscm --data-dir ./agentscm-data workspace checkpoint ./work checkpoint-1 1
```

The result is a complete native revision, not a sparse proof or partial tree. Workspace creation still inventories the complete base and therefore requires fewer than 100,000 combined file and directory entries within its 128-depth limit. Selected materialization and checkpoint capture allow at most 64 MiB per file and 128 MiB total, and working-tree traversal retains its 100,000 selected-entry and 1,000,000 visited-entry bounds.

Full command checks materialize the exact candidate with their separate 10,000-file and 256 MiB snapshot limits; their existing regular-file-only entry profile is unchanged. Automated model review uses finite exact-source selectors and records coverage against the exact subject. Both retain their existing exact-subject evidence and coverage semantics: a sparse workspace does not narrow the source under test or turn incomplete review into complete review. Git snapshot ingress separately allows less than 100,000 combined entries, 64 MiB per file and 128 MiB total.

## Continue through landing and Git

A checkpoint advances only the workspace base and generation. Continue through the existing lifecycle:

1. Update the Change to a complete review context for the checkpointed source.
2. Run the installed source-bound workflow. Fix failures or review findings in the workspace and checkpoint a new revision.
3. Use `prepare-landing` against the current target and inspect its exact diff and context.
4. Approve that prepared subject, then `integrate` with the expected target state.
5. If Git remains the external authority, use the existing reconcile/export/remote publication flow and observe the accepted forge result separately.

If applying a revision would introduce an ancestor ignore or agent-instruction file outside the persisted scope,
expand that path explicitly before retrying application. This keeps the selection configuration
visible in the workspace rather than silently changing capture rules.

The Change version, workspace generation, target version and policy epoch remain separate fences. A passing historical run cannot approve or land later bytes. Native landing, Git branch publication and forge merge are distinct outcomes.

## Boundaries

Task scope reduces initial materialization only when the selected paths are smaller than the full supported revision. It does not claim lower model usage, lower cost or support for very large monorepositories. AgentSCM adds no transparent page faults, automatic dependency discovery, shared cache fleet or new remote mutation authority here. Local work remains available offline once the required immutable source objects are present; missing objects stay explicit.
