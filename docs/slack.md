# Slack adapter

The Slack adapter is an optional Core Edition integration for a single-server Unix Hub. It renders bounded, repository-scoped AgentSCM views in a private App Home and accepts only signed Slack events, slash commands, and typed modal submissions at `POST /v1/hub/slack` (`src/hub/slack.rs`). A successful installation is separate from verified Slack delivery and rendering.

The adapter is designed for private status and coordination. It does not read channel history, copy raw Slack messages, disclose work in shared channels, control a remote coding agent, or perform browser administration, approval, landing, source transfer, forge administration, or merge. Use the existing Hub browser UI for those human actions. The Open Hub button uses an operator-configured HTTPS Hub origin and carries no credential.

## Installation

Use a matching CLI and Hub build containing these commands. Install the [official Slack CLI](https://docs.slack.dev/tools/slack-cli/) version 4.8.0 or newer and sign in to the intended workspace. AgentSCM does not download or silently authorize another CLI. Workspace approval remains with Slack administrators. The CLI’s operator authorization is separate from the app’s `commands` bot scope; review Slack’s displayed management permissions during login.

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

   Add `--allow-coordination-writes` only when enabling the two structured forms. `--api-url http://127.0.0.1:7332` can select a local control endpoint; the public callback must still be HTTPS. An existing config must match exactly; a different binding requires deliberate operator configuration.
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

Without `--verify`, doctor performs local validation only. With it, doctor checks current Hub binding identity and Slack bot identity, and sends a signed public callback challenge. An echoed challenge establishes reachability only; it does not prove HMAC enforcement or intended-server identity. These checks do not establish Slack-origin delivery, App Home rendering, form outcomes or human approval. Complete the checklist below in the real workspace. No tunnel, cloud resource or public endpoint is provisioned automatically.

### Manual installation and additional bindings

Customer-operated installations can import [examples/slack-manifest.json](../examples/slack-manifest.json) through Slack's app settings, replace its placeholder URLs and upload [the app icon](assets/agentscm-icon.png). Alternatively, `agentscm hub slack manifest --hub-url https://hub.example.com` prints the configured manifest. Marketplace listing is not required to create a workspace-local app; Slack's distribution and workspace policies still apply.

Use explicit bindings, one repository per Slack user (up to 32 users). Before starting the Hub, an administrator should verify the exact values in the Slack and forge admin consoles:

- Slack app ID (`A...`), workspace/team ID (`T...`), bot ID (`B...`) from the intended app installation, and user ID (`U...`). Use IDs, not display names.
- Forge provider and host, forge user ID, repository name, and stable repository ID.
- A dedicated AgentSCM token scoped to that provider, host, user, repository, and read-only access. Never reuse a coding-agent credential. If the pilot explicitly needs Slack start/checkpoint writes, keep this same dedicated token read-only and set `allow_coordination_writes` to `true`. The Hub creates an opaque in-process grant for only those two work operations; the bearer itself remains unable to use general write HTTP/MCP routes.

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
      "agent_token_file": "/absolute/private/agentscm-slack/read-only-agent.token",
      "allow_coordination_writes": false
    }
  ]
}
```

The adapter checks the bot token with Slack [`auth.test`](https://docs.slack.dev/reference/methods/auth.test/) before outbound views and caches the verified workspace and bot identity for at most 60 seconds. A mismatched workspace, mismatched bot, or non-bot token fails closed. Independently copy `bot_id` from the intended app installation record during operator setup; do not derive the expected identity from an arbitrary supplied token. A conflicting `app_id` is also rejected when Slack returns it.

Each member signs in to the intended Hub using their own forge identity and creates a separate read-only credential. Save stdout directly into the private token file, with a restrictive umask; do not paste it into Slack:

```sh
agentscm hub login --hub https://hub.example.com --provider github
umask 077
agentscm hub token create --hub https://hub.example.com --name slack-coordination --repo org/repository --read-only --ttl 7d > /absolute/private/agentscm-slack/read-only-agent.token
```

The repository must already be connected by the Hub operator. Use the exact provider for the member's account. Verify `GET /v1/hub/whoami` through the existing token tools without printing the bearer, then compare numeric owner/repository identity to the binding. Renew before expiry and revoke replaced credentials. Supply each dedicated credential to the operator through an approved secret channel, never a Slack conversation, issue or source file. Container mounts must be readable only by the Hub runtime UID (the standard image uses UID 10001).

The example Slack app manifest is [examples/slack-manifest.json](../examples/slack-manifest.json); its URL is a placeholder and its values must be replaced before import.

After changing the config or rotating a secret, restart the Hub; there is no runtime config reload. Rotating the dedicated AgentSCM token changes the binding fingerprint: reconciliation clears the previous view, then the user must reopen Home or request a refresh to see the current authorized projection. Slack `app_uninstalled` and `tokens_revoked` events write an installation revocation tombstone and fail queued refreshes. Startup checks the stored app/team identity and does not clear a revoked installation automatically. Do not reset or reuse a revoked journal as a recovery shortcut; treat re-enrollment as a deliberate operator action with a newly verified installation and binding.

The `hub_url` must be an HTTPS origin with no credentials, query, fragment or extra path. Scope configuration is operator-owned; a Slack request cannot select a repository or token file. Revoke the dedicated AgentSCM credential when removing a binding or deactivating its Slack user. The adapter does not poll Slack membership. Before disclosure, it rechecks current AgentSCM authorization. A background check attempts to replace previously published Home views when access is revoked or a binding changes, normally within 30 seconds while the worker and Slack API are available. This is best effort: already disclosed copies and in-flight responses cannot be recalled.

## Slack app manifest

The example manifest uses only the scopes required by the current adapter: `commands`. The adapter calls `auth.test`, `views.publish`, and `views.open`, which require no additional scopes. It requests no history, search, file, channel, or message scopes. Set the same absolute HTTPS endpoint for the slash command, Events API, and interactivity request URL. The endpoint must be reachable by Slack and must terminate at `/v1/hub/slack`.

The manifest enables the Home tab and subscribes only to `app_home_opened`, `tokens_revoked`, and `app_uninstalled`. Slack's [manifest reference](https://docs.slack.dev/reference/app-manifest/) defines these fields; [interactivity](https://docs.slack.dev/interactivity/handling-user-interaction/) also documents that modal submissions use the interactivity request URL.

Create the app with the manifest URL settings omitted initially if Slack cannot yet verify the endpoint. Save the app signing secret, install it into the intended workspace, and save the bot token. Configure and start the Hub, then enable Events API and interactivity and add the slash-command URL. The signed URL-verification challenge is handled before a user lookup. Replace `https://example.invalid` before importing the manifest. Do not add message events or broader scopes for this adapter.

