---
title: "Local workspace snapshot timeline"
---

# Local workspace snapshot timeline

> **Experimental (alpha).** This local native-workspace feature may change without migration.
> It is not an attached-Git history, backup system, approval record or automatic agent audit.

The workspace timeline records selected observations of an existing native AgentSCM workspace.
Like native workspaces, the timeline and its local client adapters require Unix (including macOS).
Each changed observation creates an immutable source revision and adds an entry to that
workspace's local journal without checkpointing, advancing or otherwise changing the workspace.
Unchanged observations reuse the matching revision and add no entry.

Capture is deliberately small: it scans the workspace, writes changed source objects and records
the result. It starts no timer, command check, test or model call. Manual capture and
agent-triggered capture use the same journal and retention budget.

## Capture and inspect

All commands use the global `--data-dir` that owns the workspace's source objects. `ROOT` must be
an existing native workspace; attached Git worktrees use their separate lifecycle.

```sh
agentscm --data-dir DATA workspace capture ROOT
agentscm --data-dir DATA workspace timeline ROOT --limit 50
agentscm --data-dir DATA workspace timeline ROOT --limit 50 --before NEXT_ID
agentscm --data-dir DATA workspace entry ROOT ID
agentscm --data-dir DATA workspace diff ROOT LEFT RIGHT --limit 100
agentscm --data-dir DATA workspace diff ROOT LEFT RIGHT --limit 100 --after CURSOR
```

For a workspace created before timeline support, the first capture or writable workspace command
migrates its metadata. Read-only state and status inspection remain available. Timeline reads do
not migrate metadata and return an empty timeline until an entry has been recorded.

The timeline is newest first. `--before` continues from the returned timeline cursor. Entry reads
show either a captured snapshot or an exact-source check association. Diff results are bounded;
use the returned cursor with `--after` until no continuation is present. Pagination is an
inspection boundary, not proof that the timeline covers activity from before this feature was
enabled. The diff's `entries` list is filtered to paths inside both recorded scopes. A page can
be empty and still have a continuation cursor; keep following `next`. Cursors and missing-object
identifiers describe the underlying complete source. Path selection is not an access-control
boundary, and comparison may require source objects outside the selected paths.

A check association records both the source revision observed when the check began and the
revision actually tested. That lets readers distinguish a test of the selected bytes from a test
of some other revision. The link is a reference to retained evidence, not the evidence itself.
When its retained workflow is available, the entry view also projects that workflow's current
source applicability and landing binding. Applicability still depends on the current Change
version, review context, policy, target and target revision. Exact native workflow checks record
their own check-start associations; a capture hook does not run a check. Existing native review,
human approval and landing requirements are unchanged. Recording a check link is best effort:
if the timeline is full or a capture holds its writer lease, the check still runs and reports
that its link was not retained. Inspect the workflow by its run ID in that case.

