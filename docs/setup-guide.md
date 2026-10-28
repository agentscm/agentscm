---
title: "Guided setup: `agentscm quickstart`"
---

# Guided setup: `agentscm quickstart`

> **Alpha: not for production.** The CLI and the files it writes may change without migration.

`agentscm quickstart` (and `npx agentscm`, which runs it) sets up this computer, once, for all your projects and every coding agent on it. It does not need a Git repository and never writes to one. It asks no question while it sets up agents. `agentscm uninstall` undoes the recorded setup. Hub traffic goes only to the Hub you use. Product usage telemetry is a separate staged capability and is effectively inactive until an HTTPS endpoint and data-handling profile are configured. The full flow is Unix only (macOS and Linux); on [Windows](install.md#windows) it runs without agent setup.

```sh
npx agentscm                                   # run a Hub on this computer
npx agentscm --hub-url https://hub.example.com # join a Hub that already runs
```

## Usage telemetry choice

When telemetry is configured and eligible, quickstart shows the disclosure and offers an opt-out before the first event can be sent. Provisioning the endpoint and profile alone does not record that notice or enable sending. A `consent-required` profile stays inactive until you affirmatively run `agentscm telemetry on` or make the equivalent interactive choice; `--yes` never supplies that consent. A `permitted-default` profile may enable collection after quickstart shows the notice and completes its default-on flow, subject to the operator's legal determination. An `unknown` profile stays inactive.

Your choice is stored in the per-user telemetry configuration. `agentscm telemetry off` persists the opt-out and deletes queued events and telemetry identifiers without sending an opt-out event. `DO_NOT_TRACK=1`, `AGENTSCM_TELEMETRY=off` and the global `--no-telemetry` flag provide stronger overrides. See [usage telemetry](telemetry.md) for the fields, status and inspection commands.

## What it does

1. **Looks around** and prints what it found: the coding agents on this computer (Claude Code, Codex, Cursor, GitHub Copilot, Gemini CLI), and whether a Hub is already configured and answering. Nothing is changed.
2. **Gets a Hub.**
   - **Run one here (no flag).** The community edition is the whole platform, so your computer runs its own Hub. On Apple Silicon macOS with `launchctl` it runs as a per-user LaunchAgent (plist in `~/Library/LaunchAgents`, data in `~/.agentscm/hub-native/<project>`), starts at login and restarts after a crash. Other hosts use Docker (`--hub-runtime docker`, a running Docker Engine with Compose) or join a Hub. It binds to `127.0.0.1:7332`. On first run it opens a one-time page so you create a GitHub App for your Hub: click **Create**, then **Install** and choose your repositories. It then asks for your GitHub login (offered from `gh` or `git config github.user`), makes that account the Hub's administrator and restarts the Hub with sign-in enabled. GitHub repositories only today.
   - **Join one (`--hub-url URL`).** Connects this computer to a Hub that already runs. Deploying a Hub for a team is your job: see [hosted setup](hosted.md) and ask your coding agent to work through it with you. To share a Hub from your own computer, see [Tailscale](install.md#share-your-local-hub-over-tailscale).
3. **Signs you in** through your browser (device code if the Hub offers it). When an agent runs the command, it prints the sign-in link and waits for you to finish. With `--agent-token-file` it skips sign-in and uses that token.
4. **Sets up every agent it found**, at user scope, for all your projects. For each agent it runs the same code as [`agentscm client setup`](team-member-onboarding.md): it installs the hooks, the MCP server and the coordination skill (see [coding agents in every project](install.md#coding-agents-in-every-project)), and for Claude Code allows the AgentSCM tools in `~/.claude/settings.json`. `--only AGENT` limits it to the agents you name. A checkout is connected to the Hub automatically when an agent first starts in it, so a fresh clone or a new worktree needs no setup. If one agent fails, the others still finish and the output says which failed, in one line each.
5. **Summary**, and a record in `~/.config/agentscm/quickstart.json` (0600) of what it changed, which `uninstall` reads. Running quickstart again detects what exists and keeps it. A run that writes nothing says **No changes made.** It ends with what to do next: fully quit and reopen your agents, then ask one in a repository, "Get started with AgentSCM". Codex also needs you to run `/hooks` once and trust the AgentSCM hooks. It prints the undo command and, on macOS, how to open the menu bar app (`agentscm tray`).

On macOS the tray is optional. Pass `--tray` to open it after successful setup. Quickstart never enables Start at Login; that setting is in the tray menu and is off by default.

To preview, run `agentscm quickstart --dry-run`: it prints what it would do and writes nothing.

If the Hub isn't answering, quickstart reports the failure. Explicitly requested Hub setup that remains incomplete exits non-zero.


Quickstart and uninstall share one lock for the user configuration directory. An overlapping run stops with a retry message before changing installed state. The private `quickstart.lock` file remains after uninstall so later runs use the same lock.

## Optional change-record capture

Quickstart's coordination hooks do not enable the experimental change-capture path. It needs a
checkout-bound client connection: on Unix, quickstart's agent step and `agentscm client setup`
save one, or save a write-capable connection with `agentscm client connect`. Then an operator may
configure Claude Code capture once:

```sh
agentscm change-capture configure \
  --root /absolute/repository \
  --spool /absolute/private/spool \
  --worktree-id checkout_example \
  --setup-digest 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --action-rule Bash=execution:partial:process_exit_code
```

Choose the absolute spool outside public repository source. `metadata-only` is the default; do not
choose `local-conversation` until its local retention and access policy are deliberate. The command
adds only marked capture hooks and preserves unrelated hooks. Disable capture with
`agentscm change-capture unconfigure --root /absolute/repository`; this removes AgentSCM's marked
hooks but retains the checkout's private spool safety identity.
Use `agentscm change-capture status --root /absolute/repository` and the read-only `inspect` or
non-resumable `export` commands to diagnose retained evidence. Reconfiguring that checkout to a
different spool identity is unavailable in this version. `agentscm uninstall` does
not own this separate configuration. See [local change capture](change-capture.md) and the [implementation status](implementation/agentscm-build-status.md)
for the tested local slice and remaining host, platform and release limits.

## Other flags

The flags `--hub-url`, `--yes`, `--only`, `--dry-run` and `--json` are the ones `quickstart --help` shows. The rest still parse and are for scripts and special cases. Without a terminal quickstart refuses unless you pass `--yes`, which takes the recommended setup answers. Telemetry still requires an earlier eligible choice or explicit enablement.

| Flag | Meaning |
| --- | --- |
| `--hub local\|existing\|skip\|URL` | How to get live warnings (with `--yes` and no flag, an already configured hub is kept, otherwise skip). A URL means `--hub existing --hub-url URL`, so `--hub URL` means the same as in the `hub` and `client` commands. |
| `--hub-runtime native\|docker` | Local runtime. Native is the default on supported Apple Silicon macOS hosts; Docker must be selected explicitly elsewhere with `--yes`. |
| `--hub-port PORT`, `--hub-project NAME` | Isolated local instance; defaults to port 7332 and a home-specific name. Resume reuses the record. |
| `--hub-url URL` | The hub for `--hub existing`; still accepted. |
| `--agent-token-file PATH` | Use this agent token instead of signing in; it is checked against the hub and used where it is. Needed for agent setup without a terminal. |
| `--agent claude\|codex\|cursor\|copilot\|gemini` | Set up this coding agent (repeatable). Default: Claude Code if the `claude` CLI is on `PATH`. |
| `--no-agents` | Do not set up any coding agent. |
| `--slack-config-file ABSOLUTE_PATH` | Install a prepared private Slack bundle into a managed local Hub. See [Slack setup](slack.md). Requires `--hub local`; `--yes` alone never enables it. |
| `--provider github\|gitlab` | Forge to sign in with (default: from `origin`). |
| `--github-login LOGIN` | Your GitHub login for the local hub's administrator. With `--yes` it is required unless `gh api user` works; an unverified `git config github.user` is only offered in the interactive prompt. |
| `--init` | Also run `agentscm init-repo` for this repository. Without it, quickstart writes nothing to the repository. |
| `--tray` | On macOS, open the optional status-bar companion after setup succeeds. `--yes` does not open it unless this flag is present. |
| `--no-collisions` | Skip the collision check. |

With `--yes` or without a terminal quickstart never starts a browser or device sign-in. If there is no saved sign-in, or the hub no longer accepts it (it expired, or the hub restarted), quickstart says so at once and sets up no agent: run it at a terminal, or pass `--agent-token-file`. A new local hub still requires GitHub App setup in a browser; `--yes` prints the link and returns exit status **3** without waiting. Finish that step, then resume with the printed command, which includes the runtime, port and project. Exit status **0** means the requested setup completed; other failures exit non-zero.

Non-interactive `--yes` leaves a fresh telemetry notice and choice pending under every profile. It does not enable telemetry; use an earlier eligible choice or explicit `agentscm telemetry on`.

Example: `agentscm quickstart --yes --hub existing --hub-url https://hub.example.com --agent-token-file ~/hub.token --agent claude`.

## Undo: `agentscm uninstall`

It reads `quickstart.json`, shows exactly what it will remove, and asks (or `--yes`):

- If a step fails or is skipped (a file you changed, Docker not running), `quickstart.json` keeps only the records of what is left, uninstall prints the exact command to finish by hand (for example `docker compose -p agentscm-hub -f ~/.agentscm/hub/compose.yaml down`), and it exits non-zero. The record is deleted only when everything was undone.
- For every checkout quickstart connected, it runs [`agentscm client remove`](team-member-onboarding.md): the agents' entries come out of the checkout's files, the Claude Code or Codex plugin is uninstalled when no other connected checkout uses it, the managed token is revoked on the hub (the token revokes itself, so no sign-in is needed), and then the connection and the token file are removed. If the hub can't be reached or refuses, uninstall says so and exits non-zero, and the connection, its token file and the record stay so that running `uninstall` (or `agentscm client remove`) again retries. A token file you supplied is never deleted or revoked. A connection quickstart found and kept, rather than made, is left alone.
- The sign-in session quickstart created is ended (the hub is told, best effort) and its file removed.
- The program files directly in `~/.agentscm/bin` named `agentscm` or `agentscm-*` are removed, even ones quickstart didn't create; the confirmation list names them first. Nothing else there, and no directories, are touched.
- The local hub is stopped using its recorded runtime and identity. A native LaunchAgent is removed before its executable. Its data (or the Docker volume and setup files) are kept unless you choose deletion before the final confirmation, or pass `--remove-hub-data`. Changed service files are retained with their record for manual review. The GitHub App and installations remain on GitHub; delete them in GitHub Settings if no longer needed. Keeping hub data lets reinstall reuse the app. Native installs retain a private ownership receipt alongside that data. Uninstall refuses symlinked managed configuration roots; restore the recorded paths before retrying.
- Finally `quickstart.json` is deleted (see above). If it doesn't exist, nothing was installed by quickstart and uninstall says so.
- In the agents' files only AgentSCM's own entries are removed; anything you added there stays.

## Limits

- The release workflow builds and tests native `linux/amd64` and `linux/arm64` children before assembling the versioned image index. Older published images may contain only AMD64. Quickstart does not silently enable emulation: an operator can explicitly choose `DOCKER_DEFAULT_PLATFORM=linux/amd64`, subject to the Docker runtime supporting it. Emulation is slower and is not universal. Remote Docker contexts are refused because their loopback port would not reach this machine.
- The Docker fallback needs Docker Engine, Compose and the published image `ghcr.io/agentscm/agentscm:<version>` (the version of this binary, without a `v`). Images are published with each release from 0.1.0-alpha.1. To use another image, set `AGENTSCM_HUB_IMAGE`; quickstart saves it in the hub's `.env`. If the image can't be pulled, quickstart says so and offers an existing hub or skipping.

Existing npm packages and installed binaries retain their original Docker image defaults. During
the organization-rename window, a successful old-package download/provenance check does not
prove that its binary can create or recreate a Docker Hub. After the operator verifies the moved
image's version and digest, use the existing override with that exact reference, for example
`AGENTSCM_HUB_IMAGE='ghcr.io/agentscm/agentscm:<version>@sha256:<verified-digest>'`
when running `agentscm quickstart --hub local --hub-runtime docker`. Quickstart prefers this
explicit override and saves it in `~/.agentscm/hub/.env`; inspect its preview and retained `.env`
so a previous old-namespace override does not survive unnoticed. Keep the same project, volumes
and Hub data. Fresh Docker setup after the rename remains unqualified until this override or a
new release is verified on the intended host.

- The local hub's first-run setup is GitHub only today. For a GitLab repository connect to an existing hub.
- Platform selection and generated Docker files have unit coverage; fake-Docker integration tests verify failed pulls write no setup files and uninstall keeps data by default. Matching-image acquisition, runtime lifecycle, GitHub App setup and sign-in require live qualification before release. Emulation needs its own runtime-specific qualification.
- The agent setup step reuses `client setup`, which has unit and stub-hub coverage but no live qualification through quickstart. Other hosts and clients need their own live qualification. The hooks are advisory: a hub problem is reported as "coordination unknown" and never blocks the agent.
- Sign-in through a browser is covered by the existing [hub](hub.md) sign-in tests, not by a quickstart end-to-end test.
