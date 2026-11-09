# Messages to your own agent session

A person signed in to the Hub's web page can send a short message to an agent session they already run: for example, ask the agent that leads a stale Work to finish or release it. The Hub never starts a session. The message waits until that session makes a call the Hub can answer, and reaches the session through that call's reply.

## Trust model

- **Who can send.** Only the Hub's own web page, signed in with the browser cookie, can send. Every change also carries the page's CSRF token. A computer's or a script's bearer session is refused, and so is an agent token.
- **Who can receive.** A message names an agent token and a Hub session. The token must be one of the sender's own live, writing agent tokens for that repository. When the message names a Work, that session must lead it.
- **What the text is.** The Hub writes the frame ("Message from *login* in the Hub, your own person (id …)", the mode's instruction and the reply tool). The note inside it is the sender's own words, quoted as a JSON string. The Hub never inserts agent text (Work titles, checkpoints) or forge text (issue and pull request titles). A Work is named by its id only, and the agent reads it with `agentscm_work_status`.
- **Who answers.** Only the receiving token's own session can answer, with `agentscm_hub_message`. The answers are `accepted`, `declined` and then `done`. The Hub page shows these receipts; nobody else reads them.
- **Storage.** Each person and repository keeps at most 50 messages in Hub settings. The oldest message is dropped first.

## Modes

Each person sets one mode for all repositories, and can override it for a single repository. The default is Manual.

| Mode | What the session sees | What the Hub page shows |
| --- | --- | --- |
| **Auto** | The message, with "act on it now, within your task's rules". | Queued, then delivered, accepted, done. |
| **Manual** | The message, with "show it to your person and wait for their yes in this session". | The same; the agent answers only after the person's yes or no. |
| **Ignore** | Nothing; sending is refused. | The item stays a to-do. The page's copy-and-paste path still works. |

The mode is read when a message is delivered, so changing it to Ignore holds any queued messages.

## Delivery tiers

The Hub records three facts per session, in memory, from the session's own calls:

- the MCP `initialize` `clientInfo` (name and version), which the `agentscm client mcp` proxy passes through;
- the hook the session reports through (`hook:claude`, `hook:codex`, `hook:cursor`, `hook:copilot`, `hook:gemini`, `hook:generic`);
- the agent token's label.

After a Hub restart these facts are lost: a session's tier reads `unknown` until its next call.

From these facts the Hub derives the best tier it can use:

| Tier | Clients | When it arrives |
| --- | --- | --- |
| `hook` | Claude Code and Codex (the reply to `UserPromptSubmit` and to a checked `PreToolUse`, as `hookSpecificOutput.additionalContext`). Cursor (a checked edit's `additional_context`). | At the next prompt or checked tool call. No tool call is needed. |
| `mcp` | Any MCP client, including Gemini CLI, Copilot CLI, Antigravity, Devin, Amp and opencode. | At the session's next AgentSCM tool call, as an extra text block in the result. |
| `unknown` | A session not seen since the Hub started. | Use copy and paste from the Hub page. |

These client hooks fall back to the `mcp` tier:

- Copilot CLI's `preToolUse` has no context field.
- Gemini CLI shows the hook's text to the person, not to the model.

A refused edit's reply carries only the refusal, so a queued message waits for the next call. The start context is cut to fit the host's limit, so the Hub does not deliver a message there either. Each reply carries at most three messages.

### Push to an idle session

None of the tiers above wakes an idle session: each waits for the session's next call. One client documents a way to push to an idle session, and it is not built:

- **Claude Code channels** are a research preview. A channel is an MCP server that declares `capabilities.experimental['claude/channel']` and sends `notifications/claude/channel`. This works in an interactive terminal session started with `--dangerously-load-development-channels server:<name>` when the server is not on the allowlist. That flag shows a warning at each launch.
  - Channels are not available with Bedrock, Vertex or Foundry, and not in `-p` or the Agent SDK.
  - Support in the desktop app is not documented.
  - The client sends no delivery receipt.
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
  - `sessions`, with `token`, `token_name`, `session`, `tier`, `name`, `version`, `hook`, `hook_context`, `mcp` and `seen_at`;
  - `messages`, newest first.
- `PUT /v1/hub/session/agent-messages/mode` with `{repository, this_repository, mode}` sets a mode; `mode: null` clears it.
- `POST /v1/hub/session/agent-messages` with `{repository, token, session, work_id?, note}` queues a message and returns it (`201`). The note is 1 to 1000 bytes, with newlines allowed.
- `DELETE /v1/hub/session/agent-messages?repository=…&id=…` withdraws a message that is still queued.

A message's `state` is `queued`, `delivered`, `accepted`, `declined`, `done` or `withdrawn`. `via` is `hook:<agent>` or `mcp`, and `mode` is the mode it was delivered under.

## MCP

`agentscm_hub_message` takes `{id, status: accepted | declined | done, session}`. An agent can answer `accepted` or `declined` while the message is `delivered`, and `done` once it is `delivered` or `accepted`.
