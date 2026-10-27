# Open the Hub from this computer

Your team's Hub shows shared coordination: who owns which task, overlaps and handovers. Your
committed files and saved agent conversations stay on your computer. To see both in one page, open
the Hub from your computer:

```text
agentscm open
```

Run it in any connected checkout (or choose **Open Hub** in the AgentSCM menu bar app on macOS).
Your browser opens the Hub on **Repository explorer**, already connected: the page names your
computer and repository, shows your files and saved conversations, and shows your team's work.
There is no token to paste and no address to enter.

`agentscm open` needs two things it will ask for if missing: the folder must be connected to a Hub
(`npx agentscm --hub-url <url>` connects it and signs you in), and this computer must be signed in to that Hub
(`agentscm hub login --hub <url>`).

## Sign in once per computer

`agentscm hub login` (or **Sign In with GitHub…** in the menu bar app) signs this computer in to
the Hub, not only you. On a desktop it opens your browser: authorize once, and the browser lands
on the Hub page saying who signed in and as which computer. The terminal says it is waiting, every
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
  computer replaces its credential; the Hub never matches computers by name.
- **Agents connect themselves**, if you say so. AgentSCM's agent plugin runs a quick check
  whenever an agent starts. The sign-in asks once at a terminal: "Turn on
  auto-connect for <hub>? [Y/n]" (`--no-auto-connect` answers no). A sign-in from the menu bar
  app, or one made before this question existed, is asked in the menu bar app or at the next
  interactive `agentscm hub` or `agentscm client` command. Change it with
  `agentscm hub auto-connect on|off [--hub URL]`. With it on, when a coding agent starts a session
  in a checkout that has no connection, its hook connects the checkout to the first such Hub (the
  default Hub first, at most four) that has the repository and where you have the role, and the
  agent's context starts with `AgentSCM: coordinating on owner/name.` It writes only AgentSCM's
  own files under your config directory, never a file in the checkout, and runs the same checks as
  `client connect`. It waits at most 2.5 seconds; a slower attempt says nothing and finishes for
  the next session. In a repository no such Hub has, the check stays silent, and that Hub is not
  asked about it again for an hour. A Hub that sees your origin is a fork of a repository it has
  says so, and names the repository to use as `origin`. Any other attempt is retried after five
  minutes.
- **Tokens renew.** An agent token minted through this computer lasts 24 hours. When less than 12
  hours are left, the next hook call renews it in the background: the Hub re-checks your access, a
  new token is saved, and the old one is revoked. A running MCP server switches to the new token.
  A laptop that slept for days renews at the first hook call after waking.
- **Access is re-checked every time.** Each mint and renewal checks that you are still enabled
  and allowed on the Hub and still have the repository role (on a public repository Triage or
  more, on a private one read; on GitLab Reporter for private, Developer otherwise). The Hub asks
  the forge with the repository connection's own credential, never a stored token of yours. When
  something refuses, the agent's context, the menu bar app (**Needs attention**), `agentscm open`
  and the CLI show the Hub's own sentence, such as
  `alice has Read on public repository acme/widgets; ask a repository owner for Triage or higher`,
  and the current token keeps working until it ends or is revoked. When the Hub or the forge does
  not answer, nothing is marked as needing attention; the next hook call tries again.

`agentscm hub whoami` shows, for each saved Hub, who this computer is signed in as, the
computer's name, whether auto-connect is on and the checkouts connected to it; the menu bar app
shows the same at the top of its menu. `agentscm client doctor` checks a checkout's connection
through this computer's credential, the way the next renewal would, so it reports a verified
connection even when no agent is running.

