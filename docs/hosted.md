# Hosted hub setup (agent-guided)

> **Not supported in the alpha. It will likely break and is not for production.** APIs, CLI, storage and schemas may change without migration, and the authority database cannot be downgraded. Nobody is on call for a hosted hub. See the [changelog](../CHANGELOG.md) for known limits.

This guide is for a coding agent working through a hosted hub setup with a person. A hosted hub is one shared `agentscm hub serve` process on a server, which a team's agents connect to over HTTPS.

**First, confirm with the person that they really want hosted.** If they only want live collision warnings for their own machine, stop and have them run:

```sh
npx agentscm --local
```

## How to work

- Go one step at a time. After each step run its check and show the person the result.
- If a check fails, stop and ask the person. Do not improvise server, firewall, DNS or proxy changes, and do not skip a failed check.
- Never paste secrets into chat, issues, logs or commit messages. This covers OAuth client secrets, agent tokens, database and S3 passwords, and the one-time setup link. Pass secrets as `*_FILE` variables or from the person's own shell.
- Do not describe anything in the [roadmap](roadmap.md) marked Proposed as working.

## 1. Prerequisites

Ask the person for each of these. Do not create accounts for them.

- A server they control, with either Docker and Docker Compose, or the `agentscm` binary.
- A DNS name (this guide uses `hub.example.com`) and a reverse proxy that terminates TLS. The hub does not terminate TLS. Plain `http` is accepted only for loopback addresses.
- A GitHub or GitLab account that can administer the repository, to create the sign-in app. Sign-in uses the forge's OAuth app; `AGENTSCM_GITHUB_*` or `AGENTSCM_GITLAB_*` variables enable it ([sign-in configuration](hub.md#sign-in-configuration-environment-only)).
- PostgreSQL and an S3-compatible store only if they want the `postgres` profile. The default is SQLite and local object files, which is the simpler choice. PostgreSQL has no TLS in this build.

## 2. Run the hub

### Compose (recommended)

On the server, from a checkout of this repository, with the public URL set to the HTTPS address the team will use:

```sh
export AGENTSCM_PUBLIC_URL=https://hub.example.com
docker compose -f compose.hub.yaml --profile default up -d --build
```