## Commands and views

The slash command is `/agentscm`:

- `/agentscm` or `/agentscm status` queues a private Home refresh.
- `/agentscm work`, `/agentscm work <id>`, `/agentscm briefing`, `/agentscm coordination`, `/agentscm handoffs`, and `/agentscm workflows` open bounded private views.
- `/agentscm start` opens the structured start modal.
- `/agentscm checkpoint <id>` opens the structured checkpoint modal.

A slash-command response acknowledges the request and points the user to App Home; it is not a refresh delivery receipt or execution receipt. App Home projections are generated fresh by `views.publish`. Duplicate events and actions are journaled idempotently, and each retry renders a fresh projection rather than replaying stale blocks.

Start records an AgentSCM work record under the Slack coordination identity. It does not start or stop a coding agent. Checkpoints are limited to typed AgentSCM records: progress, decision, blocker, pause, finish, or handover, plus a next action. An optional field explicitly resolves the recorded blocker. The checkpoint carries the work generation and expected update sequence; stale submissions are rejected and the user should refresh the work view before retrying. A Slack user can checkpoint only active or blocked work owned by that user's Slack binding and the Slack session. Paused, handed-over and finished work cannot be reactivated by these forms. Reported work is not proof of execution, tests, review, publication, landing, or merge.

Every configured bearer must be read-only. Bindings without the additional operator write opt-in cannot submit start/checkpoint writes. Every Slack view is bounded and may show partial coverage or retention gaps. The view footer states when it was rendered and whether display limits truncated the projection.

