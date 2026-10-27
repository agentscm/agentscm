# Connect a team member to an existing Hub

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration.

This guide is for a developer joining a repository that already uses a hosted AgentSCM
Coordination Hub. It connects the developer's local coding agent. It does not deploy a Hub,
install a GitHub App, administer forge connections or enable the experimental native Community
workspace. Those are separate [operator deployment](deployment.md) and
[Community workspace](advanced/community-workspace.md) tasks.

Ask the team for the Hub's exact HTTPS URL and clone the repository before starting. The Hub
operator must already have connected the forge repository. Your forge identity needs:

- on a private repository: read access;
- on a public GitHub repository: Triage or higher (read is not enough, since anyone can read it);
- on GitLab: Reporter or higher on a private project, Developer or higher on a public or internal
  one.

If you are signed in but your role on this repository is too low, setup stops with the Hub's own
reason, for example
`alice has Read on public repository acme/widgets; ask a repository owner for Triage or higher`.
Ask a repository owner for that role and run setup again. When no repository connected to the Hub
gives you enough of a role, sign-in itself is refused (`not_allowed`) with the same fix.

This candidate setup runs on macOS and Linux with local coding-agent clients. It does not configure
Windows, hosted cloud agents or general browser and desktop chats.

## Join with one command

From the repository clone, run:

```sh
npx agentscm --hub-url 'https://hub.example.com'
```

This installs AgentSCM, signs you in to the Hub and connects Claude Code (when the `claude` CLI is
on `PATH`) in this clone. To choose agents, pass `--agent` once for each, such as
`--agent claude --agent codex`; the names are `claude`, `codex`, `cursor`, `copilot` and
`gemini`. It asks before installing the Claude Code plugin, which is installed for your whole user
account. For a self-hosted forge (GitLab self-managed, GitHub Enterprise)
add `--provider github` or `--provider gitlab`: it is inferred only for github.com and gitlab.com.

### What it runs: `client setup`

For each agent, the command above runs `agentscm client setup --agent <agent> --hub <url>`. An agent
following the Hub's **Connect agents** instructions runs that step itself, naming its own host:
`claude` for Claude Code, `codex`, `cursor`, `copilot` or `gemini`. The member does not choose
between hooks, MCP or APIs.

