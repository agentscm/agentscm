---
name: AgentSCM Hub
description: A calm, read-only coordination Hub for seeing who owns each piece of agent work.
colors:
  primary-light: "#4F5BD5"
  primary-dark: "#7C86FF"
  chrome-light: "#E3E6EF"
  chrome-dark: "#05060B"
  canvas-light: "#F4F5F9"
  canvas-dark: "#0F1220"
  surface-light: "#FFFFFF"
  surface-dark: "#171B2A"
  surface-head-light: "#F9FAFC"
  surface-head-dark: "#1B2032"
  elevated-dark: "#1A1E30"
  nav-active-light: "#D3D7F5"
  success: "#12B76A"
  warning: "#F79009"
  error: "#F04438"
  success-ink-light: "#067647"
  warning-ink-light: "#B54708"
  error-ink-light: "#B42318"
  success-ink-dark: "#6CE9A6"
  warning-ink-dark: "#FEC84B"
  error-ink-dark: "#FDA29B"
typography:
  display:
    fontFamily: "-apple-system, BlinkMacSystemFont, SF Pro Text, Inter, Segoe UI, Roboto, Helvetica Neue, Arial, sans-serif"
    fontSize: "clamp(24px, 2.5vw, 28px)"
    fontWeight: 700
    lineHeight: 1.12
    letterSpacing: "-0.025em"
  title:
    fontFamily: "-apple-system, BlinkMacSystemFont, SF Pro Text, Inter, Segoe UI, Roboto, Helvetica Neue, Arial, sans-serif"
    fontSize: "24px"
    fontWeight: 700
    lineHeight: 1.12
    letterSpacing: "-0.025em"
  body:
    fontFamily: "-apple-system, BlinkMacSystemFont, SF Pro Text, Inter, Segoe UI, Roboto, Helvetica Neue, Arial, sans-serif"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.5
  label:
    fontFamily: "-apple-system, BlinkMacSystemFont, SF Pro Text, Inter, Segoe UI, Roboto, Helvetica Neue, Arial, sans-serif"
    fontSize: "12px"
    fontWeight: 650
    lineHeight: 1.4
    letterSpacing: "0.03em"
rounded:
  sm: "8px"
  md: "12px"
  lg: "14px"
  xl: "20px"
spacing:
  xs: "4px"
  sm: "8px"
  md: "12px"
  lg: "16px"
  xl: "20px"
  section: "28px"
  section-spacious: "32px"
components:
  navigation:
    backgroundColor: "{colors.chrome-light}"
    activeColor: "{colors.nav-active-light}"
    typography: "{typography.body}"
    rounded: "{rounded.sm}"
    height: "40px"
    width: "248px"
  filter-field:
    backgroundColor: "{colors.surface-light}"
    textColor: "{colors.primary-light}"
    rounded: "{rounded.md}"
    height: "44px"
    padding: "0 12px"
  work-row:
    backgroundColor: "{colors.surface-light}"
    textColor: "{colors.primary-light}"
    rounded: "{rounded.sm}"
    padding: "12px 14px"
  primary-action:
    backgroundColor: "{colors.primary-light}"
    textColor: "{colors.surface-light}"
    rounded: "{rounded.md}"
    height: "44px"
---

# Design System: AgentSCM Hub

## Overview

**Creative North Star: "The Calm Work Register"**

AgentSCM Hub is a quiet coordination surface for teams running coding agents. Ownership and the reported next step receive the strongest visual weight, while status and history remain easy to inspect without turning the page into a long feed. The visual language inherits the Hub: neutral surfaces, a violet navigation accent, system sans typography, restrained rules, and semantic labels that accompany color.

The Work section is a focused expression of that world. Three plain primary tabs answer the stand-up jobs directly: Working now, Recently finished, and Needs coordination. Current work keeps the four progress lanes. Recent completions come from a server-bounded time window. Needs coordination leads with durable directed requests and their receipt, reply and resolution state; server-computed detected overlaps remain visibly separate. The fixed-watermark briefing remains available as a secondary Change history view.

**Key Characteristics:**
- Ownership, task, and reported action lead each work card.
- Neutral surfaces and fine borders carry hierarchy.
- Violet, rose, amber, and green fields communicate labelled work state.
- Four desktop progress lanes collapse into a mobile lane selector and one visible queue.
- Recently finished and coordination are first-class overview sections, with visible time and completeness boundaries.
- No new imagery or decorative assets are part of the Work surface.