## Delivery and recovery behavior

The private journal tracks possible disclosures before dispatch, including unknown results, until an acknowledged replacement or revocation cleanup. A binding change cannot overwrite another binding's unresolved disclosure record.

The private journal retains opaque IDs, binding fingerprints, page names, request hashes, states, timestamps, and bounded retry counters. It does not retain raw Slack payloads or message text. The briefing view shows a bounded first page and its completeness/retention flags. Continue pagination and explicitly acknowledge a complete catch-up using the existing AgentSCM clients; Slack never advances an acknowledgement cursor automatically.

`views.publish` deliveries retry transient Slack rate limits and server errors up to eight attempts; exhausted rows become `failed` for operator inspection. Duplicate event/action IDs are ignored. Signed Home-open events for the configured app and workspace are acknowledged even when the user is unbound, the credential is unavailable, or refresh admission fails; no repository content is disclosed. Lifecycle-event failures remain retryable until revocation is recorded. New requests supersede older queued views for the same user. This is a refresh-driven client; it does not push an unsolicited activity feed.

When a work proposal overlaps another claim, the modal confirms the durable coordination request and the ungranted claim separately; resubmitting the same action retains that outcome. Correctable UTF-8/text/scope validation errors keep the nonce open for editing.

Core AgentSCM mutations are different. If a start or checkpoint result is ambiguous, the action is marked `uncertain` and is not blindly replayed. Refresh and inspect the current AgentSCM work state before deciding what to do. There is no action-inspection endpoint. Action nonces and their outcome metadata, including uncertain saves and the associated Home warning, expire after ten minutes; expiry does not establish whether a Core mutation committed. Delivery requests expire after 24 hours in every state. Do not treat an old pending row as proof that a Slack refresh completed.

## Dogfood checklist

Local checks can use synthetic IDs, signed requests, a private test endpoint, and a fake Slack API; they do not prove live Slack installation or network access. Verify at least:

- Config and secret files reject relative paths, symlinks, wrong ownership, and group/world-readable permissions; the database parent rejects unsafe permissions.
- The Hub starts with `AGENTSCM_SLACK_FILE`, exposes `POST /v1/hub/slack`, and rejects missing or invalid Slack signatures, app IDs, team IDs, and user bindings.
- The manifest is valid JSON and has exactly the endpoint, scopes, Home setting, command, interactivity setting, and three event subscriptions shown in the example.
- A valid bound user can render status, work, briefing, coordination, handoffs, and workflows; duplicate refresh requests do not create duplicate deliveries.
- A binding without the write opt-in cannot open a writing modal. An explicitly enabled coordination binding can start work, checkpoint its own Slack-owned work, and receives a stale-generation error after another update.
- Another Slack user, another repository binding, malformed page/work IDs, expired action nonces, revoked AgentSCM tokens, `tokens_revoked`, and `app_uninstalled` are rejected or stop delivery.
- A simulated transient `views.publish` error retries within the eight-attempt bound; an ambiguous core mutation is not replayed; failed rows remain inspectable.
- No test sends a message to a channel, requests Slack history, persists raw Slack payloads, or exposes credentials.

Live dogfood remains unverified until the administrator installs the app, verifies the HTTPS request URL, supplies the real signing secret and bot token, opens App Home, exercises the commands and modal flows, and confirms revoke/uninstall behavior in the actual workspace.