Run exactly one profile at a time. The other profile is `postgres` (PostgreSQL and an S3-compatible store); set your own `AGENTSCM_PG_PASSWORD` (URL-safe), `AGENTSCM_S3_ACCESS_KEY_ID` and `AGENTSCM_S3_SECRET_ACCESS_KEY` before using it, because the defaults in `compose.hub.yaml` are local-trial values. See [Running in containers](hub.md#running-in-containers).

The compose file publishes `127.0.0.1:7332` only. The proxy in step 3 forwards to it. Optional `AGENTSCM_POLL_SECONDS` (default 300) sets the forge polling interval.

### Binary

```sh
export AGENTSCM_DATABASE_URL=sqlite:///srv/agentscm/hub/hub.sqlite
agentscm hub serve --bind 127.0.0.1:7332 --public-url https://hub.example.com \
  --objects /srv/agentscm/hub/objects --secrets /srv/agentscm/hub/secrets
```

The database directory must exist. The secrets directory is created mode 0700, and the hub refuses to start if group or others can reach it. Other flags: `--poll-seconds`, `--activity-days`, `--public` (needed only for a non-loopback `--bind`). Run it under a supervisor with a single instance; see [Deployment](deployment.md).

Check: `docker compose -f compose.hub.yaml --profile default ps` shows `hub` running, or the binary stays up with no startup error.

## 3. TLS and public URL

Put a reverse proxy (Caddy, nginx, Tailscale Funnel or similar) in front of `127.0.0.1:7332` and terminate TLS there. `--public-url` (compose: `AGENTSCM_PUBLIC_URL`) must be exactly the HTTPS origin browsers and the forge use. The OAuth redirect URI is `https://hub.example.com/v1/hub/login/<provider>/callback`, where `<provider>` is `github` or `gitlab`. Keep the setup link out of proxy access logs and do not log `Authorization` headers. The proxy should also enforce deadlines, size limits and rate limits ([HTTPS and proxy boundary](deployment.md#https-and-proxy-boundary)).

Check, from any machine:

```sh
curl -fsS https://hub.example.com/v1/hub/health
```

It returns `{"status":"ok","storage":"sqlite",...}` (or `postgres`). This proves only that the process and database answer. It does not prove sign-in, a forge connection or an agent connection.

## 4. Sign-in and administrators

Sign-in needs a forge app, and creating the GitHub App needs a signed-in administrator, so a fresh hub writes a one-time setup link (valid 10 minutes, single use; a restart makes a new one). Follow [First run](hub.md#first-run):

1. Read the link on the server: `docker compose -f compose.hub.yaml exec hub cat /data/secrets/bootstrap-setup-url` (service `hub-postgres` for the `postgres` profile). Give it to the person to open in a browser signed in to GitHub. Do not echo it into chat.
2. The person creates the app and installs it on one repository. The hub then prints `AGENTSCM_GITHUB_CLIENT_ID=...` and `AGENTSCM_GITHUB_CLIENT_SECRET_FILE=...`.
3. Set those two variables plus `AGENTSCM_ADMINS=github:YOUR_LOGIN` (prefer the numeric form `github:#ID`), then restart the hub. Sign-in is not reloaded without a restart. Optionally set `AGENTSCM_ALLOWED_USERS` to restrict who can sign in.
4. For GitLab, or a self-managed forge, use the `AGENTSCM_GITLAB_*` or `AGENTSCM_GITHUB_URL` variables in [sign-in configuration](hub.md#sign-in-configuration-environment-only).

Check, from the person's machine:

```sh
agentscm hub login --hub https://hub.example.com --provider github
agentscm hub connections list
agentscm hub connections refresh <id>
```

`list` shows the connection and `refresh` discovers and polls its repositories.

## 5. Connect the team's clients

Each developer, from a clone of the connected repository:

```sh
npx agentscm --hosted --hub-url https://hub.example.com
```

This prints the hosted caveat and a link to this guide, then sets up that machine against the hub (sign-in, agent token and, when Claude Code is found, its hooks), the same as `agentscm quickstart --hub existing --hub-url URL`. For another agent (`claude`, `codex`, `cursor`, `copilot` or `gemini`) use the one-step form from [team member onboarding](team-member-onboarding.md):

```sh
npx agentscm client setup --agent codex --hub https://hub.example.com
```

The host owns every project, plugin, hook and MCP trust prompt. The person must approve those themselves; do not edit trust records. Each repository needs its own agent token, and a token's repository must be connected on the hub.

## 6. Check that it worked

```sh
agentscm client list
agentscm client check
```

`list` is local and prints no tokens. `check` verifies the saved connection against the hub. Then:

- Open `https://hub.example.com/ui/` and sign in with a read-only token minted for the repository: `agentscm hub token create --read-only --name status-page --repo owner/name` ([status page](hub.md#status-page)).
- Start a real agent task and confirm its session appears for that repository.

A passing `check` proves only the local binding. See [Verify trust and behavior](team-member-onboarding.md#verify-trust-and-behavior) for per-agent proof, and do not fabricate hook or MCP traffic to qualify an agent.

## 7. Known breakages and limits

- Hub sessions live in memory, so a restart signs everyone out. Revoked or expired tokens do not release their intents.
- Nothing is locked. Intents and collision warnings are advice. Only edit and write tool calls are announced, not shell writes.
- GitHub and GitLab sync and GitHub publishing have been tested only against in-process fakes, not live forges. GitLab merge requests cannot be created.
- Schemas may change between alpha releases without migration. Back up before upgrading, and use the same `agentscm` version on the hub and clients. SQLite backup is cold: stop the hub first ([backup](hub.md#running-in-containers)).
- One writable hub process per data directory. There is no high availability, failover or online backup, and nothing is measured at tens or hundreds of agents.
- The stock `Dockerfile` default command runs the unauthenticated read-only `agentscm serve`. Use `compose.hub.yaml` (which runs `hub serve`) and never expose `serve` on a network.
- Windows supports only a subset of commands. The five agent integrations are candidate behavior and are not fully qualified against a hosted hub.
- Not implemented, so do not rely on them: optional native authority and cutover from Git, and measured scale ([roadmap](roadmap.md)).

Full list: [changelog known limits](../CHANGELOG.md), [hub limits](hub.md#limits), [deployment](deployment.md).

## 8. Reporting problems

Open an issue at <https://github.com/counterbranch/agentscm/issues> with the version, the step that failed and the exact error. Remove tokens, URLs with setup links, and private repository names first. Report suspected vulnerabilities privately, not in an issue; see [SECURITY.md](../SECURITY.md).
