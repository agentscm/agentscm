---
title: "Manage connections on this computer"
---

# Manage connections on this computer

The installed native engine connects your coding agents to a Hub.
Use the existing setup command to connect to your team's Hub:

```sh
npx agentscm --hub-url https://hub.example.com
```

The Hub is the authority for shared Work, repository access and coordination. AgentSCM has no web interface; native source/workflow commands, local capture data
and an explicitly hosted Hub's API and database are unaffected by that.

## Sign in once per computer

`agentscm hub login` signs this computer in to
the Hub, not only you. On a desktop it opens your browser: authorize once, and the browser finishes on a small local result page and the terminal reports the signed-in account and computer. The terminal says it is waiting, every
30 seconds says so again, and gives up after 3 minutes with what to do next. Over SSH or without a
display (or with `--device`, or `AGENTSCM_NO_BROWSER=1`) it shows a code to enter on another
device instead; when the Hub's GitHub App does not allow that, it says so and uses the browser
sign-in. Every failed step names what failed, for example that the sign-in link expired or that
the forge did not accept the code. What signing in gives you:

- **It lasts until revoked.** The Hub keeps a credential for this computer (only its digest, in
  the Hub's database), so a Hub restart or deploy does not sign you out, and there is no 8-hour or
  30-day limit. A computer unused for 90 days is signed out. The credential is kept in the
  owner-only `$XDG_CONFIG_HOME/agentscm/hub-sessions.json` (default under `~/.config`), next to
  the 8-hour session, and is sent only to the Hub that issued it. Signing in again on the same
  computer replaces its credential; the Hub never matches computers by name. Replacing it revokes
  the agent tokens the old credential issued, so the sign-in renews the token of each checkout
  connected to that Hub, and names any it could not renew with the command that reconnects it.
- **Agents connect themselves**, if you say so. AgentSCM's agent plugin runs a quick check
  whenever an agent starts. The sign-in asks once at a terminal: "Turn on
  auto-connect for <hub>? [Y/n]" (`--no-auto-connect` answers no). A sign-in made before this
  question existed is asked at the next interactive `agentscm hub` or `agentscm client` command. Change it with
  `agentscm hub auto-connect on|off [--hub URL]`. With it on, when a coding agent starts a session
  in a checkout that has no connection, its hook connects the checkout to the first such Hub (the
  default Hub first, at most four) that has the repository and where you have the role, and the
  agent's context starts with `AgentSCM: coordinating on owner/name at <Hub>. This checkout was just
  connected.` Every later session in a connected checkout starts with the same line, without the
  last sentence, followed by whether conversation sharing is on for the session. When something
stops coordination (the token was refused, or access ended), that reason replaces the line. It writes only AgentSCM's
  own files under your config directory, never a file in the checkout, and runs the same checks as
  `client connect`. It waits at most 2.5 seconds; a slower attempt says nothing and finishes for
  the next session. In a repository no such Hub has, the check stays silent, and that Hub is not
  asked about it again for an hour. A Hub that sees your origin is a fork of a repository it has
  says so, and names the repository to use as `origin`. Any other attempt is retried after five
  minutes. The agent's start context also names the other repositories connected on this computer,
  and an AgentSCM call with absolute paths in one of them, or with `root` set to its checkout, goes
  to that repository.
- **Tokens renew.** An agent token minted through this computer lasts 24 hours. When less than 12
  hours are left, the next hook call renews it in the background: the Hub re-checks your access, a
  new token is saved, and the old one is revoked. A running MCP server switches to the new token.
  A laptop that slept for days renews at the first hook call after waking.
- **Agent labels carry no host name.** The token's label is `client-<id>-<hex>`, where `<id>` is a random machine id kept in a private
  `machine-id` file next to `hub-session.json` (never derived from the host name). To choose a name
  instead, write one line to `machine-name` in the same directory; labels then read `<name>-<hex>`
  (ASCII letters, digits, `-` and `_`, at most 50). Tokens already stored keep their old label until
  the token is replaced, and Work records that already copied it keep it. Hook and MCP text,
  `agentscm intent`, Slack and pull-request text mask it; structured JSON fields such as `agent.name`
  and `lead_agent` and your own `agentscm hub token list` listing still
  show it.
- **Access is re-checked every time.** Each mint and renewal checks that you are still enabled
  and allowed on the Hub and still have the repository role (on a public repository Triage or
  more, on a private one read; on GitLab Reporter for private, Developer otherwise). The Hub asks
  the forge with the repository connection's own credential, never a stored token of yours. When
  something refuses, the agent's context and the CLI show the Hub's own sentence, such as
  `alice has Read on public repository acme/widgets; ask a repository owner for Triage or higher`,
  and the current token keeps working until it ends or is revoked. When the Hub or the forge does
  not answer, nothing is marked as needing attention; the next hook call tries again.

`agentscm hub whoami` shows, for each saved Hub, who this computer is signed in as, the
computer's name, whether auto-connect is on and the checkouts connected to it. Until you choose, `whoami` says what auto-connect does and
prints the command that chooses it (`agentscm hub auto-connect on|off --hub URL`). `agentscm client
doctor` checks a checkout's connection through this computer's credential, the way the next renewal
would, so it reports a verified connection even when no agent is running. It also prints one line
for each session that has reported to the Hub, latest first, and `--json` prints the Hub's reply
instead. `client setup` and `client check` print when the agent token ends and, for a token minted
through this computer, that it renews automatically; `agentscm client renew` renews it by hand if
renewal fails. A token file given to `client setup` is never renewed.

Your computers: `agentscm hub devices list` shows each one with its name, last use and the
repositories connected through it, live ones first; `agentscm hub devices rename NAME` renames
this one; `agentscm hub devices revoke dev_…` signs one out (from any of your computers). Revoking a computer revokes every
agent token minted through it, so its agents stop at their next call, and ends the session that
signed it in. `agentscm hub logout` signs this computer out the same way and warns if the Hub could
not be told. Hub administrators can revoke any computer. See
[computer credentials](hub.md#computer-credentials).

Sign-ins made before computers had credentials keep working until their session ends; while that
session lives, the next connect, renewal enrolls this computer without
asking. Existing 30-day tokens keep working and are renewed into 24-hour ones once the computer
has a credential. A sign-in that has already expired needs one more sign-in.

## Local controls

```sh
agentscm client status --check
agentscm client doctor
agentscm client service status
agentscm client service stop
agentscm client service start
agentscm hub whoami
agentscm hub auto-connect off --hub https://hub.example.com
agentscm hub devices list
agentscm hub logout --hub https://hub.example.com
```

`client status` distinguishes configured connections from observed native integration coverage.
`client doctor` reports the Hub binding and native onboarding evidence; missing hooks or trust are
not a successful connection test.

On macOS, the verified installer/setup flow configures a per-user launchd resident from the same
versioned native executable. It retries retained reports, observes only qualified native host exits
and refreshes declared dependencies. It runs no model, starts no coding agent, sends no native
wake and serves no HTTP listener. Existing native hook waiters still deliver notices; a closed
agent session remains a resume/recovery action.

`client service stop` keeps the resident stopped across login and later setup/updates; `start` is
the explicit way to enable it again. `client service status` reports the invoked, installed and
running versions separately and retains unknown recovery state. `client status` also reports
pending/stalled journal coverage. Installing a package version changes the installed binary; only
the next setup/quickstart run hands the resident over to that version.

An observed exit that the Hub cannot qualify is parked locally without ending the Hub session or
changing its Work. Pending reports remain eligible for recovery; lease expiry still applies.
Status separates pending and retained journal records and reports `journal_full` when the
512-record limit is reached. Uncertain attempts are kept; new journal admissions and Codex queue
dispatch stop at that limit. Claude and Copilot can still deliver hook context, but a full journal
cannot retain its transport report. Restore connection or file access where possible; seek support
for a full unresolved journal instead of deleting evidence that prevents duplicate execution.

A source/manual quickstart does not newly enable managed startup. Use `client service start`
explicitly on macOS, or `client run` in a Unix foreground session. Linux managed startup is not
implemented; Windows retains next-call delivery. Without a running resident, ordinary hooks/MCP
and lease expiry remain available, with no background dependency-refresh guarantee.

Inspect conversation capture with `agentscm change-capture status` in the checkout. To stop sharing
your own conversation bodies for that repository on every computer, use `agentscm change-capture
opt-out`; `opt-in` resumes only while the repository shares them. `set-mode --mode metadata-only`
changes one local capture binding. `unconfigure` disables that checkout's capture and retains its
spool. Turning sharing off stops future uploads; existing Hub copies need explicit deletion or
expiry. See [capture controls and recovery](change-capture.md).

`agentscm client remove` removes a checkout's AgentSCM wiring and revokes its owned agent token.
`agentscm uninstall` removes what setup recorded; Hub and capture data are retained by default.
A resident is stopped before connections, credentials or program files are retired. Its explicit
stopped preference and pending journal evidence remain in private config. A changed executable,
service file or unknown receipt blocks cleanup and retains the files needed for inspection.
A failed remote revocation leaves the state needed to retry and reports that the credential may
remain valid. Do not delete that recovery state to hide an incomplete uninstall.

## Earlier local dashboards

`agentscm open` no longer starts a detached presentation service. If an earlier installation left
one running, `agentscm open --stop` confirms its instance before stopping it. A mismatched or
unreachable instance is left unchanged with a recovery diagnostic. Existing private files under
`front-door/` are not automatically deleted. This cleanup does not stop an optional self-hosted Hub.

## Limits

- Auto-connect sends the checkout's forge host and `owner/name` to each opted-in Hub (up to four),
  including Hubs that do not have that repository.
- A stolen computer credential can mint repository agent tokens until revoked or expired from
  inactivity. Keep the config directory private and revoke a lost computer.
- Losing repository access is checked at token renewal; revoking the computer or disabling the
  account revokes its agent tokens immediately.
- AgentSCM serves no web interface; setup, controls and diagnostics use the CLI, MCP and HTTP APIs.
