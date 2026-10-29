---
title: "Git snapshot capture"
---

# Git snapshot capture

> **Experimental (alpha).** This part of AgentSCM is not part of the main path, may change without migration, and has had limited testing. See the [advanced index](README.md).

`agentscm git-snapshot` turns one explicit, clean Git commit into one native root revision (no parent) and writes a machine-readable receipt. After capture, work continues with native commands only; Git is not needed.

This is a one-way snapshot, not Git history import. It does not read other commits, branches or tags. It does not write to the Git repository, change `HEAD`, or move any protected target. It creates no Change or approval. It assumes a single trusted local operator on a local filesystem. Nothing here is a production or multi-user claim.

## Setup

Keep three locations separate and outside the checkout: the data directory, the receipt, and the workspace. The workspace must also be outside the data directory.

```sh
agentscm --data-dir ../agentscm-data init

agentscm --data-dir ../agentscm-data git-snapshot preflight \
  --repository . --revision COMMIT

agentscm --data-dir ../agentscm-data git-snapshot capture \
  --repository . --revision COMMIT --receipt ../agentscm-receipts/snapshot.json
```

`COMMIT` is the full lowercase commit ID (40 hex digits for SHA-1, 64 for SHA-256). Branch names, tags and abbreviations are rejected. `--git` is the absolute path of the Git executable (default `/usr/bin/git`); a relative path is rejected. `--exclude PATH` is repeatable.

`preflight` prints `{"ok":true,"preflight":{...}}`. It is read-only and never opens or creates `--data-dir`. A report with `"eligible": false` is still a successful command; read `cleanliness`, `unsupported`, `limit_violations` and `filters`. `capture` prints `revision`, `receipt`, `existing` and `batches`. It fails with a `conflict` error when the tree is not eligible and writes no objects in that case.

`capture` requires an initialized data directory (it does not create one) and checks, before any write, that:

- the data directory is neither equal to nor nested with the Git top level or the common Git directory, in either direction;
- the receipt path is outside the Git top level, the common Git directory and the data directory, and its parent directory exists;
- an existing receipt path is a regular file of at most 64 MiB whose content matches the receipt this capture would write in every field except `native.revision` (otherwise `conflict`, before anything is published).

Then start a Change on the new revision. The review context below is an example; replace `REPOSITORY_ID` with the `repository` value from `init`, `REVISION` with the captured revision and `COMMIT` with the Git commit. See [review contexts](review-context.md) for field rules.

```json
{
  "version": 1,
  "change": {"repository": "REPOSITORY_ID", "id": "snapshot-demo"},
  "change_version": 1,
  "revision": "REVISION",
  "base": null,
  "target": "main",
  "policy_epoch": 0,
  "intent": "Continue work from the Git snapshot",
  "acceptance_criteria": ["Edits checkpoint without Git"],
  "scope": ["src/lib.rs"],
  "exclusions": [],
  "omissions": ["criteria not yet reviewed"],
  "dependencies": [
    {"identity": "https://github.com/OWNER/REPO/issues/NUMBER", "state": {"state": "declared"}},
    {"identity": "git:sha1:COMMIT", "state": {"state": "declared"}}
  ],
  "recipe": {"state": "unknown"},
  "toolchain": {"state": "unknown"},
  "environment": {"state": "unknown"},
  "required_evidence": [],
  "required_capabilities": ["agentscm.review-context/v1"]
}
```

`base` is `null` because the captured revision has no parent and the target does not exist yet. The `omissions` entry and the `unknown` recipe, toolchain and environment keep readiness `incomplete`; `start` reports that, and approval and integration are denied for such a context. Dependencies are declarations: they record the issue and commit that motivated the snapshot, not proof that anything was checked. Use `git:sha256:COMMIT` for a SHA-256 repository.

```sh
agentscm --data-dir ../agentscm-data start context.json
agentscm --data-dir ../agentscm-data workspace create ../agentscm-work snapshot-demo REVISION
# edit files under ../agentscm-work
agentscm --data-dir ../agentscm-data workspace checkpoint ../agentscm-work OPERATION_ID 0
```

Each checkpoint takes a new operation ID and the expected workspace generation (`0`, then `1`, ...). The workspace contains no `.git`. Excluded and unsupported entries are absent from it. Workspace checkpoints do not update the Change's review context; see [workspace recovery](workspace.md).

## Initialize and land