## Colors

The palette is a neutral light/dark canvas with white or deep blue-black content surfaces, a restrained violet accent, and labelled green, amber, and rose state colors.

### Primary
- **Hub violet, light** (`#4F5BD5`): Navigation, links, focus outlines, lane emphasis, and active controls in the light theme.
- **Hub violet, dark** (`#7C86FF`): The same accent role in the dark theme.

### Neutral
- **Chrome, light / dark** (`#E3E6EF` / `#05060B`): The top bar and the side rail, one shared surface.
- **Sheet canvas, light / dark** (`#F4F5F9` / `#0F1220`): The page itself, inset in the chrome.
- **White surface** (`#FFFFFF`): Light content, card, and control surfaces.
- **Deep surface** (`#171B2A`): Dark content surfaces, with a 1px top highlight (`inset 0 1px 0 rgba(255,255,255,.04)`).
- **Box head band** (`--hub-surface-head`, `#F9FAFC` / `#1B2032`): The tinted head of every evidence box.
- **Rules** (`--hub-rule` / `--hub-rule-strong`): 10% / 18% ink in light, 8% / 15% white in dark.
- **Elevated dark surface** (`#1A1E30`): Dark elevated controls and overlays.
- **Theme text and border:** Use `--cb-text`, `--cb-text-2`, `--cb-text-3`, and `--cb-border` so contrast follows the active theme.

### Named Rules
**The Semantic Field Rule.** State fields use violet for active or handover, rose for blocked, amber for paused or stale, and green for finished or active assignment; every field includes text.

**The Colour For Meaning Rule.** State is carried by tone, never by grey: a change waiting to merge is violet, one still getting ready is muted, a fully passed landing is green, and failed or blocked is rose. A box takes the tone its data implies (contested files rose when any exist, otherwise green; pull requests touching held files amber when any; workflow runs and recent changes rose when one failed). Counts stay plain and every tone carries text.

**The Ownership First Rule.** Keep the lead agent and task legible before source identities, bookkeeping, and technical identifiers.

## Typography

**Display Font:** System sans (`-apple-system`, BlinkMacSystemFont, SF Pro Text, Inter, Segoe UI, Roboto, Helvetica Neue, Arial, sans-serif)
**Body Font:** The same system sans stack
**Label/Mono Font:** `ui-monospace`, SF Mono, JetBrains Mono, Menlo, Consolas, monospace for identifiers and paths only.

**Character:** Compact, direct, and highly legible. Weight and line-height establish hierarchy; the Work surface avoids display faces and ornamental type.

### Hierarchy
- **Answer** (550, 22px, 1.38, `-0.015em`, 68ch measure): The one-sentence answer under each page's question. Facts inside it are 700-weight links with a thin offset underline, coloured by tone; the question above it is a 14px 600 `--hub-text-3` heading.
- **Display** (700, `clamp(24px, 2.5vw, 28px)`, 1.12): Work detail titles.
- **Title** (700, 24px on narrow screens, 1.12): Mobile title treatment and detail headings.
- **Body** (400, 14px, 1.5): Card content, descriptions, action text, and history rows.
- **Label** (650, 12px, 1.4, `0.03em`): Lane counts, table headings, state labels, and compact metadata.
- **Supporting text** (400, 13px): Timestamps, source hints, and secondary ownership context.

### Named Rules
**The System Sans Rule.** Use the system sans for product copy and reserve monospace for identifiers, source paths, branches, and work IDs.

## Layout

The Work surface begins with section navigation, a direct heading, read-only load status, and plain Working now / Recently finished / Needs coordination tabs. Search and counts apply only to the returned overview slice. The overview identifies its request-time upper bound and exposes incomplete current, recent, or overlap slices rather than implying a global all-clear. While the page is visible, the overview refreshes every 15 seconds without resetting the selected tab or search. A failed refresh keeps the last readable snapshot and labels its load time and error.

Current work uses four desktop progress lanes: In progress, Blocked, Handover, and Paused. The board is a four-column grid with 14px gaps, 12px lane corners, 14px lane-heading and 10px card insets, and an internally scrolling height of `clamp(240px, calc(100dvh - 570px), 460px)`. Recently finished uses a two-column task grid with the exact overview window above it. Needs coordination starts with a fixed-watermark page of compact directed-request summaries. Each summary exposes the stage, proposal, requester-to-target relationship and first concrete path reason; its native disclosure reveals all returned paths, receipts, responses and notes. Detected overlaps, other attention signals and recent decisions remain separate, count-labelled disclosures that are collapsed by default.

