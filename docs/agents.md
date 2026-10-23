# Coding agents: what is supported

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See the [README](../README.md) for status and the [changelog](../CHANGELOG.md) for known limits.

AgentSCM warns an agent when another agent already holds a file it is about to edit. How well that works depends on how the agent connects to the [hub](hub.md). There are two ways, and most agents can use both:

- **Hooked.** The agent runs a hook before each edit and the hub's answer is put in front of the model without the agent having to choose to ask. This is the more reliable path (see [Support tiers](#support-tiers)). Where the hook can refuse a tool call (Claude Code, Codex, Copilot), the first edit in a session to a file another agent holds is refused once, with a factual reason naming the other agent and the options: do other work, ask the user, or repeat the edit. Repeating it passes. The refusal also asks the holder to hand the file over, and says so; the same request can be made with the `agentscm_ask` MCP tool. The holder sees the request on its next call and answers with `agentscm_handoff` ([handoff](hub.md#handoff)). Handoff is experimental ([handoff](hub.md#handoff)).
- **Advisory (MCP).** The hub serves coordination [MCP](https://modelcontextprotocol.io) tools at `POST /mcp`: `agentscm_start`, `agentscm_check`, `agentscm_status` and `agentscm_done` for advice, and `agentscm_ask` and `agentscm_handoff` for the experimental handoff. The agent calls them because its instructions (`AGENTS.md`) tell it to. A model can forget or skip them, so treat this as a best effort.

The read-only `agentscm_workflow_status` MCP tool inspects a retained native skill workflow when the operator has configured a repository mapping. It preserves recorded outcomes, current source applicability and unavailable capabilities as separate facts. See [skill workflows in the hub](advanced/skill-workflow-hub.md).

The separate local `agentscm --data-dir PATH skill-workflow mcp` stdio server lets an operator give
an MCP-capable client read-only reusable-workflow inspect and handoff tools, with configure and check
available only through explicit startup flags. Its native store, workspace and execution backends
are fixed at startup and cannot be supplied by a tool call. See
[reusable workflow templates](advanced/reusable-workflows.md#local-mcp-front-door).

The [asynchronous stand-up](standup.md) adds durable engineering work and checkpoint handovers alongside file intents. Its agent-authenticated CLI and MCP surfaces can retrieve current unfinished work and a bounded catch-up before a task, then create or reuse an explicit work item and report meaningful changes. `agentscm_work_briefing` reads the briefing; `agentscm_start` resolves and claims explicit work; `agentscm_work_checkpoint` records a meaningful change; and `agentscm_work_ack` deliberately advances a completely delivered writer briefing. A supported session-start hook can render the bounded briefing, and a safely linked end hook can record pause or handover. Hooks do not create a goal, acknowledge delivery, infer decisions or meaningful progress, or claim completion as accepted or landed. The durable workflow also provides a session-specific catch-up cursor and a directed coordination inbox with explicit receipts, replies and validated resolution. Requests appear in later MCP calls and supported hooks; disconnected clients catch up through the inbox. A model can also skip an MCP call. A reported checkpoint is coordination context, not trusted execution or landing evidence.

Nothing is locked: a refusal happens once per session and file, a repeat always passes, and any failure lets the edit through. Cursor's hook cannot refuse, so Cursor gets a note just after the edit, and an agent that only has MCP gets advice only. Details: [warn once](hub.md#warn-once).

## Support tiers

| Tier | Agents | How |
| --- | --- | --- |
| **Wired by `agentscm init-repo`** | Claude Code, Codex CLI, Cursor, GitHub Copilot in VS Code | MCP config, hooks and instructions committed to the repository. |
| **Wired by `agentscm client setup --agent claude\|codex\|cursor\|copilot\|gemini`** | Claude Code, Codex CLI, Cursor, GitHub Copilot in VS Code, Gemini CLI | Binds one checkout to an existing hub and installs that agent's native hooks and MCP config locally, without committing files. Candidate behavior: not yet qualified end to end across all five clients or against a hosted hub ([onboarding](team-member-onboarding.md)). |
| **Recipe** (hand configuration; not tested here) | Copilot coding agent and Copilot CLI, Gemini CLI and Antigravity, Windsurf, Devin, Amp, OpenCode, Cline | Point the agent's MCP client at the hub and give it the `AGENTS.md` snippet. |
| **Advisory MCP only** | Roo | No hook; relies on the model following the instructions. |
| **Not supported** | Aider | No hook or MCP client to attach to. |
| **Untested** | Jules | Unknown. |

"Wired" means the generated files match each vendor's current documentation (see [Verified against](#verified-against)). It does not mean the combination has been run in every product version. The hook client is tested against a stub hub with sample payloads written from the vendors' documentation, not in live sessions of each agent. Claude Code is the reference integration. The Codex, Cursor and Copilot wiring follows their documented hook formats. Gemini CLI gets instructions only: `init-repo` writes a `GEMINI.md` that imports `AGENTS.md`, but no MCP config and no hook, so nothing checks its edits unless you add the hub as an MCP server by hand (see [recipes](#recipes-for-other-agents)).

## Wire a repository

One person on the team runs, from the repository:

```sh
agentscm init-repo --hub-url https://hub.example.com
```

(`agentscm init` is the older command that prepares a hub data directory; `init-repo` is the one that writes agent config.) It shows the files it will create or change, with a diff, and asks once. `--agents claude,codex,cursor,copilot,gemini` limits it, `--yes` skips the question. It is safe to run again: files that are already right are reported unchanged and nothing is written. It never writes a token. Commit the result in one pull request.

| Agent | Files |
| --- | --- |
| Claude Code | `.mcp.json`, `.claude/settings.json`, `CLAUDE.md` (one line, `@AGENTS.md`) |
| Codex CLI | `.codex/config.toml` (a marked block), `.codex/hooks.json` |
| Cursor | `.cursor/mcp.json`, `.cursor/hooks.json`, `.cursor/rules/agentscm.mdc` |
| Copilot in VS Code | `.vscode/mcp.json`, `.github/hooks/agentscm.json`, `.github/copilot-instructions.md` |
| Gemini CLI | `GEMINI.md` (one import line; instructions only, no MCP config or hook) |
| All | `AGENTS.md` (a marked block telling agents to start, to call `agentscm_check` before editing unless Claude Code, Codex or Copilot hooks already check their edits, to say so when coordination is unavailable, to do other work or ask when another agent holds a file, to answer handoff requests, and to finish) |

Each developer then creates their own agent token (`agentscm hub token create --repo OWNER/NAME`, see [work intents](intents.md)) and sets it once in their shell profile:

```sh
export AGENTSCM_HUB_TOKEN=<your agent token>
```

On Windows PowerShell, `setx AGENTSCM_HUB_TOKEN "<your agent token>"` and restart the terminal. Tokens never go into checked-in files: the generated config only names the variable. Copilot in VS Code asks for the token once, through VS Code's secret prompt, for its MCP server; its hook command (`agentscm hook copilot`) still reads `AGENTSCM_HUB_TOKEN`, so set the variable there too.

These files need a hub that serves `/mcp` and `/v1/hub/hook/<agent>`. `init-repo` merges into existing JSON files: your own entries are kept, but keys are rewritten in alphabetical order, and a file with comments (JSONC) is left alone with a message.

### Choose source at task start and when dependencies grow

The generated instructions also teach selective source use in native and Git workflows:
inspect paths before reading contents, start with a bounded working set, and explicitly add
dependencies before editing them. Native operations need an operator-provided store and exact
revision; coordination setup does not create either. Git-only tasks can use a separate sparse
worktree without adopting native source control. Small or tightly coupled tasks can use a full
checkout. Neither mode fetches automatically when an editor opens a missing file.

Follow the [agent source workflow](advanced/agent-source-workflow.md) for safe commands,
dependency expansion, full-tree validation and occasional measured savings reports. Existing
installations receive the updated marked instruction block by rerunning `init-repo`; read its
proposed diff and reload agent instructions. This remains model-followed guidance, not proof that
every agent obeyed it. Hooks still handle coordination only.

### What needs the `agentscm` binary

- **Claude Code needs no install.** Its hooks are HTTP hooks that call the hub directly, and `.mcp.json` is a remote MCP server. Claude Code does not support HTTP hooks for `SessionStart`, so the session starts, for the hub, at the first edit check.
- **Codex, Cursor and Copilot call `agentscm hook <agent>`** on each developer's machine, because their hooks run commands. Install it with `npx agentscm` or see [install](install.md). Their MCP servers need no install.

If the binary or the hub is missing, the hooks do nothing visible (see below). MCP still works wherever the agent supports it.

## What `agentscm hook` does

`agentscm hook <claude|codex|cursor|copilot|generic> [--hub-url URL]` reads the agent's hook JSON on standard input, sends it unchanged to `POST /v1/hub/hook/<agent>` with the token as a bearer header (for an edit, the JSON includes the edit's contents; the hub keeps only [the few fields it needs](hub.md#data-the-hub-stores)), and prints the hub's reply, which is already in the agent's own hook output format.

- The token comes from `AGENTSCM_HUB_TOKEN`, else the file named by `AGENTSCM_HUB_TOKEN_FILE`, else `hub.json` as for `agentscm intent`. It is never a command-line argument.
- The hub URL comes from `AGENTSCM_HUB_URL`, else `--hub-url` (the generated hook files carry it), else `hub.json`. A token stored in `hub.json` is only ever sent to the hub that file names, so a checked-in `--hub-url` cannot redirect it; to use a different hub, set `AGENTSCM_HUB_TOKEN`.
- `--repo OWNER/NAME` (written by `init-repo` from the `origin` remote when it names exactly two path segments) adds `?repo=` to the hook and MCP URLs. If the token belongs to another repository the hub answers "coordination unknown" and records nothing, which catches one `AGENTSCM_HUB_TOKEN` reused across repositories.
- It sends the Git root of the agent's working directory as `X-Agentscm-Root` so the hub can make edited paths repository-relative.
- It gives up after 3 seconds.
- `AGENTSCM_HUB_TOKEN` is sent to whatever hub URL the command carries, because you set it for this purpose. Treat a pull request that changes a `--hub-url` in a hook file like a change to your CI secrets: review it.
- It prints only an allowlist of the hub's reply: context and message fields, Cursor's `permission` (always `allow`) and one kind of permission decision, a `deny` with a non-empty reason (the warn-once refusal), and that only for a pre-tool event and once per identical edit per session (a marker file, like the unknown note; a repeat of the same edit passes). It drops everything else (`ask`, `allow`, `defer`, a deny without a reason, `continue`, `decision`, `stopReason`, updated or modified inputs and tool output), so a wrong reply cannot stop the agent or rewrite its call.

### Hooks fail open

The command always exits 0. If anything goes wrong (no hub configured, no token, the hub is unreachable or slow, it rejects the token, or it sends something unusable) it prints the agent's valid "no information" reply, which for most events is `{}` and for a few events is a one-line `AgentSCM: coordination unknown (<reason>)` note, given once per agent session (a marker file under `~/.config/agentscm/hook-notes/`, removed after 2 days); later failures in that session print the no-op reply. A broken setup never stops an edit. The same holds on the hub side: its hook endpoint answers HTTP 200 in the agent's format even for a bad token.

The cost is that failures are quiet. If warnings never appear, run `agentscm intent start` in the repository, or check the token, the URL and that the hub is up.

## Limits

- A warning appears only for edits the agent makes through its file-edit tools. Files written by shell commands are not announced.
- Claude Code, Codex and Copilot answer before the edit, in the same hook run, and can refuse it once. Claude Code shows a hook's added context to the model only after the tool ran, so a note alone arrives too late; that is why a contested edit is refused instead. The Copilot CLI's `preToolUse` has no field for a note, so it gets the refusal but no other notes. Cursor's `preToolUse` hook has no documented way to carry text to the model without denying the call, so `init-repo` wires Cursor's `postToolUse` hook instead: the file is announced and the warning arrives just after the first edit of it. Treat Cursor as warn-just-after.
- Claude Code's HTTP hook cannot tell the hub where the repository root is (the project directory is not available to HTTP-hook headers), so the hub makes paths relative to the directory the agent was started in. Start Claude Code at the repository root; from a subdirectory its edits are announced under shorter paths and may not match other agents' paths. The command hooks (`agentscm hook`) send the Git root and do not have this limit.
- Codex hooks run only when the project is trusted, and Codex asks the developer to review new or changed hooks in `/hooks` once; regenerating the hook file asks again.
- Claude Code asks each developer to approve the project's MCP server and trust the folder before project hooks run.
- Where the edited file path sits in Cursor, Copilot and Codex payloads is only partly documented; the hub reads the fields that vendor documents and ignores the rest.
- The hub is trusted. Its replies go into each agent's context, and the URL that decides which hub that is sits in committed config: a hook's `--hub-url` and the MCP server URL. Whoever controls that URL controls the text the agents using the config see, and receives their token and hook payloads. Review changes to those files, and treat the hub as trusted.
- Coverage depends on every agent on the team being wired. An unwired agent is invisible. A pull-request check as a backstop is planned, not built.
- Stand-up claims persist across agent sessions, but only reported checkpoints are visible. A stale or unknown claim remains claimed; inspect the work and use the explicit takeover flow instead of assuming it is available.

## Recipes for other agents

These are not generated and not tested here. The shape is always the same:

1. Add `https://<hub>/mcp` as a remote (streamable HTTP) MCP server with the header `Authorization: Bearer <agent token>`. Where the agent's config supports it, read the token from an environment variable instead of writing it into a checked-in file.
2. Make sure the agent reads `AGENTS.md` (or paste the snippet from it into the agent's own instructions file).
3. If the agent can run a command before edits and wants a JSON answer, `agentscm hook generic` accepts `{"agent", "session", "event", "tool", "paths", "task"}` on standard input.

Grok has no native AgentSCM setup, hook or workflow adapter in this repository. A Grok host with an
MCP client can use the generic remote Hub recipe above, or an operator can separately configure the
local reusable-workflow stdio server. Neither route is a live-tested Grok integration.

### Report usage to work insights

All supported clients can use the provider-neutral `agentscm_work_usage` tool on the Hub MCP,
including through the local client proxy. The native workspace task MCP is a separate surface and
does not expose Hub work tools. The equivalent universal CLI path is
`agentscm work usage --request FILE|-`. Claude Code, Codex CLI, Gemini CLI, Cursor and Copilot use
the same record. Grok has no native AgentSCM adapter; when configured as a Hub MCP client, it can
use the same MCP tool.

These are structured reports, not automatic usage capture. Use `unknown` for an agent, provider or
model label the client cannot determine, and omit token or cost fields that are unavailable. Do not
treat missing values as zero. A `provider_result` basis is still a caller report; the Hub does not
verify it against a provider. See [work insights](work-insights.md) for the record and evidence limits.

`agentscm client reporting on|off|status` controls silent completion observations for a saved
checkout connection; the default is off. Enabled native session-end hooks and Hub MCP completion
calls record only known completion metadata, with usage and cost unknown. After a setting change,
a running MCP proxy stops on its next request without forwarding it; restart it before continuing.
See [completion reporting](work-insights.md#optional-completion-reporting)
for supported calls and coverage limits.

## Workflow lifecycle capability matrix

Matrix version 3, updated 2026-10-22 (see [Verified against](#verified-against)). It records, per client surface, what this repository's code and tests cover for the workflow lifecycle. Only the two CLI rows' Stop cells rest on a run in the real client product; every other cell is fixture-tested, unverified or unsupported.

Cell values:

- **Fixture-tested**: exercised by tests with synthetic payloads or a stub hub built from vendor documentation.
- **Live-qualified**: verified in a real client session, with the mode and version named in the cell. Scope is that mode and version only.
- **Documented, not verified**: the vendor documents the capability, but AgentSCM installs no adapter and nothing was run.
- **Unsupported**: no wiring exists, or the vendor surface cannot do it.
- **Not verified**: not wired or not checked against this surface.

| Surface | MCP participation | Edit coordination | Completion trigger (Stop) | Blocking on completion |
| --- | --- | --- | --- | --- |
| Claude Code CLI | Not verified: `init-repo` writes `.mcp.json` from vendor docs; no client session was run. The hub tools are covered in the last row | Fixture-tested: pre-edit hook, refuses once per file and session | Live-qualified (print mode, CLI 2.1.304): local `claude-stop`. Fixture-tested: `remote-stop`, `task-stop` | Live-qualified (print mode, CLI 2.1.304): `claude-stop` blocked with the failing test's output, passed after the fix and stopped after 8 blocks. Fixture-tested: `remote-stop`, `task-stop`; at most 8 consecutive continuations; per vendor documentation checked 2026-10-20 the Claude cap resets on tool calls |
| Claude Code desktop/IDE | Not verified: MCP config is the CLI's | Not verified: hook support in these surfaces is not checked | Not verified | Not verified |
| Codex CLI | Not verified: `init-repo` writes `.codex/config.toml` from vendor docs; no client session was run | Fixture-tested: pre-edit hook, refuses once per file and session | Live-qualified (`codex exec` with `--dangerously-bypass-hook-trust`, CLI 0.163.0): local `codex-stop`. Fixture-tested: `remote-codex-stop`, `task-codex-stop` | Live-qualified (same mode): one block with feedback, then a pass after the fix. The 8-block bound was not live-tested; it is fixture-tested. Per vendor documentation checked 2026-10-20 Codex documents no cap of its own |
| Codex IDE/desktop | Not verified | Not verified: hook support is not checked | Not verified | Not verified |
| Cursor | Not verified: `init-repo` writes `.cursor/mcp.json` from vendor docs; no client session was run | Fixture-tested: post-edit note only; the hook cannot refuse | Not verified: no Stop adapter | Unsupported: no adapter, the edit hook cannot refuse, and per vendor documentation its `stop` hook can only return `followup_message` to continue (default `loop_limit` 5), not block |
| Copilot | Not verified: `init-repo` writes `.vscode/mcp.json` from vendor docs; no client session was run | Fixture-tested: pre-edit hook, refuses once; the CLI gets the refusal but no notes | Not verified: no Stop adapter | Documented, not verified: Copilot CLI and cloud agent document `agentStop` with `decision: block` and `stop_hook_active`, capped at 8 consecutive blocks in the CLI. VS Code agent hooks (preview) document a `Stop` event; not verified. AgentSCM installs no adapter for either |
| Any MCP client | Fixture-tested: the hub's `POST /mcp` coordination tools ([hub](hub.md#agent-sessions-mcp-and-hook-endpoints)), the workspace [agent-task MCP](advanced/agent-workflow-tasks.md) and the local [reusable-workflow stdio server](advanced/reusable-workflows.md#local-mcp-front-door) are exercised with synthetic protocol clients, not live vendor sessions. Hub coordination tools are advice only. The local server's configure and check tools require explicit startup grants; its paths and backends are fixed by the operator. A model can still skip calls | Unsupported: MCP alone has no hook, so no edit is intercepted | Unsupported: MCP has no completion event | Unsupported |

Live qualification, 2026-10-20: one macOS machine, a synthetic fixture, a local native store, a Docker command check and a Claude review step. Claude Code print mode used `claude -p` with `--settings` pointing at a file written by `install-stop`. Codex used `codex exec` with hooks in an isolated `CODEX_HOME` `hooks.json` written by `install-stop`; hooks fired only with `--dangerously-bypass-hook-trust`, and a native workspace also needs `--skip-git-repo-check`. In both, an independent `skill-workflow check` confirmed a receipt for the current revision after the fix. Not live-tested: interactive Claude Code, Claude desktop/IDE, Codex interactive (`/hooks` trust), Codex IDE/desktop, the hosted `remote-*` stops and the agent-owned `task-stop` and `task-codex-stop`. The hosted and agent-owned stops are fixture-tested against a synthetic local HTTP hub harness, including token revocation, an unreachable endpoint, uncheckpointed edits and a stale subject.

Sources: the edit hooks and wiring are the "Implemented (alpha)" fixture-tested row in the [roadmap](roadmap.md) and are described in [warn once](hub.md#warn-once) and [work intents](intents.md); the coordination and workspace MCP endpoints are described in [hub](hub.md#agent-sessions-mcp-and-hook-endpoints) and [agent-owned workflow tasks](advanced/agent-workflow-tasks.md); the Stop adapters are described in the [Stop hook guide](advanced/claude-code-workflows.md#opt-in-claude-code-stop-hook). Stop hooks are installed by `skill-workflow install-stop`, not by `init-repo`.

A Stop trigger only prompts the agent to run the check. Omitting, misconfiguring or bypassing a hook does not bypass native landing policy, which requires an exact-current receipt independently.

## Verified against

The file formats were checked against the vendors' public documentation in October 2026: [Claude Code hooks](https://code.claude.com/docs/en/hooks) and [MCP](https://code.claude.com/docs/en/mcp), the Codex documentation for hooks and MCP servers, [Cursor hooks](https://cursor.com/docs/hooks) and [MCP](https://cursor.com/docs/mcp), and the GitHub Copilot and VS Code documentation for hooks and `mcp.json`. Matrix version 2 adds live CLI runs of Claude Code 2.1.304 and Codex CLI 0.163.0 (2026-10-20) and the vendor pages checked the same day: [Cursor agent hooks](https://cursor.com/docs/agent/hooks), [Copilot hooks configuration](https://docs.github.com/en/copilot/reference/hooks-configuration) and [VS Code agent hooks](https://code.visualstudio.com/docs/copilot/customization/hooks). Vendors change these formats; if a generated file stops working, please open an issue.