Complete the context's omissions and unknown fields, replace the Change's context with `context`, approve the captured revision, then run `initialize-target` with `expected_target` set to `null`. The captured head may have a parent. An existing target is rejected.

For workspace work T to A to B on that target, the Change only needs to exist at the version you pass; `prepare-landing` rebinds it using the supplied context. Run `prepare-landing` with `source_tip` B, that Change version, and a context that names B, base T, and the next Change version. Approve the returned landing revision with the returned change version and context digest, then run `integrate` with `expected_target` equal to T. See the README for request shapes.

This flow is trusted-local and tested on synthetic fixtures. It is not a live cutover.

## Profiles `agentscm-git-snapshot/v1` and `v2`

Only the committed tree of the selected commit is read (`git ls-tree -r`). Untracked and ignored files are never captured.

Supported, with portable paths:

- regular blobs (mode `100644`), captured as regular entries;
- executable blobs (mode `100755`), captured as executable entries;
- symlinks (mode `120000`), captured as symlink entries whose blob is the target bytes. The target must pass the store's canonical rule: UTF-8, at most 1,024 bytes, relative, `/`-separated, no empty or `.` component, any `..` only as leading components and no more than the link's directory depth, at least one normal component with a portable path and no reserved name (`.git`, `.agentscm`, `.agentscm-workspace`, `agentscm-data`). A target is never resolved against the filesystem, so a dangling in-tree target is accepted.

The report's and receipt's `profile` is `agentscm-git-snapshot/v1` when no captured entry is `100755` or `120000`, and `agentscm-git-snapshot/v2` when any is. A tree of only `100644` entries (or whose other entries are all excluded) therefore produces the same revision ID and receipt bytes as before this change. Each receipt entry's `mode` records its kind: `100644` regular, `100755` executable, `120000` symlink (`size` is then the target length).

Only the kind is preserved. Group and other permission bits, setuid and setgid bits, timestamps, ownership and extended attributes are not recorded, because Git does not record them either. A workspace creates files with the process default mode (`0666`, or `0777` for executables, minus the umask).

Each of these makes the tree ineligible (`unsupported[].kind`):

| Kind | Cause |
| --- | --- |
| `symlink_target` | Mode `120000` whose target is not an in-tree relative canonical path (absolute, escaping the tree, reaching `.git` or another reserved name, empty, longer than 1,024 bytes, not UTF-8, or otherwise non-canonical). The detail names the rule. The link is never captured. |
| `gitlink` | Mode `160000` (submodule). Its content is another repository. |
| `unknown_mode` | Any other mode or object type. |
| `non_portable_path` | Path is not valid UTF-8, or fails native path validation: ASCII only, at most 1,024 bytes, components at most 255 bytes, no empty, `.` or `..` component, no backslash, no control characters or `<>:"\|?*`, no component ending in `.` or space, no Windows device names (`con`, `prn`, `aux`, `nul`, `com1`-`com9`, `lpt1`-`lpt9`, with or without an extension). |
| `case_alias` | Two captured path spellings, including directory prefixes, differ only in ASCII case. |
| `file_directory_conflict` | A captured path is also a directory prefix of another, compared case-insensitively. |
| `reserved_namespace` | Any path component equals `.git`, `.agentscm`, `.agentscm-workspace` or `agentscm-data`, ignoring ASCII case. |
| `ignored_tracked` | A tracked path matches the repository's own ignore rules (`git ls-files --cached --ignored --exclude-standard`). |
| `invalid_exclusion` | An `--exclude` path that is not a tracked symlink or gitlink in the commit. |

`ignored_tracked` only reports conflicts with the repository's ignore rules. It is not secret detection. Nothing in this profile scans content for credentials or personal data. Review what is committed before capturing, and keep receipts and data directories private because they list every path.

Declared exclusions: `--exclude PATH` applies only to a tracked symlink or gitlink whose path is valid UTF-8 and equals `PATH` exactly. The entry is left out of the revision and listed in the receipt's `excluded` array with its mode, object ID and size, and it is included in `manifest_sha256`. A symlink that is not declared is captured under profile v2 when its target is valid. Executable files, non-portable paths, hostile symlink targets and everything else cannot be excluded; change the commit instead.