The same three reads are available over HTTP when the operator starts
`agentscm serve --timeline-workspace ROOT`. That pins one workspace, and requests cannot name
another. See [local template and timeline reads](api.md#local-template-and-timeline-reads).
Capture and fork are not available over HTTP.

## Fork an earlier observation

Forking materializes the selected entry's source revision into a new directory and preserves the
entry's whole-tree or selected-path scope.

```sh
agentscm --data-dir DATA workspace fork ROOT ID CHILD CHANGE --fork-root FORK_ROOT
```

`FORK_ROOT` must already exist, while `CHILD` names a new direct child that does not exist. The
destination becomes a new native workspace for an existing Change in the same repository. That
Change must differ from the source workspace's Change, and its current review context must name
the selected entry's source revision. Forking does not create a Change, approve source, land it,
publish it or merge it.

## Local agent clients

The workspace client provides a local stdio MCP server with its filesystem authority fixed when
the process starts:

```sh
agentscm --data-dir DATA workspace-client mcp --workspace ROOT
agentscm --data-dir DATA workspace-client mcp --workspace ROOT --fork-root FORK_ROOT
```

The common tools are `agentscm_snapshot_capture`, `agentscm_snapshot_timeline`,
`agentscm_snapshot_entry` and `agentscm_snapshot_diff`. `agentscm_snapshot_fork` is enabled only
when the operator starts the server with `--fork-root`. Tool calls cannot choose another data
directory, workspace, fork root, executable or model. The server is local stdio; it is not the
Hub MCP endpoint and does not add a network listener. The optional HTTP reads above are served by
`agentscm serve`, not by this process.

Five local clients have completion-hook adapters. Use user-level settings so these files remain
outside the source workspace:

| Client | Completion event | Hook settings | MCP settings | Qualification |
|---|---|---|---|---|
| Claude Code | `Stop` | `~/.claude/settings.json` ([hooks](https://code.claude.com/docs/en/hooks#hook-locations)) | `~/.claude.json` ([MCP scopes](https://code.claude.com/docs/en/mcp#mcp-installation-scopes)) | Synthetic settings and hook-payload fixtures; no live client run yet |
| Codex | `Stop` | `~/.codex/hooks.json` ([hooks](https://developers.openai.com/codex/hooks)) | `~/.codex/config.toml` ([configuration](https://developers.openai.com/codex/config-reference#mcp-servers)) | Synthetic settings and hook-payload fixtures; no live client run yet |
| Cursor | `stop` | `~/.cursor/hooks.json` ([hooks](https://cursor.com/docs/hooks)) | `~/.cursor/mcp.json` ([MCP](https://cursor.com/docs/mcp)) | Synthetic settings and hook-payload fixtures; no live client run yet |
| GitHub Copilot CLI | `agentStop` | A named file such as `~/.copilot/hooks/agentscm-workspace.json` ([hooks](https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/use-hooks)) | `~/.copilot/mcp-config.json` ([MCP](https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-mcp-servers)) | Synthetic settings and end-turn payload fixtures; no live client run yet |
| Gemini CLI | `AfterAgent` | `~/.gemini/settings.json` ([hooks](https://geminicli.com/docs/hooks/)) | The same `~/.gemini/settings.json` ([MCP](https://geminicli.com/docs/cli/tutorials/mcp-setup/)) | Synthetic settings and hook-payload fixtures; no live client run yet |

Installation edits operator-local client settings outside the source workspace and previews the
exact changes by default. Inspect the preview before repeating with `--apply`:

```sh
agentscm --data-dir DATA workspace-client install \
  --workspace ROOT \
  --agent AGENT \
  --hook-settings HOOK_SETTINGS \
  --mcp-settings MCP_SETTINGS
```

The settings directories must already exist. Stop the client before using `--apply`, then restart
it and verify both the hook and MCP server. Claude Code, Codex, Cursor and Copilot require separate
hook and MCP files, and the installer refuses the same resolved path for both. Gemini permits the
same settings file for both changes. Use exactly the same path spelling for a shared file;
paths that differ only by letter case are refused for every client.

An applied preview confirms only that AgentSCM updated the selected files. It does not prove that
the client loaded them or that the feature is enabled. Current Codex releases enable hooks by
default under `features.hooks`; `codex_hooks` is a deprecated alias used by older releases. If an
older Codex build gates hooks behind `codex_hooks`, enable that feature before qualification.
Check the effective feature and hook trust in Codex. In Gemini CLI, use the `/hooks` panel to
confirm the installed hook is enabled. Then confirm the local MCP server appears in the client's
MCP status view.

The installer refuses an existing AgentSCM capture hook it cannot safely adopt. For Claude Code
and Codex it also refuses a separate capture hook beside an existing automatic AgentSCM workflow
`Stop` hook. The workflow check records its own exact check association. A completion hook is
best effort and does not block the client when capture fails.

Other local hosts that can launch a stdio MCP server can use the generic command directly. Grok
is not a separately qualified integration: AgentSCM has no native Grok hook adapter and no live
Grok host validation. A cloud-hosted client or API, including hosted Grok, cannot reach a stdio
process on the operator's machine. Generic tool calls are voluntary, so missing calls create gaps
rather than inferred activity. The stdio protocol has synthetic fixture coverage but no live
generic-host validation.

## Retention and security limits

The journal admits at most 4,096 entries and 1 GiB of cumulative changed source bytes. It pauses
new entries when either bound is reached; it does not evict earlier entries. An agent-triggered
capture consumes the same limits as a manual capture. There is no history backfill, garbage
collection or automatic retention policy. This is an admission limit for retained entries, not a
physical disk quota. Source publication happens before the final journal fence: a crash, deadline
or concurrent workspace change can leave immutable objects without a timeline entry. Those
objects are not automatically reclaimed.

Snapshots are immutable source history. If a later edit deletes a credential or other sensitive
text, an earlier retained snapshot still contains it. Rotate exposed credentials and treat the
store as containing that value; a later capture does not erase old objects, and there is no
selective timeline erasure command. Deleting the workspace removes its timeline journal, while
content-addressed source objects remain in the data store because garbage collection is not
implemented.

The workspace root, data directory and optional fork root should be separate operator-controlled
directories. Snapshot capture is observation, not isolation: code running as the same local user
may still read or change accessible files. A timeline entry does not prove an agent authored the
bytes, saw the entry or performed every intermediate edit.
