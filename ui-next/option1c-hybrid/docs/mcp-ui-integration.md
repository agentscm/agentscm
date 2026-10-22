# MCP and the Hub UI: integration design (proposal)

Status: proposal only. Nothing here is implemented. Researched October 2026; spec status changes quickly, so re-check the links before building.

## The question

"MCP operations on the UI itself" covers two different directions:

1. **Server to agent host.** The hub's MCP server returns interactive views (a work card, the directed-request queue, contested files) that Claude, ChatGPT, VS Code and similar hosts render inline in the conversation.
2. **Web page to browser agent.** The option 1c web UI exposes its own operations (navigate, search, open a work item, read the current view) as tools that an agent running in the browser can call.

By "the Google spec" the user most likely means **WebMCP** (`document.modelContext`). Google and Microsoft engineers co-author it, and it is the spec that lets a web page offer MCP-style tools from the UI itself. Google's other spec, **A2UI**, solves a different problem: agents *generating* UI as declarative JSON. Both are covered below.

## Spec landscape

| Spec | What it does | Status (Oct 2026) | Fit for AgentSCM |
|---|---|---|---|
| [MCP Apps, SEP-1865](https://modelcontextprotocol.io/seps/1865-mcp-apps-interactive-user-interfaces-for-mcp) (`io.modelcontextprotocol/ui`) | MCP servers ship HTML views as `ui://` resources; hosts render them in sandboxed iframes; views talk to the host over postMessage JSON-RPC | The SEP is marked Final (Extensions Track); [spec 2026-01-26](https://github.com/modelcontextprotocol/ext-apps/blob/main/specification/2026-01-26/apps.mdx) is labelled Stable. Support in ChatGPT, Claude, Goose and VS Code is reported second-hand ([pydantic-ai#6613](https://github.com/pydantic/pydantic-ai/issues/6613)); **not verified here** | **Direction 1.** The official standard; builds on MCP-UI |
| [MCP-UI](https://mcpui.dev) | The community project MCP Apps grew out of; SDKs (`@mcp-ui/server`, `@mcp-ui/client`) now target MCP Apps | Its legacy `UIResource` types (`rawHtml`, `externalUrl`, `remoteDom`) predate MCP Apps; **current SDK naming is uncertain** | Use as the SDK, not as a separate protocol |
| [WebMCP](https://webmachinelearning.github.io/webmcp/) (`document.modelContext`) | A page registers tools (`registerTool`) that browser agents discover (`getTools`) and run (`executeTool`) | W3C Web Machine Learning CG Draft Community Group Report, 20 Oct 2026, **not on the Standards Track**. Chrome preview in Feb 2026, then an origin trial (around Chrome 149, reported). The entry point moved from `navigator.modelContext` to `document.modelContext`; dates differ between sources. Firefox and Safari have announced nothing | **Direction 2.** Feature-detect it and treat it as experimental |
| [A2UI](https://a2ui.org/specification/v0.9-a2ui/) (Google) | Agents send declarative JSON component trees that the client renders with its own native components; no agent-supplied code runs | v0.9.1 is recommended; [v0.9 announcement](https://developers.googleblog.com/a2ui-v0-9-generative-ui/). Mainly carried over A2A and AG-UI. **Whether it has an MCP transport binding is unverified** | Watch only. The hub already owns its views, so it doesn't need agent-generated UI |
| OpenAI Apps SDK | ChatGPT's app surface | Reported to now align with MCP Apps; **unverified** | Covered by MCP Apps |

## Recommendation

- **Direction 2 first: WebMCP tools in the 1c UI, read-only.** It is the smallest useful step: frontend only, no hub or protocol change. It reuses the data the page has already loaded, and it is exactly the "operate the UI" capability being asked about. It is feature-detected, so browsers without WebMCP see no change.
- **Direction 1 second: MCP Apps views on the hub's read-only tools.** This needs hub changes: the server currently implements only `initialize`, `ping`, `tools/list` and `tools/call`, with no `resources/*`. The views would be the same evidence boxes as 1c, rendered inside agent hosts.
- **Don't adopt A2UI now.** The hub serves fixed, trusted views rather than generated ones. Revisit if a host the team uses renders only A2UI.

## Direction 1: hub MCP server serving views (MCP Apps)

### How it works

- A tool declares `_meta.ui.resourceUri` (the older flat `_meta["ui/resourceUri"]` key is deprecated).
- The resource uses MIME type `text/html;profile=mcp-app` and is a complete HTML document. The host renders it in a sandboxed iframe.
- The view receives the tool's result through `ui/notifications/tool-result` and can call `tools/call`, `resources/read`, `ui/message`, `ui/open-link` and `ui/update-model-context`.
- `_meta.ui.visibility` is `["model"]`, `["app"]` or both. Hosts must reject an app's call to a tool that lacks `"app"`.
- With no `csp` declared, the host applies `connect-src 'none'`. That suits us: the view never needs the hub's network address or a token.
- Servers check that the host advertised `capabilities.extensions["io.modelcontextprotocol/ui"]` before attaching UI metadata, and always keep the text result as a fallback.

### Views mapped to existing tools

These are the read-only token's tools, from `read_only_call` in `src/hub/mcp.rs`:

| Resource | Attached to tool | Data it renders | 1c component reused |
|---|---|---|---|
| `ui://agentscm/work-overview` | `agentscm_work_overview` | Current work by lane, recently finished, overlaps, completeness flags | `pages/Work.tsx` `Board`, `Finished`, `OverlapRow` |
| `ui://agentscm/coordination-inbox` | `agentscm_coordination_inbox` | Directed requests with stage, per-target receipt and reply | `pages/Work.tsx` `Request`, `RequestQueue` |
| `ui://agentscm/briefing` | `agentscm_work_briefing` | Change history page | `pages/Work.tsx` `History` |
| `ui://agentscm/workflow-run` | `agentscm_workflow_status` | Run steps, observations, findings | `pages/Workflows.tsx` run graph and step detail |

The contested-files view is computed in the browser by `derive.ts` from `/work` and `/activity`; no MCP tool returns it. Add a read-only `agentscm_status` view only after that tool's output covers holdings and refusals. `agentscm_status` exists today but is writer-only.

### Calls a view may make

- **Phase 1: views are read-only.** A view may call only the same read-only tools, with `visibility: ["model","app"]`, to refresh or page (for example the fixed-watermark "Next page"). That matches the dashboard rule that reading never acknowledges, replies or resolves anything.
- **Writes stay with the agent.** `agentscm_coordination_ack`, `agentscm_coordination_reply`, `agentscm_work_checkpoint` and the rest stay model-only (`["model"]`), called by the agent with its writer token. If buttons are wanted later (for example "Acknowledge"), the view should send `ui/message` asking the agent to do it, not call the write tool itself. That keeps every write tied to an agent session, its compare-and-set fence and idempotency key, and the host's approval prompt.

### Hub changes needed

- `initialize`: read the client's `capabilities.extensions`, and advertise `resources` when UI is supported.
- Add `resources/list` and `resources/read`. Both are read-only methods, so add them to `read_only_call`.
- Add `_meta.ui` to the four tools in `tools()`.
- Build the views: one HTML file per view, with its CSS and JS inlined and no external origins. The hub's existing `/ui/` Content-Security-Policy model does not apply inside the host's sandbox.
- **Size:** the 1c bundle is about 365 kB of JS plus 345 kB of CSS (mostly Bootstrap). Views should be a small separate build. **Size limits per host are unknown.**

## Direction 2: the 1c web UI exposing tools (WebMCP)

### Tools to register

All tools are read-only or navigation-only, annotated `readOnlyHint: true`. Every tool that returns teammate text also sets `untrustedContentHint: true`.

| Tool | Input | Does | Backed by |
|---|---|---|---|
| `hub_navigate` | `section` (enum: now, work, activity, workflows, workspace, setup, health, settings), optional `view` (finished, coordination, history) | Sets the hash route | `App.tsx` `SECTIONS`, Work's hash tabs |
| `hub_search` | `query` (string, max 200 characters) | Returns the same hits as the ⌘K palette: pages, agents, held files, work | `TopNav.tsx` `CommandPalette` hit list (extract it into a shared function) |
| `hub_open_work` | `work_id` (`wrk_…`) | Navigates to `#work/<id>` and returns the item | `workHref`, `fetchStandupWork` |
| `hub_read_view` | none | Returns a structured summary of the current section: the Right-now sentence, counts, lanes, requests, plus each section's completeness flags | `useHub` data, `derive()`, overview poll |
| `hub_list_contested` | none | Contested files with holders and handoff state | `derive().contested` |
| `hub_coordination_requests` | `include_closed` (boolean) | The page of directed requests already loaded | `fetchCoordination` |

### Implementation sketch

- In `App.tsx`'s `Shell`, call `(document.modelContext ?? navigator.modelContext)?.registerTool(...)` once per tool, passing an `AbortController` signal that is aborted on unmount and on sign-out. The `navigator` fallback is for older origin-trial builds.
- Do nothing when neither is present.
- Each `execute` reads the React state already loaded and returns a JSON string. No new hub requests, apart from `hub_open_work` reusing the existing work-detail GET.

## Data and security model

- **Tokens:**
  - In the web UI the read-only token lives in browser storage. No WebMCP tool returns it, takes it as input, or returns `whoami.token.id`; the token's display name is enough.
  - In MCP Apps views the token never reaches the view at all. Data arrives through the host from the tool result.
- **Read-only:** both directions only read or navigate. The hub already refuses non-read tools for read-only tokens (`read_only_call`), so a mistake in the UI cannot escalate.
- **Prompt injection:** task titles, notes, checkpoint summaries, reasons and handoff notes are all teammate-supplied (as `api.ts` already says), so an agent reading them can be manipulated.
  - Mark those results `untrustedContentHint` (WebMCP) and return them as data fields, never as instructions.
  - Clip their length, as the hub's existing 500-byte text bounds already do.
  - Keep tool descriptions free of anything dynamic.
- **Completeness:** tool results carry the server's completeness flags (`current_complete`, `overlaps_complete`, retention gaps), so an agent can't treat a clipped page as an all-clear. This is the same rule as in PRODUCT.md.
- **No acknowledgement by reading:** neither direction calls `agentscm_work_ack` or `agentscm_coordination_ack`. Reading must stay non-mutating.
- **Origin:** WebMCP is gated by the `tools` Permissions Policy, which defaults to `'self'`. The hub's `/ui/` CSP is unaffected, because tools run in the page's own script.

## Phased plan

1. **Smallest useful step:**
   - Add WebMCP `hub_navigate`, `hub_read_view` and `hub_search` to 1c, feature-detected.
   - Test them with a stub `document.modelContext` in the existing node test scripts, and with Chrome's WebMCP origin trial if available.
   - About 100 lines in a new `src/webmcp.ts` plus a hook in `Shell`. No hub change.
2. **The rest of the WebMCP set:** `hub_open_work`, `hub_list_contested` and `hub_coordination_requests`, plus the injection annotations.
3. **One MCP Apps view on the hub:**
   - Add `resources/list` and `resources/read`, the host-capability check, and `ui://agentscm/work-overview` attached to `agentscm_work_overview`.
   - Verify in one host that has MCP Apps (Claude or VS Code) while keeping the text fallback.
4. **Remaining views:** the coordination inbox, the briefing and the workflow run. App-callable paging on the same read-only tools.
5. **Optional, needs maintainer agreement:** view buttons that ask the agent, via `ui/message`, to acknowledge or reply. Writes always stay with agent tokens.

## Open questions

- Which hosts the team uses, and whether each one actually renders MCP Apps today (the support claims above are second-hand).
- How MCP Apps views reach a hub that agents connect to over HTTP with a bearer token. The data flows through the host, which is fine, but `ui/open-link` targets back to `/ui/#work/<id>` need the hub's public URL.
- Whether to ship the views from the Rust binary (embedded like `ui/dist`) or as a separate asset.
- WebMCP's final API name and lifecycle: `navigator` versus `document`, and the origin-trial end date. Re-check before shipping phase 1 outside a feature flag.
- Whether A2UI gains an MCP binding that a target host requires.
- Per-host limits on resource size and on CSP/`connectDomains`.

## Sources

- [SEP-1865: MCP Apps](https://modelcontextprotocol.io/seps/1865-mcp-apps-interactive-user-interfaces-for-mcp)
- [MCP Apps spec 2026-01-26 (ext-apps)](https://github.com/modelcontextprotocol/ext-apps/blob/main/specification/2026-01-26/apps.mdx)
- [pydantic-ai#6613 (host support claims)](https://github.com/pydantic/pydantic-ai/issues/6613)
- [WebMCP draft (W3C WebML CG)](https://webmachinelearning.github.io/webmcp/)
- [WebMCP technical notes (W3C CG)](https://w3c-cg.github.io/aikr/webMCP/webmcp-technical-notes.html)
- [State of WebMCP, July 2026 (secondary)](https://www.spronta.com/blog/state-of-webmcp-july-2026/)
- [A2UI v0.9 specification](https://a2ui.org/specification/v0.9-a2ui/)
- [A2UI v0.9 announcement, Google Developers Blog](https://developers.googleblog.com/a2ui-v0-9-generative-ui/)
