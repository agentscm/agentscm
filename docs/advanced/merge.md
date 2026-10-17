# Durable local native merges

> **Experimental (alpha).** This part of AgentSCM is not part of the main path, may change without migration, and has had limited testing. See the [advanced index](README.md).

Merge sessions compare native source revisions without invoking Git. They keep
base and side identities, deterministic conflicts, the current selected blob
references and generation-checked operation outcomes in a repository-bound
SQLite journal. This is a trusted-local workflow. It grants no protected-target
admission, authentication, remote publication or retention lease.

Begin from an initialized external repository data directory:

```sh
agentscm --data-dir ./data merge begin begin-1 LEFT RIGHT
agentscm --data-dir ./data merge show SESSION
agentscm --data-dir ./data merge bases SESSION --limit 100
agentscm --data-dir ./data merge conflicts SESSION --limit 100
agentscm --data-dir ./data merge conflict SESSION CONFLICT_ID
```

Each successful mutation returns the exact session and generation. States are
`planning`, `awaiting_base`, `conflicts`, `ready`, `publishing`, `completed` and
`aborted`. Views page bases and conflicts; conflict pages omit preview bytes.
`pending_operation` identifies accepted unfinished work even when a resolution
has not changed the session phase or generation.
`--cursor FILE` reads a JSON string from a previous page's `next` value. Cursors
bind the session, generation and view, with a local checksum. They are not
authorization tokens. A changed generation invalidates them.

All best common ancestors are retained in sorted identity order. One best base
is selected automatically. Multiple best bases require an explicit choice from
the recorded set; unrelated histories require explicit consent to an empty
file-set baseline. This never invents an ancestor revision:

```sh
agentscm --data-dir ./data merge base SESSION base-1 GENERATION base.json
```

`base.json` is either `{"choice":"revision","revision":"REVISION"}` or
`{"choice":"empty"}`. Empty is rejected when actual common bases exist.

## Decisions and conflicts

Identical sides and one unchanged side select actual blob references directly.
Same-file disjoint edits use bounded exact line edit scripts. Automatic
composition requires a unique optimal alignment to the base; repeated-line
alignment ambiguity remains a durable content conflict. A nonempty output
segment without a terminating newline may only be the final nonempty segment;
otherwise the merge remains a content conflict instead of joining distinct
lines or inventing a newline. Empty deletion segments and terminal missing EOF
newlines remain valid. The pinned text merger supplies overlap previews. The
automatic profile is at most 128 KiB and 512 lines per input, UTF-8 without NUL,
compatible uniform LF or CRLF, preserving EOF. Inputs outside that profile or
shared text budgets remain durable `text_profile` or `binary` conflicts.
Overlap marker text is an inspection preview, capped at 32 KiB per conflict and
1 MiB per plan; it is never automatic resolved source. No semantic-merge claim
is made.

Edit/delete, add/add, exact-content rename ambiguity, rename/edit or differing
rename targets, file/directory and portable-name collisions remain inspectable
conflicts. Rename detection uses exact blob identity, not similarity or intent.
Related structural paths form one group. Original base/left/right references
remain visible after resolution. A resolution view also retains the exact
current selected blob references or deletions, indexed into the immutable
ordered `paths` list to avoid duplicating long path names.

```sh
agentscm --data-dir ./data merge resolve SESSION resolve-1 GENERATION CONFLICT_ID resolution.json
agentscm --data-dir ./data merge complete SESSION complete-1 GENERATION
```

Resolution JSON choices are `{"choice":"left"}`, `right`, `base`, `delete`,
or `{"choice":"bytes","content":[104,101,108,108,111]}`. Manual bytes apply
only to a single-path conflict. Group choices apply to that group's original
paths. Combined exact and portable structural validity is checked before a selected
result is acknowledged. A fresh operation at the current generation may
revise a selected conflict before publication. The CLI caps JSON at 16 MiB;
the library caps manual file content at 64 MiB.