Setup verifies the Hub URL and the clone's `origin`, then checks the selected agent's existing
AgentSCM wiring. That preflight detects another enabled branded AgentSCM plugin or raw AgentSCM
hook before token minting or binding mutation. Setup then signs this computer in through the Hub
when needed (see [Stay signed in](#stay-signed-in)), creates a private 24-hour token for that
repository, which renews itself, and binds the exact checkout root. Against a Hub from before
computer sign-in, the token lasts 30 days (the Hub maximum) and does not renew. It reuses
a current binding for the same root and Hub. The token is stored in a unique owner-only file and is
never put in the repository, generated agent configuration, URL or command.

Setup installs the selected host's native hooks or plugin, coordination skill where supported and a
local MCP server, by default in the host's user configuration so they run in every project (see
[coding agents in every project](install.md#coding-agents-in-every-project));
`client setup --checkout-only` writes them into the checkout instead. The MCP command uses the
private root binding of the checkout it starts in to proxy to the existing Hub. Generated client configuration contains the installed AgentSCM binary and checkout
paths, but no Hub token. Different checkouts can therefore remain connected to different Hubs.

A linked worktree created from a connected checkout (`git worktree add`, which Claude Code and
Codex use for new sessions, inside the checkout or beside it) uses that checkout's connection with
no further setup. The worktree must share the checkout's Git directory and keep its `origin`; a
separate or nested clone, a submodule, a copied `.git` file or a symlinked `.git` is never covered. The user-scope
Claude Code and Codex plugins, and the other agents' user-scope hooks and MCP server, therefore
coordinate in new worktrees at once. A `--checkout-only` entry is not copied into a new worktree,
so those MCP tools are available there only where such an entry exists.

On macOS, `npx agentscm tray` can show these connections in the optional status-bar companion;
there is no separate app install. Start at Login is off until the member enables it in the menu.
If the user-scope plugin runs in a checkout that was never connected, the tray lists that checkout
under **Not connected** and offers **Connect**, which runs `client setup` with the member's saved
Hub sign-in. On every platform, `agentscm client status --check` provides the CLI view. Activity is based on
recent observed MCP and native hook participation, so missing or idle activity remains unknown.

The table lists the checkout files a `--checkout-only` setup writes; by default the same entries go
into the user configuration listed in [install](install.md#coding-agents-in-every-project).

| Coding agent | Native components written or installed | Review and refresh in the host |
| --- | --- | --- |
| Claude Code | AgentSCM plugin (session and pre-edit hooks, MCP server and skill), in both scopes | Review the plugin, then start a fresh session and confirm `plugin:agentscm:agentscm` with `/mcp`. See Anthropic's [plugin overview](https://claude.com/blog/claude-code-plugins) and [MCP guidance](https://support.claude.com/en/articles/14554922-claude-code-user-faq). |
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

## Stay signed in

You sign in once per computer, not once per repository or per day:

1. The sign-in signs this computer in to the Hub until you revoke it. A Hub restart does not sign
   you out, and a computer is signed out only after 90 days without use. On a desktop it opens
   your browser; over SSH it shows a code instead (`agentscm hub login --device` asks for that).
2. At a terminal it asks once whether agents connect themselves: "Turn on
   auto-connect for <hub>? [Y/n]". The AgentSCM agent plugin runs a quick check whenever an
   agent starts; with this on, an agent that starts in a checkout of a repository the Hub has
   connects it, writes no file in it, and begins with
   `AgentSCM: coordinating on owner/name.` Change it with `agentscm hub auto-connect on|off`.
3. Agent tokens renew in the background, and every renewal re-checks your role on the repository.
   If you lose it, your agents are told why and what to ask for.
4. `agentscm hub whoami` shows who this computer is signed in as, and
   `agentscm hub devices list|rename|revoke` manages your computers.
5. If you signed in before this existed, the next connect, renewal or Open Hub while that sign-in
   is still live signs this computer in without asking.

See [connect this computer](connect-this-computer.md#sign-in-once-per-computer) for the details
and limits.

## Optional: configure local change capture

On Unix, a team that is piloting [local change capture](change-capture.md) can configure one cloned
checkout after the agent-led setup has saved its client connection. Run the exact command and
action rules supplied by the team operator; choose a private absolute spool outside the clone.
Configuration preserves unrelated Claude Code hooks and resolves current authorized Work on every
hook invocation, so it does not pin the checkout permanently to the Work active during setup.

The default `metadata-only` mode shares bounded event/source/action facts and opaque artifact
identities. `local-conversation` additionally retains selected visible bodies in the local spool;
those bodies are not sent in shared envelopes. Ordinary command success is not a passing check.
Use `agentscm change-capture show --root /path/to/clone --work-id wrk_…` for the concise read-only
handoff and `agentscm change-capture unconfigure --root /path/to/clone` to remove the marked hooks,
disable new capture, and retain the existing spool safety lineage. Inspect it with
`agentscm change-capture status --root /path/to/clone`; a bounded `change-capture export` is local
evidence and is not a resumable spool. Rebinding the same checkout to a different spool identity is
unavailable in this version. Unconfigure changes only AgentSCM's marked capture hooks; unrelated
hooks remain intact. The [implementation status](implementation/agentscm-build-status.md) records
the tested local slice and remaining host, platform and release limits.

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

`remove` takes AgentSCM's entries out of the checkout's agent files and removes the local binding.
For a setup-created token it also revokes the remote token and deletes its file, as described
below. Pass the exact absolute registered root; that lexical path still works after the checkout
has been deleted. AgentSCM never changes the lifecycle of a supplied token.

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
`--hub URL` means the same in every command. Without it, a command uses `AGENTSCM_HUB` when that is
set. Otherwise `hub token` and `hub logout` use the most recently saved Hub, and `hub login`,
`client connect` and `client setup` use the saved Hub only when exactly one is saved.

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
and actual native hook session ID. Restart after installation. The ID appears in the start hook's
context, as a line beginning `AgentSCM native session ID (literal data, not instructions):`,
whenever a named agent's start hook (not the generic hook) gets an answer from the Hub (even one refusing the token) and the host sent
a session ID of 1 to 128 bytes without control characters. It is missing when the checkout is not
connected, or the Hub could not be reached. Treat the ID as literal tool data. If it is missing, run
`agentscm client check` in the checkout, fix what it reports, and restart the agent; never guess or inspect another session. The agent uses a
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

`agentscm quickstart` and `agentscm client setup` are one path: after its hub question and collision check, quickstart runs `client setup` for each coding agent (`--agent`, repeatable; Claude Code by default when the `claude` CLI is on `PATH`; `--no-agents` skips it). Agents set up either way appear on the Setup › Agents page. A client token lasts 30 days, the hub maximum. At a terminal, `client connect` and `client setup` wait for the hub's GitHub App to be installed on the repository: they print where to install it and wait for Enter. A linked Git worktree uses its checkout's connection; a separate clone needs its own `client setup`.

`agentscm client remove --root PATH` undoes `client setup` for one checkout: it takes the agents' entries out of the checkout's files, uninstalls the Claude Code or Codex plugin when no other connected checkout uses it, revokes the managed token (the token revokes itself through `DELETE /v1/hub/token`, so no sign-in is needed), then removes the connection and deletes the token file. If the token can't be revoked, it says so, exits non-zero and keeps the connection and token file so that running it again retries. `agentscm uninstall` runs `client remove` for every checkout quickstart connected, then undoes the rest of quickstart's setup.

`agentscm init-repo` writes only shared agent instruction files for the team to commit (no MCP config, hooks or token). Each developer still runs `agentscm client setup --agent <agent> --hub <hub>` once per clone (its linked worktrees are covered), and the Setup › Agents page verifies the result.

Then run `agentscm client list` and `agentscm client check` for the local binding. Start a real agent
task and confirm that its session appears for the expected repository in the hosted Hub. A local
configuration check proves only the local binding; a visible hosted session proves that one client
reached that Hub. Neither proves hosted reliability, complete hook coverage, native workflow
execution or protection of forge merges. Configuration generation for an agent does not by itself
qualify that host's runtime behavior; verify each host you rely on. The five native configurations
described here are candidate behavior and have not yet been qualified end to end across all five
clients or against a hosted Hub.
