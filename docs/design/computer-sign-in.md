# Design: sign in once per computer

> **Status: design.** This page describes a change under review. Until [the roadmap](../roadmap.md)
> lists it, none of it is implemented. Today's behavior is in [hub](../hub.md) and
> [connect this computer](../connect-this-computer.md).

## Problem

A developer signs in to a Hub with `agentscm hub login`. Today:

- The Hub keeps the human session in process memory only (`Pending.sessions` in
  `src/hub/login.rs`). A Hub restart or deploy signs everyone out.
- The session lasts at most 8 hours (`SESSION_SECONDS`), capped again by the forge grant's
  `expires_in`. Nothing refreshes it. The client treats an expired saved entry as signed out.
- Minting an agent token checks repository permission with the person's own forge token, held in
  that session (`create_token`, rule `may_use`). No refresh token is stored anywhere.
- `client setup` mints one agent token per checkout for 30 days, the maximum
  (`MAX_AGENT_TOKEN_TTL_SECONDS`). Nothing renews it, so every connected checkout stops after
  30 days.
- In an unconnected checkout the hook only reports to the tray. The tray's Connect reuses the saved
  session and cannot start a sign-in (#116).

Goal: sign in once per computer; agents then connect and keep working in any checkout of a
repository connected to that Hub, for as long as the person keeps the needed role, with no
per-repository command and no periodic re-login.

## Options considered

1. **Keep the person's forge refresh token on the Hub** and refresh their user token at each mint.
   Keeps `may_use` byte-for-byte, but the Hub database then holds a long-lived credential that acts
   as the person on the forge (for GitLab, `read_api` on everything they can read). It must be
   encrypted at rest, rotated on every use (GitHub rotates refresh tokens) and serialized across
   Hub processes, and an OAuth App without expiring tokens has no refresh token at all.
2. **A Hub-issued computer credential, with permission re-checked through the Hub's own connection
   credential.** The Hub already holds a GitHub App installation token (or a configured PAT) for
   every connected repository. It can ask the forge what role a given forge user has on that
   repository. No new forge secret is stored.

**Choice: option 2.** It stores nothing that acts as the person on the forge, and it re-checks the
person's current role at every mint and renewal, so losing the role takes effect at the next
renewal. Its cost is a second, equivalent permission check (below), which must fail closed.

## The computer credential

- **Issue.** A sign-in started by the client names the computer (`device_name`, at most 64
  printable characters). The default is the macOS computer name when the person set one, else
  "<login>'s <model>" (for example "brandonfla's MacBook Pro"), never the raw host name (#120).
  `agentscm hub device rename NAME` changes it. After the existing checks pass (allow list, enabled user,
  can read a connected repository), the Hub creates a computer record and returns its secret once,
  next to the session. A person with a live session can also enroll one with
  `POST /v1/hub/devices` (used to upgrade existing sign-ins, see Migration).
- **Format and storage.** 32 random bytes as hex, like other Hub secrets. The Hub stores only a
  domain-separated digest (`agentscm.hub.device.v1`) in the persistent store
  (`hub_devices`: `id dev_…`, `owner`, `name`, `digest`, `created_at`, `last_used_at`,
  `revoked_at`, `revoked_by`). A restart or deploy signs nobody out.
- **Client storage.** In the existing owner-only `hub-sessions.json` entry for that Hub (mode 0600,
  written by the same private writer), beside the session: `device: {id, secret, name}` and
  `auto_connect: bool`. It is sent only to the Hub that issued it.
- **Lifetime.** No fixed end. A credential unused for 90 days stops working (sliding idle expiry;
  `last_used_at` is written at most once an hour). Disabling the person stops it at once;
  re-enabling restores it unless it was revoked or went idle.
- **Rotation.** The secret is not rotated on use: several hook processes on one computer use it
  concurrently, and rotate-on-use would race and sign people out. A sign-in replaces a record only
  when the client presents that record's credential (`replaces_device`): the old record is revoked
  with its agent tokens. The Hub never matches computers by name, so two laptops with the same name
  are two records. A leaked credential is handled by revocation, which every computer list exposes.
- **Scope.** The credential authenticates only these routes. It is refused on every human-session
  route (including admin routes, even for people in `AGENTSCM_ADMINS`) and on every agent route;
  agent tokens and sessions are refused on these.

| Route | Purpose |
| --- | --- |
| `GET /v1/hub/device` | This computer and its owner: status for the tray and hook |
| `POST /v1/hub/device/tokens` | Mint or renew an agent or read-only token for the owner |
| `DELETE /v1/hub/device/tokens/{id}` | Revoke one of the owner's agent tokens |
| `DELETE /v1/hub/device` | Sign this computer out (revokes it and its agent tokens) |
| `GET /v1/hub/devices` | The owner's computers |
| `DELETE /v1/hub/devices/{id}` | Revoke one of the owner's computers |

Revoking another of one's own computers is allowed from a computer credential because it only
removes access: it lets a person remove a lost laptop from the one in front of them. The worst a
stolen credential can do with it is sign its owner's other computers out. The `device_revoked`
event records which computer did it. A computer cannot revoke itself through
`DELETE /v1/hub/devices/{id}` (400); it signs out with `DELETE /v1/hub/device`.

## Permission at every mint and renewal

`POST /v1/hub/device/tokens` takes `{name, repository, read_only?, replaces?, auto?}`. In order:

1. The owner is enabled, and listed in `AGENTSCM_ALLOWED_USERS` when that is set (administrators
   pass, as at sign-in). Otherwise `user_disabled` or `not_allowed`.
2. The repository is connected on the owner's identity host (`repository_named`), else
   `not_connected`.
3. With the connection's own credential (installation token for `app`, the PAT for `pat`):
   - GitHub: `GET /repos/{full_name}` for `id` (must equal the connected id) and `private`, then
     `GET /repos/{full_name}/collaborators/{login}/permission`. The reply's `user.id` must equal
     the owner's forge user id, so a renamed or re-registered login cannot match someone else.
     A private repository needs any role (`permission` other than `none`). A public one needs
     `role_name` triage, write, maintain or admin (or `permission` write or admin for a custom
     role). This is the `may_use` rule expressed in the collaborator API.
   - GitLab: `GET /projects/{id}` for `visibility`, then `GET /projects/{id}/members/all/{user_id}`
     (inherited and invited-group members included). Reporter (20) or more on `private`, Developer
     (30) or more otherwise.
   - A 404 or a missing field is `no_repository_role`. When the connection's credential cannot
     answer (the connection is `auth_failed`, or the forge answers 401/403 because the credential
     lacks the members scope), the result is `access_check_unavailable` (403, "the Hub can't check
     your access to owner/name"). A forge 5xx or network failure is `permission_unavailable` (503).
     Nothing is minted in any of these cases, and the caller keeps its current token.
   - The check is one function, `repository_role(connection, repository, person) -> Allowed |
     Refused(kind)`, with the forge check as its only source today. An admin-issued per-repository
     grant (for a Git host that is neither GitHub nor GitLab) could be added as a second source
     without changing the computer credential, its routes or renewal. It is not built.
4. Mint. A device-minted agent token lasts **24 hours** (`DEVICE_AGENT_TOKEN_TTL`); read-only ones
   30 minutes, as the local page uses them. The token row records `device_id`. With
   `replaces: tok_…` (owned by the same person and for the same repository), the old token is
   revoked in the same transaction.
5. An `auto: true` mint for a repository this computer had no token for writes an
   `auto_connected` event.

Manual `hub token create` keeps its 30-day maximum; only the device path is shortened.

## Renewal

- The client renews a device-minted token when less than 12 hours remain, or when it has expired.
  The hook does it on its normal path, at most once per checkout every 5 minutes on failure, with
  the existing short hook timeout, under the client lifecycle lock, by compare-and-swap on
  `connections.json` and the token file (outside the checkout). The old token is revoked through
  `replaces`. A laptop asleep for days renews at the first hook call after waking.
- A pre-existing 30-day token (no `device_id`) is renewed the same way once the computer has a
  credential.
- A refusal (`no_repository_role`, `not_allowed`, `user_disabled`, `sign_in_required`) stops
  renewal for that checkout. The current token keeps working until it expires or is revoked. The
  hook adds one line to the agent context with the reason and the fix, and reports it to the tray,
  which marks the checkout as needing attention.

## Auto-connect

- Opt-in per Hub: `auto_connect` in that Hub's saved entry. The sign-in asks once, "Connect agents
  automatically in repositories on this Hub? [Y/n]", Yes by default; `--no-auto-connect` answers
  No without a prompt. The help says that the checkout's origin (forge host and owner/name) is sent
  to every Hub with auto-connect on. It can be changed later
  (`agentscm hub auto-connect on|off [--hub URL]`).
- Only at session start (Claude Code `SessionStart`, and the first hook call of a session for agents
  without one), never on every tool call. When an agent starts in a Git checkout with no client
  connection, the hook re-derives the origin from the checkout (`origin_identity`), and for each
  saved Hub with `auto_connect` (at most 4, the default Hub first) asks `POST /v1/hub/device/tokens`
  with `auto: true`. `not_connected` moves on to the next Hub; any other refusal stops.
- A `not_connected` answer is cached per (Hub, origin) for an hour in the client state directory;
  other failures are throttled for 5 minutes. If the attempt cannot finish within the hook's time
  budget, the hook skips it and says nothing to the agent.
- When the origin looks like a fork (the forge reports a parent) and the Hub answers
  `not_connected`, the tray shows "your origin is a fork of owner/name; add that remote". The client
  never guesses the upstream.
- On success it binds the checkout through the existing connect path: invocation-program fence,
  client lifecycle lock, revalidation of root, origin and `connections.json`, token file under the
  config directory. It **never installs agent files into the checkout** (no `.mcp.json`,
  `.claude/…`, `.git/info/exclude` edits); the user-level plugin that ran the hook keeps working.
  The agent's context gets one line: `AgentSCM: coordinating on owner/repo`.
- Failures are throttled per checkout (5 minutes) and reported to the tray with the reason.

## Revocation and administration

- Owners see their computers (`GET /v1/hub/devices`): `id`, `name`, `created_at`,
  `last_used_at`, `revoked_at`, `current`, and `repositories` (repositories with a live agent
  token minted through it). Revoking a computer revokes every agent token minted through it in
  the same transaction, so its agents stop at their next call.
- Administrators (fresh human session, as for every admin route today):
  - `GET /v1/hub/admin/devices`: every computer, with its owner.
  - `DELETE /v1/hub/devices/{id}` with an admin session revokes anyone's computer.
  - `GET /v1/hub/admin/repositories`: connected repositories with the number of computers and live
    agent tokens using each.
  - `GET /v1/hub/admin/sign-in-refusals`: the last 200 refused sign-ins and computer mints within
    30 days: `at`, `count`, `provider`, `login`, `forge_user_id`, `reason`, `via`. One row per
    (provider, forge user id, reason): a repeat updates `at` and bumps `count`, so one person
    retrying cannot push others out. This is a change: today nothing is stored for a refused
    person. The rows hold no secret and are pruned on insert.
- Events (append-only log): `device_created`, `device_revoked` (with `by`), `auto_connected`.

## Migration

- Saved sign-ins without a credential keep working until their session ends. The first client
  action that finds a live session but no credential (hook renewal, tray, `client setup`) enrolls
  one with `POST /v1/hub/devices`, with no prompt.
- People who signed in before this change were never asked about auto-connect. The next
  interactive moment asks once: a one-time tray prompt "Connect agents automatically in repos on
  <Hub>?" (Yes / Not now), or the next interactive `agentscm` command at a terminal. The answer is
  remembered either way (`auto_connect_asked`).
- Existing 30-day agent tokens keep working until they expire and are renewed into 24-hour device
  tokens once a credential exists. A person whose session has already expired signs in once.
- The schema change is additive (`hub_devices`, `hub_sign_in_refusals`, nullable
  `agent_tokens.device_id`), on SQLite and PostgreSQL.

## Threats and limits

- **Stolen credential file.** It grants agent tokens for the owner's repositories until revoked or
  idle for 90 days; it cannot sign in to the Hub's web pages, act as an administrator, or read the
  forge. Mitigations: owner-only file, revocation from any of the person's computers or by an
  admin, `last_used_at` in every list. Not bound to hardware.
- **Role loss.** Takes effect at the next renewal (at most about 24 hours, usually 12) or at once
  when the computer or user is revoked or disabled.
- **Forge check equivalence.** The collaborator API reports org base and team roles; GitLab
  `members/all` includes inherited and invited-group members. Custom GitHub roles are judged by
  their base `permission`, which can refuse a custom read-plus-triage role on a public repository
  (fails closed).
- **PAT and GitLab token connections** need the token to be allowed to read collaborator or
  member permissions; if the forge refuses, the result is `access_check_unavailable`, never a grant.
- **No live user forge token.** The credential never depends on the person's forge token after
  sign-in (GitLab user tokens last about 2 hours, GitHub App ones 8).
- **Hub database.** Holds only digests; a dump does not yield usable credentials.
- **Origin disclosure.** Auto-connect sends the checkout's forge host and owner/name to every Hub
  with auto-connect on (at most 4, default first), including Hubs that do not have it.
- **Browser sessions** for the Hub's web pages still live in Hub memory, so the web UI still signs
  out on a Hub restart or deploy. Agents and the tray do not. That is a follow-up.
