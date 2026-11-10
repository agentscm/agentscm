---
title: "Coding agents: what is supported"
---

# Coding agents: what is supported

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See the [README](../README.md) for status and the [changelog](../CHANGELOG.md) for known limits.

AgentSCM warns an agent when another agent already holds a file it is about to edit. How well that works depends on how the agent connects to the [hub](hub.md). There are two ways, and most agents can use both:

- **Hooked.** The agent runs a hook before each edit and the hub's answer is put in front of the model without the agent having to choose to ask. This is the more reliable path (see [Support tiers](#support-tiers)). Where the hook can refuse a tool call (Claude Code, Codex, Copilot, Gemini CLI), the first edit in a session to a file another agent holds is refused once, with a factual reason naming the other agent and the options: do other work, ask the user, or repeat the edit. Repeating it passes while that holder keeps the file; if the file changes hands, the next attempt is refused again. The refusal also asks the holder to hand the file over, and says so (naming the session it asked, such as `bob's Claude Code session 3f2a1b9c`, when it asked one); the same request can be made with the `agentscm_ask` MCP tool. The holder sees the request on its next call and answers with `agentscm_handoff` ([handoff](hub.md#handoff)). Handoff is experimental ([handoff](hub.md#handoff)).
- **Advisory (MCP).** The hub serves coordination [MCP](https://modelcontextprotocol.io) tools at `POST /mcp`: `agentscm_start`, `agentscm_check`, `agentscm_status` and `agentscm_done` for advice, and `agentscm_ask` and `agentscm_handoff` for the experimental handoff. The agent calls them because its instructions (`AGENTS.md`) tell it to. A model can forget or skip them, so treat this as a best effort. A session lists only the core coordination tools by default; rarely needed ones (promises, receipts, declarations, pull-request link setters, reconciliation, transition history) stay callable by name and appear in the list only with the full set, turned on by `AGENTSCM_MCP_TOOLS=all` in the `agentscm client mcp` server's environment or `?tools=all` on a direct Hub URL (see [the Hub's MCP tools](hub.md)).

The read-only `agentscm_workflow_status` MCP tool inspects a retained native skill workflow when the operator has configured a repository mapping. It preserves recorded outcomes, current source applicability and unavailable capabilities as separate facts. See [skill workflows in the hub](advanced/skill-workflow-hub.md).

The separate local `agentscm --data-dir PATH skill-workflow mcp` stdio server lets an operator give
an MCP-capable client read-only reusable-workflow inspect and handoff tools, with configure and check
available only through explicit startup flags. Its native store, workspace and execution backends
are fixed at startup and cannot be supplied by a tool call. See
[reusable workflow templates](advanced/reusable-workflows.md#local-mcp-front-door).

The [asynchronous stand-up](standup.md) adds durable engineering work and checkpoint handovers alongside file intents. Its agent-authenticated CLI and MCP surfaces can retrieve current unfinished work and a bounded catch-up before a task, then create or reuse an explicit work item and report meaningful changes. `agentscm_work_briefing` reads the briefing; `agentscm_start` resolves and claims explicit work; `agentscm_work_checkpoint` records a meaningful change; and `agentscm_work_ack` deliberately advances a completely delivered writer briefing. A supported session-start hook can render the bounded briefing, and a safely linked end hook can record pause or handover. Hooks do not create a goal, acknowledge delivery, infer decisions or meaningful progress, or claim completion as accepted or landed. The durable workflow also provides a session-specific catch-up cursor and a directed coordination inbox with explicit receipts, replies and validated resolution. Requests appear in later MCP calls and supported hooks; disconnected clients catch up through the inbox. A model can also skip an MCP call. A reported checkpoint is coordination context, not trusted execution or landing evidence.

Nothing is locked: a refusal happens once per session, file and hold, a repeat passes while the same hold keeps the file, and any failure lets the edit through. The exceptions are edits under managed Work: a lead whose hold lapsed gets `LEASE_LAPSED`, and an edit not linked to a current assignment of its session gets `EDIT_UNLINKED` (see [required rules](#required-rules-and-the-session-id)); both repeat on every retry until a start or checkpoint changes the Work. In any connected repository, a session's first edit of a file that can be held, while it has declared no Work, gets `WORK_UNDECLARED` once, naming the `agentscm_start` call that declares it, because its person and the team cannot see it and its waits never reach its person. The text also names the CLI route (`agentscm work start` inside the file's checkout) for a session whose MCP tools are bound to another repository. A repeat of that edit passes, and the Hub tells a session again only after a day or a restart. When another session holds the file, that file's warn-once note rides in the same refusal, so the repeat passes both. A host that cannot refuse (Cursor, the generic hook) gets the same text as a note beside the edit; a subagent and a file that is never held are never asked. Health counts these as `undeclared`. `LEASE_LAPSED` advice names the `takeover_generation` the resume needs and the same CLI route. Cursor's hook cannot refuse, so Cursor gets a note just after the edit, and an agent that only has MCP gets advice only. Details: [warn once](hub.md#warn-once).

An edit that resolves a Git conflict is not checked. When a merge, rebase, cherry-pick, revert or stash apply stops on a conflict, Git reports the conflicted files as unmerged until each is marked resolved. Resolving one is not parallel authoring: both sides are already committed, and the conflict has to be resolved in that checkout whoever holds the file. So the connected hook client (`agentscm hook <agent> --client-connection`) asks Git which of the edit's paths are unmerged and leaves those out of what it sends to the Hub. The edit is then checked against no hold and no Work scope (a coordination request the session has not answered still holds it), takes no hold of its own and changes nobody else's, and the agent is told in one line: `AgentSCM: unresolved Git conflict in docs/guide.md in this checkout, so this edit was not checked against other agents' holds and holds nothing. Change only what resolves the conflict; the normal check returns once Git no longer reports the path as unmerged.` Every other path in the same call is checked as usual, and so is the same file once Git stops reporting it as unmerged: after `git add`, or when the merge or rebase ends. A holder of the file hears of the merged result as it hears of any other change to a file it holds: a note when the pull request that carries it is opened, and another when it is merged ([connections and sync](hub.md#github-and-gitlab-connections-and-sync)). Limits: macOS and Linux with Git 2.43 or newer; when Git does not answer within half a second the path is checked as usual; a hook run without `--client-connection` checks every path; and the Copilot CLI has no field for the line.

## What each agent gets

What one session of each agent gets once `agentscm client setup` has wired it, the host has
loaded the hooks and the Hub answers. The first five rows are the agents `client setup` wires;
the last is any client that has only the Hub's MCP server.

