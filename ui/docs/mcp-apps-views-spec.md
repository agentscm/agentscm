# MCP Apps views for the Hub

The work overview is shown only when a person asks to see the work. Agent planning, paging and coordination polling return data without mounting cards. Synthetic HTTP and fake-host tests cover this contract; mounting and delivery in real MCP Apps hosts remain unverified.

## Tools and resources

- `agentscm_show_overview` is a read-only tool for an explicit request from a person. It returns full overview data and full `actionable_status`, without requesting a generated briefing. When `enabled` and `work_overview` are true, its `_meta.ui.resourceUri` is `ui://agentscm/work-overview`.
- `agentscm_work_overview` and `agentscm_coordination_inbox` carry no `_meta.ui`. Agents keep using these data tools for their own reads.
- `agentscm_value_metrics` keeps its existing `ui://agentscm/value-metrics` card and settings behavior.
- `resources/list` offers the overview and value-metrics HTML when both switches permit them. There is no inbox resource. `resources/read` uses the same authentication and read-only authorization as tools; unknown or disabled URIs return invalid parameters.
- `initialize` advertises resources when the client declares the `io.modelcontextprotocol/ui` extension and views are enabled. HTTP tool lists are stateless and include view metadata for all clients; a host that ignores it still gets text and structured results.

The tool description conveys the human-request boundary to the agent; the Hub cannot independently authenticate that a person requested a tool call. Disabling views removes metadata and resources, while tools continue returning data.

## Settings compatibility

The Hub default and each person’s override retain `enabled` and `work_overview`. The latter controls overview and value-metrics cards. The retired `coordination_inbox` field is ignored only when loading persisted settings, preserving the remaining choices and null inheritance; it is omitted when settings are saved. New settings requests remain strict. The Settings page offers the master and overview switches, with per-user choices taking precedence over the administrator’s Hub default.

## Overview presentation

The self-contained HTML uses the shared actionable-status projection. It leads with active-task and needs-you counts, lists actions for the person, and folds all work and evidence beneath them. Claim freshness, held reservations, provider coverage and self-reported outcomes remain distinct. Incomplete coverage stays visible.

Before the first result, the card says “Loading work overview…”. A rejected tool call displays its text as an error, including on the first result. RPC errors and malformed results also show an error with Refresh available. A successful, complete result with no work or actions has an explicit empty state. Failed refreshes preserve the last successful data, its timestamp and the error; a later success clears the error.

Every untrusted string is rendered as text, never parsed as markup. Long task titles are clipped. The card retains the existing Twilight colors, focus styles and reduced-motion treatment. It has no external dependencies, tokens or Hub address.

## Refresh behavior

Refresh and the quiet timer call only `agentscm_work_overview` with `detail: "full"`, never the show tool. A compact initial result triggers a full data read. No refresh can advertise another card through tool metadata.

- Refresh runs 30 seconds after a successful result while the card is visible. Failures double the interval up to five minutes, and success resets it.
- Document visibility and intersection pause the timer. Returning refreshes when the interval has elapsed.
- Only one call is in flight; repeated manual refreshes queue one follow-up.
- Open folds, focused controls and scroll position survive repainting. Changed summaries and errors are announced politely.
- The card sends no conversation messages, acknowledgements, checkpoints or other writes. Opening it never marks anything reviewed.

## Qualification boundaries

Rust integration tests verify tool lists, settings, authorization and resource exposure. UI fake-host tests verify initial loading, tool and RPC errors, malformed and empty results, recovery, full-detail reads, timer/backoff, single-flight behavior, text safety and focus preservation. The dependency-free `node tools/mcp-views-check/check.mjs` checks the overview resource with a minimal DOM stub and the shared synthetic status fixture.

These checks do not establish how a live host mounts cards, handles resizing, delivers initialization/results, suspends offscreen frames or includes app-initiated tool results in model context. Those remain host-specific runtime qualification.

## Observed contribution view

`ui://agentscm/value-metrics` extends the resource set above. It uses the same `agentscm.observed.v1` structured projection as `agentscm_value_metrics` and the Hub's observed contribution section. It is enabled with the existing Work overview view setting. Repository-token reads remain repository-scoped. The HTML contains no credentials, external assets or additional data requests; refresh and evidence pagination use only `agentscm_value_metrics` through the host.

The view follows the Hub's Twilight palette with host-provided light/dark appearance. A human-readable repository and UTC period lead the page. Aligned contribution counts and recorded resources follow, while one coverage statement keeps uncertainty visible. Coverage reasons, exact source and evidence identities, assurance/applicability, accounting bases, raw micros and projection identity are available in native keyboard-accessible `details` disclosures. Charge and reference-price amounts remain separate, with exact integer-micro decimal formatting even below one cent. Missing values remain unknown. Evidence summaries use the evidence type and state; long identities appear in detail. At narrow iframe widths, the sections follow the same reading order in one column.

The currently unsupported outcome categories—dismissed episodes, false-trigger classifications, failed follow-ups and cancelled follow-ups—display `Not captured` when their fields are null. Other missing observations remain `Unknown`; a supported recorded zero remains `0`.

The contribution view clears earlier evidence on a refused or incompatible read. Each app-initiated request has a monotonically increasing epoch; older success and failure responses cannot overwrite the current request or Work filter. MCP tool results carry `_meta["agentscm/value-metrics-scope"]` with their original `work_id`, `since` and `until` arguments so a different scope cannot replace the current view.

Unsolicited host tool-result notifications do not carry a correlated request generation. The app therefore accepts only its initial host delivery. An explicit refresh, pagination, or subsequent tool-input notification permanently fences earlier unsolicited host results for that iframe. A new input clears the old display; the user can refresh to retrieve the new scope through an epoch-bound request. Repeating the same input after a refusal cannot make a late old host result current again. This preference for an explicit read preserves the access boundary when notification ordering is ambiguous.

The view makes no read solely because it rendered, never acknowledges context and never changes receipt preferences, Work or merge authority. Local bridge rendering and transport tests do not establish qualification in an installed native MCP app host.
