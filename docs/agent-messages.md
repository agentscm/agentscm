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
- **Who answers.** Only the receiving token's own session can answer, with `agentscm_hub_message`. The answers are `accepted` (the check before acting), `declined` (before or after `accepted`: in Manual, the person's no comes after the agent verified the message and showed it), and `done`. The Hub page shows these receipts; nobody else reads them.
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

After a Hub restart these facts are lost: a session's tier reads `unknown` until its next hook call or AgentSCM tool call. Until a hooked session's next hook call, its next AgentSCM tool call can carry a message instead.

A session is offered a message once it has shown itself in one of two ways. One is a start, prompt or checked tool hook call that carries no subagent marker (Claude Code's `agent_id`, Copilot's `agentId`). The other is an AgentSCM MCP tool call that names the session's `session`. A session whose hook carries context takes its messages only in its hook replies, where a subagent's call is told apart, never in an MCP result. An MCP call cannot say whether a subagent made it, because a subagent makes its AgentSCM calls with its parent's session. So a message delivered in an MCP result tells a subagent to leave it for the session that started it, and one left unanswered is offered again at a later call.

From these facts the Hub derives the tier it can use:

| Tier | Clients | When it arrives |
| --- | --- | --- |
| `wake` | Claude Code with the AgentSCM plugin, while its background hook waits (seen within the last 90 seconds). | Within seconds, idle or busy: the hook wakes the session with the notice. |
| `hook` | Claude Code and Codex (the reply to `UserPromptSubmit` and to a checked `PreToolUse`, as `hookSpecificOutput.additionalContext`). Cursor (a checked edit's `additional_context`). Copilot's PascalCase hooks. | At the next prompt or checked tool call that is not a subagent's. No tool call is needed. |
| `mcp` | Clients whose hook cannot carry context: Copilot CLI and Gemini CLI (or any generic hook). Every client with MCP and no hooks: Codex without its hooks, Antigravity, Devin, Amp, opencode. | At the session's next AgentSCM tool call, as an extra text block in the result. Never sooner: no host but Claude Code puts a server's push in front of the model (see [Other clients](#other-clients)). |
| `unknown` | Nothing from the session itself since the Hub started, only its subagents' hook calls. | Nothing is offered until the session calls; use copy and paste from the Hub page meanwhile. |

The tier follows the client, not the first call: a Claude Code session takes its messages in its hook replies even before its first prompt, never in an MCP result.

These client hooks use the `mcp` tier:

- Copilot CLI's `preToolUse` has no context field.
- Gemini CLI shows the hook's text to the person, not to the model.

In the `mcp` tier a single MCP call cannot say whether a subagent made it, so the notice tells a subagent to leave the message for the session that started it.

A refused edit's reply carries only the refusal, so a queued message waits for the next call. The start context is cut to fit the host's limit, so the Hub does not deliver a message there either. Each reply carries at most three messages.

A message is never delivered:

- to a subagent's hook call: those are skipped, and a session whose hook carries context takes its messages only in its own hook replies (a subagent of a client without such a hook can receive one in an MCP result; its notice tells it to leave it);
- to a session the Hub has heard nothing from itself since it started (tier `unknown`);
- by an MCP call that names no `session`;
- in a hook reply whose outcome is unknown, because that reply's text may be dropped.

A delivered message that is not answered within 60 seconds is offered again at the next call, in case the reply that carried it never reached the model. Only a message never delivered wakes a session, so an agent that does not answer is not woken again and again.

### Waking an idle Claude Code session

The Claude Code plugin that `agentscm client setup` installs adds a background hook, `agentscm hook-wait claude`, at `SessionStart` and at every `Stop`, declared with `"asyncRewake": true`. Claude Code documents that it runs such a hook in the background and, when the hook exits with code 2, wakes the session, an idle one included, and shows the hook's stderr to the model as a system reminder. Plain `"async": true` does not wake an idle session, and a `Stop` hook alone cannot reach a session that is already idle.

- The hook reads the session's id and directory from its input, finds the checkout's saved connection, and holds `POST /v1/hub/agent-messages/wait` open with the agent token, up to 25 seconds per request, then asks again.
- When a message is sent, the Hub answers the waiting request at once with the same notice a hook reply carries and marks the message delivered via `wake:claude`, exactly as a hook reply would, so it is not delivered twice. The hook prints the notice and exits 2. The woken turn's `Stop` starts the next waiter.
- The notice still proves nothing alone: the agent answers `accepted` with `agentscm_hub_message` and acts only on that reply.
- The same wait also wakes a lead for a coordination request to Work it leads that it has neither acknowledged nor answered. The notice opens with `AgentSCM coordination request`, names the request, the Work, the requester and the files (filtered, never the request's title), and points at `agentscm_coordination_inbox`, `agentscm_coordination_ack` and `agentscm_coordination_reply`. The Hub reads the session's coordination inbox once per wait, so a new request wakes it within about 25 seconds, and wakes a session once per request: a lead that does not acknowledge it is not woken again at every turn (a Hub restart forgets this, so such a request wakes it once more). A wake whose answer never reaches the hook, because the wait was replaced or the connection dropped, is not repeated; the request still comes with the session's next hook reply or tool result. If a live lead has still not acknowledged it 10 minutes after it was made, its person is asked in the Hub's Needs you.
- A session can lead Work in another repository that its computer has connected: its calls about that repository go through that repository's own agent token, while its waiter waits with the token of the checkout the session runs in. So once per wait the Hub also reads the person's other live agent tokens on this Hub for Work that the same session leads with them, and wakes the session for a request there in the same way. That notice also names the repository and says to pass `root`, set to the session's checkout of it, on the three calls. The Hub reads with the person's own other token, checked as on that token's own call, and the Work's lead record must name that token and this session; the notice carries the request's id, the Work, the requester, the files and the repository's name, and no title. A session without a waiter gets the same notice where it gets a person's messages, in a prompt or checked tool call's reply or an AgentSCM tool result, read at most once every 25 seconds. A notice for a request in another repository that never reaches the model is not repeated in this one: the session sees the request at its next call in that repository, and its person is asked after 10 minutes.
- One waiter runs per session. A later start for the same session finds the first one's lock file held and exits; on the Hub a newer wait for the same token and session replaces an older one, which ends empty and never takes a message.
- A waiter ends silently (exit 0) when its 7 days are up, when the process that started it is gone, when its session ended (the plugin's `SessionEnd` hook runs `agentscm hook-wait claude --end`, which leaves an end marker the waiter sees before its next request, so `/clear` ends the old session's waiter too), when the checkout is not connected, when the person's mode is Ignore, when the token cannot receive messages, when the Hub does not serve the route, or when the Hub has been unreachable for 15 minutes. An error never wakes the session. The next turn's `Stop` starts a new waiter.
- The hook never waits for a subagent: `SessionStart` and `Stop` fire only in the session a person leads, and the hook exits at once for input that carries a subagent's `agent_id`. A waiting hook is top-level evidence for its session.
- Each Hub process holds at most 4096 waits at once and 64 per person; past either it answers `429` and the hook asks again later. A Hub that runs several processes counts each one on its own.

What this does not cover:

- **Other hosts.** Only Claude Code has the background hook. Codex, Cursor, Copilot, Gemini CLI and MCP-only clients still get a message at their next hook or AgentSCM tool call, as in the tiers above.
- **Sessions started before this change.** A session reads its plugin's hooks when it starts, so a session that was already running when the client was updated has no waiter until it restarts.
- **Windows.** The waiter exits at once on Windows, because it cannot tell there when its session has ended. A Windows session gets its messages at its next hook or tool call.
- **Requests in another repository, the cases not covered.** A session is woken for Work it leads in another repository only when both repositories are on the same Hub and the person's token for the other one is live and can write. The Hub reads the person's eight newest such tokens. A token that was replaced after the Work started is no longer read, so that Work's request waits for the session's next call in its repository. A session the Hub knows only by the shared name `default` is not looked up under other tokens, and two clients of one person that report the same session name can get each other's notice. In each case the lead's person is asked after 10 minutes as above.
- **Ignore.** A person whose mode is Ignore has no waiter (the wait answers `410`), so their sessions are not woken for coordination requests either. A request to Work that such a session leads with another token still comes with its next call, as for a session without a waiter.
- **Clients older than this change** read only a Hub message notice as a wake, so they are woken for no coordination request.
- **Not yet observed.** The wake itself, a real idle Claude Code session woken by this hook, has not been observed with this build; it is to be tried once the client built from main is installed.

### Other clients

MCP has server-to-client messages, but a host decides what reaches the model. Read in each host's own documentation or source on 2026-11-10, only Claude Code channels put a server's push in front of the model and can start a turn:

| Host | What reaches the model | Wakes an idle session | What only the person sees |
| --- | --- | --- | --- |
| Claude Code | [Channels](https://code.claude.com/docs/en/channels-reference) (`notifications/claude/channel`): a research preview, opted in per session with `--channels` or `--dangerously-load-development-channels`, stdio servers only, off by default on Team and Enterprise | Yes, as documented; events sent while busy join the next turn | Elicitation dialogs; `list_changed` refreshes lists |
| Codex CLI | Nothing: [its MCP client](https://github.com/openai/codex/blob/main/codex-rs/rmcp-client/src/logging_client_handler.rs) logs `notifications/message` and `resources/updated` | No | Elicitation |
| Codex `app-server` | Not MCP: a client that runs the thread can `turn/start` or `thread/inject_items` ([app-server](https://learn.chatgpt.com/docs/app-server)) | Only for that client, not a terminal session already running | Its own notifications |
| Cursor | Nothing documented ([MCP](https://cursor.com/docs/context/mcp)) | No | Elicitation |
| VS Code with Copilot | Nothing documented ([MCP](https://code.visualstudio.com/api/extension-guides/ai/mcp)) | No | Elicitation, sampling prompts, the output channel |
| Copilot CLI | Nothing documented; `copilot --acp` is for sessions the ACP client creates ([ACP](https://docs.github.com/en/copilot/reference/copilot-cli-reference/acp-server)) | No | Sampling and elicitation prompts |
| Gemini CLI | Nothing: [its MCP client](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/tools/mcp-client.ts) only re-reads lists on `list_changed` | No | Progress |
| Zed | Nothing ([MCP](https://zed.dev/docs/ai/mcp)) | No | None |

The Claude Code background hook already wakes a Claude Code session (tier `wake`), so the Hub sends no MCP push: every other client gets a message at its next AgentSCM tool call, and nothing here or on the Hub page says it is instant. The 2026-07-28 MCP revision also moves sampling and elicitation inside a tool call, so neither can start one.

A push would need the local proxy (`agentscm client mcp`) to hold a stream from the Hub for the whole host session: the `GET` event stream with its `Mcp-Session-Id` under the 2025-11-25 transport, or a `subscriptions/listen` response stream under 2026-07-28. It would write each notification to the host on standard output. The Hub's MCP endpoint is stateless today and serves neither stream.

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

`agentscm_hub_message` takes `{id, status: accepted | declined | done, session}`, all three required. A call without `session` is refused with `invalid_request`, recovery `provide_field` and the field `session`: the Hub never guesses which of the token's sessions is answering. An agent can answer `accepted` while the message is `delivered`, and `declined` or `done` once it is `delivered` or `accepted`; `declined` and `done` are final. Any other answer, an unknown id, a withdrawn message, or `accepted` while the person's mode is Ignore is refused with `invalid_or_changed` and a message that says why and what the agent may still do.
