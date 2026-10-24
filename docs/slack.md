# Slack adapter

The Slack adapter is an optional Core Edition integration for a single-server Unix Hub. It tells people, by direct message as it happens, about the coordination moments that need a person, and lets a file's holder answer a handoff with a button. It accepts only signed Slack lifecycle events and button clicks at `POST /v1/hub/slack` (`src/hub/slack.rs`). A successful installation is separate from verified Slack delivery.

Agents already get every coordination warning in their own replies. People hear about three moments:

| Hub event | When | Who gets a direct message |
|---|---|---|
| `edit_repeated` | An agent edits a file again after AgentSCM refused the first edit because another agent holds it. | The holder's person, with **Hand it over** and **Keep it** buttons, and the repeating agent's person. |
| `handoff_decline` | A holder's agent declines a handoff request. | Both people. |
| `task_overlap` | An `agentscm_start` creates work that reads like other ongoing work or an open pull request. | The starting agent's person and the person leading each matched work item. |

Everything else (overlap notes, refusals, open or merged pull request notices, granted handoffs) stays between agents. There is no dashboard to refresh: the Hub UI at `/ui/` is the place to look around, and every message links to it.

The adapter does not read channel history, post to channels, copy raw Slack messages, control a remote coding agent, or perform browser administration, approval, landing, source transfer, forge administration, or merge. The only write a Slack user can make is answering a handoff for their own agent. The Open Hub button uses an operator-configured HTTPS Hub origin and carries no credential.

## Installation

