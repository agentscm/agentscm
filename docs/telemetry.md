---
title: "Usage telemetry"
---

# Usage telemetry

> **Staged and inactive by default in this source checkout.** Published alpha releases before this change do not contain product telemetry. This capability cannot transmit until an operator configures an HTTPS endpoint and a data-handling profile and the user-facing notice and choice are completed. AgentSCM has no production telemetry endpoint configured or deployed today.

AgentSCM can collect bounded product usage indicators to help the maintainers understand installation, first use and which coarse feature areas are used. It does not treat a stable identifier as anonymous, and its profiles are configuration controls rather than a guarantee of compliance in every jurisdiction.

## What an event contains

Each version-one CLI batch identifies the installation with a stable random identifier. Each event contains only:

- a random event identifier;
- the UTC calendar day, without an exact timestamp;
- the AgentSCM version and normalized operating system and architecture;
- one typed feature category and a coarse outcome; and
- a count of `1`.

Feature events are deduplicated to one event for each feature category and UTC day. The event keeps the first observed `succeeded` or `failed` outcome that day; it is not a command count or a daily success/failure rate. `installation` and `first_use` are observed at most once for an identifier. First use requires successful meaningful CLI work; quickstart, setup and uninstall do not qualify. Delivery is best effort: an event lost to queue expiry, rejection or exhausted retries is not regenerated with a new identifier or observation day. Feature categories are `quickstart`, `uninstall`, `init_repo`, `collisions`, `autopsy`, `intent`, `work`, `native_source`, `workflow`, `team`, `hub`, `git_bridge`, `skill_workflow` and `setup`.

The stable random installation identifier is pseudonymous. By default, events contain no source or source contents, repository or file paths, prompts, account data, workflow text, arbitrary command arguments or hashes. Version one observes meaningful CLI use only. It does not claim Hub participation, hook or MCP activity, completed handoffs, retained coordination, or server outcomes that the CLI did not observe.

Configured batches are sent to the exact configured HTTPS URL, which must end in `/v1/telemetry/batches` and have no credentials, query or fragment. No production endpoint or processor account is configured today. The recommended future design is a first-party relay to PostHog Cloud EU. Its free plan currently retains raw events for one year and offers a data processing agreement; the account, project settings, contract, destination and current retention must be verified before activation. EU hosting and a DPA do not establish universal compliance. Requests about access, correction, deletion, export, restriction, objection or consent withdrawal use the contact published in the public privacy notice.

## When it can send

Both an HTTPS endpoint and an eligible data-handling profile are required:

- `permitted-default` may enable collection after the notice is shown and before-first-send opt-out is offered. Selecting this profile records an operator decision; it does not itself prove that default collection is lawful for every user or location.
- `consent-required` remains inactive until the user affirmatively runs `agentscm telemetry on` or makes the equivalent interactive choice.
- `unknown` remains inactive.

`agentscm telemetry configure` only provisions the recipient and profile. It does not record that the user saw the notice or consented, so status remains `waiting` until the user explicitly runs `agentscm telemetry on`. With no endpoint, AgentSCM creates no telemetry identifiers, queue or history, so later configuration cannot replay earlier use. A consent-required profile also does not queue pre-consent activity. Quickstart does not ask. `agentscm telemetry on` shows the disclosure and records the choice, and nothing is collected before it, even under `permitted-default`.

Verified npm and shell installers pass a process-tree installation marker to the requested AgentSCM command. The client can record installation only after the notice and choice gates permit collection. It does not persist a pre-choice installation history, so declining or leaving the choice pending cannot later replay that installation. The marker can be inherited by child processes, is stripped from the telemetry sender, and is an installer assertion rather than proof that public telemetry is authentic. Source builds do not automatically count as installations.

## Inspect and change the choice

```sh
agentscm telemetry status
agentscm telemetry inspect
agentscm telemetry off
agentscm telemetry on
```

`status` returns `enabled`, `disabled` or `waiting`. Its reason distinguishes an environment or invocation override, a persistent opt-out, an unset endpoint, an unknown profile, a notice or consent requirement, and an enabled configuration. `inspect` displays the bounded event shape and current state without turning telemetry on.

`off` persists the choice, deletes queued records and telemetry identifiers, and sends no opt-out event. A successful `off` writes that state before fencing an older sender; it may wait for a bounded in-flight request to finish. It prevents that sender from starting another request but cannot retract an event the receiver already accepted. `on` shows the disclosure and records an affirmative preference or consent, but the effective state remains waiting if the endpoint or eligible profile is missing. Configuration is stored per user at `$XDG_CONFIG_HOME/agentscm/telemetry.json`, or `~/.config/agentscm/telemetry.json` when `XDG_CONFIG_HOME` is unset.

These overrides take precedence over the stored choice and eligibility:

```sh
DO_NOT_TRACK=1 agentscm COMMAND
AGENTSCM_TELEMETRY=off agentscm COMMAND
agentscm --no-telemetry COMMAND
```

A set `CI` environment variable also disables telemetry as an environment override. The order is `DO_NOT_TRACK=1`, `AGENTSCM_TELEMETRY=off` or `CI`, then `--no-telemetry`, then persistent `off`, then endpoint/profile/notice/consent eligibility.

## Configure a staged endpoint

These commands are for development or a future distributor with an approved receiver and disclosure:

```sh
agentscm telemetry configure --endpoint https://example.com/v1/telemetry/batches --profile permitted-default
agentscm telemetry configure --profile consent-required
agentscm telemetry configure --profile unknown
agentscm telemetry configure --clear-endpoint
```

The endpoint must be an absolute HTTPS URL ending in `/v1/telemetry/batches`, without credentials, a query or a fragment. `--endpoint` and `--clear-endpoint` cannot be combined; either may be combined with `--profile`. Removing the endpoint makes telemetry inactive.

Changing the endpoint or profile clears pseudonymous identifiers, queued events, retained deduplication keys, notice state and consent. It resets an unset or enabled preference to `unset`; an explicit persistent `off` remains `off`. Repeating the current endpoint and profile does not count as a change. Provisioning does not itself show or accept the user-facing notice.

## Delivery and local durability limits

The client treats only HTTP `202 Accepted` from the configured relay as successful delivery. That response reports that the upstream vendor accepted the request; it is not proof of durable ingestion, retention or later queryability.

Transient delivery failures are retried only when a later eligible AgentSCM command opportunistically starts the sender. There is no self-scheduled retry service or guaranteed eventual delivery. One sender process handles at most two batches with a four-second HTTP dispatch budget, and no single HTTP request may exceed three seconds. This is not a wall-clock guarantee for local filesystem operations. Sender settings-lock acquisitions do not wait on contention. Opt-out or a recipient change persists the new state before waiting up to eight seconds for the sender fence; if that wait fails, the command reports an error instead of claiming the older sender has drained. A successful return confirms the fence.

On Unix, telemetry state replacement writes and syncs a private temporary file, atomically renames it over the state file, and syncs the containing directory. Equivalent Windows runtime and directory crash durability has not been qualified.
