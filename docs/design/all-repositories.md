---
title: "Design: All repositories home page"
---

# Design: All repositories home page

> **Status: experimental.** Built as described below, with the differences listed under
> [What was built](#what-was-built). See [the roadmap](../roadmap.md) for what is qualified and
> [hub](../hub.md#signing-in-to-the-hubs-web-pages) for the route. The "Current state" section
> records the code as it was before this change.

## Problem

A person signs in to the Hub's web pages and lands on one repository's Dashboard, chosen for them.
If that repository has no work, the page is empty and does not say that other repositories are
busy. A person who works across several repositories has no page that answers "where do I need to
look?". They have to open each repository from the repository menu, one at a time.

## Current state

The web pages read one repository per read-only token.

- `ui/src/webRead.ts:46-60` (`firstAllowed`) mints a token for the first repository the Hub allows,
  in the order from `tryOrder` (`ui/src/webRead.ts:41`): the one picked last time in this browser,
  then the rest. `ui/src/webRead.ts:194` calls it once at sign-in. The page then opens that
  repository and nothing else.
- The list it tries comes from `sessionRepositories` (`ui/src/devices.ts:188-204`): repositories of
  the person's live agent tokens and computers, plus every connected repository for a Hub admin,
  sorted by name and capped at 200. A first-time admin therefore opens the alphabetically first
  connected repository, which may be empty. A first-time person who is not an admin and has no token
  or computer gets an empty list, even when they can read connected repositories on the forge.
- `mintWebReadToken` (`ui/src/devices.ts:174-181`) calls `POST /v1/hub/session/read-token` with one
  `repository`. Each call stores a new 30-minute token on the Hub
  (`DEVICE_READ_TOKEN_TTL_SECONDS`, `src/hub/device.rs:16`).
- The repository menu on a web sign-in (`WebRepoSwitcher`, `ui/src/TopNav.tsx:191-242`) switches
  the single open repository by minting again. Its note at `ui/src/TopNav.tsx:234` says
  repositories appear only once the person has an agent token or a computer for them. With a
  pasted token, the menu says the token is scoped to one repository (`ui/src/TopNav.tsx:269`).
- The shell (`ui/src/App.tsx:309`) renders one `Shell` around one read token. Every rail page
  (`ui/src/App.tsx:48-77`) is about that one repository.

The Hub checks access per repository, at mint time.

- `web_read_token` (`src/hub/login/devices.rs:475-502`) accepts only the cookie sign-in and calls
  `mint_check` (`src/hub/login/devices.rs:415-459`): the allow list, a connected repository with
  this name, then `repository_role` (`src/hub/login/devices.rs:585-650`). The role check asks the
  forge with the repository connection's own credential (GitHub collaborator permission, GitLab
  member access), confirms the repository id and the person's forge user id, and applies the same
  role rule as sign-in. A refusal is recorded and returned in the refusal shape. There is no cache:
  every mint is one forge request.
- `create_token` (`src/hub/login.rs:1525-1612`), the CLI path, does the equivalent check with the
  person's own forge token. Sign-in itself probes up to `MAX_SIGN_IN_PROBES` (200,
  `src/hub/login.rs:51`) connected repositories in `reads_connected`
  (`src/hub/login.rs:1132-1159`) and stops at the first one the person can read.
- Every token is bound to one repository id. The work overview (`GET /v1/hub/standup/overview`,
  `src/hub/work_http.rs:177-191`) takes the repository from the token (`work_who`,
  `src/hub/store_sqlite.rs:322`), as do the other reads.

The roadmap lists an "Orchestrator view" as Proposed (`docs/roadmap.md:62`): one read-only page
across sessions with claims, intents, resource claims, skill gates, branch collisions and the
main-red signal.

## The page

**All repositories** lists every connected repository the signed-in person can read now, one row
each:

| Column | Meaning | Source rule |
| --- | --- | --- |
| Repository | `owner/name` and forge | the connected repository |
| Unfinished | count of unfinished tasks, split into *moving* and *stuck* | work items not `finished`; *stuck* is `blocked`, or a lead whose claim is not `active` (the `needsYou` rules in `ui/src/needs.ts:27`) |
| Needs you | count of things a person should decide | blocked, quiet lead, handover offered, declared-scope overlap and contested file, as in `needsYou` |
| Agents | agents connected now | distinct live agents, by the rule in `liveAgents` / `sameAgent` (`ui/src/derive.ts`) |
| Last activity | when the repository last changed | newest Hub event or work update |

