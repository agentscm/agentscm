# AgentSCM Hub UI

The UI the hub serves at `/ui/`: a status page for the coordination hub: a top header (repository, search, health, account), a
grouped side rail and breadcrumbs. React 18, Bootstrap 5.3 and Bootstrap Icons. Repository status is read-only (GET only, read-only token). The
Workspace section, signed in with a forge session, has the hub UI's workspace actions; every write
goes through `ui/src/workspace.ts` with the CSRF header, a request ID and a retained retry for
unknown outcomes.

## Run

```sh
ui/dev-seed/start.sh                 # seeded hub on 127.0.0.1:7332, writes dev-seed/.hub.env
npm install && npm run dev           # http://127.0.0.1:5184
```

Open `http://127.0.0.1:5184/#token=<READ_TOKEN from ui/dev-seed/.hub.env>`. `MOCK=1 npm run dev`
uses `ui/dev/mock.ts` instead of a hub.

## Check

```sh
npm run lint        # ESLint, plus the data-layer checks in scripts/, no warnings allowed
npm test            # Vitest: unit + integration (integration needs the hub)
npm run test:e2e    # Playwright: Chromium, Firefox, WebKit, Pixel 7 and iPhone 14, against :5184
npm run test:all    # lint, tsc, Vitest, Playwright
```

Another hub or UI port (e.g. a second worktree already holds 7332 and 5184):
`AGENTSCM_DEV_HUB_PORT=7372 ui/dev-seed/start.sh`, then in `ui/` run the suites with
`AGENTSCM_HUB=http://127.0.0.1:7372 AGENTSCM_UI_PORT=5194` set (vite proxy, Vitest integration and Playwright all read them).

Integration and e2e tests need the seeded hub on :7332 (default) and skip with a message without it. The MCP
tests start work through the hub's JSON-RPC endpoint with seeded agent tokens, so each run leaves
finished "E2E probe" items under Recently finished. Install the browsers once with
`npx playwright install chromium firefox webkit`. Desktop projects run the whole suite; the two phone
projects run `tests/e2e/mobile.spec.ts` (drawer, narrow header, no sideways scroll on any page).

## Not covered by the dev hub

Pull requests (no forge connection), skill workflow runs (native workflows not configured) and the
workspace (needs forge sign-in) render their empty or unavailable states against it. Their filled
states are tested with fixture data instead, through a stubbed fetch that answers from
`ui/dev/mock.ts` (the same synthetic hub as `MOCK=1 npm run dev`) plus per-test overrides:

- Pull requests: https-only links, hostile URLs rendered as text (`tests/unit/review-fixes.test.tsx`).
- Workflows (`tests/integration/fixtures-workflows.test.tsx`): run list paging with the server
  cursor, page filters and run-ID lookup; run detail with the step graph, failed, blocked and
  optional steps; review findings by severity, unresolved items, covered paths and evidence; stale
  applicability and stale or unavailable landing bindings; step deep links and the
  Workflows › Run › Step breadcrumbs; malformed, non-JSON, oversized (declared and streamed), 404
  and 503 responses.
- Workspace (`tests/integration/fixtures-workspace.test.tsx`): the signed-out entrance with and
  without sign-in providers; roles, capabilities and their gating (actions absent without the
  capability, and gated sections never requested); runs paging with the before cursor; starting a
  run (exact subject, profile, generation, CSRF, request ID), its 409, its unknown outcome and the
  Pending operation recovery retry, and a mismatched success; confirmed cancel and approve/land
  with the retained landing retry; adding and editing members with the list generation, their 409
  causes (conflict, last administrator, member limit) and paging that stops on a generation change;
  saving settings and its 409; the workspace workflow deep link with steps; sign-out. Every test
  checks that writes stay within the exact allowed set (DELETE `/session`, POST runs, cancel,
  approve and integrate, PUT members and settings).

No end-to-end browser test covers the signed-in workspace, because the dev hub has no forge
sign-in; those flows are covered by the fixture tests above.

## Change records

The read-only change-record view opens at `#change-record/<work-id>` and reads
`GET /v1/hub/repos/<repository>/change-records/<work-id>` with the status-page token. It shows
the bounded Work intent and scope, inspectable Git or native source binding, retained decision,
next action and known gaps without requesting conversation content. A retained agent decision is
shown as a proposal rather than human approval. Action type, execution outcome, verification
result, current applicability, assurance, evidence availability and coverage remain separate;
ordinary command success is never presented as a passing check, and a passing historical result
or unavailable artifact is never presented as a current complete check. The page loads once and
refreshes only when the viewer asks. The shared response is at most 48 KiB. It carries at most 16
scope entries and 16 decision references, then reports exact omitted counts when fitting the
envelope removes trailing entries; the page shows those omissions rather than treating the visible
arrays as complete.

The focused UI tests use synthetic response fixtures. They cover the response allowlist, bounded
read, typed Git and native source facts, agent-proposal attribution, execution versus verification,
route parsing, the concise filled state, missing records and authorization denial. Real Hub browser
coverage follows both the Verify rail and a Work-detail link into the authenticated projection,
including the explicit not-yet-captured state. The seeded Hub does not retain a filled change
record, so the filled browser state remains fixture-tested.
