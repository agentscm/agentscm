# Install

> **Alpha: not for production.** APIs, CLI, storage and schemas may change without migration. See the [README](../README.md) for status and the [changelog](../CHANGELOG.md) for known limits.

Choose a version from [Releases](https://github.com/counterbranch/agentscm/releases). In the examples
below, replace `<version>` with that version without its leading `v`. Container images are
versioned; alpha releases do not publish a `latest` image tag. For reproducible installs, keep the
selected version or image digest in your deployment configuration.

These docs describe the branch or tag you are viewing. A command present on `main` may not be in
a published binary yet. Use the documentation at the matching release tag, or
[build from source](../CONTRIBUTING.md#set-up) to try unreleased features.

## Choose local or hosted

The first question the setup guide asks is where AgentSCM should run:

- **Local, on this machine (recommended for the alpha).** The hub runs on your computer and your agents use it. Run `npx agentscm --local` to skip the question. Without a terminal (CI, or an agent's shell), add `--yes --github-login YOUR_LOGIN`.
- **Hosted, a shared hub on a server for a team.** If the team's hub already runs, `npx agentscm --hub-url https://hub.example.com` connects this machine to it and signs you in through your browser. Operating your own hosted hub is not supported in the alpha: it will likely break and is not for production. Quickstart does not set up a server; `npx agentscm --hosted` prints the [hosted guide](hosted.md) for you to work through with your coding agent.

`install.sh` passes the same flags: `curl -fsSL .../install.sh | sh -s -- --local`.

### If you are an agent installing AgentSCM for someone

1. Ask the person: "Do you want to run AgentSCM locally on this machine (recommended for the alpha), join your team's existing hosted hub, or operate a new hosted hub for a team (unsupported in the alpha)?"
2. Local: run `npx agentscm --local` in their terminal so they can answer its prompts. If you have no terminal, ask for their GitHub login and run `npx agentscm --local --yes --github-login LOGIN`.
3. Hosted: follow [hosted.md](hosted.md) step by step with them. If their team's hub already runs, ask for its URL and have them run `npx agentscm --hub-url URL` in their terminal, since it signs them in. Without a terminal, run it with `--yes` only after they have run `agentscm hub login --hub URL` themselves.

Three ways to get AgentSCM. The first two download the release binary, check it, keep it in `~/.agentscm/bin` (`%USERPROFILE%\.agentscm\bin` on Windows) and start the setup guide (`agentscm quickstart`).

The download and verification steps do not send product telemetry. After verification, the installers invoke the binary to record one installation indicator only when telemetry was already configured and permitted for this user. Pass `--no-telemetry`, set `AGENTSCM_TELEMETRY=off`, or set `DO_NOT_TRACK=1` before installing to suppress it. On first use, a build containing the staged capability follows the [setup choice and telemetry controls](telemetry.md). No event can be sent until an HTTPS endpoint and data-handling profile are configured.

## npx

```sh
npx agentscm            # setup guide; flags go to `agentscm quickstart`
npx agentscm uninstall  # any other subcommand is passed through as given
npx agentscm tray       # optional macOS status-bar companion
```

Needs Node 18 or later. The package has no dependencies and no install scripts. On Windows, run the same command in PowerShell:

```powershell
npx agentscm
```

See [Windows](#windows) for what the Windows build supports.

## Shell script

```sh
curl -fsSL https://github.com/counterbranch/agentscm/releases/download/v<version>/install.sh | sh
```

`install.sh` is POSIX shell only (Linux and macOS); on Windows use `npx agentscm`. The `install.sh` attached to each release is pinned to that release. Run from a clone, it needs `AGENTSCM_VERSION=<version>`. When piped, it reads prompts from `/dev/tty`.

## Docker

```sh
docker run --rm -it ghcr.io/counterbranch/agentscm:<version> --help
```

Use this as an explicit fallback on a supported Linux AMD64 or ARM64 Docker daemon. A macOS or Windows host needs a Linux VM, such as Docker Desktop. The release workflow builds both image architectures; use a release that publishes the matching architecture. An unsupported host architecture still needs a supported Docker daemon or an existing remote hub. The image's default command serves on all interfaces inside the container, so when you publish a port, bind it to loopback: `-p 127.0.0.1:7331:7331` (`7332` for the hub). See [deployment](deployment.md) for running the hub.

## What is verified

- Checksum, always. `npx agentscm` checks the binary against checksums shipped inside the npm package, which is published with npm provenance. Those checksums are written in the release workflow from the build artifacts and attested with them, never copied from the GitHub release, so a release asset replaced after publication does not match them. `install.sh` fetches `SHA256SUMS` from the same release as the binary, so without `gh` it catches corruption but does not prove authenticity.
- Build provenance, only when the GitHub CLI (`gh`) is installed and signed in (`gh auth status` succeeds). The binary must be attested by this repository's `release.yml` for the matching tag, or it is deleted and not run. If `gh` is missing or not signed in, the installer says why and that it checked the checksum only.
- To make a missing or signed-out `gh` an error, set `AGENTSCM_REQUIRE_PROVENANCE=1`.

Successful installer verification prints one summary line. `npx agentscm` prints its notice and
that line only when it first downloads a version, and never before `--help`; later runs start
AgentSCM directly. To see them on every run, with the verification command's details, set
`AGENTSCM_INSTALL_VERBOSE=1`. Verification failures always show
diagnostics and stop installation.

Checks run before anything is installed or linked.

## Where files go

On Linux and macOS, `~/.agentscm/bin/agentscm-<version>` is the binary, and `~/.agentscm/bin/agentscm` is a symlink to the latest installed one.

On Windows, `%USERPROFILE%\.agentscm\bin\agentscm-<version>.exe` is the binary, and `agentscm.exe` in the same directory is a copy of the latest installed one, not a symlink (symlinks need extra privileges on Windows). If `agentscm.exe` is running when you install a newer version, it cannot be replaced; the installer warns, keeps the old copy and runs the new versioned binary.

Quickstart prints the installed executable's full path, so it works before `agentscm`
is on PATH. On macOS or Linux, use `export PATH="$HOME/.agentscm/bin:$PATH"` in the
current shell, or add that line to your shell's startup file for future terminals.
You can also keep using `npx agentscm <subcommand>`.

Remove them with `agentscm uninstall`. It leaves `~/.config/agentscm` (or `$XDG_CONFIG_HOME/agentscm`)
with empty `.lock` files and a `connections.json` that lists no connections. The lock files stay on
purpose: AgentSCM commands coordinate through them, and removing one while another command runs
could let two commands change the same files at once. The folder can also keep `telemetry.json`
(your telemetry choice and an installation id), `hook-notes/` markers, an empty `tokens/` folder
and, after `agentscm open`, `front-door/` (run `agentscm open --stop` first). To remove everything,
delete the folder when no AgentSCM command is running.

The optional macOS tray is part of this same binary; no separate app install is needed. Run
`npx agentscm tray` to open it. On macOS, once an interactive quickstart has connected your
agents, it asks once: "Keep AgentSCM in the menu bar, and start it when you log in?" Yes (the
default) opens the tray and turns on Start at Login; No leaves both off and quickstart does not ask
again. With `--yes` or without a terminal it does not ask, and the tray stays off unless you add
`--tray`, which opens it without changing Start at Login. You can switch Start at Login in the tray
menu at any time. When quickstart connects agents on macOS it also writes a small
`~/Applications/AgentSCM.app` that runs `~/.agentscm/bin/agentscm tray`, so Spotlight, Launchpad
or the Dock can bring the menu bar app back after a restart. It is generated on your Mac, so it
carries no download quarantine. Running quickstart again leaves it as it is, and an app of that
name that AgentSCM did not write is never replaced. The alpha binary is not signed or notarized, and the
tray does not update independently; install a newer AgentSCM release through the same installer.
Each checkout menu groups its status, opening actions, a **Share agent conversations with Hub**
switch, **Agent Integrations**, **Connection Details**, **Recent Activity** and **Troubleshooting**. Connection Details shows the full checkout path and
connection values.

The **Share agent conversations with Hub** item is off by default. Turning it on takes two steps:
the first click only opens the status window with what sharing does and adds **Confirm: share
conversations with <hub>** to that checkout's menu, and only that item changes anything. The
confirmation names the Hub shown, and is refused if the checkout has since been connected to another
one. Turning it on for a connected
checkout sets capture up first if it never was (one step, documented defaults; see
[change capture](change-capture.md#one-step-setup-from-the-menu-bar-and-the-local-page)) and
switches that checkout to `shared-conversation` mode: new conversations are redacted on this
machine on a best-effort basis and uploaded to the Hub, kept for the number of days the Hub reports (30 unless its operator changed it), where only you (the person who shared them) and Hub
admins can read them. The status window says this before it turns on. The same switch is under
**Settings → Conversation sharing** on the Hub page opened from this computer. Turning it off sets `metadata-only` (not a previous `local-conversation`); conversations stay on this machine,
and copies already uploaded remain until they expire or you delete them. Nothing local is deleted.
It is the same change as `agentscm change-capture set-mode`; see
[sharing conversations](change-capture.md#sharing-conversations-with-the-hub).

Copied reconnect, repository-guidance and Terminal setup commands use the running executable's
full path, so they work before `agentscm` is on your shell's `PATH`.

The Start at Login switch shows **On** or **Off**, and **Updating…** while a change is saved.
**Open Hub** uses the saved sign-in for that exact Hub to create a read-only browser token valid
for 30 minutes. It opens the Hub UI with that token in the URL fragment; the UI removes the
fragment immediately. An existing browser sign-in is preserved, so sign out there first when
switching repositories. If the saved sign-in is missing or expired, the tray shows an error
with a sign-in instruction. Under **Troubleshooting → Hub Sign-in**, copy the GitHub or GitLab
login command for that exact Hub and run it in Terminal; then retry Open Hub. A tray built from
source without `--features ui` has no local Hub page, so Open Hub opens your default Hub's `/ui/`
page instead and says so in the menu.

**Forget This Checkout…** in a connected checkout's menu runs the same removal as
`agentscm client remove --root ROOT` after you choose its confirm item: the agents' AgentSCM files,
the connection and a Hub token setup made (which is revoked) are removed; the folder is not. If the
Hub cannot revoke that token, the connection is kept and the result says so. Under **Not
connected**, **Forget This Checkout** only hides the row until an agent runs there again.

For a connected checkout, choose **Agent Integrations** in its menu to configure Claude
Code, Codex, Cursor, Copilot or Gemini. This installs AgentSCM's existing hooks, MCP connection
and coordination skill; the native client's own trust prompts still apply. **Reapply** reruns
that setup without signing in again.
When the user-scope Claude Code or Codex plugin (or another client-connection hook) runs in a Git
checkout that has no connection, the hook tells a running tray, and the checkout appears under
**Not connected** with its repository, agent and last-seen time. A linked worktree is listed as its
main checkout. Only checkouts whose `origin` is one GitHub or GitLab repository are listed; the
report stays on this machine and is sent only while a tray is running. Once a tray has received
it, the hook repeats it at most every five minutes per agent and checkout, so a tray opened during
a session lists the checkout at the agent's next hook. The tray forgets it after an hour, after
sleep, or on quit, keeping at most eight. **Connect with**
an agent runs the same steps as `agentscm client setup --agent AGENT --hub HUB` for the main
checkout without starting a new sign-in. It uses the Hub another existing checkout of the same
repository is connected to, when that checkout's folder still exists and its Hub sign-in has not
expired; otherwise the Hub of your most recent saved sign-in. The result window names the Hub. The row then becomes a normal connected checkout; if that connection
is removed later, the checkout is listed again only once an agent runs there again. When there is no
saved sign-in, or it has expired, **Copy Terminal Setup** copies `agentscm quickstart` for that
checkout. Connect a checkout to a different Hub through quickstart.
Setup opens a status window with progress, the result and the client's next steps. It does not
install the coding client itself. Open the configured checkout in that client, review its own
approvals, and start a fresh session before checking activity in the tray.
If macOS starts the tray with a minimal `PATH`, an installed Claude Code or Codex CLI may be
unavailable there. Use **Copy Terminal Setup** for that agent. The tray only copies this
command: it runs normal `client setup` only after you paste and run it in Terminal. That guided
command may contact the Hub, request forge sign-in, and create or replace a managed coordination
token; the native client's own trust prompts still apply.
Uninstall removes the owned tray login setting and `~/Applications/AgentSCM.app` (only while it is
exactly what quickstart wrote) before removing its program. A running tray can
be closed with **Quit AgentSCM Tray**.

For an optional customer-operated Slack app, follow [Slack setup](slack.md). The same preparation commands support an installation agent, the official Slack CLI and manual app settings. A prepared private bundle can be supplied to `quickstart --hub local --slack-config-file ABSOLUTE_PATH`; installing the AgentSCM binary alone does not connect Slack.

## Coding agents in every project

`agentscm client setup --agent AGENT` (and quickstart) installs AgentSCM for your user account by
default, so the agent's start-up check runs in every project, including a fresh clone, and only
contacts a Hub in a checkout connected to one. Quickstart says so and asks first: "Install
AgentSCM for ... in all your projects?" With `--yes` it answers yes after printing the same
notice. It does not ask again for an agent that is already installed this way.

Setup never writes to the repository: hooks, MCP entries, tokens and the Hub URL live in your user
account and `~/.config/agentscm`, so `git status` stays clean, which matters on a public
repository. Only `agentscm init-repo` (or `quickstart --init`) writes committable instruction
files, and they name no Hub.

| Agent | Hooks | MCP server | Coordination skill |
|---|---|---|---|
| Claude Code | user-scope plugin | in the plugin | in the plugin |
| Codex | user-scope plugin | `$CODEX_HOME/config.toml` (`~/.codex`) | in the plugin |
| Cursor | `~/.cursor/hooks.json` | `~/.cursor/mcp.json` | `~/.cursor/skills/coordinate-edits/` |
| Gemini CLI | `~/.gemini/settings.json` | `~/.gemini/settings.json` | `~/.gemini/skills/coordinate-edits/` |
| GitHub Copilot | `$COPILOT_HOME/hooks/agentscm-client.json` (`~/.copilot`) | `$COPILOT_HOME/mcp-config.json` | `$COPILOT_HOME/skills/coordinate-edits/` |

In a shared file, setup changes only AgentSCM's own entries: the `agentscm` MCP server, hooks
that run `agentscm hook AGENT --client-connection`, and the block between
`# agentscm:client-begin` and `# agentscm:client-end` in Codex's `config.toml`. Everything else is
kept, with the same values, though a JSON file is rewritten with its keys in sorted order. Before
writing each file it prints the diff of AgentSCM's own entries (not the rest, which can hold other
servers' credentials), and it refuses a file that already has an `agentscm` entry it did not
write. A file that is a link, into a dotfiles repository say, is written through, not replaced.

The user-scope MCP server finds the checkout from the directory the agent starts it in (Cursor:
from `${workspaceFolder}`). In a checkout that is not connected it offers no tools and answers
without contacting anything. When the checkout gets connected while the agent runs, the server
tells the agent its tools changed at the agent's next MCP request; an agent that ignores that
shows the tools in its next session.

The agents' own approvals still apply: Codex asks you to trust new or changed hooks in `/hooks`,
Gemini CLI skips settings hooks and MCP servers in a folder you have not trusted, and Cursor asks
to enable a new MCP server.

`--checkout-only` writes the hooks, MCP entry and skill into the checkout instead, as earlier
versions did, so they run only there; quickstart does this when you answer No (Claude Code and
Codex are then not set up, since their hooks come only from a user-scope plugin). Claude Code is
always set up through its plugin, and setting it up takes out the `.mcp.json` entry earlier
versions wrote into each connected checkout. The two cannot
be mixed for one agent: setting an agent up for all projects takes AgentSCM's files for that agent
out of every connected checkout, and a checkout-only setup is refused while the agent is installed
for all projects. `agentscm client remove` and `agentscm uninstall` take the user-scope entries out
once no other connected checkout uses that agent.

Other agents (Grok Build, Windsurf, Cline, Amp, opencode, Kiro, Aider) have no native AgentSCM
setup; [other agents](agents.md#other-agents) says what each supports and how to wire it by hand.

## Platforms

Linux and macOS, x86_64 and arm64. Linux binaries are static (musl). Windows, x86_64 only (see [Windows](#windows)); Windows on arm64 may run the x64 binary under emulation, which is untested. For other hosts, check whether a local Linux AMD64 or ARM64 Docker daemon is supported. Docker availability and emulation depend on the host; they are not a universal fallback.

The CLI platform and the managed hub runtime are separate. Quickstart defaults to a native user service on Apple Silicon macOS. Linux, Intel macOS and Windows currently use an explicitly selected Docker hub or an existing hub; installing a native CLI does not install a native managed service on those hosts. See the [setup guide](setup-guide.md).

## Windows

The Windows binary (`x86_64-pc-windows-msvc`) is statically linked to the C runtime, so it needs no Visual C++ redistributable. Install it with `npx agentscm` in PowerShell, or download it by hand as described below.

It supports the setup guide (`agentscm quickstart` and `agentscm uninstall`, without the collision check, which needs the Unix process runner, and without setting up coding agents, since `agentscm client setup` is Linux and macOS only), the command hook that Cursor and Copilot call (`agentscm hook`), `agentscm init-repo`, the hub client and the hub (`agentscm hub serve`, `hub login`, `hub token`), and `agentscm intent`. The native workspace and ledger, `agentscm collisions`, the Git snapshot, export and reconcile bridge, command checks (which need Docker), the team service and the recovery commands are Linux and macOS only.

This is an alpha port. It is built and tested in CI on Windows, but few people have run it by hand. `agentscm uninstall` cannot delete the `agentscm.exe` that is running it; it says so and leaves that file for you to delete.

## Downloaded in a browser

If you download a binary from the release page instead of using `npx agentscm` or `install.sh`, check it first (see below). The binaries are not code-signed or notarized in the alpha, so the operating system warns about them.

- macOS: Gatekeeper blocks a quarantined, unsigned binary. After checking the checksum and attestation, remove the quarantine mark: `xattr -d com.apple.quarantine agentscm`.
- Windows: SmartScreen may show "Windows protected your PC". Choose "More info", then "Run anyway", only after you have checked the checksum and attestation.

`npx agentscm` and `install.sh` verify the checksum, and the provenance when `gh` is installed and signed in, before running anything. A file written by Node or `curl` does not get the mark a browser adds (quarantine on macOS, mark-of-the-web on Windows), so a browser-less install normally does not hit these prompts. Antivirus software can still scan or block a new unsigned executable.

## Verify by hand

On Linux and macOS:

```sh
cd "$(mktemp -d)"
base=https://github.com/counterbranch/agentscm/releases/download/v<version>
curl -fsSLO "$base/SHA256SUMS" -O "$base/agentscm-<target>"
shasum -a 256 -c --ignore-missing SHA256SUMS   # on Linux: sha256sum -c --ignore-missing
gh attestation verify agentscm-<target> --repo counterbranch/agentscm \
  --signer-workflow counterbranch/agentscm/.github/workflows/release.yml \
  --source-ref refs/tags/v<version> --deny-self-hosted-runners
```

On Windows (PowerShell):

```powershell
$base = "https://github.com/counterbranch/agentscm/releases/download/v<version>"
Invoke-WebRequest "$base/SHA256SUMS" -OutFile SHA256SUMS
Invoke-WebRequest "$base/agentscm-x86_64-pc-windows-msvc.exe" -OutFile agentscm-x86_64-pc-windows-msvc.exe
Get-FileHash -Algorithm SHA256 agentscm-x86_64-pc-windows-msvc.exe   # compare with its line in SHA256SUMS
gh attestation verify agentscm-x86_64-pc-windows-msvc.exe --repo counterbranch/agentscm `
  --signer-workflow counterbranch/agentscm/.github/workflows/release.yml `
  --source-ref refs/tags/v<version> --deny-self-hosted-runners
```

The three extra flags are what `npx agentscm` and `install.sh` pass: without them, an attestation from any workflow or branch of the repository would pass.

`<target>` is one of `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`. The Windows asset name ends in `.exe`.

## Publishing to npm (maintainers)

The `npm` job in `release.yml` runs after the GitHub release, in the `npm` environment. Configure required reviewers on that environment. It publishes with npm trusted publishing (OIDC): npm trusts this workflow and environment, so no token is stored. The job copies the attested build's `SHA256SUMS` (from the workflow artifact, not the release), `LICENSE` and `NOTICE` into the package and publishes with `--ignore-scripts` under the `latest` dist-tag (every release is a prerelease, so `npx agentscm` needs one; switch to `next` for prereleases after the first stable release).

A tag containing `-dry` skips the publish and succeeds; for any other tag the job fails if npm does not accept the publish, so a green release always has the package published. A dry-run tag must still equal the `Cargo.toml` version, so use a `-dry` version such as `0.2.0-dry1`. `packages/npm/package.json` `version` must equal the tag version. `NOTICE` must exist at the repository root, or the release stops before anything is pushed.