| Agent | Rules at session start | Edit check | An edit can be refused | Session end reported | Waiting for you reported | Subagents under the parent's session | Messages from the Hub |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Claude Code | Yes, with the session ID | Before the write | Yes | Yes | Permission prompt, question, end of turn | Yes | At session start, on each prompt, and at an edit, which is held until the session answers |
| Codex | Yes, with the session ID | Before the write (`apply_patch`) | Yes | Yes | Approval request, end of turn | Not checked: a call that names a subagent is handled as Claude Code's, but whether Codex names one was not verified | At session start, and at an edit, which is held until the session answers |
| Cursor | Yes, with the session ID | Just after the write | No | Yes | End of turn only | No | At session start and just after an edit |
| GitHub Copilot | Yes, with the session ID | Before the write | Yes | Yes | Permission prompt, question, end of turn | Not checked: a call that names a subagent is not held for a request addressed to its parent, but whether Copilot sends the parent's session for it was not verified | At session start; before an edit only as the refusal that holds it |
| Gemini CLI | Yes, with the session ID | Before the write (`write_file`, `replace`) | Yes | Yes | Tool permission prompt, end of turn | No | At session start; before an edit as Gemini CLI's `systemMessage`, which does not hold the edit |
| A client with no hooks (MCP only) | In the MCP server's instructions, when the client shows them; no session ID | Only when the agent calls `agentscm_check` | No | Only when the agent calls `agentscm_done` | No | No | Only in the replies to its own AgentSCM tool calls |

What the columns mean:

