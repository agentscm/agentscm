---
title: "Versioned source graphs and local inspection"
---

# Versioned source graphs and local inspection

> **Experimental (alpha).** This part of AgentSCM is not part of the main path, may change without migration, and has had limited testing. See the [advanced index](README.md).

Existing `AGSCM-R1\0` revision objects keep version 1, zero or one parent,
and their exact bytes and SHA-256 IDs. The new `AGSCM-R2\0` encoding carries
version 2 and two through eight ordered, unique parents. Both use a big-endian
u32 version, u8 parent count, 32 raw bytes per parent ID and a 32-byte tree ID.
Unknown magic/version, an invalid parent count, duplicate parents, truncated
IDs and trailing bytes are rejected. Parent order is part of revision identity.

`Engine::checkpoint_with_parents` starts from the first parent's tree and applies
explicit file changes. It does not automatically merge the other trees. A
protected target accepts a candidate only when its first parent equals the exact
expected current target. A local offline chain requires a new landing revision
with parents `[current target, offline tip]`, the intended result tree and a fresh
exact ReviewContext and approval. `prepare-landing` builds that
`[current target, offline tip]` revision with the tip's tree. An empty target
still accepts only a genesis revision through `integrate`; `initialize-target`
separately initializes an absent target from a head with any number of parents,
after complete closure qualification, and rejects an existing target.

## Entry kinds

Each exact-index file entry has a kind: regular, executable or symlink. A symlink's blob is its target bytes. Regular entries keep the `AGSCM-L1\0` leaf, so a tree of only regular files keeps its existing object and revision IDs. An executable or symlink entry is stored in an `AGSCM-L2\0` leaf, which is the L1 layout plus one kind byte; decoding rejects an L2 leaf that carries the regular kind. The kind is part of the tree, so a mode-only change or a retarget is a new tree and a new revision, and it appears in diffs. Only these three kinds exist: other permission bits, timestamps and ownership are not recorded. A symlink target must be an in-tree relative canonical path (see `store::validate_symlink_target`); it is checked on every write and on sync import, and nothing follows it. Existing revisions and older readers' regular-only trees are unaffected; a tree with an L2 leaf needs a reader that knows entry kinds.

## Resumable inspection

```sh
agentscm --data-dir data history REVISION --limit 100
agentscm --data-dir data relation LEFT RIGHT --limit 100
agentscm --data-dir data diff-page LEFT RIGHT --limit 1000
```

Each response has `page.next`. Save that JSON string to a file and pass its path
with `--cursor FILE` to resume the same operation and immutable roots. `next =
null` is the completion signal. A page can contain no entries and still have a
continuation. The original `diff` command retains its existing bounded one-page
behavior and path ordering; paged diff has its own traversal order. Paged diff
permits 1 through 4,096 entries and advances at most 64 manifest frontier frames
per page, plus bounded index lookups per encountered file. Its cursor is at most
64 KiB.

History uses an iterative first-parent-ordered graph walk. Shared ancestors are
emitted once. Each page examines at most 1,000 frontier revisions; relation
examines that bound separately for each head. Graph state permits at most
100,000 distinct revisions per head and 16 MiB of cursor bytes. State stores the
visited set and frontier, so resume does not reread prior revisions; parsing and
serializing the bounded cursor still costs work proportional to retained state.
These limits are alpha operation bounds, not measured scale guarantees.

Unavailable graph/index objects return `needed_objects` and a continuation.
First-parent history stops at an unavailable frontier so retrieval can resume
that ordered walk. A failed diff page emits no entries and retains its previous
position; retry after retrieval compares that entire page again. An initial
missing dependency returns a start cursor bound to the revision pair. No
incomplete page is reported as complete. Relation remains `incomplete` while an encountered object is unavailable, or
`pending` while traversal has more work. Only after both histories finish does
it report `equal`, `left_ancestor`, `right_ancestor` or `diverged`. History inspection
is not full source-closure qualification. Missing file blobs may still need to
be retrieved before materialization or publication.

Cursor checksums detect corruption and bind the requested immutable roots.
They are trusted-local inspection state, not secret-authenticated evidence or
admission authority. A trusted local caller can construct a different state;
network clients need server-authenticated cursor handling before these APIs can
support untrusted pagination.

## Remaining source gates

This graph increment does not implement automatic three-way merge, durable
conflict sessions, sparse partial offline drafts, resumable network chunks, Git
migration or the independent protocol verifier. Complete offline source offers
continue to require full source and history qualification. Partial drafts cannot
be admitted as published revisions. Qualification and retention are separate:
a successful local validation does not prevent a later collector or operator
from removing objects.