Resolution first records its exact choice and any manual bytes durably. Recovery
reconstructs and hash-verifies that input before publishing a blob. A structural
collision returns a durable `rejected` outcome, retaining the current plan and
generation so a fresh operation can correct the choice. Missing, uncertain or
corrupt source leaves the exact operation pending; it is not converted to a
deterministic rejection.

Completion first journals publication intent, then publishes dependencies and
an R2 revision with ordered parents `[left,right]`, and qualifies actual complete
candidate closure before acknowledging success. Equal heads have a journaled
no-op. Distinct heads with equal trees retain both ordered parents. Completion
does not move a Change context or protected target. Review and admission must
bind the resulting exact source and current context separately.

## Recovery and workspace application

Every mutation binds repository, session, operation ID, expected generation and
exact inputs. Identical retries return the original durable outcome; mismatched
operation reuse conflicts. Pending inputs are durable before planning or source
publication. If begin fails before returning a session, find it by operation:

```sh
agentscm --data-dir ./data merge operation begin-1
agentscm --data-dir ./data merge recover SESSION
agentscm --data-dir ./data merge abort SESSION abort-1 GENERATION
```

Missing or uncertain source cannot become a completed outcome. Recovery resumes
the frozen operation after required objects arrive. Abort closes pending work
and invalidates its generation; late computation cannot acknowledge success.
Objects written before an abort can remain unreferenced. Exact retry depends on
the intact journal; restoration of older backups has no acknowledged-outcome
guarantee.

Apply requires an opaque receipt obtained from the actual completed session:

```sh
agentscm --data-dir ./data merge apply ./work SESSION MERGE_GENERATION apply-1 WORKSPACE_GENERATION
agentscm --data-dir ./data workspace recover ./work --choice complete
```

The receipt binds repository, session generation, left and resulting revision.
The workspace base must equal left, and actual first-parent/complete-source
checks precede application. The workspace journal freezes both generations and
the source bindings before filesystem mutation. Its existing staged replacement
and recovery path preserves unexpected dirty bytes as blocked outcomes, and
unrelated edits remain. Roll-back uses `workspace recover --choice roll-back`.
Workspace schema v1 upgrades transactionally to v2, preserving old staged data
and operation outcomes. Older binaries reject v2; back up source and workspace
consistently before upgrading valuable data. See [workspace limits and filesystem
race boundaries](workspace.md).

## Bounds and remaining gates

Ancestry admits at most 100,000 union revisions, 800,000 edges and 6,600,002
charged steps. Planning admits at most 100,000 files per manifest/union, 4,096
retained changed/grouped paths, four million charged steps, one million examined
directory prefixes and 100,000 retained folded prefixes. Shared automatic text
budgets are eight million conservative line-pair work units and 32 MiB each of
inputs and outputs. These are admission bounds, not measured time or stack
guarantees. Source planning/publication reads cap actual encoded bytes at
256 MiB per operation; native objects retain their individual parser bounds.
Per-object buffers and up to three native file buffers can still be live.

Plans cap serialized bytes at 16 MiB, bases at 8 MiB, outcomes at 4 KiB and
retained journal operations at 10,000. Active sessions reserve terminal row and
generation capacity for completion or abort. Graph, source, manifest or journal
limits fail explicitly and can leave pending work for inspection/recovery/abort;
they do not produce a partial successful merge. Text-profile/budget limits are
retained per-path unresolved conflicts.

Pending resolution intent caps JSON metadata at 4 KiB and raw manual bytes at
64 MiB per operation, with a 128 MiB global pending-byte admission cap. Completion,
rejection and abort clear the temporary payload atomically while retaining the
request digest and durable outcome. This is logical record cleanup, not secure
filesystem erasure or a backup retention guarantee.

Local objects persist because no collector runs. Journal references are not
collector-visible pins. Before any collecting/remote deployment, session inputs,
bases, resolution blobs, candidate, workspace and backup roots need the same
authorized retention and collector serialization as publications. Authenticated
views, sparse/chunk exchange, remote pointer convergence, exhaustive termination
injection, measured scale and independent interoperability remain required.
