# Guided setup: `agentscm quickstart`

> **Alpha: not for production.** The CLI and the files it writes may change without migration.

`agentscm quickstart` (and `npx agentscm`, which runs it) sets AgentSCM up for the repository you are in. You choose from short menus; it never asks you to edit a file. Before each write it says what it will write and asks once. `agentscm uninstall` takes it all back. Nothing is sent anywhere except to the hub you choose; there is no telemetry. Unix only (macOS and Linux).

## What it does

1. **Looks around** and prints what it found: whether you are in a Git repository, its `origin` (GitHub, GitLab or other) and `owner/name`, the Git version, whether Docker and `docker compose` work, whether a hub is already configured and answering, and whether Claude Code is installed. Nothing is changed.
2. **Checks for collisions now** (default yes). It runs the [collision check](collisions.md) with worktrees and prints, for example, `7 branches/worktrees checked: 2 would conflict, 3 touch the same files.` and the top three pairs with the files they share. Old Git or a missing `origin/HEAD` gets one plain sentence and setup continues.
3. **Live warnings** (optional): a hub is a small shared server that lets agents on different machines see each other's work as it happens. Choose one:
   - **Local hub with Docker.** GitHub repositories only. Writes `~/.agentscm/hub/compose.yaml` and `~/.agentscm/hub/.env` (0600), pulls the hub image, and starts it as the Docker Compose project `agentscm-hub` on `127.0.0.1:7332`. On first run it opens the hub's one-time link so you can create a GitHub App; click **Install** on the success page and choose your repository. It then asks for your GitHub login (offered from `gh` or `git config github.user`), makes that account the hub's administrator, and restarts the hub with sign-in enabled.
   - **An existing hub.** A hub your team already runs; give its URL.
   - **Skip.** Only the collision check and hooks; run quickstart again later to add a hub.
4. **Sign-in and agent token.** It signs you in through your browser (device code if the hub offers it), creates an agent token for this repository (a key that lets agents on this machine use the hub for that one repository; named `claude-<host>`, valid 30 days), and writes it to `~/.config/agentscm/tokens/<owner>__<name>.token` (0600). A token file that is already there and wasn't made by quickstart is backed up first and never deleted by `uninstall`. It then writes `~/.config/agentscm/hub.json` with the hub URL, a default token file and this repository's entry in a per-repository `tokens` map. If the hub's GitHub App is not installed on the repository yet, it tells you exactly what to click and waits for Enter.
5. **Claude Code hooks** (if Claude Code is found, or you pass `--hook`). Choose this project (`.claude/settings.local.json`) or every project (`~/.claude/settings.json`). The three hooks from `hooks/claude-code.example.json` are merged into the file: existing settings and hooks are kept, hooks already present are not added twice, the exact JSON to be added is shown first, and the original is copied to `<file>.agentscm-backup-<timestamp>`. A file that is not valid JSON is left untouched. The JSON is rewritten with sorted keys. Symlinked settings files stay symlinks: the file they point at is updated. A file that can't be read is never overwritten.

   The hook commands must keep working after quickstart exits, so: if the running `agentscm` is in `~/.agentscm/bin` (where the installer and `npx agentscm` put it), the hooks call the symlink `~/.agentscm/bin/agentscm`; else if a non-cache `agentscm` on `PATH` is the same program (not under npx, `~/.npm` or a temp directory), they call plain `agentscm`; otherwise quickstart copies the program to `~/.agentscm/bin/agentscm-<version>` (0755, directory 0700) and points the symlink `~/.agentscm/bin/agentscm` at it, and the hooks call the symlink. With `--yes` and no `--hook`, hooks are added to this project only if a hub was set up; otherwise they are skipped.
   **Project hooks use HTTP.** When you choose "this project" and a hub is set up, quickstart writes Claude Code's HTTP-hook form instead of the commands above: a `PreToolUse` hook for edits and a `SessionEnd` hook that call the hub directly, with the agent token in the `env` block of `.claude/settings.local.json` (a new file is created 0600). No program is copied, so there is no `SessionStart` hook (Claude Code does not support HTTP hooks for that event; the hub starts the session at the first edit check). The preview shows `<your agent token>`, never the token. That file now holds a secret: keep it out of Git. Older command-form hooks quickstart added to the same file are replaced, and `agentscm uninstall` removes the hooks and the `env` entry. The "every project" choice and the no-hub case keep the command form, because one environment variable cannot serve several repositories' tokens.
