# MCP Apps views for the hub: implementation spec (proposal)

Status: implemented in the work-overview and coordination-inbox views with hub-side on/off settings (`agentscm_view_settings`, Settings page in the 1c UI) and a quiet [auto-refresh](#auto-refresh). Not verified against a real host yet. Extends the earlier `mcp-ui-integration.md` note (Direction 1), which is not in this repository. Host support is documented for Claude, VS Code Copilot, Cursor and ChatGPT; Claude Code is unverified (probably no). Re-check before shipping.

## Adversarial panel record

A simulated panel (one model playing four roles, not real people) was told to attack the plan. What survived:

| Role | Attack | Outcome in this spec |
|---|---|---|
| AI engineer | The agent cannot see the iframe; the card is for the human. Teammate-written text rendered as HTML is an injection and XSS path. | Text and structured result stay the primary contract. All teammate text is escaped and clipped. |
| Tech leader | Four views for hosts we cannot all verify is speculative, and the places agents run (Claude Code) may not render. | Ship two views, behind a host-capability check. Measure before adding more. |
| Product manager | Humans ask "who is doing what" and "are we colliding" far more than they browse history. A live-looking feed is a promise MCP cannot keep (no server push). | Work overview first, inbox second. Briefing and workflow-run are cut. Manual refresh only; label data age. |
| DevOps | `resources/read` is a new unauthenticated-looking surface, with unbounded size, no caching story and no logs. | Same auth as tools, size cap, content hash, cache headers, request logging, no polling. |

The "manual refresh only" and "no polling" outcomes above were later relaxed for the views' own read-only tool calls. `resources/read` is still never polled. See [Auto-refresh](#auto-refresh).

Unresolved: whether any host feeds app-initiated tool results back into model context (unverified). Test this first.

## Goals and non-goals

Goals: show a human, inside the chat, (1) what every agent is doing and (2) which directed coordination requests are open, using the hub's existing read-only tools.

Non-goals: writes (ack, reply, resolve, checkpoint), pushing into the conversation (`ui/message`, `ui/update-model-context`), animation, briefing and workflow-run views, contested-files view, WebMCP, A2UI.

## Wire changes (`src/hub/mcp.rs`)

1. `initialize`: read `params.capabilities.extensions["io.modelcontextprotocol/ui"]`. Only when present: advertise the `resources` capability and attach `_meta.ui` to tools. Otherwise behave exactly as today.
2. `tools/list`: for `agentscm_work_overview` add `_meta: {"ui": {"resourceUri": "ui://agentscm/work-overview"}}`; for `agentscm_coordination_inbox` add `ui://agentscm/coordination-inbox`. Keep `visibility` `["model","app"]` for these two read-only tools only.
3. Add `resources/list` and `resources/read` as read-only methods (same gate as `read_only_call`). MIME type `text/html;profile=mcp-app`. Unknown URI returns an invalid-params error.
4. Tool results are unchanged: they keep the text content and structured content they return today. Never make the UI the only carrier of information.
5. Do not declare `csp` or `permissions`. The view makes no network calls except `tools/call` through the host.

## Views

Each view is one self-contained HTML file: inline CSS and JS, no external origins, no framework unless the size cap holds. Target under 60 kB. Both render as the same deep-violet "Twilight" panel in light and dark hosts (system fonts, inline SVG icons). Every status colour also has a text label and an icon. The work overview now leads with one sentence and only the rows that need a person; all work, the briefing and the catch-up are folded; the in-view search box was dropped. The inbox shows a four-step track per request. The lane layout below is the original proposal.

### `ui://agentscm/work-overview` (build first)

Data: `WorkOverview` in `src/hub/work.rs`: `current`, `recent`, `overlaps`, `current_complete`, `recent_complete`, `overlaps_complete`, `history_retention_gap`, `recent_since`, `until`. Read the `WorkItem`, `WorkOutcome` and `WorkOverlapPair` structs for exact fields.

Layout, top to bottom:
1. Header: title, "Updated HH:MM:SS" (from when the result arrived), Refresh button.
2. Right-now sentence: "N agents working, M overlaps need coordination, K blocked."
3. Four lanes: In progress, Blocked, Handover, Paused. Each item shows task title first, then agent, next action, age of last report. Handover shows from and to. "Still assigned" warning on handover and paused items and on stale claims. Stale and unknown claims stay claimed.
4. Overlaps strip: pairs of agents with the shared paths; show "N more paths omitted" when clipped.
5. Recently finished: collapsed, with count.

Completeness: if `current_complete` is false show "Showing a partial list" at the top. An empty overlap list says "No overlaps" only when `overlaps_complete` is true; otherwise "Overlap check incomplete". Show a retention-gap note when `history_retention_gap` is true.

### `ui://agentscm/coordination-inbox` (second)

Data: the page returned by `agentscm_coordination_inbox`: requests with stage, requester, targets, receipts, replies, paths, and the fixed watermark values for paging.

Layout: header (open count, updated time, Refresh); one row per request with a stage pill (Awaiting receipt, Awaiting reply, Replied, Resolved, Withdrawn), requester to target agents, task title, overlapping paths (first 3, then "N more"), a four-step progress bar; expand for receipts and replies. Footer states: "Read-only. Reading never acknowledges." Next page button holds the watermark; returning to a fresh first page is explicit. Show "history omitted" where the server bounds receipt or reply history; never infer a missing receipt from a clipped list.

## Interaction rules

- The only calls a view makes: `tools/call` to its own tool (refresh, paging). Never any ack, reply, resolve, withdraw, checkpoint or start tool.
- Refresh is a button and also runs on a quiet timer while the view is on screen (see [Auto-refresh](#auto-refresh)). Show data age.
- If a call fails, keep the last data and label it "Could not refresh. Showing data from HH:MM:SS".
- Links to the hub UI use `ui/open-link` and need the hub's public URL; omit the link if unconfigured.

## Auto-refresh

A view that is on screen keeps itself current without spending model tokens. It re-calls its own tool through the same `tools/call` path as the Refresh button. It never uses `ui/message` or `ui/update-model-context`, so the view itself adds nothing to the conversation.

- Cadence: 30 s after the last result (a fresh result pushed by the host also restarts the clock). After a failed call the wait doubles (60 s, 120 s, 240 s) up to 5 min, and returns to 30 s after one success.
- Only while visible: the timer runs when `document.hidden` is false and an `IntersectionObserver` on the view reports it intersecting. Hiding or scrolling away clears the timer. Coming back refreshes at once if the wait has already elapsed, otherwise when it does. Hosts that do not fire `visibilitychange` for offscreen iframes are covered by the observer; a host that does neither keeps polling while the iframe exists.
- Never stacked: one call per view is in flight at a time. A timer that fires during a call does nothing. A button press during a call is queued, last press wins, and sent when the call returns.
- Keeps the person's place: open folds (including source details in the catch-up) are remembered by key, the focused control and scroll position are restored after each repaint, and the inbox re-calls the page the person is on (`after`/`through`) rather than the first page. The overview's catch-up delivery is a fixed window and is not re-fetched: pages already loaded by Load more stay, and new records arrive in the next delivery after Mark reviewed. The overview's briefing is read again after each successful refresh, replacing its own earlier 30 s timer.
- Failure: the last data stays with "Could not refresh. Showing data from HH:MM:SS" and an amber dot. "Updated HH:MM" moves on every success, including when nothing changed.
- Changes are announced politely: a visually hidden `role="status"` / `aria-live="polite"` region outside the repainted area is updated only when the one-line summary changes (overview: agents working and things that need a person; inbox: open requests per stage). Focus is never moved.
- Limits: a repaint replaces the page content, so a text selection is lost every 30 s. Each refresh cycle is two read calls on the overview (its tool and the briefing) and one on the inbox, for as long as the view stays on screen. Whether any host feeds app-initiated tool results back into model context is still unverified (see the unresolved question above); if one does, these refreshes would reach it too.
- Possible follow-up, not built: an opt-in that pushes a short "needs you" message into the conversation, limited to changes that need the person. That spends model tokens on every push, so it should stay off by default.

## Security

- Escape every string. Task titles, notes, reasons and paths are teammate-written. Use `textContent`, never `innerHTML` with data. Clip long strings (hub bounds already apply: 500 bytes).
- No tokens, no hub address inside the view or the resource.
- `resources/read` uses the same bearer-token check and read-only gate as `tools/call`.
- Cap each resource at 256 kB; reject builds that exceed it.
- Serve content with a version in the URI query or `_meta` so hosts can cache; log every `resources/read` with the principal.

## Acceptance criteria

1. A client without the UI extension gets byte-identical `initialize` output to today. `tools/list` is stateless over HTTP, so it also carries `_meta.ui` and the `agentscm_view_settings` tool for every client; hosts that ignore unknown metadata are unaffected.
2. A client with it sees `resources` and `_meta.ui` on exactly the two tools; `resources/list` lists exactly two URIs.
3. A read-only token can `resources/read`; a bad token cannot. A write tool cannot be called from a view.
4. Rendering fixtures: complete, partial (`current_complete` false), overlap incomplete, retention gap, empty, 100 items, long strings, markup in task titles (must display as text).
5. Both views pass a dark-mode and 375 px width check and show text labels for every status.
6. Manual check in Claude Desktop and VS Code Copilot with a real seeded hub; record what each host does with Refresh results and whether it keeps offscreen iframes running.

## Order of work

1. Capability check plus `resources/list` and `resources/read` with a stub view; Rust tests for the criteria above.
2. Work-overview view, then verify in two hosts.
3. Inbox view.
4. Decide on more views only after step 2 results.

## Not verified

Per-host size limits, whether any host adds app-initiated results to model context, and Claude Code behaviour.