Cleanliness: eligible only if `HEAD` equals the selected full commit and `git status --porcelain=v2` reports no tracked change (staged, unstaged, renamed or unmerged). Entries marked skip-worktree or assume-unchanged (`ls-files -v` tag `S` or lowercase) hide edits from `status`, so each counts as a tracked change. Status ignores submodule state (`--ignore-submodules=all`); gitlinks are unsupported unless excluded. Untracked non-ignored files are counted in `untracked_count`, allowed, and never captured. The ignored-tracked check runs only when the tree is clean; for a dirty tree it is skipped, so it cannot flag problems alongside a dirty tree. A shallow repository is reported (`shallow`) but not rejected.

Filters: any `filter.*` key in the repository configuration makes the tree ineligible and is listed in `filters`. When one is configured, preflight does not run `status` or `ls-files --ignored` at all (they can run clean filters), so `cleanliness.clean` is `false` with `tracked_changes` 0 and `head` read by `rev-parse`. The tree is still classified with `ls-tree`. Captured bytes are the stored blobs, which could differ from a filtered checkout.

Bounds (a violation appears in `limit_violations` and makes the tree ineligible):

| Limit | Value |
| --- | --- |
| Per-file bytes | 64 MiB (67,108,864) |
| Total content bytes | 128 MiB (134,217,728) |
| Files plus directories | fewer than 100,000 (reported maximum 99,999), the local workspace bound |
| Directory depth | 128 (count of `/` in a path), the local workspace bound |
| Path length, component length | 1,024 and 255 bytes (see `non_portable_path`) |

The store's limits of 4,096 changes, 16,384 directory prefixes and 128 MiB apply per batch (see below). Capture additionally enforces 128 MiB as a whole-snapshot bound, so content bytes alone never cause a second batch.

## Git invocation safety

Git is a trusted, operator-selected executable. The minimum supported version for the snapshot bridge is 2.45.0. Before any repository-scoped command, AgentSCM runs the absolute executable as `git --version` with an empty environment except `LC_ALL=C`, a 60 second timeout and a 4,096-byte combined standard-output and standard-error budget. It accepts one UTF-8 line in Git's `x.y.z`, `x.y.z.windows.N` or `x.y.z (Apple Git-N[.N...])` form and rejects older versions and every other suffix or prerelease form with the typed `UnsupportedGitVersion` error. A nonzero probe exit, timeout, excess output and malformed output produce the same error without inspecting the repository. Executable-startup I/O failures, such as a missing or nonexecutable Git path, remain I/O errors.