Your computers: `agentscm hub devices list` shows each one with its name, last use and the
repositories connected through it, live ones first; `agentscm hub devices rename NAME` renames
this one; `agentscm hub devices revoke dev_…` signs one out (from any of your computers). The
Hub page opened with `agentscm open` lists and revokes them too. Revoking a computer revokes every
agent token minted through it, so its agents stop at their next call, and ends the session that
signed it in. `agentscm hub logout` signs this computer out the same way and warns if the Hub could
not be told. Hub administrators can revoke any computer. See
[computer credentials](hub.md#computer-credentials).

Sign-ins made before computers had credentials keep working until their session ends; while that
session lives, the next connect, renewal, Open Hub or `agentscm open` enrolls this computer without
asking. Existing 30-day tokens keep working and are renewed into 24-hour ones once the computer
has a credential. A sign-in that has already expired needs one more sign-in.

## What happens

- The first `agentscm open` starts AgentSCM's local Hub page in the background, on
  `http://127.0.0.1:7340` (or another free port). It keeps running after the terminal closes;
  `agentscm open --stop` stops it. The macOS menu bar app starts the same page when it starts
  (if this computer has a connected checkout), so its Open Hub opens at once, and stops the page
  again when you quit it, if it was the one that started it.
- The page is the same Hub page your team's Hub serves, served from your computer. It reads your
  files and saved conversations directly. It fetches team data from each repository's own Hub
  with a read-only token for that one repository, minted from this computer's Hub sign-in. That
  token stays in the local service; the browser never receives it.
- One browser connection covers every repository connected on this computer. When there is more
  than one, a switcher at the top of the page changes repository. Each repository uses its own Hub
  and its own read-only access.
- Saved conversations appear only for repositories where change capture was already set up.
  Opening the page never starts capture and never uploads conversations.
- **Settings → Conversation sharing** lists the checkouts on this computer with a switch for
  sharing agent conversations with the Hub. Apart from disconnecting this browser
  (`DELETE /v1/local/session`), it is the only setting the page changes on this computer; see below.

## Conversation sharing from the page

Turning a switch on sets capture up for that checkout if it never was (same defaults as the menu
bar app; see [sharing conversations](change-capture.md#one-step-setup-from-the-menu-bar-and-the-local-page))
and switches it to `shared-conversation`. The page first shows what that does: conversations are
redacted on this computer (best effort), uploaded to that checkout's Hub, kept for the number of days that Hub reports (or, if it did not
say, for as long as its operator sets, 30 days unless changed) and readable by you and Hub admins. Only after you confirm does it send the request, with an
explicit `confirm_sharing`; the service refuses a request without it and changes nothing. Turning
it off needs no confirmation, switches the checkout to `metadata-only` and stops future uploads.
The request is a `POST` to `/v1/local/capture`, which needs the same live browser connection, loopback
Host and own-origin check as every other route here, sets no cross-origin headers, accepts at most
1 KiB, and names the checkout by the `id` the list gives it, never by a path. The id is derived
from the service's random instance and the checkout's path and mapped back only by the service, so
it is opaque to the browser and valid for one service run (ids change after a restart). It is a
name, not a secret: the unauthenticated identity probe exposes the instance, so anyone who also
knew a path could compute it; the capture routes themselves need a live browser connection. If a
change names a checkout that is no longer listed, the page re-reads the list and asks you to review
and try again. Two
checkouts of one repository and Hub are listed and controlled separately. The request also carries
the repository and Hub the page showed; if the checkout is connected to anything else by then, the
service refuses to turn sharing on and changes nothing.
A page opened from a Hub elsewhere has no such control and points to the menu bar app and `agentscm open`.

## How the connection is protected

- `agentscm open` asks the running service for a connection code using a secret stored in an
  owner-only file (`$XDG_CONFIG_HOME/agentscm/front-door/service.json`, default under
  `~/.config`). Before sending that secret it checks which process is listening on the port.
- The code reaches the browser through an owner-only file in that directory, not on a command
  line, where other users of the computer could list it. The browser URL carries the code only in
  the fragment, which browsers do not send to servers. The page removes it from the address bar
  and history, then exchanges it once, within two minutes, for a browser connection. The service
  accepts the exchange only from its own page.
- The connection lives only in the running service's memory. Stopping or restarting the service
  ends every browser connection, so a connection taken from a browser cannot be used against a
  later service or another program on the same port. Choose Open Hub or run `agentscm open`
  again to reconnect.
- The service answers only on loopback, refuses requests that name another host (DNS
  rebinding) and refuses pages from any other origin. It forwards only the Hub reads the page
  makes, in their plain form, never Hub sign-in, workspace or token areas. Requests to the service
  itself bypass any configured HTTP proxy.
- Hub tokens the service mints last 30 minutes at most, minted with this computer's
  credential (or, for a sign-in from before computers had one, its session). It revokes them
  when it is stopped normally (`agentscm open --stop`, quitting, or Ctrl-C); if it is killed or
  crashes, they simply expire. After you sign this computer out of a Hub (`agentscm hub logout`),
  the token minted from that sign-in is revoked the next time the page reads that Hub, and
  otherwise expires.
- For a self-hosted forge, the service mints a token only for the forge the Hub reports for the
  checkout's own connection, as the menu bar app checks it.

## Lifetime and disconnecting

- A connection lasts while the local service runs, for up to seven days. To renew it, choose Open
  Hub or run `agentscm open` again.
- If a repository stops being connected on this computer (its checkout moved, or its connection
  was removed), the page says so and offers the others; the browser stays connected.
- **Settings → This computer → Disconnect this browser** ends the connection for this browser.
- `agentscm open --stop` ends every connection.

## Without the menu bar app

Everything above works from a terminal on macOS and Linux with `agentscm open`. The tray only adds
a menu item for it.

The hosted team Hub keeps working as before for shared work, signed in with a read-only token. Its
This computer pages explain that your files and conversations open with `agentscm open`. The
manual setup in [Repository explorer](repository-explorer.md#advanced-connect-without-agentscm-open)
remains for a Hub page served elsewhere.

## Limits

- Auto-connect sends the checkout's forge host and `owner/name` to every Hub with auto-connect on
  (up to four), including Hubs that do not have that repository.
- A stolen credential file can mint agent tokens for your repositories until the computer is
  revoked or unused for 90 days. It cannot open the Hub's web pages, act as an administrator or
  read the forge.
- Losing a repository role takes effect at the next renewal, usually within 12 hours and at most
  24; revoking the computer or disabling the account takes effect at once.
- An older AgentSCM binary that rewrites `hub-sessions.json` drops this computer's credential from
  it (the Hub keeps it until revoked or idle); sign in again with the current one.
- `agentscm open` gives the browser an owner-only local file that forwards it to the page. A
  browser that cannot read files in your config directory (for example some sandboxed Linux
  browser packages), or a computer where `.html` files open in an editor, does not open the page;
  `agentscm open` prints the file to open by hand.
- The page runs on this computer only. Over SSH, `agentscm open` has no local browser to hand the
  connection to; use the team's Hub there, with the manual setup for files.
- Templates and Timeline read a native workspace chosen with `agentscm serve --timeline-workspace`;
  this page does not serve them and shows their labelled preview data.
- Capture records a session in the repository where the agent started, so a session that edited
  several repositories appears under the first.
- Hub areas that use their own browser sign-in (Community workspace) open on the team's Hub.
