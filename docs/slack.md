---
title: "Slack adapter"
---

# Slack adapter

The Slack adapter is an optional Core Edition integration for a single-server Unix Hub. It tells people, by direct message as it happens, about the coordination moments that need a person, and lets a file's holder answer a handoff with a button. It accepts signed lifecycle events, narrow app-mention status queries and button clicks at `POST /v1/hub/slack` (`src/hub/slack.rs`). A successful installation is separate from verified Slack delivery.

Agents already get every coordination warning in their own replies. People hear about four coordination moments and, with attention messaging enabled, two work transitions:

| Moment | When | Who gets a direct message |
|---|---|---|
| `edit_repeated` | An agent edits a file again after AgentSCM refused the first edit because another agent holds it. | The holder's person, with **Hand it over** and **Keep it** buttons, and the repeating agent's person. |
| `handoff_decline` | A holder's agent declines a handoff request. | Both people. |
| `task_overlap` | An `agentscm_start` creates work that reads like other ongoing work or an open pull request. | The starting agent's person and the person leading each matched work item. |
| `coordination_requested` | An `agentscm_start` is refused because its scope overlaps work whose lead agent is paused or has stopped checking in, so that agent cannot answer the request. | The person leading that work, once per request. |
| Work enters `blocked` | The shared status first observes a personal blocker; experimental, requires `reminders.enabled`. | The lead agent's person named by the shared status, with an **Open Hub** link to `#work/<id>`. |
| Work enters `handover` | The shared status first observes a handover offer; experimental, requires `reminders.enabled`. | The work owner's person named by the shared status, not necessarily the current lead's person, with an **Open Hub** link to `#work/<id>`. |

