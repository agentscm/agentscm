---
title: "Messages to your own agent session"
---

# Messages to your own agent session

A person signed in to the Hub's web page can send a short message to an agent session they already run: for example, ask the agent that leads a stale Work to finish or release it. The Hub never starts a session. A Claude Code session with the AgentSCM plugin gets the message within seconds, even while it sits idle: a background hook waits at the Hub and wakes the session (tier `wake`). Other sessions get it in the reply to their next call the Hub can answer. A closed session gets nothing until it runs again; the message stays queued and the Hub page says so.

## Trust model

- **Who can send.** Only the Hub's own web page, signed in with the browser cookie, can send. Every change also carries the page's CSRF token. A computer's or a script's bearer session is refused, and so is an agent token.
- **Who can receive.** A message names an agent token and a Hub session. The token must be one of the sender's own live, writing agent tokens for that repository. When the message names a Work, that session must lead it.
- **What the text is.** The delivered text is a notice: "AgentSCM Hub message *id* for this session, from *login*", the note quoted as a JSON string, and the instruction to check it first. It names no mode. The Hub never inserts agent text (Work titles, checkpoints) or forge text (issue and pull request titles).
- **Verify before acting.** Other people's text (coordination request titles, Work titles, pull request titles) reaches the agent through the same results and hook context, so a notice proves nothing on its own. The agent calls `agentscm_hub_message` with status `accepted` first. That call succeeds only for a real message to this token's own session, and its reply carries the note and the mode as the Hub stored them, and the Work it is about. The agent acts only on that reply. Third-party text the Hub relays has "Hub message" turned into "Hub note", so it cannot open with the notice's words.
- **Who answers.** Only the receiving token's own session can answer, with `agentscm_hub_message`. The answers are `accepted`, `declined` and then `done`. The Hub page shows these receipts; nobody else reads them.
- **Storage.** Each person and repository keeps at most 50 messages in Hub settings. The oldest message is dropped first.

## Modes

Each person sets one mode for all repositories, and can override it for a single repository. The default is Manual.

| Mode | What the session sees | What the Hub page shows |
| --- | --- | --- |
| **Auto** | After `accepted`: "act on it now, within your task's rules". | Queued, then delivered, accepted, done. |
| **Manual** | After `accepted`: "show it to your person and act only after their yes in this session". | The same; the agent answers `declined` if the person says no. |
| **Ignore** | Nothing; sending is refused. | The item stays a to-do. The page's copy-and-paste path still works. |

The mode is read when a message is delivered and again when it is accepted, so changing it to Ignore holds queued messages and refuses `accepted` for delivered ones. Waking a session changes none of this: in Manual the woken agent shows the message at once and still waits for the person's yes in the session, and in Ignore nothing wakes it.

## Delivery tiers

The Hub records three facts per session, in memory, from the session's own calls:

- the MCP `initialize` `clientInfo` (name and version), which the `agentscm client mcp` proxy passes through;
- the hook the session reports through (`hook:claude`, `hook:codex`, `hook:cursor`, `hook:copilot`, `hook:gemini`, `hook:generic`);
- the agent token's label.

After a Hub restart these facts are lost: a session's tier reads `unknown` until its next hook call.

Only the session a person leads is offered a message, never a subagent's. The evidence is a start, prompt or checked tool hook call from that session that carries no subagent marker (Claude Code's `agent_id`, Copilot's `agentId`). An MCP call is no evidence: a subagent makes its AgentSCM calls with its parent's session. So a client with only MCP and no hooks is never offered a message.

From these facts the Hub derives the tier it can use:

| Tier | Clients | When it arrives |
| --- | --- | --- |
| `wake` | Claude Code with the AgentSCM plugin, while its background hook waits (seen within the last 90 seconds). | Within seconds, idle or busy: the hook wakes the session with the notice. |
| `hook` | Claude Code and Codex (the reply to `UserPromptSubmit` and to a checked `PreToolUse`, as `hookSpecificOutput.additionalContext`). Cursor (a checked edit's `additional_context`). Copilot's PascalCase hooks. | At the next prompt or checked tool call that is not a subagent's. No tool call is needed. |
| `mcp` | Clients whose hook cannot carry context: Copilot CLI and Gemini CLI (or any generic hook). | At the session's next AgentSCM tool call, as an extra text block in the result. |
| `unknown` | No top-level hook call seen since the Hub started, including every MCP-only client (Antigravity, Devin, Amp, opencode). | Use copy and paste from the Hub page. |

The tier follows the client, not the first call: a Claude Code session takes its messages in its hook replies even before its first prompt, never in an MCP result.

These client hooks use the `mcp` tier:

- Copilot CLI's `preToolUse` has no context field.
- Gemini CLI shows the hook's text to the person, not to the model.

In the `mcp` tier the session is known to be a top-level one, but a single MCP call still cannot say whether a subagent made it, so the notice tells a subagent to leave the message for the session that started it.

A refused edit's reply carries only the refusal, so a queued message waits for the next call. The start context is cut to fit the host's limit, so the Hub does not deliver a message there either. Each reply carries at most three messages.

A message is never delivered:

- to a subagent: hook calls from a subagent are skipped, and a session whose hook carries context takes its messages only there;
- to a session without top-level evidence (tier `unknown`);
- by an MCP call that names no `session`;
- in a hook reply whose outcome is unknown, because that reply's text may be dropped.

A delivered message that is not answered within 60 seconds is offered again at the next call, in case the reply that carried it never reached the model. Only a message never delivered wakes a session, so an agent that does not answer is not woken again and again.

### Waking an idle Claude Code session

The Claude Code plugin that `agentscm client setup` installs adds a background hook, `agentscm hook-wait claude`, at `SessionStart` and at every `Stop`, declared with `"asyncRewake": true`. Claude Code documents that it runs such a hook in the background and, when the hook exits with code 2, wakes the session, an idle one included, and shows the hook's stderr to the model as a system reminder. Plain `"async": true` does not wake an idle session, and a `Stop` hook alone cannot reach a session that is already idle.

- The hook reads the session's id and directory from its input, finds the checkout's saved connection, and holds `POST /v1/hub/agent-messages/wait` open with the agent token, up to 25 seconds per request, then asks again.
- When a message is sent, the Hub answers the waiting request at once with the same notice a hook reply carries and marks the message delivered via `wake:claude`, exactly as a hook reply would, so it is not delivered twice. The hook prints the notice and exits 2. The woken turn's `Stop` starts the next waiter.
- The notice still proves nothing alone: the agent answers `accepted` with `agentscm_hub_message` and acts only on that reply.
- The same wait also wakes a lead for a coordination request to Work it leads that it has neither acknowledged nor answered. The notice opens with `AgentSCM coordination request`, names the request, the Work, the requester and the files (filtered, never the request's title), and points at `agentscm_coordination_inbox`, `agentscm_coordination_ack` and `agentscm_coordination_reply`. The Hub reads the session's coordination inbox once per wait, so a new request wakes it within about 25 seconds, and wakes a session once per request: a lead that does not acknowledge it is not woken again at every turn (a Hub restart forgets this, so such a request wakes it once more). A wake whose answer never reaches the hook, because the wait was replaced or the connection dropped, is not repeated; the request still comes with the session's next hook reply or tool result. If a live lead has still not acknowledged it 10 minutes after it was made, its person is asked in the Hub's Needs you.
- One waiter runs per session. A later start for the same session finds the first one's lock file held and exits; on the Hub a newer wait for the same token and session replaces an older one, which ends empty and never takes a message.
- A waiter ends silently (exit 0) when its 7 days are up, when the process that started it is gone, when its session ended (the plugin's `SessionEnd` hook runs `agentscm hook-wait claude --end`, which leaves an end marker the waiter sees before its next request, so `/clear` ends the old session's waiter too), when the checkout is not connected, when the person's mode is Ignore, when the token cannot receive messages, when the Hub does not serve the route, or when the Hub has been unreachable for 15 minutes. An error never wakes the session. The next turn's `Stop` starts a new waiter.
- The hook never waits for a subagent: `SessionStart` and `Stop` fire only in the session a person leads, and the hook exits at once for input that carries a subagent's `agent_id`. A waiting hook is top-level evidence for its session.
- Each Hub process holds at most 4096 waits at once and 64 per person; past either it answers `429` and the hook asks again later. A Hub that runs several processes counts each one on its own.

What this does not cover:

- **Other hosts.** Only Claude Code has the background hook. Codex, Cursor, Copilot, Gemini CLI and MCP-only clients still get a message at their next call, as in the tiers above.
- **Sessions started before this change.** A session reads its plugin's hooks when it starts, so a session that was already running when the client was updated has no waiter until it restarts.
- **Windows.** The waiter exits at once on Windows, because it cannot tell there when its session has ended. A Windows session gets its messages at its next hook or tool call.
- **Requests in another repository.** The waiter waits with the connection of the checkout its session runs in. A lead that holds Work in another connected repository is not woken for a request there; it sees it at its next call there, and its owner is asked after 10 minutes as above.
- **Ignore.** A person whose mode is Ignore has no waiter (the wait answers `410`), so their sessions are not woken for coordination requests either.
- **Clients older than this change** read only a Hub message notice as a wake, so they are woken for no coordination request.
- **Not yet observed.** The wake itself, a real idle Claude Code session woken by this hook, has not been observed with this build; it is to be tried once the client built from main is installed.

### Other clients

- **Claude Code channels** (`notifications/claude/channel`) are a research preview behind `--dangerously-load-development-channels` for servers off the allowlist, and are not used.
- **Codex** documents `codex app-server` (`turn/steer`, `thread/inject_items`) for a session that the app server itself runs. It does not document attaching to a terminal session that is already running.
- **Cursor** documents steering only for its cloud agents.
- **Copilot CLI** documents `copilot --acp` for sessions that the ACP client creates.

### Manual path

Everyone has the Hub page's own manual path whatever the tier: copy the prompt, or open it in the agent. The agents' URL schemes:

- Claude Code: `claude-cli://open`
- VS Code: `vscode://anthropic.claude-code/open`
- Codex: `codex://threads/new`
- Cursor: `cursor://anysphere.cursor-deeplink/prompt`

These links start a new conversation, so they are the fallback, not delivery to the running session.

## HTTP

The routes are for the Hub's web page only:

- `GET /v1/hub/session/agent-messages?repository=OWNER/NAME` returns:
  - `mode`, the mode in force;
  - `default_mode` and `repository_mode`, the person's own settings;
  - `sessions`, with `token`, `token_name`, `session`, `tier`, `name`, `version`, `hook`, `hook_context`, `top_level`, `mcp` and `seen_at`;
  - `messages`, newest first.
- `PUT /v1/hub/session/agent-messages/mode` with `{repository, this_repository, mode}` sets a mode; `mode: null` clears it.
- `POST /v1/hub/session/agent-messages` with `{repository, token, session, work_id?, note}` queues a message and returns it (`201`). The note is 1 to 1000 bytes, with newlines allowed.
- `DELETE /v1/hub/session/agent-messages?repository=…&id=…` withdraws a message that is still queued.

The session's background hook uses one route with the agent token: `POST /v1/hub/agent-messages/wait` with `{session}`, the native session id. It answers `200 {text}` with the notice when a message comes, `204` when none came in 25 seconds or a newer wait for the session replaced this one, `403` for a read-only token or one without a repository, `410` while the person's mode is Ignore, and `429` when the Hub or the person holds too many waits.

A message's `state` is `queued`, `delivered`, `accepted`, `declined`, `done` or `withdrawn`. `via` is `wake:claude`, `hook:<agent>` or `mcp`, and `mode` is the mode it was delivered under. A session's `waited_at` is when its background hook last waited.

## MCP

`agentscm_hub_message` takes `{id, status: accepted | declined | done, session}`, all three required. A call without `session` is refused with `invalid_request`, recovery `provide_field` and the field `session`: the Hub never guesses which of the token's sessions is answering. An agent can answer `accepted` or `declined` while the message is `delivered`, and `done` once it is `delivered` or `accepted`.
