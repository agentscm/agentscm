# Connect a team member to an existing Hub

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration.

This guide is for a developer joining a repository that already uses a hosted AgentSCM
Coordination Hub. It connects the developer's local coding agent. It does not deploy a Hub,
install a GitHub App, administer forge connections or enable the experimental native Community
workspace. Those are separate [operator deployment](deployment.md) and
[Community workspace](advanced/community-workspace.md) tasks.

Ask the team for the Hub's exact HTTPS URL and clone the repository before starting. The Hub
operator must already have connected the forge repository. Use a forge identity that can read the
repository.

This candidate setup runs on macOS and Linux with local coding-agent clients. It does not configure
Windows, hosted cloud agents or general browser and desktop chats.

## One agent-led setup

Use a candidate build or release that includes `client setup`. From the repository clone, choose
the name of the coding agent you are connecting:

```sh
npx agentscm client setup --agent claude --hub 'https://hub.example.com'
```

For a self-hosted forge (GitLab self-managed, GitHub Enterprise) add `--provider github` or
`--provider gitlab`: it is inferred only for github.com and gitlab.com, and the Hub UI's copied
command does not include it.

The five agent names are `claude` for Claude Code, `codex`, `cursor`, `copilot` and `gemini`. There
is no default: the copied Hub instructions tell the running agent to identify its own host and pass
the matching name. The member does not choose between hooks, MCP or APIs.

Setup verifies the Hub URL and the clone's `origin`, then checks the selected agent's existing
AgentSCM wiring. That preflight detects another enabled branded AgentSCM plugin or raw AgentSCM
hook before token minting or binding mutation. Setup then signs you in through the Hub when needed,
creates a private eight-hour token for that repository and binds the exact checkout root. It reuses
a current binding for the same root and Hub. The token is stored in a unique owner-only file and is
never put in the repository, generated agent configuration, URL or command.

Setup installs the selected host's native hooks or plugin, coordination skill where supported and a
checkout-scoped local MCP server. The MCP command uses the private root binding to proxy to the
existing Hub. Generated client configuration contains the installed AgentSCM binary and checkout
paths, but no Hub token. Different checkouts can therefore remain connected to different Hubs.

