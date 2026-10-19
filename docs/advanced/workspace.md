# Native workspaces and recovery

> **Experimental (alpha).** This part of AgentSCM is not part of the main path, may change without migration, and has had limited testing. See the [advanced index](README.md).

AgentSCM workspaces are ordinary directories materialized from an immutable native revision. They use `store::Engine`. Git is run only to read exclusion settings when the workspace root has a `.git` (see [source selection](#source-selection)). The local alpha supports workspace creation, status, checkpoint, undo, completed native merge application and interrupted-operation recovery. See [durable merge sessions](merge.md) for local divergence and conflict resolution. Authenticated remote synchronization and the remaining gates below are not yet implemented.

Every workspace contains a reserved `.agentscm-workspace` directory. It stores a SQLite journal, exact staged checkpoint bytes and recovery files. The whole namespace is excluded from status and checkpoints. A source revision, or a working-tree file, with any path component equal to `.git`, `.agentscm`, `.agentscm-workspace` or `agentscm-data`, ignoring ASCII case, is rejected: on a case-insensitive filesystem a variant such as `.AGENTSCM-WORKSPACE` would otherwise resolve into this directory, and Git metadata never belongs in native source. A `.git` directory or `gitdir` file at the workspace root is skipped as metadata and never captured; a `.git` anywhere deeper blocks status and checkpoint unless a rule excludes it. Do not edit this directory.

The CLI data directory must be an initialized AgentSCM repository outside the workspace root. The CLI verifies that the workspace's repository identity matches that authority. Library callers must provide the corresponding trusted `Engine`; a matching revision digest alone is not repository authorization.

## Lifecycle

To land workspace work on a protected target, use `prepare-landing` on the checkpointed tip. Workspace generation and base remain distinct from the Change version and target version, and preparation does not modify the workspace.

Create a workspace for an existing Change and revision:

```sh
agentscm --data-dir ./agentscm-data workspace create ./work change-1 REVISION
```

The destination must not exist. Creation first reads and validates the complete bounded source snapshot, including the reserved namespace, entry, byte and depth limits. It then reserves the destination with a non-overwriting directory create, writes durable initializing state, and materializes the captured bytes. A rejected source leaves no destination. Existing paths are never replaced. A crash before the initialization journal exists can leave an unrecognized empty destination; inspect and remove it manually if appropriate. A crash after the journal exists is recoverable.

Inspect the workspace state or compare its ordinary files with the current base:

```sh
agentscm --data-dir ./agentscm-data workspace state ./work
agentscm --data-dir ./agentscm-data workspace status ./work --limit 1000
```

Status rejects special files, symbolic links that are not valid entries, nonportable paths and portable-name collisions. Traversal is bounded to 100,000 filesystem entries, 128 MiB of file bytes, 64 MiB per file and 128 directory levels. `complete: false` means the traversal or output limit prevented a complete result. Checkpoint, undo and merge application refuse incomplete traversal.

After editing files, checkpoint the exact observed bytes:

```sh
agentscm --data-dir ./agentscm-data workspace checkpoint ./work checkpoint-1 0
```

The final argument is the expected workspace generation. Workspace generation is distinct from the Change version, protected-target version, policy epoch and future server authority generations. A stale generation fails without changing the workspace.

A successful workspace checkpoint advances only the workspace base and generation. It does not update the Change's review context or authorize a protected-target move. After the final checkpoint, prepare the exact landing subject, inspect its complete context and diff, and approve that subject at its returned Change generation before integration.

Checkpoint first commits the exact changed contents and deletions to the workspace journal. It then publishes an immutable child revision through `Engine` and atomically advances workspace state. If publication succeeds but the process loses its response, the pending journal retains the bytes needed to reproduce the same revision. Reusing an operation ID with the same request returns its durable outcome; reuse with different input is a conflict.

Undo names the completed checkpoint to reverse:

```sh
agentscm --data-dir ./agentscm-data workspace undo ./work undo-1 1 checkpoint-1
```

Undo restores the prior base only when every affected path still matches one of the journaled states. A later edit, untracked occupant, directory collision, link or special file blocks the operation without overwriting that path. Unrelated untracked files, modified files and empty directories remain unchanged. Tracked file-to-directory and directory-to-file transitions remove known staged files first, prune only empty staged ancestors and then install replacement files with a non-overwriting hard link. Replacement bytes are fully written and synced in the metadata namespace before mutation. The base and workspace generation move only after all affected paths reach the prior state.

## Source selection

Status, checkpoint, undo, merge application and recovery capture the working tree through one selection. Preview is a read-only report of that selection.

**What is selected.** Every path in the base revision is always selected, so a rule can never hide an edit to a tracked file or its deletion; to remove a tracked file, delete it. Untracked files and directories are selected unless a rule excludes them. An excluded directory is not entered. A tracked file inside an excluded directory is still selected; untracked files beside it stay excluded.

**Rule sources.** Rules use gitignore syntax. From lowest to highest precedence:

1. Global Git excludes: `core.excludesFile`, else `$XDG_CONFIG_HOME/git/ignore` or `~/.config/git/ignore`. An empty `core.excludesFile` means no global file. Read only when the workspace root has a `.git`.
2. `$GIT_DIR/info/exclude`, which beats the global file. Read only when the workspace root has a `.git`.
3. `.gitignore` files, always, including in a workspace with no Git. A deeper file beats a shallower one.
4. `.agentscmignore` at the workspace root. It is an ordinary workspace file, captured like any other and read from the working tree whether or not it is in the base revision. A higher rule or a `.gitignore` entry can keep it out of the revision.
5. `.agentscm-workspace/exclude`. It is private to this workspace and never transferred.

The highest source with a matching rule decides. Within one file the last matching rule wins, and `!pattern` re-includes. A path inside an excluded directory cannot be re-included; re-include the directory itself.

**Matching.** The matcher is original code, tested against `git check-ignore`. Case sensitivity follows `core.ignorecase` for a Git root and a probe of the workspace filesystem otherwise. Files with CRLF line endings work.

Limits:

- Unsupported patterns are `Invalid` errors that name the file and line: a run of two or more stars other than a whole `**` path segment (a leading `**/`, `/**/`, a trailing `/**`, or the whole pattern `**`; so `a**/b`, `a/***/b`, `***`, `k**` and `**` before an escaped slash are refused), a NUL byte in a rule, a pattern over 4,096 bytes, and a character class whose range end is not ASCII.
- Case folding is ASCII only.
- Budgets are `Limit` errors that name the file; shorten the file or exclude its directory. A rule file may be at most 64 KiB. One scan may read at most 8 MiB and 100,000 patterns of rules in total, with at most 1 MiB and 10,000 patterns resident at once (the `.gitignore` files of the directories being walked plus the fixed sources). Matching work in one scan is capped at 3x10^9 units, a few seconds of matching.
- A `.gitignore` that is a symbolic link inside the tree is ignored as a rule source. `.agentscmignore` and the private exclude must be regular files.
- Non-UTF-8 bytes in a rule file are decoded lossily.

**Git at the root.** When the root has a `.git`, status and checkpoint run `git` to read its exclusion settings. This needs a working `git` (2.31 or later, for `--path-format`) and a repository Git accepts (ownership and `safe.directory` apply) whose top level is exactly the workspace root; otherwise they fail with an explicit error and capture nothing. Git configuration includes are honored. The user's `PATH`, `HOME` and Git configuration environment are trusted. Every scan (status, checkpoint, preview, undo, merge application, recovery) runs three Git commands, one `rev-parse` and two `config` reads, each with a 10 second timeout and bounded output. Without a root `.git`, Git is not run; the scan instead writes a short-lived case-probe file in `.agentscm-workspace` and removes it.

**Preview.** Before the first hosted `skill-workflow remote-check` or any other transfer, list what will be sent (`remote-init` creates the workspace, so preview applies after it):

```sh
agentscm --data-dir ./agentscm-data workspace preview ./work --limit 1000
```

The JSON `preview` object has `base_revision`, `generation`, `included` (captured files and links, each with `path` and, if a `!` rule re-included it, `rule`), `excluded` (each with `path`, `directory` and the deciding `rule` as `source` and `pattern`), `included_total`, `excluded_total`, `complete` and, if an operation is pending, `pending`. `included` lists files and symbolic links only. Lists are sorted by path and cut at `--limit` (default 1000); `--limit 0` is an error. Recording is capped at 10,000 entries per list, and truncated rule attribution also makes `complete` false. `complete` is false when a list was cut short. A workspace that is not ready, or whose scan hit a traversal limit, returns empty lists with `complete: false`. Preview takes the same write transaction as the other scans, so it waits for a concurrent operation on the workspace. Preview shows paths and rules, never file contents. Excluded counts are top-level entries: an excluded directory counts once and its contents are not visited or counted. `workspace status` reports the same count as `excluded_total`.

**Excluded is not deleted.** Undo, merge application and refresh never overwrite or delete a local file that a rule excludes. If an incoming path collides with such a file and the current base does not track it, the operation is blocked and nothing is written.

**Not a credential scanner.** Untracked files that no rule excludes are captured, and tracked secrets stay captured. Review the preview and keep credentials out of the workspace.

**Not covered.** Offline local backup (`src/local_recovery.rs`) still copies the whole workspace, ignored files included. Full attached-Git synchronization (commit mapping, refresh and publication) is separate future work; this change only honors Git's exclusion rules during native capture.

## Executable files and symbolic links

A workspace tracks three entry kinds: regular files, executable files and symbolic links. A file is executable when its owner execute bit (`0o100`) is set, as Git decides it; no other permission bit is tracked. Materialization creates a regular file with the process default mode (`0666` minus the umask) and an executable with `0777` minus the umask; if the umask clears the owner execute bit, the operation fails rather than writing a non-executable file. Group, other, setuid and setgid bits, timestamps and ownership are not preserved.

A symbolic link is read with `lstat` and never followed. Its target must be an in-tree relative canonical path (`store::validate_symlink_target`: relative, no empty or `.` component, leading `..` components no deeper than the link's directory, then at least one portable non-reserved component, at most 1,024 bytes, UTF-8). A link whose target fails the rule makes status and checkpoint fail; an absolute or escaping target is never materialized. Dangling in-tree targets are allowed. Materialization refuses to write or remove through a symbolic-link ancestor and removes a link itself, never its target.

The workspace probes its filesystem rather than assuming support. Creation always runs the full probe inside the new workspace's metadata directory, whatever the source tree holds: it must create and read back a symbolic link, and a file set to `0600` must read back non-executable while `0700` reads back executable. If the probe fails, creation removes what it made and returns `unsupported filesystem`. Every working-tree scan (status, checkpoint, undo, merge application and recovery) repeats the executable probe first, so an existing workspace on a mount that reports every file executable (for example a CIFS mount with a fixed `file_mode`) is refused instead of reporting every regular file as changed. Before materializing an executable or symlink, the symlink probe also runs. A failed probe returns `unsupported filesystem` before anything is journaled or installed, rather than flattening the entry to a regular file or dropping it. Probe files live in the metadata directory and are removed; one left behind by an interrupted probe is swept by the next probe, and until then blocks offline backup of that workspace.

Status reports `kind_changed` with `before` and `after` kinds when a path keeps its place but changes kind: a mode-only change, or a regular file replaced by a link or the reverse. A `kind_changed` path may also have changed content: the kind change takes precedence in the report. After upgrading, files in an existing workspace that were previously made executable with `chmod +x` appear as `kind_changed` (regular to executable). Checkpoint publishes the observed kinds, so a mode change or retarget becomes a new revision. Commands that cannot represent these kinds (Git export profile v1, execution snapshots, merge and source-paths checks) refuse the revision with `unsupported entry kind`. Offline local backup refuses a workspace tree that contains a symbolic link with `unsupported backup entry`, before any output is published.

## Interrupted operations

State and status expose one pending operation. Complete it after a restart:

```sh
agentscm --data-dir ./agentscm-data workspace recover ./work --choice complete
```

For a pending checkpoint, completion publishes only the staged bytes. For initialization, it fills missing files and accepts already written files only when their bytes match the requested revision. For undo or merge application, each affected path must be the before state, after state or a recognized journal temporary. Divergent bytes produce a blocked outcome and remain untouched.

Cancel a pending checkpoint or roll a partial undo or merge application back to its prior state:

```sh
agentscm --data-dir ./agentscm-data workspace recover ./work --choice roll-back
```

Initialization cannot be rolled back automatically. AgentSCM does not delete a workspace root because it cannot prove that another process did not add files after creation.

One SQLite immediate-writer transaction serializes bounded filesystem mutation for a workspace across processes. Independent workspaces have independent journals and can checkpoint concurrently into the immutable source store. SQLite uses WAL, full synchronous commits and a bounded busy timeout.

## Trust and durability boundary

This remains a trusted-local alpha. It detects links and verifies bytes immediately before destructive steps, but portable `std::fs` calls cannot prevent a hostile local process from swapping a path between inspection and open. Quiesce external editors and filesystem writers during undo and recovery: without descriptor-relative compare-and-swap, a concurrent edit between the final byte check and unlink can still be lost. Hostile-local race resistance requires descriptor-relative no-follow operations or a separately qualified platform implementation. A filesystem that cannot create hard links across the workspace path layout cannot complete file replacement. I/O or sync uncertainty returns an error and leaves the operation pending; it is never reported as success.

Keep the authority data, immutable source objects, and each workspace journal and working tree consistent in an offline backup while all native writers and external editors are stopped. Source-only offline bundles do not include protected targets, approvals, policy or review contexts. Exact retry applies while the relevant journals remain intact. Restoring an older backup can lose acknowledged outcomes: a missing result is unknown, not proof that the operation never completed. Physical writer isolation and actual external mirror state must be reconciled before resuming; copying a repository identity does not fence an old writer.

## Remaining work

Bounded multi-parent source history is available. Durable local native merge sessions and conflicts are available. Resumable selective offline synchronization, authenticated conflict views, remote pointer publication and the full recovery qualification are not yet implemented:

- Two workspaces from one base must diverge and later converge through declared multi-parent three-way merge. Disjoint edits must produce the same final tree; overlapping edits must retain base and both sides as inspectable conflict state.
- A conflict resolution creates a new revision and requires review of that exact subject. Undo and restart must retain unresolved conflicts.
- Bounded source enumeration and object transfer must resume after interruption without retransmitting admitted objects. Missing local or remote objects remain explicit.
- Reconnection must exchange missing objects before conditionally publishing a remote pointer. Divergent remote state returns a conflict and is never overwritten silently.
- Offline checkpoint, diff and merge work only over the locally available closure. A complete-repository claim requires all required history and objects.

Current workspace tests kill a real subprocess after its pending checkpoint becomes durable, then recover the exact staged bytes. Focused fixtures simulate interrupted initialization, pending checkpoint, mixed before/after undo state, a truncated internal recovery file, a partially installed file/directory transition, divergent recovery and roll-back. Other tests cover both file/directory transition directions, exact retry, stale input, dirty files, untracked files and empty directories, status bounds, link rejection and two independent workspaces.

Exhaustive process termination at every journal, source-publication and file-replacement boundary remains a qualification gate. The later merge/sync exit test must additionally disconnect both workspaces, create divergent edits, resume an interrupted exchange and prove convergence or durable conflict without silent loss.