At 860px and below, the board becomes two columns, directed-request targets become a single queue, and detected-overlap rows stack the reason above the involved work. Change history keeps its date column in the stacked record. At 640px and below, the Work surface uses 16px horizontal padding with an 8px side bleed, controls keep 44px targets, lane buttons wrap at a 110px basis, only the selected lane is visible as one scrollable queue, recently finished becomes one column, and each involved work item receives its own separated row. The search input remains visually present while its redundant label is visually hidden and remains available to screen readers.

## Density

**The Two Densities Rule.** The Hub has two density modes, chosen in Settings and stored per browser (`agentscm.density`, set on `<html data-density>` before first paint). **Spacious** is the default; **Compact** is the dense register for big screens and is exactly the layout the sections above describe. Spacious overrides tokens only, all in `ui/src/density.css` (tour card values at the end of `ui/src/tour/tour.css`); the rules in `styles.css` carry the compact value as the `var()` fallback, so Compact sets nothing.

| Token | Compact | Spacious (phone) |
| --- | --- | --- |
| Body size (`--bs-body-font-size`) | 14px | 15px |
| Section and page gap (`--hub-gap`) | 24px | 32px (26px) |
| Masthead space below (`--hub-answer-pad`) | 28px | 36px (28px) |
| Top bar (`--hub-top-h`) | 56px | 62px (56px) |
| Box head (`--hub-head-h`, `--hub-head-x`) | 52px, 18px | 58px, 22px (54px, 18px) |
| Box body (`--hub-pad-box`) | 16 18 18 | 20 22 22 (18 18 20) |
| List row (`--hub-row-y`) | 12px | 16px (14px) |
| Activity row (`--hub-event-y`) | 8px | 11px (10px) |
| Nav row, group gap (`--hub-nav-h`, `--hub-nav-group`) | 40px, 18px | 44px, 24px |
| Lane and work-card padding | 11 14 10 / 12 14 | 14 16 12 / 16 18 (12 14 11 / 14 16) |
| Tour card width, body line height | 400 (480 welcome), 1.5 | 440 (520), 1.6 |

Touch targets stay at 44px in both modes and phones get the lighter increase.

## Elevation & Depth

**The Three Planes Rule.** The Hub has three planes, each one tonal step apart, so navigation never reads as content: **chrome** (`#E3E6EF` / `#05060B`; the top bar and rail share one tinted surface with no dividing rules), the **sheet** (`#F4F5F9` / `#0F1220`; the content column, inset with a 14px top-left radius and a clearly visible edge, carrying the breadcrumbs as its own header line), and **surfaces** (`#FFFFFF` / `#171B2A`; boxes and cards on the sheet). Tiles inside a box are tonal (`--hub-sunken`), never bordered, so a box never holds more boxes. On screens up to 960px wide the rail becomes a drawer and the sheet loses its corner. `main` takes focus on navigation for assistive technology but shows no ring, because it isn't a control.

The Work register is flat and structural. Depth comes from tonal lane fields, a one-pixel `--cb-border` rule, white or deep content surfaces, and a small violet hover tint. The feature adds no shadow vocabulary or decorative gradient. Focus-visible targets use a 2px violet outline with a 3px offset, and reduced-motion users receive no row hover transition.

### Named Rules
**The Visible Step Rule.** Each plane is at least one visible tonal step from its neighbour in both themes; if nav, header and sections read as one grey field, raise the step, not the borders.

**The Masthead Rule.** The question, its one-sentence answer and any facts row form a masthead: the question carries its page icon in brand ink, the answer stays at reading size, and a full-width hairline with 28px of space below separates it from the content.

**The Head Band and Cap Rule.** Every box head is a `--hub-surface-head` band under a 2px top cap, the same vocabulary as the Work lanes. Ok, warn and bad boxes cap in their tone; brand boxes cap quietly (brand at low strength) and muted boxes in `--hub-rule-strong`.

**The Section Rhythm Rule.** Stacked boxes sit 24px apart (`--hub-gap`); the masthead hairline adds 8px before the first section.

