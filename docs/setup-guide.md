# Guided setup: `agentscm quickstart`

> **Alpha: not for production.** The CLI and the files it writes may change without migration.

`agentscm quickstart` (and `npx agentscm`, which runs it) sets AgentSCM up for the repository you are in. You choose from short menus; it never asks you to edit a file. Before each write it says what it will write and asks once. `agentscm uninstall` undoes the recorded local setup. Hub traffic goes only to the hub you choose. Product usage telemetry is a separate staged capability and is effectively inactive until an HTTPS endpoint and data-handling profile are configured. Unix only (macOS and Linux).

## Usage telemetry choice

When telemetry is configured and eligible, quickstart shows the disclosure and offers an opt-out before the first event can be sent. Provisioning the endpoint and profile alone does not record that notice or enable sending. A `consent-required` profile stays inactive until you affirmatively run `agentscm telemetry on` or make the equivalent interactive choice; `--yes` never supplies that consent. A `permitted-default` profile may enable collection after quickstart shows the notice and completes its default-on flow, subject to the operator's legal determination. An `unknown` profile stays inactive.

Your choice is stored in the per-user telemetry configuration. `agentscm telemetry off` persists the opt-out and deletes queued events and telemetry identifiers without sending an opt-out event. `DO_NOT_TRACK=1`, `AGENTSCM_TELEMETRY=off` and the global `--no-telemetry` flag provide stronger overrides. See [usage telemetry](telemetry.md) for the fields, status and inspection commands.

## What it does

1. **Looks around** and prints what it found: whether you are in a Git repository, its `origin` (GitHub, GitLab or other) and `owner/name`, the Git version, whether Docker and `docker compose` work, whether a hub is already configured and answering, and whether the `claude` CLI is on `PATH`. Nothing is changed.
2. **Where AgentSCM runs** (`--local` or `--hosted` skips the question): a hub is a small shared server that lets agents on different machines see each other's work as it happens. Choose one:
   - **On this machine (recommended for the alpha; `--local`, same as `--hub local`).** GitHub repositories only. On Apple Silicon macOS with `launchctl`, the default runs the installed native binary as a per-user LaunchAgent. Its plist is in `~/Library/LaunchAgents`, with private configuration and data in `~/.agentscm/hub-native/<project>`. It starts at login and restarts after a crash. Persistent service output goes to `/dev/null`; startup failures include bounded `launchctl` diagnostics. Other hosts can explicitly choose the Docker fallback (`--hub-runtime docker`), which requires a running local Linux Docker Engine and Docker Compose. Docker selects the daemon's native ARM64 or AMD64 image and acquires it before writing `~/.agentscm/hub/compose.yaml` and `.env` (0600).

     Both runtimes bind to `127.0.0.1:7332` by default. `--hub-port` selects another port; `--hub-project` selects an instance name. The default name incorporates the home directory, so isolated installations do not share a service or volume. Resume reuses the recorded runtime, port and project. Quickstart refuses to switch an existing instance's runtime or identity. Port, authentication, configuration and startup failures are reported without switching runtimes.

     On first run it opens the hub's one-time link so you can create a GitHub App; click **Install** on the success page and choose your repository. It then asks for your GitHub login (offered from `gh` or `git config github.user`), makes that account the hub's administrator, and restarts the hub with sign-in enabled.
   - **Hosted (`--hosted`).** Unsupported in the alpha. Quickstart prints a link to the [hosted guide](hosted.md) to work through with your coding agent and sets up no server. If your team's hub already runs, give its URL (or `--hub-url URL`) and quickstart connects to it, as `--hub existing` does.
   - **Skip.** Only the collision check; run quickstart again later to add a hub.
