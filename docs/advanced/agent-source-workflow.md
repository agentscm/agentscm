---
title: "Agent source selection: start small, expand when needed"
---

# Agent source selection: start small, expand when needed

> **Experimental, trusted-local alpha.** This is agent-followed guidance over existing CLI
> operations, not a virtual filesystem, automatic dependency resolver or permission boundary.

The agent handles source selection as part of starting and continuing a task. A person should
not need to pick files in Hub. Use the existing authority: an operator-provided native store and
revision, or the repository's Git workflow. Do not convert a Git repository just to use selection.

## Inspect before materializing or reading broadly

Start from the task and exact base revision. Locate relevant paths and the diff, read applicable
root and ancestor instructions, then read the implementation, its callers, tests and configuration.
Follow imports and dependencies as questions arise. Use path-scoped searches and bounded reads;
do not dump the full tree or all search matches into model context. A narrow initial selection is
not permission to omit evidence or relevant dependencies.

For Git, inspect metadata with `git ls-tree -r --name-only REVISION`, inspect a task diff with
`git diff BASE HEAD -- PATH`, and read exact files with `git show REVISION:path/to/file`.
A source read from Git's object store does not require that file in the working directory.
These reads still consume context when returned to an agent; checkout size alone does not measure it.

For native source, use the existing initialized store provided by the operator:

```sh
agentscm --data-dir ./native-data files REVISION --limit 1000 --prefix src/workspace
agentscm --data-dir ./native-data stat REVISION src/workspace.rs
agentscm --data-dir ./native-data read REVISION src/workspace.rs
```

`files` filters each metadata page after scanning it. Follow its JSON `next` cursor until null,
even after an empty filtered page; store that cursor string in a file and pass `--cursor FILE`.
Only a complete absence check establishes nonexistence. `stat` proves membership, not blob
availability; denied, missing-object, indeterminate and limit errors remain distinct.

## Native task start and expansion