The 2.45.0 floor is the version the snapshot bridge checks before it reads a repository. The tagged 2.45.0 manuals document the complete command surface used here: [`git` global options](https://github.com/git/git/blob/v2.45.0/Documentation/git.txt), [`rev-parse`](https://github.com/git/git/blob/v2.45.0/Documentation/git-rev-parse.txt), [`config`](https://github.com/git/git/blob/v2.45.0/Documentation/git-config.txt), [`status`](https://github.com/git/git/blob/v2.45.0/Documentation/git-status.txt), [`ls-files`](https://github.com/git/git/blob/v2.45.0/Documentation/git-ls-files.txt), [`ls-tree`](https://github.com/git/git/blob/v2.45.0/Documentation/git-ls-tree.txt) and [`cat-file`](https://github.com/git/git/blob/v2.45.0/Documentation/git-cat-file.txt). This establishes upstream support at the minimum; the runtime probe has been exercised with Apple Git 2.50.1 (Apple Git-155), but a Git 2.45.0 executable has not been run in this repository's tests.

After the version check succeeds, every repository-scoped invocation uses a 60 second timeout, bounded output (1 MiB for small queries, 16 MiB for status and ignored lists, 64 MiB for the tree listing, chunk-sized for blobs), and this environment, with everything else cleared:

```text
GIT_CONFIG_NOSYSTEM=1  GIT_CONFIG_GLOBAL=/dev/null  GIT_TERMINAL_PROMPT=0
GIT_ATTR_NOSYSTEM=1  GIT_OPTIONAL_LOCKS=0  LC_ALL=C  HOME=/nonexistent
```

and these flags: `-C <repo> --no-replace-objects --no-lazy-fetch --no-optional-locks --literal-pathspecs -c core.hooksPath=/dev/null -c core.fsmonitor=false -c core.quotePath=false -c core.untrackedCache=false`.

Repository-scoped commands run: `rev-parse` (top level, object format, shallow state, commit and tree), `config --get-regexp ^filter\.`, `rev-parse --path-format=absolute --git-common-dir`, `status --porcelain=v2 -z --branch --untracked-files=all --ignored=no --ignore-submodules=all`, `ls-files -z -v`, `ls-tree -r -l -z --full-tree`, `ls-files --cached --ignored --exclude-standard`, and `cat-file --batch`. `status` and `ls-files --ignored` run only when no `filter.*` key is configured. None runs a candidate file or hook, and with this rule none runs a filter. The tool does not fetch, push, commit, check out or write refs.

Limits of this isolation: the Git binary itself is trusted. The repository's own `.git/config` is still read; only the settings listed above are overridden, so other repository-local configuration still applies to these commands. Replace objects and lazy fetch are disabled, but repository attributes and ignore files are read as Git normally reads them. `status` may stat every tracked file. Do not point this at a repository or Git executable you do not trust. There is no sandbox.

## Receipt

The receipt is one compact JSON document with a trailing newline. It contains no timestamps, process IDs or randomness, so an identical rerun produces the same bytes (see below for what identical means). Fields:

| Field | Meaning |
| --- | --- |
| `receipt` | `agentscm.git-snapshot-receipt/v1` |
| `profile` | `agentscm-git-snapshot/v1`, or `agentscm-git-snapshot/v2` when a captured entry is executable or a symlink |
| `provenance` | Fixed text: `local operator declaration; Git observations are unauthenticated and are not review evidence` |
| `native.repository` | Authority repository ID the revision was published into |
| `native.revision` | The native revision ID |
| `native.batches` | Number of store batches used |
| `git.version` | `git --version` output |
| `git.repository` | Git top-level path as observed. This is a machine-specific path; the receipt is private operator data |
| `git.object_format` | `sha1` or `sha256` |
| `git.shallow` | Whether the repository is shallow |
| `git.commit`, `git.tree` | Commit and tree object IDs |
| `cleanliness` | `head`, `clean`, `tracked_changes`, `tracked_change_paths` (first 20), `untracked_count` |
| `manifest_sha256` | See below |
| `entries` | Captured entries: `path`, `mode` (`100644`, `100755` or `120000`), `oid`, `size`, sorted by path bytes |
| `excluded` | Declared exclusions, same shape |
| `totals` | `files`, `directories`, `bytes` of captured entries |
| `content_verification` | `sha1-rehashed` or `sha256-rehashed` |

`manifest_sha256` is the lowercase hex SHA-256 over `entries` and `excluded` merged in path-byte order, each as the bytes `"{mode} {oid} {size} {path}"` followed by a NUL. It pins the Git side of the capture, including exclusions.

`content_verification`: every blob is independently re-hashed as `blob {len}\0{bytes}` with the repository's object format and compared with its object ID. SHA-256 uses the existing SHA-256 implementation; SHA-1 uses ring's already pinned `SHA1_FOR_LEGACY_USE_ONLY` implementation rather than adding another dependency or an in-tree cryptographic implementation. This detects bytes that do not match the recorded object ID but does not provide collision resistance for SHA-1 or authenticate Git, the repository or the receipt.

Receipts created before SHA-1 re-hashing record `git-reported-sha1`. Capture now records `sha1-rehashed`, so an old receipt at the requested receipt path conflicts during the pre-publication receipt check. Preserve or move the old receipt and choose a fresh receipt path for the new capture.

The receipt records what the local operator ran and what Git reported. It is a declaration plus unauthenticated observations. It is not signed, not review evidence, and not a check result. It does not prove the checkout, Git or the operator's machine were uncompromised.

## Batching, restart and idempotency

The store limits one checkpoint to 4,096 changes, 16,384 summed directory prefixes and 128 MiB of content. Capture splits the sorted entries into contiguous batches within those limits and publishes them with `checkpoint_batches`. The result is one revision; intermediate trees are never referenced by a revision, so no intermediate revision exists. `native.batches` and the command's `batches` report the count. Because of the 128 MiB total bound, a file count above 4,096 or a prefix count above 16,384 is what produces more than one batch.

Rerunning the same request against the same tree publishes the identical revision (content-addressed, no timestamps). The receipt records checkout observations (for example the untracked file count), so it is identical only when the request, Git executable version, checkout observations and data directory are all the same. The receipt is written last, with no-clobber semantics:

- the path does not exist: written, `existing: false`;
- an identical rerun finds the same receipt content (all fields except `native.revision`, which is compared after publication): left unchanged, `existing: true`;
- the path holds anything else, including a receipt for a different commit or one differing in any observation such as the untracked count: `conflict` error before any object is published, file untouched. The pre-check compares parsed JSON while the final write compares bytes, so a file that is semantically equal but formatted differently, or that differs only in `native.revision`, passes the pre-check and conflicts at the final write, after publication; the file is still left untouched.

Each batch builds its tree in memory and writes only the objects reachable from that tree: every object file is synced before it is linked into place, then the objects directory and each touched shard directory are synced once. The next batch and the revision are written only after that, so no published revision references an object that is not durable. Superseded trees from earlier batches remain as unreferenced objects.

If capture stops before or during publication, only unreferenced objects can remain. They are retained (there is no garbage collection) and reused by a rerun. If it stops after publication and before the receipt, rerun with the same arguments to obtain the same revision and write the receipt.

The following ignored tests are the opt-in qualification fixtures. Run these exact parent tests separately; a broad `--ignored` run would also invoke an internal interruption helper that requires environment supplied by its parent test.

```sh
cargo test --locked --test integration git_snapshot_cli::capture_qualifies_shared_deep_prefixes_across_native_batches -- --ignored --exact --nocapture
cargo test --locked --test integration git_snapshot_cli::interrupted_multi_batch_capture_retries_to_complete_identical_revision -- --ignored --exact --nocapture
cargo test --locked --test integration git_snapshot_cli::snapshot_entry_limit_is_precise_before_publication -- --ignored --exact --nocapture
cargo test --locked --test integration git_snapshot_cli::snapshot_byte_limits_are_precise_before_publication -- --ignored --exact --nocapture
```

The multi-batch fixture captures 130 files below 127 shared directory components, requiring two production checkpoint batches because the 16,510 summed prefixes exceed one batch. It checks the CLI receipt, retry, native history and every captured byte. The interruption fixture kills a child process with `SIGKILL` after the first of those batches is durable, observes retained orphan objects with no revision, receipt, target move or event, and checks that retry matches an independent uninterrupted capture. This qualifies a process kill at that boundary, not power loss or every possible interruption boundary.

The limit fixtures exercise the production bounds without a 100,000-file checkout: Git plumbing makes 50,000 root directories refer to one one-file subtree, and three paths refer to one sparse 64 MiB + 1 blob. These index-free fixtures deliberately leave the checkout dirty; eligibility therefore also fails cleanliness, while the assertions independently require the exact `entries`, `file_bytes` and `total_bytes` violations and their production maxima. They check that preflight never creates its nonexistent data directory and that capture against a separately initialized store writes no objects or receipt and does not move the target. These are exact boundary observations, not performance benchmarks.

## Limits and not covered

- Performance: one observation on one machine (Apple silicon, macOS, APFS, release build), not a benchmark. Synthetic fixtures of 300 files (2.6 MB) and 5,000 small files took 49 s and 1,028 s with per-object syncs, and 7 s and 121 s with per-batch syncs. On macOS every sync is a full device flush (`F_FULLFSYNC`), so syncs dominate. `capture` reports progress on standard error; `preflight` does not estimate store time. The 60 second Git timeout applies to each Git command, not to the whole capture.
- Executable and symlink entries are preserved as kinds only (see the profile above). Consumers that cannot represent them refuse a revision holding one with `unsupported entry kind`: Git export profile v1 and Git reconciliation (which also refuses a tree whose snapshot would use profile v2), execution snapshots, merge, source-paths checks, offline local backup of a workspace tree (`unsupported backup entry`) and team source publication (source profile v3 is not accepted). A workspace refuses a filesystem that cannot hold the kinds it must write (`unsupported filesystem`).
- Snapshot capture does not import Git history, branches, tags or submodules. The separate `git-export` bridge can retain one bounded local SHA-1 commit closure as ancestry in a newly owned mirror; it does not turn that Git history into native revisions.
- SHA-1 and SHA-256 object-format repositories are exercised end to end with synthetic Git repositories, including independent blob content re-hashing, native byte readback and same-size loose-object corruption rejection before publication.
- Multi-batch capture is exercised by the opt-in CLI qualification above at one shared-prefix boundary. This is one frozen synthetic fixture, not a scale benchmark or general performance claim.
- Crash behavior is not qualified at every boundary. The opt-in qualification above covers a process killed after its first durable batch, and store tests check batch sync order and an injected failure partway through a batch flush. Power loss is not tested.
- Paths in Git that are not valid UTF-8 are refused.
- The snapshot does not detect secrets, and Git binary, repository configuration and filesystem are trusted as described above.