- Rows with something in *Needs you* come first, then rows with *stuck* tasks, then by last
  activity, newest first. A quiet repository is a short row at the bottom, not hidden.
- Clicking a row opens that repository's Dashboard: the page mints a read token for it exactly as
  the repository menu does today. Minting can still refuse (the role changed in between); the page
  then shows the refusal and keeps the list.
- A person who can read exactly one repository skips the page and lands on that Dashboard, as
  today. With none, the page says so in the same words the empty sign-in state uses
  (`unavailableCopy`, `ui/src/App.tsx:324-325`).
- Repositories the Hub could not check (forge unavailable, rate limited, connection credential
  failing) are not listed by name. The page shows "N repositories could not be checked" with a
  retry, so a forge outage does not look like lost access and does not reveal names.
- The page is read-only. It does not show task titles, paths or people; those stay on each
  repository's Dashboard, behind that repository's own token.

Pull-request overlaps (`pr` in `needsYou`) come from the repository's forge state on the
Dashboard. They are left out of the row count in the first version, and the row says "needs you"
counts exclude pull-request overlaps. Whether to add them depends on the server-side cost of that
join, decided at implementation.

## Data: two options

### (a) Client fan-out

The page mints a read token per repository and fetches each one's overview, work and activity.

- **Discovery is wrong.** The list comes from `sessionRepositories`, so a non-admin first-timer
  still sees nothing and an admin sees every connected repository. Fixing that needs a new Hub
  route anyway.
- **Writes on read.** N repositories store N tokens on the Hub at load and again every 28 minutes
  per open tab; with 50 repositories and 3 tabs that is 150 token rows per half hour, each one
  live and revocable.
- **Requests.** At least 1 mint (one forge call) plus 3 reads per repository per refresh: 200 Hub
  requests and 50 forge calls for 50 repositories, before any polling.
- **Duplicated logic.** The counts are computed in the browser from full overviews, so every row
  downloads every task.

### (b) A Hub summary route (recommended)

One request returns a small summary per repository, computed on the Hub, with each repository
authorized by the same check a mint uses. No token is stored. Nothing in the evidence argues for
(a): it does not solve discovery and costs more on every axis.

**Route.** `GET /v1/hub/session/repositories`.

**Auth.** The cookie sign-in only (`HumanSession` with `browser` set, as `web_read_token`
requires); a CLI session, computer credential or agent token gets 403. For each candidate
repository the Hub runs `mint_check` unchanged: the allow list, connected and the role through the
connection credential. Candidates are the connected repositories on the person's forge host
(`repositories_on`, capped at `MAX_SIGN_IN_PROBES`). `mint_check` would be split so the
summary can reuse its decision without recording a refusal row per repository; a refusal on this
route is an omission, not a recorded sign-in refusal. An admin gets the same per-repository check,
not every repository: the page shows what the person can read, not what the Hub holds (admins keep
Setup › Admin for that).

**Response.**

```json
{
  "repositories": [
    {
      "repository": "example-org/api",
      "repository_id": 1001,
      "provider": "github",
      "unfinished": { "total": 5, "moving": 3, "stuck": 1 },
      "needs_you": 2,
      "agents": 4,
      "last_activity_at": 1793300000,
      "complete": true
    }
  ],
  "unchecked": 1,
  "complete": true,
  "checked_at": 1793300050
}
```

`complete` is false when the candidate list hit the cap, as the work overview's
`current_complete` does. Counts are integers; a count the Hub could not compute is `null`, never 0.

**Limits and pagination.** No pagination in the first version: at most 200 candidates, one row
each, sorted on the Hub in the page's order. Attention-first order needs every row, so a cursor by
name would not help. Role checks run with a small fixed concurrency (8) and an overall deadline;
repositories not checked in time count as `unchecked`. The page refreshes the summary every two
minutes while visible and on demand, not at Dashboard polling speed, because a refresh can cost one
forge call per repository. The Hub keeps each person's role answer for a repository for 90 seconds
for this list only (never shared across people, never a failure, forgotten at sign-out), so a
granted or lost access shows within that long; mint deliberately has none.