6. **Repository config for the team** (optional; `--init` without a terminal). If a hub is set up, quickstart offers to run [`agentscm init-repo`](agents.md) for this repository, which writes MCP, hook and instruction files for Claude Code, Codex, Cursor and Copilot for you to review and commit. These are repository files: they are not recorded in `quickstart.json` and `uninstall` leaves them. For a local hub (a loopback address) the default is no, because teammates need your team's hub URL; run `agentscm init-repo --hub-url URL` once that exists.
7. **Summary** of every file written or changed, and a record of it in `~/.config/agentscm/quickstart.json` (0600). Running quickstart again detects what exists and offers to keep it. The last line of every run says whether live warnings are on (`Live warnings: on (hub <url>, repo <owner/name>)`) or off and why.

**Several repositories, one hub.** Run quickstart in each repository. Every repository gets its own token and its own entry in `hub.json`'s `tokens` map; other repositories' entries and files are never touched. If `hub.json` points at a different hub, quickstart shows it and asks before switching (a backup is kept), and says other repositories will move with it.

If the hub isn't answering at any point, quickstart says so and carries on with the remaining steps; live warnings start working once it is back.

## Running without a terminal (CI)

Without a terminal quickstart refuses unless you pass `--yes`, which takes the recommended answer to every question. Choose with flags:

| Flag | Meaning |
| --- | --- |
| `--hub local\|existing\|skip` | How to get live warnings (with `--yes` and no flag, an already configured hub is kept, otherwise skip). |
| `--hub-url URL` | The hub for `--hub existing`. |
| `--agent-token-file PATH` | Use this agent token instead of signing in; it is checked against the hub and used where it is. |
| `--agent-name NAME`, `--ttl 30d` | Name and lifetime of the token quickstart creates. |
| `--provider github\|gitlab` | Forge to sign in with (default: from `origin`). |
| `--github-login LOGIN` | Your GitHub login for the local hub's administrator. With `--yes` it is required unless `gh api user` works; an unverified `git config github.user` is only offered in the interactive prompt. |
| `--hook user\|project\|none` | Where to put the hooks (with `--yes` and no flag: this project if Claude Code is found). |
| `--init` | Also run `agentscm init-repo` for this repository (with `--yes`, it is otherwise skipped). |
| `--no-collisions` | Skip the collision check. |

Without a terminal quickstart never starts a browser or device sign-in: pass `--agent-token-file` for non-interactive setup.

Example: `agentscm quickstart --yes --hub existing --hub-url https://hub.example.com --agent-token-file ~/hub.token --hook project`.

## Undo: `agentscm uninstall`

It reads `quickstart.json`, shows exactly what it will remove, and asks (or `--yes`):

- If a step fails or is skipped (a file you changed, Docker not running), `quickstart.json` keeps only the records of what is left, uninstall prints the exact command to finish by hand (for example `docker compose -p agentscm-hub -f ~/.agentscm/hub/compose.yaml down`), and it exits non-zero. The record is deleted only when everything was undone.
- Hook entries it added are removed; everything else in the settings file stays. If the file is unchanged since quickstart wrote it, it is restored from the backup (or deleted, if quickstart created it).
- The agent token files quickstart wrote are deleted, and each token is revoked on the hub if the hub answers. A token file you supplied, or that was there before, is never deleted.
- The sign-in session quickstart created is ended (the hub is told, best effort) and its file removed.
- The program files directly in `~/.agentscm/bin` named `agentscm` or `agentscm-*` are removed, even ones quickstart didn't create; the confirmation list names them first. Nothing else there, and no directories, are touched.
- `hub.json` is deleted (or the previous copy restored) only if it is unchanged since quickstart wrote it; otherwise it is left and the reason is shown.
- The local hub is stopped (`docker compose -p agentscm-hub down`). Its data and `~/.agentscm/hub` are kept unless you confirm separately, or pass `--remove-hub-data`.
- Finally `quickstart.json` is deleted (see above). If it doesn't exist, nothing was installed by quickstart and uninstall says so.
- A file is put back from its backup only when it is byte-identical to what quickstart last wrote; otherwise only quickstart's own entries are removed and your later edits stay.

Backup files stay next to the files they protect.

## Limits

- The local hub needs Docker and the published image `ghcr.io/counterbranch/agentscm:<version>` (the version of this binary, without a `v`). Images are published with each release from 0.1.0-alpha.1. To use another image, set `AGENTSCM_HUB_IMAGE`; quickstart saves it in the hub's `.env`. If the image can't be pulled, quickstart says so and offers an existing hub or skipping.
- The local hub's first-run setup is GitHub only today. For a GitLab repository connect to an existing hub.
- The Docker and first-run steps are not run in CI: they are covered by unit tests of the exact commands and files only. The rest is tested with the real binary against an in-process hub.
- The hooks are verified with recorded Claude Code hook input, not with a live Claude Code session. They are advisory: a hub problem is reported as "coordination unknown" and never blocks the agent.
- Sign-in through a browser is covered by the existing [hub](hub.md) sign-in tests, not by a quickstart end-to-end test.