Use a matching CLI and Hub build containing these commands. Install the [official Slack CLI](https://docs.slack.dev/tools/slack-cli/) version 4.8.0 or newer and sign in to the intended workspace. AgentSCM does not download or silently authorize another CLI. Workspace approval remains with Slack administrators. The CLI’s operator authorization is separate from the app’s `chat:write` bot scope; review Slack’s displayed management permissions during login.

### Automated preparation and runtime installation

Installation agents and setup surfaces can use the same `hub slack` commands and `hub::slack::setup` service. Preparation validates identity and private files; it does not open a runtime journal. Secret values never belong in command arguments or repository files.

1. Start or select the Hub and connect the repository. Create a dedicated read-only token as described below; do not reuse a coding-agent credential.
2. Create and install a workspace-local app. A bootstrap installation needs no public callback yet:

   ```sh
   agentscm hub slack install --directory "$HOME/.agentscm/slack/app" --team-id T0123456789
   ```

   The generated project contains the minimal manifest, empty SDK hooks and the bundled AgentSCM icon. The official Slack CLI uploads the icon during installation. AgentSCM requires its upload acknowledgement and this workspace's local app record before returning a result, and returns the app ID without copying provider output. The result keeps `installation_verified` false: cached app metadata and an icon acknowledgement do not prove installation approval. `doctor --verify` checks the intended bot token; live delivery and rendering need the checklist below. It does not deploy a Slack-hosted runtime. Reuse the same project when resuming.
3. Obtain the app signing secret and installed bot token from Slack's app settings. Store each in an absolute, private `0600` file outside the repository. Independently verify the intended bot ID and your Slack user ID. Approval and credential collection are not completed by generating a manifest.
4. Prepare a binding using the dedicated read-only credential. The command derives the numeric forge user and repository identity from the authenticated Hub:

   ```sh
   agentscm hub slack setup \
     --directory "$HOME/.agentscm/slack/config" \
     --hub-url https://hub.example.com --repo org/repository \
     --app-id A0123456789 --team-id T0123456789 \
     --bot-id B0123456789 --slack-user U0123456789 \
     --signing-secret-file "$HOME/.agentscm/slack/signing-secret" \
     --bot-token-file "$HOME/.agentscm/slack/bot-token" \
     --agent-token-file "$HOME/.agentscm/slack/read-only-agent.token"
   ```

   `--api-url http://127.0.0.1:7332` can select a local control endpoint; the public callback must still be HTTPS. An existing config must match exactly; a different binding requires deliberate operator configuration.
5. Install the prepared bundle into a managed local Hub:

   ```sh
   agentscm quickstart --hub local --slack-config-file "$HOME/.agentscm/slack/config/config.json"
   ```

   Quickstart copies credentials into private runtime-owned files, rewrites their paths, and supplies `AGENTSCM_SLACK_FILE`. Native and Docker runtimes retain the bundle across resume and retained-data reinstall. Receipts detect altered files; unsafe paths or ownership fail before replacement. Omitting the flag preserves an existing managed bundle. `--yes` alone never enables Slack. Existing remote Hubs require operator installation of the private config and a restart; quickstart does not modify them.
6. Route a public HTTPS origin to that configured Hub, then activate callbacks in the same app project:

   ```sh
   agentscm hub slack install --directory "$HOME/.agentscm/slack/app" \
     --team-id T0123456789 --hub-url https://hub.example.com
   agentscm hub slack doctor --config-file "$HOME/.agentscm/slack/config/config.json" --verify
   ```

   An exact generated bootstrap project can advance to a live manifest. Arbitrary edits, a different live callback origin or a downgrade are refused. Update an intentional custom installation through Slack's settings instead.

Without `--verify`, doctor performs local validation only. With it, doctor checks current Hub binding identity and Slack bot identity, and sends a signed public callback challenge. An echoed challenge establishes reachability only; it does not prove HMAC enforcement or intended-server identity. These checks do not establish Slack-origin delivery, direct message arrival or button outcomes. Complete the checklist below in the real workspace. No tunnel, cloud resource or public endpoint is provisioned automatically.

### Manual installation and additional bindings

Customer-operated installations can import [examples/slack-manifest.json](../examples/slack-manifest.json) through Slack's app settings, replace its placeholder URLs and upload [the app icon](assets/agentscm-icon.png). Alternatively, `agentscm hub slack manifest --hub-url https://hub.example.com` prints the configured manifest. Marketplace listing is not required to create a workspace-local app; Slack's distribution and workspace policies still apply.

Use explicit bindings, one repository per Slack user (up to 32 users). Before starting the Hub, an administrator should verify the exact values in the Slack and forge admin consoles:

- Slack app ID (`A...`), workspace/team ID (`T...`), bot ID (`B...`) from the intended app installation, and user ID (`U...`). Use IDs, not display names.
- Forge provider and host, forge user ID, repository name, and stable repository ID.
- A dedicated AgentSCM token scoped to that provider, host, user, repository, and read-only access. Never reuse a coding-agent credential. The binding links the Slack user to the token's owner: direct messages are about that owner's agents in that repository, and buttons can answer handoffs only for those agents.

The Hub reads the optional `AGENTSCM_SLACK_FILE` environment variable once during startup. Point it at an absolute JSON config file with mode `0600`, owned by the Hub user. Secret files referenced by the config must also be absolute, regular, non-symlink files with mode `0600` and the same owner. Keep the signing secret, bot token, and AgentSCM token in those files; do not put secret values in JSON, Slack manifests, shell history, or logs.

The adapter's SQLite journal must be in an existing, operator-owned directory with mode `0700`. A new database file is created with mode `0600`. The config schema is implemented by `SlackConfig` and `BindingConfig` in `src/hub/slack.rs`; a pilot config has this shape:

```json
{
  "version": 1,
  "app_id": "A0123456789",
  "bot_id": "B0123456789",
  "team_id": "T0123456789",
  "hub_url": "https://hub.example.com/",
  "signing_secret_file": "/absolute/private/slack-signing-secret",
  "bot_token_file": "/absolute/private/slack-bot-token",
  "database": "/absolute/private/agentscm-slack/install.sqlite",
  "bindings": [
    {
      "slack_user": "U0123456789",
      "provider": "github",
      "host": "github.com",
      "forge_user_id": 123456789,
      "repository": "org/repository",
      "repository_id": 987654321,
      "agent_token_file": "/absolute/private/agentscm-slack/read-only-agent.token"
    }
  ]
}
```

The adapter checks the bot token with Slack [`auth.test`](https://docs.slack.dev/reference/methods/auth.test/) before every outbound call and caches the verified workspace and bot identity for at most 60 seconds. A mismatched workspace, mismatched bot, or non-bot token fails closed. Independently copy `bot_id` from the intended app installation record during operator setup; do not derive the expected identity from an arbitrary supplied token. A conflicting `app_id` is also rejected when Slack returns it.

Each member signs in to the intended Hub using their own forge identity and creates a separate read-only credential. Save stdout directly into the private token file, with a restrictive umask; do not paste it into Slack:

```sh
agentscm hub login --hub https://hub.example.com --provider github
umask 077
agentscm hub token create --hub https://hub.example.com --name slack-coordination --repo org/repository --read-only --ttl 7d > /absolute/private/agentscm-slack/read-only-agent.token
```

The repository must already be connected by the Hub operator. Use the exact provider for the member's account. Verify `GET /v1/hub/whoami` through the existing token tools without printing the bearer, then compare numeric owner/repository identity to the binding. Renew before expiry and revoke replaced credentials. Supply each dedicated credential to the operator through an approved secret channel, never a Slack conversation, issue or source file. Container mounts must be readable only by the Hub runtime UID (the standard image uses UID 10001).

The example Slack app manifest is [examples/slack-manifest.json](../examples/slack-manifest.json); its URL is a placeholder and its values must be replaced before import.

After changing the config or rotating a secret, restart the Hub; there is no runtime config reload. A revoked, expired or changed binding token stops that person's messages: queued ones fail instead of being sent. Slack `app_uninstalled` and `tokens_revoked` events write an installation revocation tombstone and fail queued messages. Startup checks the stored app/team identity and does not clear a revoked installation automatically. Do not reset or reuse a revoked journal as a recovery shortcut; treat re-enrollment as a deliberate operator action with a newly verified installation and binding.

The `hub_url` must be an HTTPS origin with no credentials, query, fragment or extra path. Scope configuration is operator-owned; a Slack request cannot select a repository or token file. Revoke the dedicated AgentSCM credential when removing a binding or deactivating its Slack user. The adapter does not poll Slack membership. Before each message and each button answer, it rechecks current AgentSCM authorization. Messages already sent cannot be recalled.

## Slack app manifest

The example manifest requests one bot scope, `chat:write`, which `chat.postMessage` (direct messages) and `chat.update` (replacing a clicked message's buttons with the outcome) need; `auth.test` needs none. Posting to a user ID delivers to the app's direct message with that user, so the manifest enables the Messages tab, read-only: people cannot write to the app. The Home tab and slash command are off. It requests no history, search, file, channel, or message-reading scopes.

The manifest subscribes only to `tokens_revoked` and `app_uninstalled`, and enables interactivity for the buttons. Slack's [manifest reference](https://docs.slack.dev/reference/app-manifest/) defines these fields; [interactivity](https://docs.slack.dev/interactivity/handling-user-interaction/) documents button clicks. Set the same absolute HTTPS endpoint for the Events API and interactivity request URL; it must terminate at `/v1/hub/slack`.

Create the app with the manifest URL settings omitted initially if Slack cannot yet verify the endpoint. Save the app signing secret, install it into the intended workspace, and save the bot token. Configure and start the Hub, then enable Events API and interactivity. The signed URL-verification challenge is handled before a user lookup. Replace `https://example.invalid` before importing the manifest.

An app created from an earlier version of this manifest (Home tab, `/agentscm` command, `commands` scope) is not upgraded by `hub slack install`: update its manifest in Slack's app settings to this one and reinstall it to grant `chat:write`.

## Messages and buttons

A background worker reads the Hub event log every half second. On its first run it starts at the newest event, so history from before installation is never sent. Each event becomes at most one message per person. A message has a short header saying what happened, two columns of fields (the files, up to five then `+N more`; the repository; your agent; their agent with its owner's login), the buttons, and one line of guidance. For example, the holder of a file another agent edited again sees:

```text
Another agent edited a file your agent holds
File                      Repository
docs/slack-live-check.md  counterbranch/agentscm
Your agent                Their agent
slack-live-holder         slack-live-test's slack-live-test-agent
[Hand it over] [Keep it] [Open Hub]
Hand it over, or keep it and agree who goes first.
```

Every header, field and line is plain text (Slack never reads it as markup), and teammate text gets the same filtering agents get. The notification text is the header and the file; it has `&`, `<` and `>` escaped, so it cannot mention a channel or add a link. Several held files come in one message without the handoff buttons.

**Hand it over** and **Keep it** answer the oldest open handoff request for that file addressed to the holder's agent, exactly as `agentscm_handoff` would: a yes moves the file to the requesting session, and both agents are told on their next call. The button works only for the person bound to the holder agent's owner, rechecked at click time. The message's buttons and guidance are then replaced with the outcome, such as `Handed to bob's claude` or `Kept; bob's claude is told on its next call`. A click after the request lapsed or was answered says so and changes nothing.

## Delivery and recovery behavior

The private journal holds a cursor into the Hub event log and the queued messages: recipient, message body, state, attempt count and due time. It does not keep Slack payloads. Messages that are sent or failed are deleted after a day; the journal holds at most 4096 rows, and past that the worker waits for deliveries to drain before reading further events.

`chat.postMessage` retries Slack rate limits and server errors up to eight attempts; then a message becomes `failed` for operator inspection. A mismatched workspace or bot token fails a message without sending it. A message whose send was interrupted by a restart is sent again, so a person may get it twice. A Hub store failure while reading an event leaves the cursor where it was, and the event is read again on the next pass; events with malformed data are skipped.

## Dogfood checklist

Local checks use synthetic IDs, signed requests and a fake Slack API; they do not prove live Slack installation or network access. Verify at least:

- Config and secret files reject relative paths, symlinks, wrong ownership, and group/world-readable permissions; the database parent rejects unsafe permissions.
- The Hub starts with `AGENTSCM_SLACK_FILE`, exposes `POST /v1/hub/slack`, and rejects missing or invalid Slack signatures, app IDs, team IDs, and slash commands.
- The manifest is valid JSON and has exactly the endpoint, scope, Messages tab setting, interactivity setting, and two event subscriptions shown in the example.
- A repeated contested edit messages both people once; only the holder's message has buttons; another person cannot use them; a click hands the file over and replaces the buttons.
- A declined handoff and similar work at start message both people; events from before installation are not sent.
- A rate-limited send stays queued; a mismatched bot, a revoked binding token, `tokens_revoked` and `app_uninstalled` stop delivery.

Live dogfood remains unverified until the administrator installs the app, verifies the HTTPS request URL, supplies the real signing secret and bot token, makes one agent repeat an edit of a file another agent holds, checks both direct messages arrive, answers with a button, and confirms revoke/uninstall behavior in the actual workspace.