| Coding agent | Native components written or installed | Review and refresh in the host |
| --- | --- | --- |
| Claude Code | AgentSCM plugin (session and pre-edit hooks plus skill); `.mcp.json` | Review the plugin and project MCP approval, then start a fresh session and confirm it with `/mcp`. See Anthropic's [plugin overview](https://claude.com/blog/claude-code-plugins) and [MCP guidance](https://support.claude.com/en/articles/14554922-claude-code-user-faq). |
| Codex | AgentSCM plugin; AgentSCM block in `.codex/config.toml` | Open **Settings → Hooks**, reload hooks, review and trust AgentSCM, then start a fresh chat. Codex loads project configuration only for trusted projects, and plugin hooks still require review ([OpenAI plugin packaging](https://developers.openai.com/plugins/build/plugins)). |
| Cursor | `.cursor/hooks.json` session and `postToolUse` hooks; `.cursor/mcp.json`; `.cursor/skills/coordinate-edits` | Trust the workspace, approve the MCP connection and start a fresh agent session. The edit hook warns after a matching edit; it is not a pre-edit refusal. See Cursor's [hooks](https://cursor.com/docs/hooks) and [agent security](https://cursor.com/docs/agent/security). |
| GitHub Copilot | Version-1 native hooks in `.github/hooks/agentscm-client.json`; `.github/mcp.json` for Copilot CLI; `.vscode/mcp.json` for the VS Code Copilot harness; `.github/skills/coordinate-edits` | Trust the folder. Restart Copilot CLI after hook changes; in VS Code select the Copilot harness, then review and start the MCP server. See GitHub's [hook guidance](https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/use-hooks), [CLI MCP guidance](https://docs.github.com/en/enterprise-cloud@latest/copilot/how-tos/copilot-cli/customize-copilot/add-mcp-servers) and [VS Code MCP guidance](https://docs.github.com/en/copilot/how-tos/provide-context/use-mcp-in-your-ide/extend-copilot-chat-with-mcp). |
| Gemini CLI | `SessionStart`, `BeforeTool` and `SessionEnd` hooks plus MCP in `.gemini/settings.json`; `.gemini/skills/coordinate-edits` | Review the settings and restart Gemini CLI before verification. The native `BeforeTool` hook covers `write_file` and `replace`. See Gemini CLI's [configuration](https://github.com/google-gemini/gemini-cli/blob/main/docs/reference/configuration.md) and [hook reference](https://github.com/google-gemini/gemini-cli/blob/main/docs/hooks/reference.md). |

The host owns every project, plugin, hook and MCP trust decision. Setup cannot approve those prompts.
It adds each newly created repository-local settings file to `.git/info/exclude`. If a settings file
was already tracked, review its diff and do not commit the machine-specific executable, checkout or
MCP configuration.

If setup finds existing AgentSCM wiring that would run the same integration twice, it stops instead
of silently stacking another copy. Review and remove only the exact old AgentSCM entries before
rerunning, or keep using the old integration. Unrelated hook and configuration entries are preserved.

For Claude Code and Codex, each generated local marketplace has a stable path. Plugin versions are
immutable and named by their content digest; setup publishes a complete version before atomically
updating the marketplace pointer. An interrupted refresh therefore does not expose a partly written
plugin.

If setup reports that the repository is not connected or your identity cannot access it, ask the
Hub operator to fix the existing repository connection. Team-member setup does not install or
reconfigure the Hub's forge application.

## Advanced: bind an existing token file

If you already have your own agent token in a private absolute-path file, bind it without guided
sign-in. Run from the clone, or pass the clone explicitly with `--root`:

```sh
agentscm client connect --hub 'https://hub.example.com' --repo owner/name \
  --token-file '/absolute/private/path/agent.token' --root '/path/to/clone'
```

On macOS or Linux, the token file must be private; run `chmod 600 '/absolute/private/path/agent.token'`
if its group or other permissions allow access.

`--root` defaults to the current directory. The command resolves it to the canonical Git root and
checks the Hub URL, origin repository, token file, live Hub identity, write-capable token kind and
exact repository before saving the local binding. It stores the token file path, never the token.
If a different binding already exists for this root, review `agentscm client list` first, then add
`--replace` only when you intend to replace it.

Inspect and maintain local bindings with:

```sh
agentscm client list
agentscm client check --root '/path/to/clone'
agentscm client remove --root '/path/to/clone'
```

Optional [completion reporting](work-insights.md#optional-completion-reporting) is off by default.
Use `agentscm client reporting on`, `off` or `status`, optionally with `--root`, to change or
inspect the checkout preference. After a setting change, a running MCP proxy stops on its next
request without forwarding it; restart it before continuing. Reconnecting to the same binding
preserves the preference; replacing the Hub, repository, forge origin or token-file binding resets
it to off. Completion observations are silent and do not supply missing model usage or billing
information.

`list` is local and never prints a token. It labels a setup-created token with its nonsecret ID and
expiry; a supplied token is labelled as externally managed. `check` verifies the selected binding
against its Hub.

`remove` removes only the local binding. It never revokes a remote token or deletes a token file.
Pass the exact absolute registered root; that lexical path still works after the checkout has been
deleted. For a setup-created token, the command prints its ID and expiry so you can revoke it
separately. AgentSCM never changes the lifecycle of a supplied token.

When setup successfully replaces or refreshes a token it created, it first commits the new binding.
Under the connection lock it preserves an old managed token file if another binding still references
it. Otherwise it deletes that unreferenced file, then best-effort revokes the old remote token. If
remote revocation fails, it prints the old token ID and expiry for manual revocation; it does not
hide or silently retry the failure. Supplied token files remain untouched.

## Several Hubs, repositories and workspaces

Keep one canonical Coordination Hub per checkout. A root binding joins one canonical Git root to
one Hub, one repository and one private token file. Another checkout may bind to another Hub.
Several repositories can use the same Hub, but each repository needs its own token.

An agent event selects a connection only when its declared working directory or workspace roots
name exactly one current registered root. Multiple matching roots, an event with no route and a
single patch spanning configured roots remain coordination-unknown. An individual edit with an
absolute path can still route to its exact registered checkout. For unknown events, existing intents
expire under their Hub TTL; do not assume they were immediately released.

Human Hub sign-ins are local and separate from agent bindings. Inspect or remove them without
printing session secrets:

```sh
agentscm hub sessions list
agentscm hub sessions remove --hub 'https://hub.example.com'
agentscm hub logout --hub 'https://hub.example.com'
```

Human sessions are keyed by Hub. Before minting a token, setup verifies that the selected session's
forge provider and host match the checkout origin; otherwise it starts the required sign-in. The
most recently saved Hub is the default for commands without `--hub`.

`sessions remove` only forgets the local human sign-in. `logout` also attempts to end that session
on the selected Hub before removing that exact local generation. A concurrently replaced sign-in
is preserved.

A **Coordination Hub** is the deployed service and trust boundary. A **connected repository** is a
forge repository an operator attached to that Hub. An **agent connection** is one developer's local
root binding and repository-scoped token. A **Community workspace** is the optional native workflow
membership, execution, approval and landing layer inside a Hub. Joining coordination does not grant
workspace membership or enable native source control.

## Verify trust and behavior

The Hub's **Connect agents** page separates live activity from setup readiness. Copy its instructions
to your agent; it guides setup and leaves account sign-in and trust decisions with you. Coding-agent
login is separate from forge login. Installed configuration, cached login, MCP discovery and a
session-start event alone do not mark an agent ready.

Your agent calls `agentscm_setup_status` through its native AgentSCM MCP server with its agent name
and actual native hook session ID. Restart after installation: the native start hook supplies this
session's validated, JSON-quoted ID in its own context. Treat it as literal tool data. If that context
is missing, repair the hooks and restart; never guess or inspect another session. The agent uses a
native file-edit tool on a disposable repository file,
removes that test change, then calls the tool again. Both transports need recent observations for
the same checkout binding, token, agent and session. Shell writes and `client check` do not supply
edit-hook proof. Cursor observes edits afterward; use `agentscm_check` before editing there.

To classify a locally captured host error or read current proof:

```sh
agentscm client doctor --agent codex --session 'actual-native-session-id' --diagnostic-stdin < host-error.txt
agentscm client doctor --agent codex --session 'actual-native-session-id'
```

Use the actual agent and session. Before a native session can start, omit `--session` when reporting
a login or trust blocker. Pending setup reports can never become proof; a later verified native
session replaces them. The first command reads at most 64 KiB and sends only a blocker
category, never raw diagnostics. Keep diagnostic files private and delete them afterward. The second
reads observations; it does not simulate MCP or hook proof. Both require a current checkout binding.
Run `client setup` again when that binding needs forge sign-in or token renewal.

| Blocker | Guided next step |
| --- | --- |
| Coding-agent login missing/expired | Use that agent's normal login flow. If no account exists, stop there; setup does not create one. |
| Hook, project or MCP approval pending | Review AgentSCM in the host controls listed above, approve it yourself, then restart the session. Never edit trust records or bypass approval. |
| Usage/quota limit | Follow the provider's reported reset/access instructions, then resume verification. Do not repeatedly probe models, change billing or buy credits automatically. |
| Unsupported account/client | Check supported provider access. No silent credential or paid API fallback. |
| Unknown | Inspect host status. Missing traffic cannot distinguish login, trust and quota failures. |

A blocker invalidates earlier proof. Both transports must be observed again to clear it; an HTTP
success report cannot create readiness. Observations expire after 15 minutes, at token expiry, on
an observed session end/restart, or on Hub restart. Revoked tokens and disabled owners are filtered
from the card. Verify each checkout and session independently.
The team card uses opaque row keys; only same-token, same-binding self-service reads return exact
session IDs. The cache retains at most 16 sessions per token, 64 per forge repository and 256 total.
New rows at capacity cannot evict other tokens' proof; capacity never implies readiness.
In-flight proof and delayed starts are fenced against lifecycle/blocker invalidations. New, ended,
expired or evicted sessions cannot earn proof until a native start is observed again. An unrelated
concurrent invalidation may require retrying verification.

This is advisory runtime observation, not remote host or trust-database attestation: an authenticated
client can imitate protocol traffic. Do not fabricate payloads or use manual HTTP probes to qualify
an agent. Ready covers the observed MCP/edit-hook path, not future quota, every edit tool or native
workflow execution. Provider diagnostics are classified from locally captured errors; the hosted Hub
does not continuously query provider accounts.

Before approving hooks or MCP configuration:

1. Verify the Hub URL with the team through a trusted channel. Repository hooks can send the Hub a
   bearer token and agent hook payloads, and Hub replies can enter agent context.
2. Review the generated local hook and MCP configuration. Confirm that it names the expected root
   binding and that no token appears in tracked files or command arguments.
3. Accept the coding client's own project, MCP and hook trust prompts. Generating or installing a
   plugin does not supply that approval.

`agentscm quickstart` and `agentscm client setup` are one path: after its hub question and collision check, quickstart runs `client setup` for each coding agent (`--agent`, repeatable; Claude Code by default when the `claude` CLI is on `PATH`; `--no-agents` skips it). Agents set up either way appear on the Connect agents page. A client token lasts 30 days, the hub maximum. At a terminal, `client connect` and `client setup` wait for the hub's GitHub App to be installed on the repository: they print where to install it and wait for Enter. Each Git worktree is its own checkout and needs its own `client setup`.

`agentscm client remove --root PATH` undoes `client setup` for one checkout: it takes the agents' entries out of the checkout's files, uninstalls the Claude Code or Codex plugin when no other connected checkout uses it, removes the connection, deletes the managed token file and revokes the token. `agentscm uninstall` runs `client remove` for every checkout quickstart connected, then undoes the rest of quickstart's setup.

`agentscm init-repo` commits only shared agent instruction files (no MCP config, hooks or token). Each developer still runs `agentscm client setup --agent <agent> --hub <hub>` once per checkout, and the Connect agents page verifies the result.

Then run `agentscm client list` and `agentscm client check` for the local binding. Start a real agent
task and confirm that its session appears for the expected repository in the hosted Hub. A local
configuration check proves only the local binding; a visible hosted session proves that one client
reached that Hub. Neither proves hosted reliability, complete hook coverage, native workflow
execution or protection of forge merges. Configuration generation for an agent does not by itself
qualify that host's runtime behavior; verify each host you rely on. The five native configurations
described here are candidate behavior and have not yet been qualified end to end across all five
clients or against a hosted Hub.