3. **Sign-in.** It signs you in through your browser (device code if the hub offers it). The session lets quickstart create the agent token in the next step. With `--agent-token-file` it skips sign-in and uses that token.
4. **Checks for collisions now** (default yes). It runs the [collision check](collisions.md) with worktrees and prints, for example, `7 branches/worktrees checked: 2 would conflict, 3 touch the same files.` and the top three pairs with the files they share. Old Git or a missing `origin/HEAD` gets one plain sentence and setup continues.
5. **Coding agents.** For each agent it runs the same code as [`agentscm client setup`](team-member-onboarding.md): Claude Code by default when the `claude` CLI is on `PATH`, or the agents you name with `--agent claude|codex|cursor|copilot|gemini` (repeatable). `--no-agents` skips this step. Quickstart writes no hooks, token files or `hub.json` of its own. For each agent, setup:
   - connects this checkout to the hub: it creates an agent token for this repository (valid 30 days, the hub maximum), keeps it in `~/.config/agentscm/tokens/client-<hash>.token` (0600) and records the connection in `~/.config/agentscm/connections.json`;
   - installs the agent's hooks, MCP entry and coordination skill. For Claude Code that is the user-scope `agentscm@agentscm-claude-client` plugin, installed through the `claude` CLI, and a Git-excluded `.mcp.json` entry for this checkout.

   If the hub's GitHub App is not installed on the repository yet, setup prints where to install it and waits for Enter. Without a terminal it needs `--agent-token-file`. Agents set up this way send `x-agentscm-client` and appear on the hub's Connect agents page like any other `client setup` agent. Each Git worktree is its own checkout and needs its own setup: run quickstart or `agentscm client setup` in it.
6. **Repository config for the team** (optional; `--init` without a terminal). If a hub is set up, quickstart offers to run [`agentscm init-repo`](agents.md) for this repository, which writes shared instruction files for Claude Code, Codex, Cursor, Copilot and Gemini CLI for you to review and commit. Hooks and MCP come from `agentscm client setup`, which each developer runs once per checkout; after writing the files quickstart prints that command with this hub's URL. These are repository files: they are not recorded in `quickstart.json` and `uninstall` leaves them.
7. **Summary** of the recorded setup, and a record of it in `~/.config/agentscm/quickstart.json` (0600). It lists the checkouts quickstart connected; the connections themselves are in `connections.json`. Running quickstart again detects what exists and offers to keep it. A run that writes nothing says **No changes made.** Setup prints the executable path for subsequent commands and, for an installed binary, an `export PATH=...` command for the current shell. When a hub answers and this repository is set up, it also prints the hub's status page (`<hub>/ui/`) and the `hub token create --read-only` command for a token to sign in there. Hints name the program path they were run with, so they work before `agentscm` is on `PATH`. The last line of every run says whether live warnings are on (`Live warnings: on (hub <url>, repo <owner/name>)`) or off and why.

**Several repositories, one hub.** Run quickstart in each repository. Each checkout gets its own token and its own entry in `connections.json`; other checkouts' entries and files are never touched.

If the hub isn't answering, quickstart reports the failure and the final live-warning status. Explicitly requested hub setup that remains incomplete exits non-zero.

Quickstart and uninstall share one lock for the user configuration directory. An overlapping run stops with a retry message before changing installed state. The private `quickstart.lock` file remains after uninstall so later runs use the same lock.

## Running without a terminal (CI)

Without a terminal quickstart refuses unless you pass `--yes`, which takes the recommended setup answers. Telemetry still requires an earlier eligible choice or explicit enablement. Choose with flags:

| Flag | Meaning |
| --- | --- |
| `--hub local\|existing\|skip` | How to get live warnings (with `--yes` and no flag, an already configured hub is kept, otherwise skip). |
| `--hub-runtime native\|docker` | Local runtime. Native is the default on supported Apple Silicon macOS hosts; Docker must be selected explicitly elsewhere with `--yes`. |
| `--hub-port PORT`, `--hub-project NAME` | Isolated local instance; defaults to port 7332 and a home-specific name. Resume reuses the record. |
| `--hub-url URL` | The hub for `--hub existing`. |
| `--agent-token-file PATH` | Use this agent token instead of signing in; it is checked against the hub and used where it is. Needed for agent setup without a terminal. |
| `--agent claude\|codex\|cursor\|copilot\|gemini` | Set up this coding agent (repeatable). Default: Claude Code if the `claude` CLI is on `PATH`. |
| `--no-agents` | Do not set up any coding agent. |
| `--provider github\|gitlab` | Forge to sign in with (default: from `origin`). |
| `--github-login LOGIN` | Your GitHub login for the local hub's administrator. With `--yes` it is required unless `gh api user` works; an unverified `git config github.user` is only offered in the interactive prompt. |
| `--init` | Also run `agentscm init-repo` for this repository once a hub is chosen (with `--yes`, it is otherwise skipped). |
| `--no-collisions` | Skip the collision check. |