Once a week, where a repository admin switched it on, each bound person also gets a private [weekly digest](#weekly-digest) of their own stale unfinished work and the repository's repeated collisions.

Other event notifications (overlap notes, refusals, open or merged pull request notices, granted handoffs) stay between agents. Private mention replies, optional work-entry notices and outstanding-action reminders use the shared deterministic status projection described below. There is no dashboard to refresh: the Hub UI at `/ui/` is the place to look around, and event notifications link to the page about their item: the contested-file box (`#box-contested`) for a repeated edit, the Inbox (`#inbox`) for a declined handoff or a request to an idle agent, and the Work page (`#work/<id>`) for similar work.

The adapter does not read channel history, post to channels, copy raw Slack messages, control a remote coding agent, or perform browser administration, approval, landing, source transfer, forge administration, or merge. A Slack user can make three writes, each for themselves only: answer a handoff for their own agent, release or complete their own stale Work from the weekly digest, and switch their own weekly digest off. The Open Hub button uses an operator-configured HTTPS Hub origin and carries no credential.

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

The example manifest requests `chat:write` for private replies and updates, and `app_mentions:read` for the [`app_mention` event](https://docs.slack.dev/reference/events/app_mention/). Its subscriptions are `app_mention`, `tokens_revoked` and `app_uninstalled`, with interactivity for the handoff and weekly digest buttons. Both callbacks terminate at `/v1/hub/slack`. The manifest does not request channel history, search, files or `chat:write.public`.

Mention the installed app in a conversation it has joined or been invited into: `@AgentSCM status`, `@AgentSCM needs`, or `@AgentSCM proceed`, optionally followed by the exact configured repository name. These are aliases for the same bounded deterministic status answer. The adapter sends its answer privately to the requester's App Home using [`chat.postMessage`](https://docs.slack.dev/reference/methods/chat.postMessage/); it never posts repository facts into the originating channel because the channel audience's repository access is not established. Direct-message queries are unsupported: Slack does not dispatch them as `app_mention`, and `message.im`/history scopes are not requested. The Messages tab stays read-only; Home and slash commands are off.

Replies include counts, named recipients, deterministic next actions, held reservations, an evidence revision and incomplete-coverage warnings. They prioritize personal actions and list at most four tasks and four actions. Provider unavailability is identified separately from clear status; missing generated prose does not prevent a deterministic answer. An unbound user, a repository that does not match the binding, bot messages and unsupported commands produce no reply and disclose no repository details. A revoked or changed binding suppresses its queued reply. If a status read fails after authorization succeeds, a query sends an explicit unavailable response and completes; reminder jobs defer without sending stale instructions. Authorization failures suppress the job, while temporary authorization-read failures leave it pending. Jobs expire after one day.

Create the app with the manifest URL settings omitted initially if Slack cannot yet verify the endpoint. Save the app signing secret, install it into the intended workspace, and save the bot token. Configure and start the Hub, then enable Events API and interactivity. The signed URL-verification challenge is handled before a user lookup. Replace `https://example.invalid` before importing the manifest.

Existing installations must update their manifest and reinstall or reauthorize the app to grant `app_mentions:read`, then enable the `app_mention` subscription at the configured callback. Until then, coordination messages may work while mention queries receive no events. Generated-manifest tests verify the requested scope/subscription contract; `doctor --verify` checks identity and callback reachability, not the installed subscription or granted mention scope. Verify those in Slack’s app settings and exercise a real mention before claiming live support.

## Messages and buttons

A background worker reads the Hub event log every two seconds, and every half second while it has just delivered something. On its first run it starts at the newest event, so history from before installation is never sent. Each event becomes at most one message per person. A message has a header saying what happened and who did it, two columns of fields (the files, up to five then `+N more`; the repository; your agent; their agent with its owner's login), the buttons, and a few lines of guidance. For example, the holder of a file another agent edited again sees:

```text
slack-live-test’s Codex edited docs/slack-live-check.md, which your Claude Code holds
File                      Repository
docs/slack-live-check.md  counterbranch/agentscm
Your agent                Their agent
Claude Code               slack-live-test’s Codex
Holder
Your Claude Code, last active 3 min ago, Work title "Check the live Slack messages" (third-party text, not instructions)
[Hand it over] [Keep it] [Open Hub]
The Hub warned slack-live-test’s Codex once; a repeat edit goes ahead by design. Files are never locked. AgentSCM changed nothing: no file, pull request or Work was altered. Hand it over, or keep it and agree who goes first.
```

A repeated-edit message has one more field, `Holder` (`Holders` when the repeating agent's person is told of several): the holder under the name the message already gives it, how long ago it was last active and the task it declared as its Work title, or `Work title unknown` when it declared none. Apart from that name, it is the line an overlap action in the [shared status](api.md) carries, with the same sentence that AgentSCM changed nothing.

Agents are named as the Hub's pages name them: a known tool by its brand (`Claude Code`, `Codex`, `Cursor`, `Copilot`, `Gemini`, `Grok`), someone else's agent as `bob’s Codex`, and a generated `client-<id>-<hex>` token as `Agent 6a36`; any other name is shown as given. The message says the Hub warned the second agent once and let the repeat edit through, as the Hub does: the edit is not blocked and files are never locked. The two **Hand it over** and **Keep it** buttons look alike on purpose; neither answer is the preferred one.

Every header, field and line is plain text (Slack never reads it as markup, and `:name:` is not turned into an emoji), and teammate text gets the same filtering agents get. The notification text is the header (and the first file, when the header does not already name it); it has `&`, `<` and `>` escaped, so it cannot mention a channel or add a link. Several held files come in one message without the handoff buttons; open the Hub to answer each one.

**Open Hub** goes to the page about the message, not to the front page: `#box-contested` for a repeated edit, `#inbox` for a declined handoff, and `#work/<id>` for similar work (the first work it reads like for the starter, the person's own work for its lead; the pull-request box if only open pull requests matched).

**Hand it over** and **Keep it** answer the oldest open handoff request for that file addressed to the holder's agent, exactly as `agentscm_handoff` would: a yes moves the file to the requesting session, and both agents are told on their next call. The Hub records the answer as given from Slack, never as the agent's own word: the Inbox shows it as `· from Slack`, as it shows a web answer as `· from the Hub`, and the requesting agent is told "amy (from Slack, for their agent codex) …". The button works only for the person bound to the holder agent's owner, rechecked at click time. The message's handoff buttons and guidance are then replaced with the outcome, and the link to the Hub stays:

| Click | Outcome line |
|---|---|
| **Hand it over** | `Handed src/x.rs to bob’s Codex. They see it on their next edit.` |
| **Keep it** | `Kept src/x.rs for now. bob’s Codex sees that on their next edit.` |
| Request already answered or lapsed | `Nothing changed. There is no open request for src/x.rs any more: it was answered or lapsed.` |
| The agent no longer holds the file | `Nothing changed. Your Claude Code no longer holds src/x.rs, so there is nothing to hand over.` |

If Slack refuses to update the message, the same outcome is sent as a new direct message that also says the buttons may still show. A holder who answers in Slack is not sent the "Your agent kept a file" message about their own answer; the requester still is. (An answer given on the Hub's web pages is treated the same way.)

A click the Hub cannot take is answered with an ephemeral message that only the clicker sees; it says what happened, that nothing changed, and what to do (sentences follow the Hub's own, in `ui/src/inboxApi.ts`):

| Cause | Message |
|---|---|
| Not the holder's person, or the agent is no longer connected | `The Hub did not accept this: it is not your agent, or it is no longer connected. Nothing changed. Answer for your own agent in the Hub.` |
| The binding token was revoked or expired, or the installation was revoked | `Your Slack link to the Hub has ended. Nothing changed. Ask whoever runs this Hub to renew your read-only Slack token, or answer in the Hub.` |
| The Hub is at its store limit | `The Hub is busy right now. Nothing changed; press the button again in a moment.` |
| A button the adapter does not recognise | `The Hub did not accept that answer. Nothing changed. Answer in the Hub instead.` |
| A store failure | `The Hub could not record that answer. Nothing changed; press the button again in a moment, or answer in the Hub.` |

The ephemeral message is sent with `chat.postEphemeral`, which uses the same `chat:write` scope. A request that fails Slack's signature, workspace or app checks still returns an HTTP error without a message, and so does one that does not finish within the 2.5-second admission window.

## Optional outstanding-action reminders

Each binding may add a `reminders` object; omission means disabled. Its `enabled` setting opts into both the immediate blocked/handover entry notice and subsequent reminders, not a second delivery path or switch. When disabled, these actions create no episode, job or message. The operator opts in explicitly, for example:

```json
"reminders": {
  "enabled": true,
  "interval_seconds": 3600,
  "max_reminders": 2,
  "escalation_user": "U0234567890",
  "escalation_after_seconds": 7200,
  "send_resolution": true
}
```

Cadence is 900–86400 seconds, each episode sends at most 1–3 successful reminders, and an optional escalation sends once after 900 seconds to seven days (never before the configured cadence). The escalation user must have a separate configured, currently authorized binding to the same provider, host, repository name and stable repository ID. No administrator is selected implicitly. One binding per Slack user remains supported; the adapter rejects ambiguous duplicate-user bindings.

A newly recorded blocked or handover episode queues one short private entry notice in the same transaction, with the work title, repository, holding agent/session and bounded plain-text blocker or handover note. The recipient can answer the blocker where the agent is running or reassign the task, or review the handover and choose an agent in the Hub. Recipients come only from the shared status's `needs_you`: blocked work goes to the lead's person; handover goes to the work owner's person, even if another person's agent now leads it. Other readers receive nothing. Directed requests and failed required checks or stopped gates retain their existing reminders, without an entry notice. Entry delivery does not consume `max_reminders`, move the reminder clock, or change escalation timing. Resolution notices still require a successful reminder; an entry notice alone does not enable one.

Work already blocked or offered for handover when the setting is first enabled gets an entry notice on its first recorded scan. Existing active journal episodes from an older build are retained without retroactive entry notices. Repeated checkpoints in the same state and generation do not create new entries, including after a restart or expiry of completed jobs. A different state or lead generation has a separate subject. A confirmed clear followed by re-entry starts a new cycle and notice; a block cleared and re-entered between scans is still one observed episode. Normal scan latency is up to a minute, not synchronous with the checkpoint; a busy journal can delay admission further. An episode and its entry job either both commit or neither does, so capacity pressure retries admission without sending an unrecorded transition.

Only a personal blocked task, handover review, unanswered directed coordination request, or a failure that blocks a merge after its agent stopped enters the reminder journal. That last case is a failed required check whose linked Work's lead is quiet, paused or handed over (a check with no linked Work never reminds), or a stopped `pr-gate` run; it resolves only once a read with complete check coverage no longer shows it. Quiet/paused work alone does not. The worker scans one due binding per pass, at most once per minute per binding, with at most 4096 retained episodes and 4096 status jobs. Every delivery and retry reauthorizes the exact binding and recomputes the shared action; changed recipients, state or generation suppress obsolete jobs. Quotas bind the action episode rather than provider polling timestamps. A confirmed Work state/generation change can close an episode and optionally send a resolution notice to its still-authorized original recipient; that means the action ended, not that all work succeeded. Omitted/truncated or otherwise unknown facts never establish resolution. Directed requests resolve only when the exact retained request is withdrawn/resolved or a complete current-target response is observed with unchanged request and Work versions; missing requests remain unknown.

Status jobs retain normalized identities and command kind, not raw mention text or source-channel content. Completed status jobs expire after a day; resolved episodes and inactive-binding history are retained for at most seven days. Rotation/configuration changes fence old jobs to their original identity, and legacy queued coordination messages without an exact binding marker are suppressed. The journal is local SQLite and single-worker even when Hub facts come from PostgreSQL; it is not a PostgreSQL delivery journal or a multiwriter service.

## Weekly digest

The weekly digest is one private direct message per bound person per week. It is the
[digest](work-insights.md#digest-stale-work-and-repeated-collisions) for the last seven days, read
with the binding's own token just before it is sent. It is never posted to a channel and never
sent to anyone but the binding's Slack user.

**Who gets one.** Delivery is off for a repository until a repository admin or a Hub admin runs
`agentscm digest enable` in a checkout of it, and each person can switch their own off with
`agentscm digest opt-out` or the **Stop these digests** button
([settings](work-insights.md#weekly-delivery)). The Slack config file has no setting for it. A
person gets a digest when all of these hold: the repository's switch is on, their own is not
off, they have a binding whose token the Hub still authorizes, and the digest has something in
it (stale Work of theirs, a path with collisions, or a colliding pair). A digest that would say
nothing is not sent. A repository with the switch on and no binding sends nothing.

**When.** Weeks start on Monday at 09:00 UTC. The worker looks every five minutes and queues the
week's digest for each binding that has not had one queued this week, so it goes out shortly
after the week starts, or shortly after delivery is switched on for a binding that had none this
week. There are no quiet hours and no per-person time zone. A digest that cannot be sent within
a day is dropped for that week, like other status jobs.

**What it holds.**

```text
Your weekly digest
org/repository, 2026-11-02 to 2026-11-09 (UTC)
Your unfinished work, quiet for 3 days or more: 2
Ship the exporter
active, branch feat/exporter; quiet 6 days; a pull request from its branch merged
[Mark complete] [Release] [Open Hub]
Tidy the parser
paused, no branch; quiet 5 days; whether a pull request from its branch merged is unknown
[Mark complete] [Open Hub]
Paths with the most refused or repeated edits
src/hub: 4 refused, 2 repeated; typically 40 minutes after the holder took the path
People whose agents collided on one path 3 or more times
amy and bob: src/hub/slack.rs, 4 times
Release frees the files a task still holds and leaves it unfinished. Mark complete ends it.
[Stop these digests]
```

- The stale Work rows are the person's own Work only, at most five (the count says when there are
  more), each with what the Hub knows about a merged pull request from its branch: merged, not
  merged, or unknown. No other person's Work title is in the message.
- The path and pair lists are about the whole repository, at most five lines each then `+N more`.
  They name paths and logins that every reader of the repository already sees on the Hub's
  activity page.
- When the Hub's event history does not cover the whole week, the guidance line says from which
  day collisions are counted and that earlier ones are unknown, not zero.
- Every line is plain text, and titles, branch names, paths and logins are cut to a bounded
  length and stripped of control and invisible characters, as in the other messages.

**Buttons.** **Mark complete** and **Release** appear on a row only when the Work's owner may make
that transition now. A click is checked like a handoff answer (the clicker's binding and its
token, at click time) and then makes the Hub's ordinary Work transition as that person, as the
owner would on the Hub's own pages: the Work must be theirs, and the transition is fenced by the
Work's generation and sequence as the digest read them. The Hub records it as a person's
transition with the reason `Released by its owner from the weekly digest in Slack.` or
`Marked complete by its owner from the weekly digest in Slack.`

| Click | Outcome line |
|---|---|
| **Mark complete** | `Marked 'Ship the exporter' complete.` |
| **Release** | `Released 'Tidy the parser'. Its files are free for other work; it stays unfinished until someone resumes or completes it.` |
| The Work changed after the digest was sent | `Nothing changed. 'Tidy the parser' changed after this digest was sent; open the Hub to see where it stands.` |
| **Stop these digests** | `Weekly digests for org/repository are off for you. Run agentscm digest opt-in in a checkout of it to get them again.` |

The outcome is added under the message and only the clicked row loses its buttons, so one digest
settles several tasks. A click the Hub cannot take is answered with the ephemeral messages listed
above, and a button that carries Work the clicker does not own gets
`The Hub did not accept this: that work is not yours. Nothing changed. Open the Hub to see whose it is.`

## Delivery and recovery behavior

The existing coordination journal holds a cursor into the Hub event log and the queued messages: recipient, message body, state, attempt count and due time. It does not keep Slack payloads. Messages that are sent or failed are deleted after a day; the journal holds at most 4096 rows, and past that the worker waits for deliveries to drain before reading further events.

`chat.postMessage` retries Slack rate limits and server errors up to eight attempts; then a message becomes `failed` for operator inspection. A mismatched workspace or bot token fails a message without sending it. A message whose send was interrupted by a restart is sent again, so a person may get it twice. A Hub store failure while reading an event leaves the cursor where it was, and the event is read again on the next pass; events with malformed data are skipped.

## Dogfood checklist

Local checks use synthetic IDs, signed requests and a fake Slack API; they do not prove live Slack installation or network access. Verify at least:

- Config and secret files reject relative paths, symlinks, wrong ownership, and group/world-readable permissions; the database parent rejects unsafe permissions.
- The Hub starts with `AGENTSCM_SLACK_FILE`, exposes `POST /v1/hub/slack`, and rejects missing or invalid Slack signatures, app IDs, team IDs, and slash commands.
- The manifest is valid JSON and has exactly the endpoint, scope, Messages tab setting, interactivity setting, and two event subscriptions shown in the example.
- A repeated contested edit messages both people once; only the holder's message has buttons; another person's click gets a private explanation and changes nothing; a click hands the file over, records it as from Slack, replaces the buttons with the outcome and keeps the Open Hub link to `#box-contested`.
- A declined handoff from the **Keep it** button messages the requester but not the clicker.
- A declined handoff and similar work at start message both people; events from before installation are not sent.
- A rate-limited send stays queued; a mismatched bot, a revoked binding token, `tokens_revoked` and `app_uninstalled` stop delivery.
- With the repository's digest switched on, a person with stale Work gets one weekly digest and a person with nothing to report gets none; **Mark complete** and **Release** change only the clicker's own Work, another person's click gets a private explanation and changes nothing, and **Stop these digests** ends that person's digests.

Live dogfood remains unverified until the administrator installs the app, verifies the HTTPS request URL, supplies the real signing secret and bot token, makes one agent repeat an edit of a file another agent holds, checks both direct messages arrive and read as shown above, answers with a button, checks the outcome line, the Open Hub link and the ephemeral explanation (a click by the wrong person is the easy one to try), and confirms revoke/uninstall behavior in the actual workspace. Mention status, blocked/handover entry notices, reminder retry/deduplication, obsolete-action suppression, escalation, resolution and the weekly digest with its buttons have offline synthetic-request and fake Slack API tests; they are not live qualification. Live mention installation, delivery and desktop/mobile rendering remain operator qualification in issue #7. The local tests use a fake Slack API, so how Slack renders the header, the fields, the ephemeral message and a message sent after a failed update has not been seen.
