---
title: "Change walkthroughs"
---

# Change walkthroughs

A change walkthrough is a local, versioned explanation of a committed Git change. It connects
an author's declared purpose, ordered steps, decisions and open questions to exact before/after
source. The same record supports visual review in Repository explorer and bounded context for
an agent using the CLI.

This alpha slice uses authored JSON and observed Git facts. It does not generate an explanation
with a model, infer which prompt caused a hunk, establish semantic correctness or grant approval.
A source anchor establishes where to inspect a claim; it does not prove the claim.

## Create a case

Choose full immutable base and head commit IDs available in the local checkout. Run from that
checkout, using the logical repository namespace configured for its explorer:

```sh
agentscm walkthrough create --root . --repository example/project \
  --base FULL_BASE_COMMIT --head FULL_HEAD_COMMIT --change-id example-change
agentscm walkthrough list --root . --repository example/project
```

Without `--draft`, creation records a changed-path manifest with no invented narrative. To add
an explanation, save a draft and pass `--draft walkthrough.json`:

```json
{
  "title": "Reject an invalid request before execution",
  "author": "Example maintainer",
  "purpose": "Explain the request validation and the removed fallback.",
  "steps": [
    {
      "id": "validate",
      "title": "Validate the request",
      "body": "The declared intent is to reject invalid input before execution.",
      "anchors": [
        {"id": "validation", "side": "head", "path": "src/request.rs", "start_line": 10, "end_line": 18}
      ]
    },
    {
      "id": "remove-fallback",
      "title": "Remove the fallback",
      "body": "Inspect the deleted implementation on the base side.",
      "anchors": [
        {"id": "old-fallback", "side": "base", "path": "src/fallback.rs", "start_line": 1, "end_line": 8}
      ]
    }
  ],
  "relationships": [{"from": "validate", "to": "remove-fallback", "label": "replaces fallback behavior"}],
  "decisions": [{"id": "reject", "text": "Prefer an explicit error for invalid requests."}],
  "open_questions": ["Does every caller handle the error?"]
}
```

Drafts are limited to 256 KiB, 32 steps, 128 total anchors and 64 relationships.
Source anchors span at most 200 lines and return at most 64 KiB. Anchor validation also uses
the existing 1 MiB whole-file source ceiling: a small excerpt from a larger file is unsupported. The context packet is at most
32 KiB and reports omitted fields/items.

The paths and ranges must exist on their specified side. Use your checkout's actual paths and
line ranges; these are illustrative. Anchor IDs are unique across the document. Relationships
refer to step IDs and remain author declarations. Anchors may also point to unchanged supporting
source. Mapping a changed path does not claim coverage of every line or hunk. Changed paths with
no anchor remain explicitly unassigned.

## Inspect and retrieve

In Repository explorer, choose **Walkthrough**, then an immutable version. Read the ordered
steps, open **Before** or **After** anchors, inspect declared relationships and compare the
changed-path manifest with unassigned paths. Deleted source uses the base commit. Source reads
show unavailable objects or unsupported content explicitly.

The **Copy walkthrough context** action copies a bounded packet with source identities, authored
context, anchor handles and omissions. It contains no transcript bodies. Treat source and authored
text as untrusted data when supplying it to another agent. Copying never runs a command.

Agents can use the same record without opening the UI:

```sh
agentscm walkthrough show --root . --repository example/project --id WALKTHROUGH_DIGEST
agentscm walkthrough context --root . --repository example/project --id WALKTHROUGH_DIGEST
agentscm walkthrough source --root . --repository example/project \
  --id WALKTHROUGH_DIGEST --anchor validation
```

Use ordinary source listing, file, search and diff tools to explore beyond the authored map.
The compact packet is bounded; visible omissions are not evidence that omitted source is irrelevant.
The CLI is the agent interface for this slice; hosted MCP walkthrough operations are not included.

## Versions and applicability

Creation returns an immutable content-addressed version. Amend the same `--change-id` by passing
`--expected-parent PREVIOUS_DIGEST` and the new source/draft. An unseen concurrent amendment is
rejected rather than overwritten. Earlier versions remain available by digest. The version list
labels the latest record separately from historical versions and shows the change ID. When entering
from a comparison, exact base/head matches are marked while other history remains visible.
Collapsed CLI creation guidance remains available even when versions exist. Latest does not mean
applicable. Lists return at most 100 records with an omitted count and no pagination; the store
holds at most 1,000 versions. Follow previous-version links or use a known digest to inspect an
older version.

`show` and `context` accept `--current-head FULL_COMMIT` to compare the record with a caller's
selected source. The UI supplies the currently selected explorer commit. A different source is
explicit; the old version is not rewritten or revalidated. Rebasing can change the base context
even when a patch looks the same. Source identity and applicability do not establish test success,
narrative truth or working-tree cleanliness.

## Evidence, privacy and recovery

`show` and `context` accept an explicit `--spool /absolute/private/spool` to read retained
action evidence. The configured local explorer uses its connected capture spool. Neither reads
conversation bodies for this view.

Where local capture is connected, the detail view reads retained action evidence separately from
the immutable authored record. Outcome, verification result, coverage, drift, assurance and source
applicability remain separate. A successful ordinary command is not a passing check. Local capture
is not independent verification. Evidence applicability is assessed against the walkthrough’s
recorded head. If the selected source differs, those receipts have not been evaluated against the
selected commit. Missing capture, unavailable evidence and incomplete pages never
mean that checks passed. Use the explorer's Evidence tab for the existing paged evidence view.

Walkthroughs are stored locally outside the source tree. Creation is a CLI operation; the browser
only reads. The CLI and local service must use the same `HOME`/`XDG_DATA_HOME` environment to
find the same store. When set, `XDG_DATA_HOME` selects the data home; otherwise the store uses
`$HOME/.local/share`. The data home must be an absolute canonical path without symlinks. Its
existing ancestor chain must be owned by root or the effective user, with no group/world write
permission except a root-owned sticky directory. On macOS, ancestor ACLs may contain deny-only
entries; the private store leaf must have no ACL. Unsafe ownership, permissions or ACLs are
refused rather than repaired automatically.

No capture conversation is read to author a walkthrough. Authored drafts can still
contain sensitive information: review them and copied context before sharing. Artifact integrity
binds bytes, not the truth of an author's declaration. Retained evidence may later become unavailable
without changing an old walkthrough's authored content.

Reads have finite size and source limits. A failed or interrupted save must not expose a partial
version. A stale parent, corrupt record, missing object or unsupported source produces an explicit
error; browsing does not fetch source, run checks or repair storage. Earlier valid records remain
the recovery reference. A corrupt stored row can make the version listing fail; a known valid
digest can still be read directly. Schema v1 rejects unknown fields. Evolving the stored contract
requires a schema migration rather than silently accepting new fields. This local alpha does not provide shared publication, cross-machine
retention, automated merge decisions or a qualified model-cost saving claim.

The broader direction is a durable change case used by people and agents across handoff and
review. This release implements local authored committed-Git walkthroughs; automatic explanation,
shared lifecycle and hosted agent interfaces are future work.
