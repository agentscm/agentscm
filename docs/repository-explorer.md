---
title: "Repository explorer"
---

# Repository explorer

The repository explorer connects committed Git source with its history and locally retained
agent evidence through a read-only HTTP API served by `agentscm serve` (see [Read API](#read-api)).
AgentSCM has no web interface for it.

This is a read-only alpha feature for Unix hosts. It reads one checkout selected by the local
operator. It does not fetch, check out a branch, run an agent or execute a check.

## Connect the optional read-only API

Setup, capture controls and diagnostics use the [existing CLI](connect-this-computer.md). Source reads remain available
through native source commands and the explicit explorer API below.

Start the local service yourself. Choose a local read secret with
at least 32 bytes and set `AGENTSCM_SERVE_READ_TOKEN` in the service environment. Use a separate
secret from your Hub credential. Start the service with absolute paths and the logical repository
ID used by your AgentSCM connection:

```text
agentscm --data-dir /absolute/local-data serve \
  --bind 127.0.0.1:7331 \
  --explorer-repo /absolute/repository \
  --explorer-repository example/project
```

`--browser-origin` is optional: set it to the exact origin of a browser page of your own that
calls the API. Every request requires the read token, even from an allowed origin. A non-loopback bind is refused for the explorer, including with `--public`.

With a manually started service, include existing local capture by adding
`--explorer-spool /absolute/private/spool`. Its configured repository namespace must match `--explorer-repository`. This is an
explicit opt-in to reading private bodies through the local service. Configure capture separately
using [Local change capture](change-capture.md). Browsing does not initialize, recover, acknowledge
or compact a spool.

Without a spool, source reads remain available. Missing services, unsupported content and malformed responses produce an error or an
explicit unavailable state. The explorer never substitutes example data.

## Understand the selected source

Selecting a branch or ref resolves it to an immutable commit. Tree, file, history, diff and
search requests use that full commit ID. Refresh observes refs again; it does not change the
checkout. The selected commit, observed local HEAD, working-tree observation and observation time
are separate facts. Uncommitted working files are not the displayed committed source.

Working-tree observation checks staged changes against the observed HEAD and lists untracked
paths. It does not inspect tracked working-file contents: Git can run repository-configured
filters while inspecting them. Observed staged or untracked changes set `dirty` to `true`;
otherwise it is `null` (unknown), never a claim that the working tree is clean.

Remote-tracking refs describe what this checkout currently holds. They do not prove the remote's
latest state or its last fetch time. Offline browsing uses locally available objects; shallow
history and missing objects remain explicit limits.

File history follows the exact selected path. A renamed file's earlier name is a separate history
query. Long author names and commit subjects end with an explicit `[truncated]` marker when
shortened to fit the response bounds; their commit IDs remain navigable. Diff rename/copy labels
are Git's similarity interpretation. Symlinks and submodules are
shown as entries and never followed. Binary and oversized files are identified without rendering
their content. Source search matches literal, case-sensitive text.

## Explore component connections and history

The `map` read takes a committed source (`commit`), optionally a comparison base (`base`) and a
directory (`path`). It groups files by directory and shows **static reference candidates** from Rust module/use
statements and JavaScript/TypeScript imports and re-exports. It is a lexical navigation aid,
not a complete dependency graph, execution trace or impact analysis. Unsupported syntax,
conditional compilation, aliases, runtime imports and generated code may be missing or ambiguous.
Rust references under `src/bin` remain unresolved because binary module roots need manifest context.
Module declarations and `use` references in files with detected path attributes remain unresolved.
Outside-view references remain distinct from unresolved references. Symlinks and submodules
are listed but never followed. No source, package resolver or model is executed.

Each side returns at most 384 files, 128 blob reads (64 KiB each, 4 MiB total), 1,024
connections and 128 unresolved references. Directory selection is deterministic, not a relevance
ranking. Both sides share one 30-second deadline. Coverage reports known omissions and unknown
remainders; choose a smaller directory when a view is incomplete. A failed read shows an error,
not an empty graph. Source changes use blob identities, so body-only edits remain visible even
when connections are unchanged. Incomplete reference comparisons never assert a removed dependency.
Renames appear as removal and addition. Repository-wide reference comparisons often remain
incomplete because unresolved-reference limits are reached; use a smaller directory for better
coverage. Grouped Rust `use` imports may remain unresolved. If a shallow checkout lacks a
comparison parent, map the head commit alone; that does not claim a comparison.

Authored walkthrough versions for the repository retain their own declarations. Those declarations, decisions and recorded checks
retain their own source identities and assurance. A map connection does not establish that a
check covered a component, or that a decision caused a change. Working-tree edits are excluded.

## Follow a change walkthrough

The `walkthroughs`, `walkthrough`, `walkthrough-source` and `walkthrough-context` reads return
immutable, locally authored versions for this checkout and repository: steps into exact base/head
source, declared relationships, unassigned changed paths and bounded agent context. Different
selected source remains explicit. Creation and amendment use the CLI; reading never writes or
calls a model. See
[Change walkthroughs](change-walkthrough.md) for the draft format, commands and limits.

## Read evidence and conversation

An action receipt records its own source binding, recipe, result and coverage. Matching a clean
recorded source to the selected commit does not say that the current working tree passed a test.
Different source and unknown applicability remain visible. A successful process with partial
coverage is not a complete verification result.

The `evidence` read lists conversations whose Work has a recorded action for the selected commit.
The `sessions` read lists every saved conversation for the repository, newest first, so a
session with no recorded action is still reachable. Being listed there means only that the
conversation belongs to its Work and session; it is not associated with any commit.

Conversation candidates share a Work and session with recorded activity. Capture v1 does not
establish which prompt caused a file or hunk, so the explorer does not present those associations
as authorship or motivation. A path selection does not strengthen that relationship. Work links
are available only when the local repository namespace matches the signed-in Hub repository.

Visible Claude prompt, assistant and tool bodies are available only when local-conversation
capture retained them. Metadata-only capture has no bodies. The Codex adapter's unsupported
prompt and assistant capture remains unavailable. Hidden reasoning is excluded. Text and tool
JSON are returned as data.

The service resolves bodies only through retained event references and validates their digest
and size. Bodies outside the current read remain explicitly unverified; incomplete validation is
not reported as complete capture. Missing, deleted, expired, oversized or damaged bodies remain unavailable. Ordinary
event associations can be lost after explicit spool compaction; the projection reports that gap
without assigning it to an invented Work or session. Retained action receipts can still be shown.

Walkthrough context includes only loaded facts, immutable source references, declared gaps and
explicit pagination or omission limits in a bounded data packet. Repository text and conversation
remain quoted data in that packet. Review private conversation before pasting a packet into another service.
Reading it does not resume a session or run a command.

## Bounds and recovery

Each HTTP response is limited to 4 MiB. Text file bodies are limited to 1 MiB. Git reads have
finite process/output budgets and paginated responses distinguish more results from a hard
read limit. A bounded result never means the remaining repository or capture was searched.

There is no persistent explorer index. Refresh recomputes the bounded projection from current
retained records. Fix an invalid checkout or capture configuration and restart the local service. A malformed or replaced spool is refused,
with detailed diagnostics kept in the local service. Existing capture recovery and export
commands remain the way to manage the spool; the explorer does not repair it implicitly.

Stopping the local service removes API access. Removing retained capture through its existing
lifecycle removes the explorer's access on a later read. The explorer creates no transcript cache,
shared body store or separate retention policy. Shared transcript access and lifecycle are outside
this local feature.

## Read API

All routes use `GET /v1/local/explorer/…` and the local service's bearer token. Browser requests must
use an explicitly allowed origin. Unknown or duplicate query fields are rejected.

| Route | Required query | Optional query |
| --- | --- | --- |
| `summary` | — | `ref` (defaults to `HEAD`) |
| `map` | `commit` | `base`, `path` |
| `tree` | `commit` | `path`, `after`, `limit` |
| `file` | `commit`, `path` | — |
| `history` | `commit` | `path`, `cursor`, `limit` |
| `diff` | `left`, `right` | `path`, `cursor`, `limit` |
| `search` | `commit`, `q` | `cursor`, `limit` |
| `evidence` | `commit` | `path`, `work_id`, `cursor`, `limit` |
| `walkthroughs` | — | `limit` |
| `walkthrough` | `id` | `current_head` |
| `walkthrough-source` | `id`, `anchor` | — |
| `walkthrough-context` | `id` | `current_head` |
| `sessions` | — | `cursor`, `limit` |
| `conversation` | `work_id`, `session_id` | `cursor`, `limit` |

Except for `summary`, source arguments must be full lowercase commit IDs. Cursors are opaque and
bound to their query. Follow the returned cursor with the same source, filters and page limit.
Requests cannot select a filesystem root, spool directory or arbitrary artifact.