**Store.** The work counts need `work_overview`'s query keyed by provider, host, repository and
repository id instead of by token (`work_who`). The change is to resolve that key from the
`mint_check` result and share the query, in both SQLite and PostgreSQL stores.

**Relation to the Orchestrator view.** This page is a first, narrow slice of the cross-repository
side: per-repository counts only, for a human. The Orchestrator view row stays Proposed. It covers
sessions, intents, resource claims, skill gates, branch collisions and the main-red signal, several
of which do not exist yet. When it is built, this route can carry more per-repository fields, and
the Orchestrator view can be this page's drill-down, not a second list.

## Naming

"Workspace" already means the Access page (`ui/src/App.tsx:64`, hash `#workspace`), whose tabs are
Members, Settings and "Repositories & runs" (`ui/src/TopNav.tsx:679`), and the repository menu's
"Open the workspace" item (`ui/src/TopNav.tsx:274`). It is also the Community workspace pilot, a
separate role system.

- Call the new page **All repositories**, at hash `#repositories`. Do not use "Workspace", "Home"
  or "Overview" for it.
- Reach it from the top of the repository menu ("All repositories", above the list) and from the
  logo. It sits outside the rail, because the rail is about one repository.
- The Access page's "Repositories & runs" tab would become "Runs" so two pages are not both
  "repositories". That rename is part of this change, not a separate one.

## What was built

- The route is `GET /v1/hub/session/repositories` in `src/hub/login/devices.rs`, with the counts
  in `src/hub/repository_summary.rs` and a token-free `repository_work_overview` in both stores.
  It reuses `repository_role` directly and an extracted `allow_listed`, so a refused repository
  records no refusal row.
- `unfinished` also carries `total`: paused and handed-over items are unfinished but neither
  *moving* nor *stuck*.
- No count is `null`. Each row has `complete` instead, false when the work overview or the live
  lists were cut short, so its counts are lower bounds; the page shows them with a `+`.
- *Needs you* counts files held by two different agents now, from live intents. Files with only
  a recent refusal or handoff in the event log, and pull-request overlaps, are not counted.
- The forge deadline is 10 seconds, under the page's 15-second request timeout.
- The page's list of repositories to open is the readable list first, then the ones the person
  has tokens or computers for (`useWebRead`), so a first-time person can open a repository.
  Someone who can read more than one lands on `#repositories` when the page opens on `#home`.
  At landing the list is fetched once, to choose; the page shows that list and refreshes it on
  its own two-minute poll. Rows do not stream in as each role check finishes.
- The local development mock (`ui/dev/mock.ts`) does not serve the route; the page shows its
  error there.
- The page is one hairline-ruled row per repository, not a table: the name, one status as a dot
  and words (coral "N need you", else amber "N stuck", else green "Nothing needs you"), then a
  line of counts and the last activity. A repository with nothing unfinished, no agents and
  nothing needing you is a shorter row. Lower-bound counts read "at least N". On a phone the
  status moves under the name.
- The Dashboard's tabs and its first-visit tips (coach marks) do not show on this page: both
  describe one repository. The tips wait for the first Dashboard.

## Docs and roadmap

- `docs/hub.md`: the route under [signing in to the Hub's web pages](../hub.md#signing-in-to-the-hubs-web-pages),
  next to `POST /v1/hub/session/read-token`, and the page in [Status page](../hub.md#status-page),
  which today says `/ui/` is "about one repository".
- `docs/api.md`: the route, response and refusals.
- `docs/roadmap.md`: a new row "All repositories page", Experimental, with what was tested (a fake
  GitHub and GitLab, never a live forge, until that is true). The Orchestrator view row stays
  Proposed and links here.
- This file lives in `docs/design/`.
- The repository menu's note no longer says repositories need a token or a computer to appear.

## Open questions

1. Whether the empty-repository case for a single readable repository should also show the page,
   so the person sees it is empty rather than broken.
2. Whether pull-request overlaps belong in the first version's *Needs you* count.