Without a terminal quickstart never starts a browser or device sign-in: pass `--agent-token-file` for non-interactive setup. A new local hub still requires GitHub App setup in a browser; `--yes` prints the link and returns exit status **3** without waiting. Finish that step, then resume with the printed command, which includes the runtime, port and project. Exit status **0** means the requested setup completed; other failures exit non-zero.

Non-interactive `--yes` leaves a fresh telemetry notice and choice pending under every profile. It does not enable telemetry; use an earlier eligible choice or explicit `agentscm telemetry on`.

Example: `agentscm quickstart --yes --hub existing --hub-url https://hub.example.com --agent-token-file ~/hub.token --agent claude`.

## Undo: `agentscm uninstall`

It reads `quickstart.json`, shows exactly what it will remove, and asks (or `--yes`):

- If a step fails or is skipped (a file you changed, Docker not running), `quickstart.json` keeps only the records of what is left, uninstall prints the exact command to finish by hand (for example `docker compose -p agentscm-hub -f ~/.agentscm/hub/compose.yaml down`), and it exits non-zero. The record is deleted only when everything was undone.
- For every checkout quickstart connected, it runs [`agentscm client remove`](team-member-onboarding.md): the agents' entries come out of the checkout's files, the Claude Code or Codex plugin is uninstalled when no other connected checkout uses it, the connection is removed, the managed token file is deleted and the token is revoked on the hub if the hub answers. A token file you supplied is never deleted.
- The sign-in session quickstart created is ended (the hub is told, best effort) and its file removed.
- The program files directly in `~/.agentscm/bin` named `agentscm` or `agentscm-*` are removed, even ones quickstart didn't create; the confirmation list names them first. Nothing else there, and no directories, are touched.
- The local hub is stopped using its recorded runtime and identity. A native LaunchAgent is removed before its executable. Its data (or the Docker volume and setup files) are kept unless you choose deletion before the final confirmation, or pass `--remove-hub-data`. Changed service files are retained with their record for manual review. The GitHub App and installations remain on GitHub; delete them in GitHub Settings if no longer needed. Keeping hub data lets reinstall reuse the app. Native installs retain a private ownership receipt alongside that data. Uninstall refuses symlinked managed configuration roots; restore the recorded paths before retrying.
- Finally `quickstart.json` is deleted (see above). If it doesn't exist, nothing was installed by quickstart and uninstall says so.
- A file quickstart wrote is put back from its backup only when it is byte-identical to what quickstart last wrote; otherwise only quickstart's own entries are removed and your later edits stay.

Backup files stay next to the files they protect.

## Limits

- The release workflow builds and tests native `linux/amd64` and `linux/arm64` children before assembling the versioned image index. Older published images may contain only AMD64. Quickstart does not silently enable emulation: an operator can explicitly choose `DOCKER_DEFAULT_PLATFORM=linux/amd64`, subject to the Docker runtime supporting it. Emulation is slower and is not universal. Remote Docker contexts are refused because their loopback port would not reach this machine.
- The Docker fallback needs Docker Engine, Compose and the published image `ghcr.io/counterbranch/agentscm:<version>` (the version of this binary, without a `v`). Images are published with each release from 0.1.0-alpha.1. To use another image, set `AGENTSCM_HUB_IMAGE`; quickstart saves it in the hub's `.env`. If the image can't be pulled, quickstart says so and offers an existing hub or skipping.
- The local hub's first-run setup is GitHub only today. For a GitLab repository connect to an existing hub.
- Platform selection and generated Docker files have unit coverage; fake-Docker integration tests verify failed pulls write no setup files and uninstall keeps data by default. Matching-image acquisition, runtime lifecycle, GitHub App setup and sign-in require live qualification before release. Emulation needs its own runtime-specific qualification.
- The agent setup step reuses `client setup`, which has unit and stub-hub coverage but no live qualification through quickstart. Other hosts and clients need their own live qualification. The hooks are advisory: a hub problem is reported as "coordination unknown" and never blocks the agent.
- Sign-in through a browser is covered by the existing [hub](hub.md) sign-in tests, not by a quickstart end-to-end test.