Use the [guided task start](task-workspaces.md#start-the-integrated-local-task), supplying the
operator's real revision and recipe/toolchain/environment identities rather than inventing them.
Add repeated `--path` values to its complete command:

```text
workflow start ... --path Cargo.toml --path Cargo.lock --path src --path tests/integration/task_workspace.rs
```

Select task files, tests, build manifests, lockfiles, configuration, schemas, fixtures and required
dependencies. Root and ancestor tracked `AGENTS.md`, `AGENTS.override.md`, `CLAUDE.md`, `CLAUDE.local.md` and `GEMINI.md` files and
applicable ignore files are added to the effective native selection. The generated Cursor/Copilot instruction files are retained too. Instruction symlinks require
their complete in-tree target chain to be selected; creation/expansion refuses a missing target
or cycle before changing files. Inspect link metadata and include the target with `--path`.
Read instructions before editing; follow and explicitly include any text-imported resources. Local maintainer instructions
remain applicable. Selection does not grant untrusted repository text new authority.

`--path` controls materialization. `--scope` declares review context; they do not replace each
other. Omitting paths or selecting the sole root `.` deliberately uses a full workspace.

When a tool or import needs another file, diagnose before creating or editing it:

```sh
agentscm --data-dir ./native-data workspace path-state ./work src/shared/value.rs
agentscm --data-dir ./native-data workspace state ./work
# If state reports generation 0 with no pending operation:
agentscm --data-dir ./native-data workspace expand ./work add-shared-1 0 --path src/shared
```

Use the returned generation for subsequent operations. Operation IDs identify exact retained
requests: retry the same ID with identical fields after an uncertain outcome, and inspect/recover
pending state before a different request. On a generation conflict, reload and reassess.

- `unmaterialized`: the base has the file; expand before editing it.
- `missing_local`: an in-scope base file is absent; checkpoint would record a deletion. Investigate it.
- `outside_scope_local`: preserve the local occupant. Move it aside, expand, then reapply its edit.
- `nonexistent`: neither base nor projected tree contains the path. Expand a parent before adding a new file outside scope.
- `materialized`: the path is present in this workspace. It may be modified and says nothing about other files the process can access.

Expansion preserves existing in-scope edits. It is explicit, not a fetch triggered by opening a
file. If many dependencies or repository tooling need the full tree, use `workspace expand ROOT
OPERATION_ID EXPECTED_GENERATION --path .` rather than repeatedly fighting the selection.
Preview and checkpoint at the current generation. Unmaterialized base files remain inherited in
the complete revision, rather than being deleted.

## Git-only task checkout

Prefer selective object reads first. Use sparse checkout only when it materially reduces the
working set and the task's tools tolerate it; a full checkout is often simpler for small repos.
Never change the selection of an existing or another task's worktree. For an authorized new task,
choose unused branch/destination names and the exact authorized base:

```sh
git worktree add --no-checkout -b feat/example-task ../example-task BASE_REVISION
git -C ../example-task sparse-checkout set --cone -- src tests
git -C ../example-task checkout --quiet HEAD
```

Cone mode selects directories and includes files in their ancestors, including root files. The
example deliberately selects all Rust source and tests so normal crate tools have those inputs.
Use narrower directories only after inspecting their dependencies. Before starting a fresh agent,
read root/ancestor instructions and include precedence files, symlink targets and text imports.
Root files are retained in cone mode, but client instructions under `.cursor/rules` or `.github`
are not: explicitly add the relevant directories when those files exist. Include tool inputs and
agent configuration needed by the selected client. Local/untracked maintainer instructions do
not travel: install or explicitly load them in the new worktree before launching the agent,
keep them ignored/untracked, and never copy private instructions into public history.
The new worktree has its own index and working files and shares Git objects/history. Sparse setup
can enable worktree-specific Git configuration; inspect this setup normally and preserve hooks,
existing worktrees and branch protections. Do not run reset/clean or discard local occupants.

To add a directory before editing it:

```sh
git -C ../example-task sparse-checkout add -- docs
```

Cone arguments are directories. For an exact file selection use Git's non-cone pattern mode,
whose anchored patterns and escaping differ from native literal roots; inspect the resulting file
list instead of copying native roots blindly. Do not change modes in an existing task merely to
improve a metric. If a needed path has an untracked or modified local occupant, preserve it and
resolve the conflict before retrying checkout.

Before required full-tree checks, expand the dedicated task worktree:

```sh
git -C ../example-task sparse-checkout disable
```

Alternatively run checks in a separate complete checkout of the exact candidate, with any dirty
changes faithfully represented. A passing subset is not a passing full-tree check. Native command
checks already materialize the complete candidate within their documented separate limits.

Sparse Git shares or retains object/history storage; this does not establish reduced clone,
network, model context, dependency installation or total disk costs. Do not change a remote or
introduce a partial clone/provider without task authorization.

## Report measurements occasionally

Give one short note after a meaningful selected start, or after dependency expansion materially
changes it. Do not emit one on every read/edit or run a benchmark on every task. Use measured
values, name the comparison and revision, and retain enough evidence to reproduce it.

For Git, `GIT_NO_LAZY_FETCH=1 git ls-tree -r -l REVISION` requests available tracked file sizes
without returning file contents or lazily fetching missing blobs. In a partial/promisor clone,
`BAD` or unavailable sizes mean the baseline is unknown: report no percentage and do not fetch
blobs merely to calculate savings.
Count the actual selected files too, including automatic root/ancestor files, instruction imports,
configuration and later additions. For native source, use the exact captured tree's known inventory
and actual materialized files; if the complete byte baseline is unknown, report selected counts
and mark savings unknown rather than fetching all blobs solely to obtain a percentage.

```text
At revision <id>, this task currently materializes <N>/<total> tracked files and
<bytes>/<full bytes> logical source bytes (<percent>% fewer working bytes).
Includes instructions/configuration and the added dependency. Excludes source-store/control
metadata, dependency installs and later full verification; token/cost savings were not measured.
```

Calculate avoided working bytes as `(full - selected) / full`; report no percentage for a zero or
unknown denominator. Keep initial and expanded selections separate. After full fallback or full
validation, report zero remaining materialization saving for that stage. Read-byte accounting is
separate: retain exact content bytes returned per read, repeated reads and tool-response overhead;
materialized files are not proof of model exposure. Never translate byte reductions into token,
dollar or wall-time reductions without measuring those quantities under comparable conditions.