**The Sentence Case Rule.** No tracked uppercase eyebrows: nav group heads, owner group heads, work-card Next and Blocker labels and table headings are 12px, weight 650, `--hub-text-3`, in sentence case. Inline links carry no appended arrow; arrows stay on real Next, Back and relationship glyphs.

**The Bounded Register Rule.** Keep the active page compact, keep pagination in the reading path, and let the fixed watermark define the briefing boundary.

**The Flat Work Rule.** Use tonal fields and restrained borders before adding elevation to a coordination row or detail section.

## Shapes

The Hub uses gently rounded controls and surfaces: 12px is the primary control and box radius, lanes use 12px, cards 10px, info bars 10px and nav rows 8px, the sheet's inset corner is 14px, and 20px remains the large theme radius. Lane headings follow the containing lane's corner. Focusable controls keep a minimum 44px height. The history table uses separators and stacked records rather than card decoration.

## Components

### Navigation rail
- **Shape:** A 248px rail (68px compact) on the chrome surface with no dividing rule. Group heads are sentence-case 12px 650 `--hub-text-3` labels; on desktop their explainers appear on hover or focus, and in the mobile drawer they are always visible. Rows are 40px, with a plain 20px icon in `--hub-text-3` and a 550-weight label.
- **Behavior:** The active row takes the `--hub-nav-active` tint, a violet icon and a 650 label, and shows the page's question beneath it, wrapped rather than truncated. Other rows keep their question in the accessible name only, since each page opens with its question as its heading. The mobile drawer has room for the questions and no hover, so it shows every one. Badges stay compact counts beside the label, or a corner dot in the compact rail.

### Top bar
- **Shape:** It sits on the chrome surface, with the search field and the header "Connect agent" action as white controls with hairline borders. The header action is deliberately secondary (an outline button with violet text) so each page's filled primary action stays the only filled violet on screen.

### Evidence boxes
- **Shape:** 12px-corner surfaces under a 2px tone cap, with a 52px head band (`--hub-surface-head`) of a plain 20px icon (tinted only for ok, warn or bad tones), a 16px 650 title, a plain count and an explainer at 0.8 rest opacity (at least 3:1 against the surface in both themes) that reaches full opacity when the box is hovered or the explainer focused, then a hairline rule over the body.
- **Behavior:** The head labels the data; it doesn't compete with it. Rows inside a box are separated by rules or proximity, not nested cards.

### Info bars
- **Shape:** One `Notice` pattern: a 10px-corner surface bar with a hairline in the tone and the tone on the icon. Warnings and errors add a soft tinted fill so problems still stand out. Retry and similar actions sit vertically centred at the end.

### Tabs and pills
- **Tabs:** One vocabulary across pages: an underline strip with a 2px violet indicator under the active tab, used for both button tabs and link tabs. Counts are small neutral or warning pills.
- **Pills:** State pills keep a soft tonal fill, an icon and text. Muted pills are metadata and use a hairline outline instead of a fill. Inside agent rows, the connection (`Hook · Codex`) is plain secondary text. Pills may wrap inside narrow cards rather than overflow.

### Progress lanes
- **Shape:** Four 12px-corner lanes on desktop with a sticky 76px heading and a scrollable card list; a defensive fifth finished lane may span the board when returned data requires it.
- **Content hierarchy:** Agent and login, task title, labelled state and claim, reported blocker or next action, participants, then last check-in.
- **State fields:** Each lane is a quiet tonal column whose state colour sits in a 2px top cap and the heading icon: violet for active and handover, rose for blocked (its heading text is rose too), amber for paused, and green for finished. Old or unknown check-ins say “still assigned.” Finished work has no assignment claim or warning.

### Work cards
- **Shape:** Neutral 10px-corner cards with 14px padding, a one-pixel border, and an 8px internal gap. The reported next action is text under a small label rather than a nested panel; only a blocker keeps a rose field. Check-in time and source share a ruled footer that wraps rather than splitting words.
- **Behavior:** Source identity is compact and secondary. The title link, lead, state, and reported action form the primary scan path; participant details follow when present. Cards carry the state label. In the In progress lane a live claim is the normal case, so the claim chip appears only when it needs attention (no recent or unknown check-in, still assigned). In Blocked, Handover and Paused, the card always shows the claim, Assigned or still assigned, because that is where an idle-looking task could be mistaken for available work.