- **Rules at session start.** The [required rules](#required-rules-and-the-session-id), and for
  a wired agent its native session ID, are the first thing in the session's context.
- **Edit check.** When the Hub is asked about an edit made with the agent's file-edit tools.
  No agent's hook sees a file written by a shell command.
- **An edit can be refused.** The hook's answer can stop the write: a file another agent holds
  ([once per hold](hub.md#warn-once)), the first edit of a session that declared no Work
  (`WORK_UNDECLARED`, once per session), `LEASE_LAPSED`, `EDIT_UNLINKED` or, in Claude Code, Codex
  and Copilot, a coordination request the session has not answered. Where it cannot, the same
  text arrives as a note.
- **Session end reported.** Setup installs a session-end hook for all five wired agents
  (`SessionEnd`; `sessionEnd` for Cursor and Copilot). When it runs, the Hub releases the
  session's file intents and pauses its unfinished Work, without recording that Work as finished.
  A session that is killed sends no end; its hold lapses with its lease
  ([holds stay live](#holds-stay-live-while-the-agent-works)).
- **Waiting for you reported.** See [waiting for you](#waiting-for-you-in-its-session). In VS
  Code, Copilot documents no notification event, so only the end of a turn is reported there.
- **Subagents under the parent's session.** A subagent's hook calls carry its parent's session,
  so its edits are checked under the parent's Work and requests are pointed at the lead session.
- **Messages from the Hub.** Coordination requests addressed to the session. Before an edit,
  Copilot's hook takes a refusal but no note, and Claude Code shows a note only after the tool
  ran, which is why a request the session must answer holds the edit where the hook can refuse.

Each row has a test in `tests/integration/harness_coverage.rs` that runs the installed hook
command with that host's payloads against a real Hub, and the hook events setup installs are
pinned by tests in `src/client_setup.rs` and `src/client_plugin.rs`. The payloads are fixtures
written from the vendors' documentation, not captures from live sessions, so the table states
what AgentSCM does with what each host is documented to send (see [Support tiers](#support-tiers)).
A recorded wait is shown only after 60 seconds, so the tests check that a wait is accepted and
never answered; what the Hub records for it is covered by the Hub's own hook tests.

## Support tiers

| Tier | Agents | How |
| --- | --- | --- |
| **Instructions by `agentscm init-repo`** | Claude Code, Codex CLI, Cursor, GitHub Copilot in VS Code, Gemini CLI | Shared instruction files committed to the repository. No MCP config or hooks; those come from `client setup`. |
| **Wired by `agentscm client setup --agent claude\|codex\|cursor\|copilot\|gemini`** | Claude Code, Codex CLI, Cursor, GitHub Copilot in VS Code, Gemini CLI | Binds one checkout to an existing hub and installs that agent's native hooks and MCP config locally, without committing files. Candidate behavior: not yet qualified end to end across all five clients or against a hosted hub ([onboarding](team-member-onboarding.md)). |
| **Recipe** (hand configuration; not tested here) | Copilot coding agent and Copilot CLI, Gemini CLI and Antigravity, Windsurf, Devin, Amp, OpenCode, Cline | Point the agent's MCP client at the hub and give it the `AGENTS.md` snippet. |
| **Advisory MCP only** | Roo | No hook; relies on the model following the instructions. |
| **Not supported** | Aider | No hook or MCP client to attach to. |
| **Untested** | Jules | Unknown. |

"Wired" means the files `client setup` generates match each vendor's current documentation (see [Verified against](#verified-against)). It does not mean the combination has been run in every product version. The hook client is tested against a stub hub with sample payloads written from the vendors' documentation, not in live sessions of each agent. Claude Code is the reference integration. The Codex, Cursor and Copilot wiring follows their documented hook formats.

## Wire a repository

One person on the team runs, from the repository:

```sh
agentscm init-repo --hub-url https://hub.example.com
```

(`agentscm init` is the unrelated command that creates the native source store in a data directory; `init-repo` is the one that writes agent instructions.) It shows the files it will create or change, with a diff, and asks once. `--agents claude,codex,cursor,copilot,gemini` limits it, `--yes` skips the question. It is safe to run again: files that are already right are reported unchanged and nothing is written. It writes no MCP config, hooks or token. Commit the result in one pull request. The instruction files name no hub, since each developer's Hub is set on their machine: they say to run `agentscm quickstart` once. `--hub-url` is optional (default: `AGENTSCM_HUB_URL`, then `hub.json`); it only fills in the `agentscm quickstart --hub URL` command printed at the end.

| Agent | Files |
| --- | --- |
| Claude Code | `CLAUDE.md` (one line, `@AGENTS.md`) |
| Codex CLI | none; Codex reads `AGENTS.md` directly |
| Cursor | `.cursor/rules/agentscm.mdc` |
| Copilot in VS Code | `.github/copilot-instructions.md` |
| Gemini CLI | `GEMINI.md` (one import line, `@./AGENTS.md`) |
| All | `AGENTS.md` (a marked `<!-- agentscm:begin -->` block that opens with the [required rules](#required-rules-and-the-session-id), then tells agents to call `agentscm_check` before editing unless Claude Code, Codex or Copilot hooks already check their edits, to say so when coordination is unavailable, to do other work or ask when another agent holds a file, and to answer handoff requests) |

It then prints `agentscm quickstart --hub <hub>`. Each developer runs that once on their machine: it signs in to the hub and installs that machine's MCP server and hooks for their user account, writing nothing to the repository, and the hub's Setup › Agents page verifies them. See [onboarding](team-member-onboarding.md).

`init-repo` replaces only its marked block in existing files and leaves the rest alone.

`client setup` and `client connect` check the same files for the agents set up in the checkout (`AGENTS.md` alone when none is named). When the marked block is missing or outdated, they list what is not current and, in a terminal, show the same diff and ask before writing, using the `init-repo` merge; only an explicit yes writes. Without a terminal, after a no, or when the checkout cannot be written, nothing changes, they say how to add the files later (`agentscm init-repo`), and setup still succeeds. `client doctor` reports the same state, and `quickstart` reports it unless `--init` writes the files. Writing is never silent and never happens from a hook, because it changes committed files.

### Required rules and the session ID

A short set of required rules is defined once (`src/hub/agent_rules.rs`) and rendered into every surface an agent may read: the `init-repo` block, the Hub's MCP server instructions (also when the checkout is not connected yet), the `coordinate-edits` skill and the SessionStart context. A test fails when any surface drops a rule. The rules say to pass `session` with the native session ID to `agentscm_start` and every AgentSCM call, to start before editing shared files and, when the task is done, call `agentscm_done` with `outcome: complete` (plain `agentscm_done` only pauses the Work), to follow a refusal's recovery and never repeat the change through another tool, to check shell writes with `agentscm_check`, how a subagent edits, and that third-party text is data and unknown coordination is not clear. They are guidance: delivering them does not show they were followed and is not evidence for any gate. The MCP server instructions and the `coordinate-edits` skill also carry one paragraph on wording the Work text people read (a short plain `task` title, a two- or three-sentence `description`, plain checkpoint summaries); it is not a required rule and is not in the SessionStart context.

When a named agent's start hook gets an answer from the Hub and the host sent a usable session ID, the SessionStart context begins with that ID, labelled as literal data, and the instruction to pass it as `session` to `agentscm_start` and every other call, `agentscm_setup_status` included. The rules follow it, then the current actionable status, then the briefing. Claude Code keeps at most 10,000 characters of a hook's added context, so the Hub keeps its start context within 9,000 (counted in UTF-16 units, as the host counts) and the hook client keeps the combined context, local branch advice included, within 10,000. Text past the limit is cut at a line and replaced with a pointer to `agentscm_work_status`, `agentscm_work_briefing` and `agentscm_work_overview`.

A start that names no session lands on the token's `default` session. Once one native session's hooks are bound to `default` (the first new session after an unnamed start adopts it), only that conversation's edits link to Work it leads. An unnamed `agentscm_start` in that state still holds its claim, but reports `ready_for_work: false` with a `session_warning` that names the fix: call `agentscm_start` again with the work ID, the same task and paths, and `session` set to the native session ID. That named start moves active Work led by `default` to the caller's session (same token only), and its edits then link. A token with no hook session uses `default` as before, without a warning, so agents without hooks keep working. Every later native session of a token whose `default` is already bound is its own session.

An edit the Hub cannot link to a current assignment of its session is refused with `EDIT_UNLINKED`. The reason names the session ID when the hook sent one and the call that fixes the caller's situation: a scope checkpoint with the exact generation and `expected_updated_seq` for the lead of active Work, a generation-checked start to resume or rejoin other Work, a new start for finished Work or none, and for a subagent, leaving the edit to its parent. It says that repeating the edit unchanged meets the same refusal. Every hook refusal (warn-once, lapsed hold, coordination request and unlinked edit) also says that making the change through another tool, such as a shell command or script, bypasses coordination: edit hooks see only file-edit tools. Like `LEASE_LAPSED`, it is refused on every retry of the same edit; only the advisory warn-once refusal passes on a repeat, and only while the same holder keeps the file.

A Claude Code subagent's hook calls carry its parent's `session_id`, with `agent_id` and `agent_type` added, so its edits are checked under the parent's Work with no join or takeover. The rules tell the parent to give a subagent its session ID for AgentSCM calls and tell the subagent not to start or take over Work; where a host does not report the parent's session for a subagent's edits, edits stay in the session that holds the claim. Evidence: Claude Code's hook and subagent documentation and, on one machine (Claude Code 2.1.310, 2026-11-05), transcript records of a subagent that carry the parent session ID. A live capture of a subagent's hook input was not possible in that run, and the Hub behaviour is tested with fixture payloads. How Codex, Copilot, Cursor and Gemini report subagent sessions was not checked.

The checkout's MCP server (`agentscm client mcp`) also fills in `session` on a call that leaves it out, for the tools that take one, so linking a start to its edits does not depend on the model copying the ID:

1. An agent's start hook records its native session ID under the agent host process that ran the hook: one small file per host and session (owner-only on Unix), in `agent-hosts` under the AgentSCM config directory. The file is removed when the session ends, and at a later session start once its host has exited. The MCP server looks through its own ancestor processes for that host and uses the recorded ID.
2. When the host has several conversations open at once, the server cannot tell whose call it is and leaves `session` out. It also leaves it out when it finds no record and the MCP client named itself as one of the hooked agents, or did not name itself. The Hub then places the call as described above.
3. Any other client runs no hooks, so the server gives it one generated name for the life of the process (`<client name>-<8 hex digits>`). Two such clients on one checkout no longer share the token's `default` session.

A `session` the agent passes itself is never changed. The Hub's `SESSION_REQUIRED` refusal is unchanged and still answers callers of `POST /mcp` that do not go through this server. Step 1 reads the process table with `ps`, so it works on macOS and Linux only.

### Calls reach the repository whose files they name

The checkout's MCP server starts in one checkout and uses that checkout's connection by default. A session often edits a second repository on the same computer, and edit hooks route each edit by the file's path. Tool calls route the same way, so Work is declared in the repository whose files change:

- An absolute path in `paths` or `path` sends the call to the connection of the checkout that contains it. The server rewrites the path repo-relative before forwarding.
- `root`, which the server adds to every tool, names another checkout by its absolute path; a path inside the checkout works too. Relative paths are then read against that checkout. Use it for calls that carry no path, such as `agentscm_work_checkpoint`, `agentscm_done` and the reads. The server takes `root` out before forwarding, so a Hub never receives a local path.
- `worktree`, on the tools that have it, routes the call when it is the absolute path of a connected checkout. It is still forwarded as the Work's label.
- Any other call goes to the checkout the server started in, and relative paths are read against that checkout, except as the next paragraph says.

A linked worktree that has its own connection (`client connect --own-connection`) is a separate agent from the main clone. Each hook call also records, in `session-routes` under the AgentSCM config directory, the root of the connection it routed the session to (one file per session and repository, owner-only on Unix, holding no token path, removed when the session ends). A call that names no checkout and carries a session, whether passed or filled in, then goes through that connection when it is saved for the same Hub and repository and still matches its checkout; otherwise the server uses its default connection. A call that names a checkout itself, with `root` or an absolute `worktree`, is always sent there and never follows the record. An absolute path in the main checkout follows the session's record, so one session stays one agent, and an absolute path inside such a worktree goes to the worktree's connection as well.

`root` is used before `worktree`, and `worktree` before paths. A call whose paths are in two repositories is refused with a message that says to call once per repository, and so is a call that mixes relative paths with another repository's absolute paths. A path or `root` in a checkout with no connection is refused with a message that names `agentscm client connect`. The server answers these refusals itself and sends nothing. Each repository keeps its own agent token; the server reads the token of the repository a call is for when the call is made. A reply from a different repository than the default's ends with a line that names the repository and its checkout and says to pass `root` on later calls about that Work.

When the checkout the server started in has no connection, or the server started outside any checkout, a call that names a connected checkout this way still goes through. Any other call says why the start checkout is not connected.

The SessionStart context names the other repositories connected on this computer (at most six, without connections whose managed token has expired), so an agent knows that a second repository is covered.

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

- **Every agent connected with `client setup`** runs the `agentscm` binary on that machine: its hooks call `agentscm hook <agent> --client-connection` and its MCP server is the stdio `agentscm client mcp` proxy, both by absolute path. Install it with `npx agentscm` or see [install](install.md).
- **Agents set up by `agentscm quickstart`** are the same: quickstart runs `client setup` for them.

If the binary or the hub is missing, the hooks do nothing visible (see below).

The stdio MCP proxy hosts a local hook relay that keeps its own HTTPS pool, without another
daemon. See [hook transport](hub.md#agent-sessions-mcp-and-hook-endpoints) for matching rules,
direct fallback, deadlines and fail-open limits.

### Optional Claude Code change capture

The experimental [change-capture adapter](change-capture.md) is separate from the coordination
hooks written by `init-repo` or quickstart. On Unix, after a checkout has a saved write-capable
`agentscm client connect` binding, an operator can run `agentscm change-capture configure` once to
add marked project-local Claude hooks while preserving unrelated entries. Each hook resolves the
session's current authorized Work and generation before it associates an event or action.

The default `metadata-only` mode sends bounded typed facts and artifact identities to the Hub.
Optional selected conversation/tool bodies, captured source bytes and command output remain in the
operator's private local spool. Ordinary shell success is an execution outcome, not a passing
verification. The local client reports pass/fail only for an exact configured verification recipe
match; the Hub stores that as caller asserted and does not validate it against an independent recipe
registry. Other agent hosts do not have equivalent coverage in this path. See the
[implementation status](implementation/agentscm-build-status.md) for qualification limits.

## What `agentscm hook` does

`agentscm hook <claude|codex|cursor|copilot|generic> [--hub-url URL]` reads the agent's hook JSON on standard input, sends it unchanged to `POST /v1/hub/hook/<agent>` with the token as a bearer header (for an edit, the JSON includes the edit's contents; the hub keeps only [the few fields it needs](hub.md#data-the-hub-stores)), and prints the hub's reply, which is already in the agent's own hook output format. A Codex payload is first reduced to the fields coordination needs.

The hooks written by `client setup` run `agentscm hook <agent> --client-connection` instead (for `claude`, `codex`, `cursor`, `copilot` or `gemini`). That mode takes the hub, repository and token only from the checkout's saved connection, makes edited paths repository-relative before sending, sends only the fields coordination needs, and refuses `--hub-url` and `--repo`. The token, hub URL, `--repo` and `X-Agentscm-Root` details below apply only without it.

- The token comes from `AGENTSCM_HUB_TOKEN`, else the file named by `AGENTSCM_HUB_TOKEN_FILE`, else `hub.json`. It is never a command-line argument.
- The hub URL comes from `AGENTSCM_HUB_URL`, else `--hub-url`, else `hub.json`. A token stored in `hub.json` is only ever sent to the hub that file names, so a `--hub-url` in a hook command cannot redirect it; to use a different hub, set `AGENTSCM_HUB_TOKEN`.
- `--repo OWNER/NAME` adds `?repo=` to the hook URL. If the token belongs to another repository the hub answers "coordination unknown (This token is for another repository; ...)" and records nothing, which catches one `AGENTSCM_HUB_TOKEN` reused across repositories.
- It sends the Git root of the agent's working directory as `X-Agentscm-Root` so the hub can make edited paths repository-relative.
- It gives up after 3 seconds.
- `AGENTSCM_HUB_TOKEN` is sent to whatever hub URL the command carries, because you set it for this purpose. AgentSCM no longer commits hook configuration; if you commit a hook command by hand, treat a change to its `--hub-url` like a change to your CI secrets: review it.
- It prints only an allowlist of the hub's reply: context and message fields, Cursor's `permission` (always `allow`) and one kind of permission decision, a `deny` with a non-empty reason, and that only for a pre-tool event. The Hub alone decides how often an edit is refused: a contested file once per hold (a repeat passes while the same hold keeps it, and a new holder is refused again), at most three advisory refusals per session in ten minutes, and `LEASE_LAPSED` and `EDIT_UNLINKED` on every retry until a start or checkpoint changes the session's Work. It drops everything else (`ask`, `allow`, `defer`, a deny without a reason, `continue`, `decision`, `stopReason`, updated or modified inputs and tool output), so a wrong reply cannot stop the agent or approve or rewrite its call. A Hub that sends a holding refusal can keep refusing that one edit, as it could already with `LEASE_LAPSED`; the hub is trusted (see [Limits](#limits)).

### Holds stay live while the agent works

A reservation on shared files is a lease. It blocks other Work only while the lead session is live: 20 minutes from that session's last renewal, or 2 hours from a `retain` (see the [Work lifecycle design](design/work-lifecycle.md#8-liveness-leases)). Only the lead session's own calls renew it, at most once a minute, so the hook client forwards at most one tool event a minute per session and answers the rest locally. A shell call is different: its start and end always reach the Hub, and while one runs the lease stays live for up to 60 minutes from the first of the shell calls still running, so a long test run or build keeps the hold. A shell call that never reports its end, because the agent crashed, holds for at most those 60 minutes. Once a lease lapses, the lead starts its Work again before it edits, even when nobody took the scope. The agent whose start takes a lapsed hold's scope is told so in the start's `reclaimed` list, because that hold's branch may have edits its lead never pushed. A lapsed lease, a stopped or idle session and a crash never mark Work finished: the Work stays unfinished, its outcome unstated, and only its hold goes.

| Agent | What renews the lead session's lease | Not covered |
| --- | --- | --- |
| Claude Code | every tool call (`PreToolUse` and `PostToolUse` for all tools), prompts, session start, edit checks; `Bash` calls while they run | a non-shell tool call longer than 20 minutes |
| Codex CLI | `PreToolUse` and `PostToolUse` for shell, `apply_patch`, MCP and local tools; shell calls while they run | hosted tools such as web search |
| Copilot | `preToolUse` and `postToolUse` for all tools; shell calls while they run | a non-shell tool call longer than 20 minutes |
| Cursor | `postToolUse` for all tools | time inside a tool call: Cursor reports no tool start the Hub can use |
| Gemini CLI | `BeforeTool` and `AfterTool` for all tools; `run_shell_command` while it runs | a non-shell tool call longer than 20 minutes |
| An agent with MCP only | named MCP session calls and checkpoints | everything between them |

The Work projection shows which case applies to a lead session as `activity.coverage`: `tool_calls` once a hook reported one of its tool calls, `tool_ends` for Cursor, whose hook reports only each call's end, `edits` for hooks that report only edits, `mcp_only`, or `unknown` when the Hub keeps no record of the session. Anything but `tool_calls` is listed as unknown coverage, with a reason that says what to run.

An agent that is not wired, or wired with an older `client setup` that hooked only edits, renews only on edits, MCP calls and checkpoints; run `agentscm client setup` again to install the tool hooks.

### Waiting for you in its session

When an agent stops to wait for its person, it makes no tool calls, which otherwise looks the same as a crash. Where the host reports the moment, the hook tells the Hub that the session is waiting and why: a **permission** prompt, a **question** (a choice, or a form an MCP server opened) or the **end of its turn**. The wait is a `session_waiting` action in that person's shared status (Needs you, the menu bar app's count and attention notice, `agentscm work status`), never anyone else's. With the wait, the hook sends a short excerpt of what it asks, so the person can see what their agent needs without opening the session. That is the end of the agent's last message when its turn ends, the questions it asks, or a permission prompt's own description. The excerpt is one line of at most 400 characters, from the end of the text, where a closing question sits. Recognizable secrets are redacted on the machine first; that redaction is best effort. The Hub keeps the excerpt in memory with the wait, quotes it in the action's text, and shows it only to that session's person. A stop that background work will resume sends none. Except for a Claude Code permission prompt ([below](#answer-a-permission-prompt-from-the-hub)), the hook never answers the prompt, the person's prompts, tool input and transcript are not sent, and the person answers in the session itself.

| Agent | Permission | Question | End of turn | What it cannot tell |
| --- | --- | --- | --- | --- |
| Claude Code | `Notification` `permission_prompt`, `PermissionRequest` | `AskUserQuestion` (`PreToolUse`), `Notification` `elicitation_dialog`, `elicitation_url_dialog`, `agent_needs_input` | `Stop` (not while background tasks run), `Notification` `idle_prompt` | whether a subagent asked, unless its hook names the subagent |
| Codex CLI | `PermissionRequest` | none | `Stop` | a question asked in plain text |
| Copilot CLI | `notification` `permission_prompt` | `notification` `elicitation_dialog` | `agentStop`, `notification` `agent_idle` | |
| Cursor | none | none | `stop` | permission waits: Cursor reports none |
| Gemini CLI | `Notification` `ToolPermission` | none | `AfterAgent` | |
| Others | `agentscm hook generic` with `{"event": "waiting", "reason": "permission"\|"question"\|"idle", "agent", "session"}` | | | |

While a session waits for its person, the Work it leads reads as waiting for that person in [shared status](standup.md#shared-actionable-status), not as stale. Everyone who can read the repository sees that it waits and on whom; only that person sees what it asks. A Claude Code session that stops while its own background tasks run waits for nobody, and the Hub notes that stop too, so its Work stays active for up to 60 minutes instead of going quiet. Neither keeps the session's hold on its files, which lapses 20 minutes after its last call as before. So that the task also says what it waits on, an agent records a pause or blocker checkpoint whose `next_action` names it before it stops.

The session's next tool call, edit check, prompt, start or end ends the wait. A tool call by a subagent ends only a wait that subagent asked, or one whose asker the host did not name, so a sibling working in parallel does not end it. A wait is shown only after 60 seconds, so someone at the keyboard never sees one. A permission prompt or question always needs you. The end of a turn needs you only while that session leads unfinished Work (active, blocked or paused), because an open chat whose work is done waits on nobody. Paused counts because a host that stops an idle session and later resumes it ends the session in between, which pauses its Work. The Hub keeps waits in memory, at most 256 per person and for at most 24 hours. When a person's waits are full, room is made from a stop on background work, then from the oldest end of a turn; a permission prompt or a question is never dropped for room. A Hub restart forgets them, and a session that waits through a restart reads as not known, not as not waiting. Run `agentscm client setup` again (or update the plugin) to install these hooks.

#### Answer a permission prompt from the Hub

For a Claude Code permission prompt, the plugin's `PermissionRequest` hook also offers the prompt to the Hub. The person who owns that session's agent token can then answer it on the Hub's web page, away from the computer the session runs on, with the choices the session gives them: allow it once, allow it with one of the session's own "do not ask again" choices, or deny it, with a reason if they type one. A question the agent asks (`AskUserQuestion`) is offered the same way and answered with its options or with typed text. The prompt stays usable in the session the whole time, and the first answer wins. There is nothing to turn on.

Beyond the wait itself, the hook sends the tool's name, what the request is for, and a salted hash of the tool input. What the request is for is one of these, at most 20,000 characters:

- a shell command, in full
- an edit's file path, never its content
- a fetch's address, without its query, fragment or sign-in part
- any other tool's whole input, as indented JSON: a subagent's start and an MCP server's tools included

So that the page can say what is asked and by which session, the hook also sends one line that says what the call does (for example "use start session (ccd_session): Fix the login") and the name of the checkout folder the session runs in. The line is a label beside the request, never a replacement for it.

Recognizable secrets are redacted on the machine first, with the same best-effort redaction as the excerpt above. The Hub keeps these in memory only, for at most 12 hours. It never stores, logs or notifies them, and it shows them only to the token's owner, signed in on the web page. The list of requests is where that person answers, so a request is on it as soon as the host confirms its prompt is on screen, a few seconds after it opens; the 60 seconds hold back only the notice in their status.

- **Only a prompt a person can see is offered.** A request is offered after the host's own `permission_prompt` notification confirms the prompt is on screen. A run with nobody at a dialog sends none, so it is never offered: its hook gives up after 20 seconds and the host decides as before.
- **The person may allow a request that is not shown in full.** The request is their own agent's, so the Hub accepts their allow whatever it shows, as the dialog in the session would. The list says exactly what is missing, in `missing`, so the page can say it beside Allow:
  - `cut_from`: how many characters the request had before it was cut to 20,000;
  - `redacted`: how many secrets were replaced on the machine;
  - `dropped`: how many characters that do not show the Hub removed (control, zero-width, direction or tag characters; a tag character can spell words the tool receives and nobody sees);
  - `unshown`: the parts of the request that are not shown, by name: at most 16 names of at most 64 characters, with secrets redacted, the last one "and N more" when there are more. For a shell command these are its settings other than the command, its description, its timeout and its background flag (for example `dangerouslyDisableSandbox`). For a file read, a search or a fetch they are its settings other than the path, pattern, query or address shown (for a fetch, its `prompt`), and the parts of the address that were left out. When nothing of a request can be shown, the one name is "the request's input".

  An allow of such a request approves what is not shown too. A person who wants to read all of it answers in the session.
- **The session's own choices.** Claude Code hands the hook the changes it suggests for this prompt, such as a rule so it does not ask again. The hook offers the kinds it can describe and says exactly what each one saves and where: `the rule Bash(npm test), in this checkout's local settings (.claude/settings.local.json)`, `access to /work/acme, for this session only`, `accept edits mode, for this session only`. The place is the one the host named: this session, the checkout's local settings, the project's shared settings or the person's user settings. These are the host's suggestions, not always the options the dialog shows: for `python3 -c "print(1)"` the dialog offered "don't ask again for: python3*" and the hook was handed a rule for that exact command. A suggestion of another kind (a rule that denies, another mode) is not offered.
- **A typed reason, and a typed answer.** With a deny the person may type up to 2,000 characters for the agent, which then reads "Denied from the AgentSCM Hub by its person, who says: …". A question is answered per question with one of its options (several where the question allows it) or with typed text of the same length, which the host takes like its own "Other" answer. A question cannot be allowed without answers.
- **What the Hub can and cannot send.** The hook prints a decision only for a reply that names its own request and input hash. A choice and an option arrive as positions: the rule that is saved is the host's own suggestion as the hook received it, and an answer's label is the host's own. The Hub cannot change the command, add input, name a rule the host did not suggest or pick an option that is not there. The one text of the Hub's that reaches the model is what the person typed, cleaned of control characters on the Hub and again in the hook. Without a typed reason the model reads only "Denied from the AgentSCM Hub by its person."
- **Failures leave the prompt to the session.** An older Hub, a Hub restart or an answer that does not fit the prompt produce no decision. A network error, a sleeping computer or a Hub that is being replaced does not end the offer: the hook keeps trying for its 12 hours, and a request whose hook was quiet for a minute is offered again when the hook is heard again.
- **The request ends when the session moves on**: the prompt is answered in the session, or the session makes its next tool call, takes a prompt, or ends.
- **Subagents are answered in the session.** A subagent's permission prompt is not offered: the host shows a subagent's dialog only after the hook returns, so the hook does not wait for it. Neither is a form an MCP server opens.
- **One Hub process.** A Hub that runs as several processes behind one address answers a request only on the process that holds it. On another process the request reads as ended.

This lets the Hub that a checkout is connected to approve a tool call at the moment the agent asks for it. To turn it off on a machine, set `AGENTSCM_REMOTE_ANSWERS=0` in the environment Claude Code runs in.

The hook uses two routes with its agent token, and the web page two with the person's web sign-in:

| Route | Body and reply |
| --- | --- |
| `POST /v1/hub/permission/register?repo=owner/name` | `{"session", "agent_id"?, "tool", "summary"?, "folder"?, "display", "missing"?, "content_hidden"?, "input_hash", "takes_reason"?, "choices"?, "questions"?}`, where `missing` is `{"cut_from"?, "redacted"?, "unshown"?}` with at most 16 names. An older hook sends `complete` in its place; unless that is true, the list names one unshown part that says the hook did not state what is missing. `takes_reason` says the hook passes on a typed reason. `choices` is at most 8 of `{"label", "saves"}`, and `questions` at most 4 of `{"question", "header"?, "options": [{"label", "description"?}], "multiple"?}` with 1 to 8 options; each text is kept as one line of at most 500 characters. A registration with `questions` needs the session's question wait, any other its permission wait. Replies `{"request_id", "input_hash", "state"}` with `state` `unconfirmed` or `pending`. 409 `request_conflict` with `reason` `not_waiting` when that session has no such wait. 429 `rate_limited` past 1,024 requests for the Hub; a session has one open request, and no request is ended to make room. |
| `POST /v1/hub/permission/poll?repo=owner/name` | `{"request_id", "input_hash", "wait_seconds"}`, held for up to `wait_seconds` (0 to 25) until the state changes. Replies `{"request_id", "input_hash", "state", "decision"}`: `state` is `unconfirmed`, `pending`, `answered` or `ended`, and `decision` is `allow`, `deny` or null. An answered reply also has what the person sent with the decision, when they sent something: `reason` (text) with a deny; with an allow `choice` (a position in the registration's `choices`) or `answers` (one per question, each `{"options": [positions]}` or `{"text"}`). A poll is the hook being heard: it also wakes a request that was asleep because its hook had been quiet. An unknown id, another token's request and a wrong hash are all 404. |
| `GET /v1/hub/session/permissions?repository=owner/name` | `{"requests": [{"item_id", "request_id", "tool", "summary", "folder", "display", "allowable", "missing", "complete", "content_hidden", "subagent", "deny_reason", "choices", "questions", "since", "state", "decision"}]}`: the person's own confirmed requests there, oldest first. `item_id` is the id of the wait's `session_waiting` action (`waiting:permission:…`, or `waiting:question:…` for a question), which lists the Work that session leads. `deny_reason` says a reason typed with a deny reaches the agent. `choices` is `[{"id", "label", "saves"}]` and `questions` as registered; both are empty when there are none. `allowable` is true: this Hub accepts an allow for every request it lists (a Hub without the field refuses one unless `complete`). `missing` is `{"cut_from", "redacted", "dropped", "unshown"}` as described above; `cut_from` is null when nothing was cut. `complete` is true when nothing is missing and the `display` is at most 4,000 characters, for a page that shows no more than that and offers Allow only then. `summary` and `folder` are null when the hook sent none. |
| `POST /v1/hub/session/permissions/answer` | `{"repository", "request_id", "decision", "reason"?, "choice"?, "answers"?}` with `allow` or `deny`, and the page's CSRF token. `reason` goes with a deny (1 to 2,000 characters, on a request with `deny_reason`); `choice` is the `id` of one of the request's choices and goes with an allow; `answers` goes with the allow of a question, one per question in order, each `{"options": [positions]}` (one position unless the question is `multiple`, none twice) or `{"text"}` (1 to 2,000 characters). Replies `{"state": "answered", "decision"}`; the same decision again is accepted. 400 `invalid_request` when what was sent does not fit the decision or the request (a question allowed without answers, a choice that is not offered), 403 `not_your_agent`, 409 `request_conflict` with `reason` `already_answered`, `answered_in_session` or `ended`. |

### Hooks fail open

The command always exits 0. If anything goes wrong (no hub configured, no token, the hub is unreachable or slow, it rejects the token, or it sends something unusable) it prints the agent's valid "no information" reply, which for most events is `{}` and for a few events is a one-line `AgentSCM: coordination unknown (<reason>; <recovery>; ref cor_...)` note (no `ref` when the hub was unreachable), given once per agent session and failure code (a marker file under `~/.config/agentscm/hook-notes/`, removed after 2 days); a session start, resume or replay always repeats it, and other later failures of the same kind print the no-op reply, which the note says. With `--hub-url`, the hook sends the agent's own paths and the checkout root beside them; an edit it cannot file under its real place in that checkout (a path through an in-repository symlink, a worktree nested in the checkout, a `cwd` outside it) gets `coordination unknown (edit path not matched to the checkout)` on every such edit, never the empty reply. That includes an edit in another checkout the agent works in that is not this repository's. One such path in a multi-file patch makes the whole patch unknown. The fields are described under [Hub errors](api.md#hub-errors). A broken setup never stops an edit. The same holds on the hub side: its hook endpoint answers HTTP 200 in the agent's format even for a bad token.

The cost is that failures are quiet. If warnings never appear, run `agentscm intent start` in the repository, or check the token, the URL and that the hub is up.

## Work in your person's other repositories

An agent token belongs to one repository, and every other coordination read is about that repository. `agentscm_work_repositories` (listed by default; agents are told to call it before planning work that may touch what their person's agents are doing in other repositories) and `GET /v1/hub/standup/repositories` read the current unfinished Work in every connected repository on the person's forge host that the token's person can read now. The Hub checks each repository with the forge as the All repositories page does, but asks again on every read instead of using that page's 90-second role cache, because this read returns Work details. A token that asks again within 30 seconds gets its previous answer back (same `checked_at`), so a polling agent cannot spend the forge's rate limit. That reuse means a lost access can be served stale to the same token for up to 30 seconds; any other token sees it at once. Each repository lists at most 10 Work items in brief (title, owner, state, claim status, lead agent and session, next action, the first three scope paths with `scope_count`, and `blocked` and `waiting` flags), and at most 100 in all, with `work_complete` per repository. Another owner's agent labels and session IDs are masked as in every Work read. Repositories the forge could not answer for are counted in `unchecked` and not named, because a name could reveal that a private repository exists. `complete` is true only when every connected repository was checked: none left unchecked and no more than the 200 the Hub checks. The top-level `work_complete` is false when the 100-item cap cut a repository's list. It is a read: it claims nothing and detects no overlap across repositories.

## Limits

- A warning appears only for edits the agent makes through its file-edit tools. Files written by shell commands are not announced.
- Claude Code, Codex and Copilot answer before the edit, in the same hook run, and can refuse it once. Gemini CLI's `BeforeTool` hook refuses the same way (`decision: deny`), and any other note before an edit goes to its `systemMessage`. Claude Code shows a hook's added context to the model only after the tool ran, so a note alone arrives too late; that is why a contested edit is refused instead. The Copilot CLI's `preToolUse` has no field for a note, so it gets the refusal but no other notes. Cursor's `preToolUse` hook has no documented way to carry text to the model without denying the call, so the Cursor hook is wired as `postToolUse` instead: the file is announced and the warning arrives just after the first edit of it. Treat Cursor as warn-just-after.
- Claude Code's HTTP hook cannot tell the hub where the repository root is (the project directory is not available to HTTP-hook headers), so the hub makes paths relative to the directory the agent was started in. Start Claude Code at the repository root; from a subdirectory its edits are announced under shorter paths and may not match other agents' paths. The command hooks (`agentscm hook`) send the Git root and do not have this limit.
- Codex hooks run only when the project is trusted, and Codex asks the developer to review new or changed hooks in `/hooks` once; changed hooks ask again.
- Claude Code asks each developer to approve the project's MCP server and trust the folder before project hooks run.
- Where the edited file path sits in Cursor, Copilot and Codex payloads is only partly documented; the hub reads the fields that vendor documents and ignores the rest.
- The hub is trusted. Its replies go into each agent's context, and the URL that decides which hub that is sits in hook and MCP config: a hook's `--hub-url` and the MCP server URL. Whoever controls that URL controls the text the agents using the config see, and receives their token and hook payloads. Review changes to those files, and treat the hub as trusted.
- Coverage depends on every agent on the team being wired. An unwired agent is invisible. A pull-request check as a backstop is planned, not built.
- The MCP server finds the hook's session through the process tree, on macOS and Linux only. On Windows, in a host that keeps several conversations in one process, or when the start hook did not run, a hooked agent's call without `session` is left for the Hub to place, as before. An MCP client started from another agent's shell, with no record of its own, gets that outer agent's session. The generic hook (`agentscm hook generic`) records no session.
- A call reaches another repository only through `root`, an absolute `worktree` or absolute paths. A call without them goes to the checkout the session started in, so later calls about Work declared elsewhere must pass `root`.
- Stand-up claims persist across agent sessions, but only reported checkpoints are visible. A stale or unknown claim is a freshness label, not a release. A reservation blocks only while its lease is live; once it lapses it blocks nobody, and the next admission over its scope clears it and fences the old lead. Inspect the work and use the explicit takeover or reopen flow instead of assuming it is available.

## Recipes for other agents

These are not generated and not tested here. The shape is always the same:

1. Add `https://<hub>/mcp` as a remote (streamable HTTP) MCP server with the header `Authorization: Bearer <agent token>`. Where the agent's config supports it, read the token from an environment variable instead of writing it into a checked-in file.
2. Make sure the agent reads `AGENTS.md` (or paste the snippet from it into the agent's own instructions file).
3. If the agent can run a command before edits and wants a JSON answer, `agentscm hook generic` accepts `{"agent", "session", "event", "tool", "paths", "task"}` on standard input. `{"event": "waiting", "reason": "permission"|"question"|"idle"}` reports that the session waits for its person (see [Waiting for you in its session](#waiting-for-you-in-its-session)).

### Other agents

These agents have no native AgentSCM setup, so `client setup` does not configure them and their
start-up check does not auto-connect a checkout. What each documents, as of October 2026:

| Agent | User-level MCP | Start-up hook | Hook input |
| --- | --- | --- | --- |
| Grok Build (`grok`, xAI) | `~/.grok/config.toml` `[mcp_servers.NAME]` | `SessionStart` in `~/.grok/hooks/*.json` | `hookEventName`, `sessionId`, `cwd`, `toolName`, `toolInput` |
| Kiro (IDE and `kiro-cli` v3) | `~/.kiro/settings/mcp.json` | `SessionStart` in `~/.kiro/hooks/*.json` | `hook_event_name`, `session_id`, `cwd`, `tool_name`, `tool_input` |
| Cline | `~/.cline/data/settings/cline_mcp_settings.json` (CLI) | `TaskStart` in `~/.cline/hooks/` | `hookName`, `taskId`, `workspaceRoots`; no `cwd` |
| Amp | `~/.config/amp/settings.json` `amp.mcpServers` | `session.start` in a TypeScript plugin, `~/.config/amp/plugins/` | plugin API: `thread.id`, `system.workspaceRoot` |
| opencode | `~/.config/opencode/opencode.json` `mcp` | `session.created` in a plugin, `~/.config/opencode/plugins/` | plugin API: `directory`, `worktree` |
| Windsurf (now Devin Desktop, Cascade) | `~/.config/devin/mcp_config.json` | none; `pre_user_prompt` is the first call | `trajectory_id`, `tool_info` |
| Aider | none | none | none |

None of these sends `agentscm hook generic` input directly. To wire one by hand:

1. Add the user-level MCP server with command `agentscm` and arguments
   `--no-telemetry client mcp`. It uses the saved connection of the checkout it starts in, which
   `agentscm client connect --hub HUB` makes, and of any other connected checkout that a call
   names ([routing](#calls-reach-the-repository-whose-files-they-name)). In a checkout without a
   connection it lists the tools, and a call for that checkout says why it cannot run. A client
   with no AgentSCM hook gets one session name per server process.
2. Add a hook, or plugin, that maps the agent's event to
   `{"event": "start"|"check"|"waiting"|"done", "agent": "NAME", "session": ID, "paths": [...], "cwd": DIR}` (a `waiting` event also carries `"reason"`),
   pipes it to `agentscm hook generic` with `AGENTSCM_HUB_URL` and `AGENTSCM_HUB_TOKEN` set, and
   maps the `{"status", "text"}` answer back to the agent's own output (for example Kiro adds a
   hook's standard output to the context, and Grok Build reads `hookSpecificOutput.additionalContext`).
   The generic hook does not use a checkout's saved connection.

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

These tools accept structured caller reports. The optional Claude change-capture history reader
can submit observed request usage automatically when an explicit local history root is configured.
Use `unknown` for an agent, provider or model label the client cannot determine, and omit token or
cost fields that are unavailable. Do not
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
| Claude Code CLI | Not verified: no client session was run. The hub tools are covered in the last row | Fixture-tested: pre-edit hook, refuses once per file and session | Live-qualified (print mode, CLI 2.1.304): local `claude-stop`. Fixture-tested: `remote-stop`, `task-stop` | Live-qualified (print mode, CLI 2.1.304): `claude-stop` blocked with the failing test's output, passed after the fix and stopped after 8 blocks. Fixture-tested: `remote-stop`, `task-stop`; at most 8 consecutive continuations; per vendor documentation checked 2026-10-20 the Claude cap resets on tool calls |
| Claude Code desktop/IDE | Not verified: MCP config is the CLI's | Not verified: hook support in these surfaces is not checked | Not verified | Not verified |
| Codex CLI | Not verified: no client session was run | Fixture-tested: pre-edit hook, refuses once per file and session | Live-qualified (`codex exec` with `--dangerously-bypass-hook-trust`, CLI 0.163.0): local `codex-stop`. Fixture-tested: `remote-codex-stop`, `task-codex-stop` | Live-qualified (same mode): one block with feedback, then a pass after the fix. The 8-block bound was not live-tested; it is fixture-tested. Per vendor documentation checked 2026-10-20 Codex documents no cap of its own |
| Codex IDE/desktop | Not verified | Not verified: hook support is not checked | Not verified | Not verified |
| Cursor | Not verified: no client session was run | Fixture-tested: post-edit note only; the hook cannot refuse | Not verified: no Stop adapter | Unsupported: no adapter, the edit hook cannot refuse, and per vendor documentation its `stop` hook can only return `followup_message` to continue (default `loop_limit` 5), not block |
| Copilot | Not verified: no client session was run | Fixture-tested: pre-edit hook, refuses once; the CLI gets the refusal but no notes | Not verified: no Stop adapter | Documented, not verified: Copilot CLI and cloud agent document `agentStop` with `decision: block` and `stop_hook_active`, capped at 8 consecutive blocks in the CLI. VS Code agent hooks (preview) document a `Stop` event; not verified. AgentSCM installs no adapter for either |
| Any MCP client | Fixture-tested: the hub's `POST /mcp` coordination tools ([hub](hub.md#agent-sessions-mcp-and-hook-endpoints)), the workspace [agent-task MCP](advanced/agent-workflow-tasks.md) and the local [reusable-workflow stdio server](advanced/reusable-workflows.md#local-mcp-front-door) are exercised with synthetic protocol clients, not live vendor sessions. Hub coordination tools are advice only. The local server's configure and check tools require explicit startup grants; its paths and backends are fixed by the operator. A model can still skip calls | Unsupported: MCP alone has no hook, so no edit is intercepted | Unsupported: MCP has no completion event | Unsupported |

Live qualification, 2026-10-20: one macOS machine, a synthetic fixture, a local native store, a Docker command check and a Claude review step. Claude Code print mode used `claude -p` with `--settings` pointing at a file written by `install-stop`. Codex used `codex exec` with hooks in an isolated `CODEX_HOME` `hooks.json` written by `install-stop`; hooks fired only with `--dangerously-bypass-hook-trust`, and a native workspace also needs `--skip-git-repo-check`. In both, an independent `skill-workflow check` confirmed a receipt for the current revision after the fix. Not live-tested: interactive Claude Code, Claude desktop/IDE, Codex interactive (`/hooks` trust), Codex IDE/desktop, the hosted `remote-*` stops and the agent-owned `task-stop` and `task-codex-stop`. The hosted and agent-owned stops are fixture-tested against a synthetic local HTTP hub harness, including token revocation, an unreachable endpoint, uncheckpointed edits and a stale subject.

Sources: the edit hooks and wiring are the "Implemented (alpha)" fixture-tested row in the [roadmap](roadmap.md) and are described in [warn once](hub.md#warn-once) and [work intents](intents.md); the coordination and workspace MCP endpoints are described in [hub](hub.md#agent-sessions-mcp-and-hook-endpoints) and [agent-owned workflow tasks](advanced/agent-workflow-tasks.md); the Stop adapters are described in the [Stop hook guide](advanced/claude-code-workflows.md#opt-in-claude-code-stop-hook). Stop hooks are installed by `skill-workflow install-stop`, not by `init-repo`.

A Stop trigger only prompts the agent to run the check. Omitting, misconfiguring or bypassing a hook does not bypass native landing policy, which requires an exact-current receipt independently.

## Verified against

The file formats were checked against the vendors' public documentation in October 2026: [Claude Code hooks](https://code.claude.com/docs/en/hooks) and [MCP](https://code.claude.com/docs/en/mcp), the Codex documentation for hooks and MCP servers, [Cursor hooks](https://cursor.com/docs/hooks) and [MCP](https://cursor.com/docs/mcp), and the GitHub Copilot and VS Code documentation for hooks and `mcp.json`. Matrix version 2 adds live CLI runs of Claude Code 2.1.304 and Codex CLI 0.163.0 (2026-10-20) and the vendor pages checked the same day: [Cursor agent hooks](https://cursor.com/docs/agent/hooks), [Copilot hooks configuration](https://docs.github.com/en/copilot/reference/hooks-configuration) and [VS Code agent hooks](https://code.visualstudio.com/docs/copilot/customization/hooks). Vendors change these formats; if a generated file stops working, please open an issue.