### Recently finished
- **Shape:** A two-column task grid on larger screens and a single queue on narrow screens, preceded by the exact server-provided start and end times.
- **Behavior:** Completion is worded as agent-reported. The section explicitly says it does not confirm verification, review or merge. Clipped results and retained-history gaps are labelled incomplete, including when no completion remains visible.

### Coordination rows
- **Shape:** A flat, ruled row with the concrete overlap reason on one side and two equal involved-work columns on the other.
- **Behavior:** Reasons pair the server-returned paths directly. Each side identifies the agent, human owner, task link, and declared scope. The bounded path-witness list warns when additional overlaps were omitted and points to both full scopes. Missing scope and stale or unknown check-ins remain visible as separate attention signals. The interface never computes a global overlap from a displayed page or implies that detecting one sent a coordination request.

### Directed request queue
- **Shape:** A ruled, fixed-watermark request list. Each compact native disclosure summary carries the proposal, stage, requester-to-target route and a concrete overlap reason. Expanding it reveals current target leads in equal 12px-corner subregions with receipt state, response state, all returned path reasons, message and time. Pagination remains directly after the compact list.
- **Behavior:** Overall stages read Awaiting receipt, Awaiting replies, Replied, Resolved or Withdrawn. Receipt and response are evaluated against each target's recorded generation. The dashboard initially requests active items only; a checkbox includes resolved and withdrawn requests. The fresh first page refreshes every 15 seconds while visible, so agent replies can appear while someone watches. Following Previous or Next freezes the first-page watermark and every subsequent page to the same bounded snapshot; Refresh requests returns to a new first-page snapshot. Reading never acknowledges, replies to, resolves or withdraws a request; agents perform those actions through authenticated MCP or HTTP flows. Target path reasons and request receipt/response projections are bounded. Explicit warnings identify omitted paths or history, and an incomplete projection never becomes an inferred receipt or reply; the server request status remains authoritative.

### Search and history pagination
- **Shape:** Search and selects use 44px controls and a 12px radius. Mobile lane buttons wrap with 44px minimum height and an `aria-pressed` selected state.
- **Behavior:** Overview search and counts are scoped to the returned snapshot. Change-history search, filters, and counts are page-local. The dashboard remains read-only and uses GET overview, coordination-list, briefing, and work-detail boundaries only.

### Recent changes table
- **Shape:** A distinct five-column table with sticky headings, 12px vertical and 14px horizontal cells, and a separately bounded scroll region. At 860px it becomes stacked records while preserving title, owner, status, reported context, and date.
- **Behavior:** Dates use the viewer's local format. The dashboard does not acknowledge the briefing.

### Detail disclosure
- **Shape:** Reported blocker, next action, and last check-in appear as a three-cell action grid with a one-pixel border; the grid stacks on narrow screens.
- **Behavior:** Technical identifiers use a native “Technical details” disclosure and stay collapsed by default. Read-only detail inspection uses the authorized work ID path.

## Do's and Don'ts

### Do:
- **Do** keep four progress lanes as the normal desktop current-work model, with finished rendered only defensively when page data includes it.
- **Do** keep the three primary overview labels literal: Working now, Recently finished, and Needs coordination.
- **Do** show the exact recent window and explicit completeness state returned by the server.
- **Do** show overlap reasons, involved agents, declared scopes, and work links together.
- **Do** show durable directed requests before raw detected overlaps, with per-target receipt and reply state.
- **Do** lead with agent, task, and reported blocker or next action.
- **Do** pair every state field with an explicit text label and retain “still assigned” for stale or unknown claims.
- **Do** keep search, filters, and counts scoped to the fetched page and preserve fixed-watermark pagination.
- **Do** keep the search input visible on mobile, hiding only its redundant visual label while preserving its accessible name.
- **Do** preserve read-only GET boundaries, visible keyboard focus, and local history dates.

### Don't:
- **Don't** turn current work and recent changes into one chronological feed.
- **Don't** derive repository-wide overlaps from a visible page or claim an all-clear when the overview is incomplete.
- **Don't** imply that opening the dashboard acknowledged a request or that overlap detection alone delivered one.
- **Don't** imply that stale or unknown claims are available work.
- **Don't** show an assignment claim or warning on reported-finished work.
- **Don't** use status color without its text label.
- **Don't** introduce a large hero, new decorative imagery, or a second visual world inside this section.
