---
name: AgentSCM Hub (Twilight)
description: A calm workroom for agent teams. A deep-violet frame holds a quiet sheet; each page answers its question in one sentence, then shows one picture of the answer.
colors:
  frame-light: "#17142e"
  frame-dark: "#0b0a17"
  frame-ink: "#ece9ff"
  frame-dim-light: "#aaa5d2"
  frame-dim-dark: "#9e99c6"
  frame-active-icon: "#b3abff"
  canvas-light: "#fafafc"
  canvas-dark: "#111119"
  surface-light: "#ffffff"
  surface-dark: "#17171f"
  sunken-light: "#f4f3f9"
  sunken-dark: "#1e1e29"
  text-light: "#1d1c28"
  text-2-light: "#4f4e63"
  text-3-light: "#6c6b80"
  text-dark: "#ededf4"
  text-2-dark: "#babacb"
  text-3-dark: "#9090a6"
  rule-light: "#efeef4"
  rule-strong-light: "#dddce6"
  rule-dark: "rgba(255, 255, 255, 0.06)"
  rule-strong-dark: "rgba(255, 255, 255, 0.13)"
  brand-light: "#6d5ce0"
  brand-ink-light: "#5947cc"
  brand-soft-light: "#f1eefd"
  on-brand-light: "#ffffff"
  brand-dark: "#a796ff"
  brand-ink-dark: "#c6bbff"
  brand-soft-dark: "rgba(167, 150, 255, 0.13)"
  on-brand-dark: "#16132a"
  coral-light: "#c8432a"
  on-coral-light: "#ffffff"
  coral-dark: "#ff7a5c"
  on-coral-dark: "#1c0f0b"
  ok-light: "#127a52"
  ok-soft-light: "#e7f6ef"
  ok-fill-light: "#2fb67c"
  warn-light: "#9a5a06"
  warn-soft-light: "#fdf2e2"
  warn-fill-light: "#eea341"
  bad-light: "#c0391f"
  bad-soft-light: "#fdeee9"
  ok-dark: "#6fdba6"
  ok-soft-dark: "rgba(80, 200, 140, 0.12)"
  ok-fill-dark: "#52c98f"
  warn-dark: "#f6bd7a"
  warn-soft-dark: "rgba(246, 170, 90, 0.12)"
  warn-fill-dark: "#f5aa5a"
  bad-dark: "#ff9c84"
  bad-soft-dark: "rgba(255, 122, 92, 0.13)"
typography:
  answer:
    fontFamily: "Onest, system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "clamp(26px, 2.8vw, 34px)"
    fontWeight: 300
    lineHeight: 1.25
    letterSpacing: "-0.025em"
  answer-condensed:
    fontFamily: "Onest, system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "21px"
    fontWeight: 300
    lineHeight: 1.3
    letterSpacing: "-0.025em"
  headline:
    fontFamily: "Onest, system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "22px"
    fontWeight: 300
    lineHeight: 1.4
    letterSpacing: "-0.02em"
  title:
    fontFamily: "Onest, system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "18px"
    fontWeight: 600
    letterSpacing: "-0.015em"
  tab:
    fontFamily: "Onest, system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "15px"
    fontWeight: 600
    letterSpacing: "-0.01em"
  body:
    fontFamily: "Onest, system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "14.5px"
    fontWeight: 400
    lineHeight: 1.5
    fontFeature: "'tnum' 1"
  body-condensed:
    fontFamily: "Onest, system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "13.5px"
    fontWeight: 400
    lineHeight: 1.5
    fontFeature: "'tnum' 1"
  label:
    fontFamily: "Onest, system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "12.5px"
    fontWeight: 500
  explain:
    fontFamily: "Onest, system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "13px"
    fontWeight: 400
    lineHeight: 1.45
  numeral:
    fontFamily: "Onest, system-ui, sans-serif"
    fontSize: "20px"
    fontWeight: 300
    lineHeight: 1.1
    letterSpacing: "-0.02em"
  numeral-dark:
    fontFamily: "Unbounded, Onest, sans-serif"
    fontSize: "17px"
    fontWeight: 300
    lineHeight: 1.1
    letterSpacing: "-0.02em"
  mono:
    fontFamily: "JetBrains Mono, ui-monospace, SF Mono, Menlo, Consolas, monospace"
    fontSize: "12.5px"
    fontWeight: 400
rounded:
  sheet-corner: "26px"
  card: "20px"
  card-condensed: "14px"
  menu: "18px"
  tile: "16px"
  tile-condensed: "12px"
  notice: "14px"
  nav: "12px"
  nav-condensed: "8px"
  tag: "6px"
  segment: "5px"
  pill: "999px"
  round: "50%"
spacing:
  gap: "32px"
  gap-condensed: "16px"
  gap-mobile: "20px"
  top-bar: "62px"
  rail: "236px"
  rail-compact: "72px"
  sheet-top: "44px"
  sheet-top-condensed: "22px"
  sheet-top-mobile: "26px"
  sheet-side: "clamp(20px, 4vw, 60px)"
  sheet-side-condensed: "clamp(14px, 2vw, 28px)"
  sheet-side-mobile: "16px"
  answer-gap: "40px"
  answer-gap-condensed: "18px"
  answer-gap-mobile: "24px"
  card-inset: "26px"
  card-inset-condensed: "16px"
  card-inset-mobile: "18px"
  row-y: "16px"
  row-y-condensed: "9px"
  nav-row: "40px"
  nav-row-condensed: "32px"
  lane-bar: "34px"
  lane-bar-condensed: "28px"
  page-max: "1160px"
  page-max-condensed: "1440px"
components:
  answer:
    textColor: "{colors.text-light}"
    typography: "{typography.answer}"
    width: "28ch"
  answer-condensed:
    typography: "{typography.answer-condensed}"
    width: "60ch"
  answer-link:
    textColor: "{colors.brand-ink-light}"
  answer-link-trouble:
    textColor: "{colors.coral-light}"
  box:
    backgroundColor: "{colors.surface-light}"
    rounded: "{rounded.card}"
    padding: "22px 26px 26px"
  box-condensed:
    rounded: "{rounded.card-condensed}"
    padding: "12px 16px 14px"
  since-card:
    backgroundColor: "{colors.surface-light}"
    rounded: "{rounded.card}"
    padding: "24px 28px 18px"
  since-card-condensed:
    rounded: "{rounded.card-condensed}"
    padding: "12px 16px 10px"
  lane-claimed:
    backgroundColor: "{colors.brand-soft-light}"
    textColor: "{colors.brand-ink-light}"
    rounded: "{rounded.segment}"
    height: "34px"
  lane-now:
    backgroundColor: "{colors.brand-light}"
    textColor: "{colors.on-brand-light}"
    rounded: "{rounded.segment}"
    height: "34px"
  lane-now-handover:
    backgroundColor: "{colors.brand-soft-light}"
    textColor: "{colors.brand-ink-light}"
  lane-now-quiet:
    backgroundColor: "{colors.warn-soft-light}"
    textColor: "{colors.warn-light}"
  lane-now-blocked:
    backgroundColor: "{colors.coral-light}"
    textColor: "{colors.on-coral-light}"
  lane-later:
    backgroundColor: "{colors.sunken-light}"
    textColor: "{colors.text-3-light}"
    rounded: "{rounded.segment}"
    height: "34px"
  map-tile:
    backgroundColor: "{colors.sunken-light}"
    textColor: "{colors.text-light}"
    rounded: "{rounded.tile}"
    padding: "16px 18px 14px"
    height: "116px"
  map-tile-condensed:
    rounded: "{rounded.tile-condensed}"
    padding: "10px 12px"
    height: "88px"
  crossing-pill:
    backgroundColor: "{colors.coral-light}"
    textColor: "{colors.on-coral-light}"
    rounded: "{rounded.pill}"
    padding: "2px 8px"
  area-drawer:
    backgroundColor: "{colors.sunken-light}"
    rounded: "{rounded.tile}"
    padding: "20px 22px 18px"
  notice:
    backgroundColor: "{colors.sunken-light}"
    rounded: "{rounded.notice}"
    padding: "12px 16px"
  status-dot:
    textColor: "{colors.text-2-light}"
    typography: "{typography.label}"
    size: "7px"
  tag-muted:
    textColor: "{colors.text-2-light}"
    rounded: "{rounded.tag}"
    padding: "0 7px"
  section-tab:
    textColor: "{colors.text-2-light}"
    typography: "{typography.tab}"
    padding: "10px 4px 14px"
  section-tab-open:
    textColor: "{colors.text-light}"
  inbox-segment:
    backgroundColor: "{colors.sunken-light}"
    rounded: "{rounded.segment}"
    height: "10px"
  button-primary:
    backgroundColor: "{colors.brand-light}"
    textColor: "{colors.on-brand-light}"
    rounded: "{rounded.pill}"
    padding: "0 14px"
  button-primary-hover:
    backgroundColor: "{colors.brand-ink-light}"
  button-outline:
    textColor: "{colors.text-light}"
    rounded: "{rounded.pill}"
    padding: "0 14px"
  button-outline-hover:
    backgroundColor: "{colors.brand-light}"
    textColor: "{colors.on-brand-light}"
  chip-active:
    backgroundColor: "{colors.text-light}"
    textColor: "{colors.surface-light}"
    rounded: "{rounded.pill}"
  field:
    backgroundColor: "{colors.surface-light}"
    textColor: "{colors.text-light}"
    rounded: "{rounded.pill}"
    padding: "0 14px"
  nav-item:
    backgroundColor: "{colors.frame-light}"
    textColor: "{colors.frame-dim-light}"
    rounded: "{rounded.nav}"
    height: "40px"
  nav-item-active:
    backgroundColor: "rgba(255, 255, 255, 0.13)"
    textColor: "{colors.surface-light}"
---

# Design System: AgentSCM Hub (Twilight)

## Overview

**Creative North Star: "The Calm Workroom"**

A deep-violet frame (the top bar and the nav rail) holds one quiet content sheet. The sheet's only shaped corner is a large 26px curve at its top left, where it meets the frame. Every page opens with one sentence that answers the page's question, then shows one picture of that answer: on the Dashboard, the track of lane bars (Now) or the tiled map of the repository (Where). The question itself sits under the active rail item and in a visually hidden h1.

The frame stays violet in both themes; only its depth changes. Inside the sheet, light mode is a near-white sheet with soft violet and light font weights, and dark mode is a violet-grey dusk sheet with lavender. Theme is `data-bs-theme="light|dark"` on `<html>`, chosen as Light, Dark or Match this device (which follows `prefers-color-scheme`). Density is `data-density="spacious|condensed"`, set on `<html>` from the saved preference before first render; Spacious is the default and only Condensed overrides anything. The world sits on Bootstrap 5.3 and the Hub's `--hub-*` custom properties. `ui/src/twilight.css` loads after `ui/src/styles.css`, re-points the shared tokens and restyles the shared components; this file describes the result.

The world rejects the KPI-card grid and the dense ruled table. Boxes are soft, frameless cards with plenty of air, lists use hairline separators, and colour is held back for state. The Hub is read-only, and the design says so plainly: the rail footer reads "Read-only. Opening a page never changes anything for your agents."

**Key Characteristics:**
- Deep-violet frame, a light or dusk sheet, and one 26px corner where they meet.
- A sentence answer leads each page. Nothing visible sits above it.
- Brand violet marks progress, the current selection and focus. Coral marks trouble and nothing else.
- Onest for every word, Unbounded only for dark-mode tally numerals, JetBrains Mono only for paths, branches and identifiers.
- Two densities share every component. Condensed changes measurements, never structure.
- One authored motion: a task's current stage sweeps in on its lane bar, carrying the holder's face.

## Colors

Violet carries the structure, near-neutral violet-greys carry the reading, and one warm coral is kept for trouble.

### Primary
- **Twilight Frame** (frame-light / frame-dark): the top bar and nav rail, and the page background behind the sheet. It goes a step deeper in dark mode so the dusk sheet still lifts off it. Names on the frame use **Frame Ink**; resting nav, secondary text and frame icons use **Frame Dim**; the active nav icon and frame focus outlines use **Frame Active Icon**.
- **Twilight Violet / Dusk Lavender** (brand-light / brand-dark): the "Now" segment of a lane bar, the selected map tile ring, the open tab's underline, merge dots on the catch-up strip, focus outlines, primary buttons, checked boxes, and the hover fill of outline buttons. **Brand Ink** is the text-weight violet (links, the Claimed segment's text, primary-button hover). **Brand Soft** is the tint behind the Claimed segment, a handover, brand notices and the halo of a brand status dot. **On Brand** is the text that sits on a violet fill.

### Secondary
- **Coral** (coral-light / coral-dark): trouble. A blocked task's Now segment, the crossing pill and crossing tile ring, a failure dot on the catch-up strip, the count of a box whose tone is bad, a bad status dot, the "needs you" and crossing links in the answer, and the rail and bell badges. **On Coral** is the text on a coral fill: white in light mode, near-black (#1c0f0b) in dark mode, where coral is bright. On the always-dark frame, badges use the bright coral in both themes.

### Tertiary
- **Status** (ok / warn / bad, each with soft and, for ok and warn, fill versions): the fill is the status dot (green for active or observed, amber for paused, quiet or waiting); the soft tint is the dot's 3px halo and the background of a toned notice; the plain token is status text. A paused or quiet task's Now segment is warn-soft with a warn-fill ring.
- **Avatar hues**: each avatar's fill is derived from a hash of the agent's name (`hsl(h 52% 56%)` with white initials in light mode, `hsl(h 60% 70%)` with #16132a initials in dark). Hues tell agents apart; they never carry state.

### Neutral
- **Canvas** (canvas-light / canvas-dark): the sheet's own ground.
- **Surface** (surface-light / surface-dark): boxes, the Since card, fields, the active segment of a segmented control, and the ring that separates overlapping avatars and dots.
- **Sunken** (sunken-light / sunken-dark): map tiles at rest, the area drawer, notices, segmented-control tracks, inline path chips, skeleton lines and lane segments not reached yet.
- **Ink ramp** (text / text-2 / text-3, both modes): primary reading; secondary reading and status words; counts, times, explanations and column headings.
- **Rules** (rule / rule-strong): hairlines between list rows; then the tab-strip baseline, outline-button borders, field borders, the catch-up rails and the outline of a lane segment with nothing reported.

### Named Rules
**The Coral Is Trouble Rule.** Coral means "a person should look here". It is used for blocked work, crossings, failures and bad counts, and for the answer's links when they point at one of those. Every other link, including a non-trouble link in the answer, is Brand Ink. Never use coral for brand, decoration or neutral time markers.

**The Frame Never Changes Hue Rule.** Light and dark swap the sheet, not the world. The frame is deep violet in both modes and only changes depth.

**The Readable Coral Rule.** Light-mode coral is the deeper #c8432a: 4.69:1 as text on the canvas and 4.89:1 under white text, so it passes AA at every size the build uses. Dark-mode coral is bright, so text on a coral fill goes dark instead of white.

**The Dot Plus Word Rule.** Status is never colour alone. Every dot sits beside a word, and every toned segment or tile carries its label or an accessible name.

## Typography

**Body Font:** Onest variable (with system-ui, -apple-system, Segoe UI, Roboto, sans-serif)
**Numeral Font:** Unbounded 300 (with Onest), for dark-mode tally numerals only
**Mono Font:** JetBrains Mono 400 (with ui-monospace, SF Mono, Menlo, Consolas), for paths, branches and identifiers only

All three are self-hosted from the `@fontsource-variable/onest`, `@fontsource/unbounded` and `@fontsource/jetbrains-mono` packages, Latin subset only, and inlined into the built CSS as data URIs, because the Hub's Content Security Policy is `font-src 'self' data:`.

**Character:** Onest is a soft, plain grotesque. Light mode sets the big text at light weights, so the sheet reads like a calm note rather than a dashboard. Tabular figures are on everywhere (`font-feature-settings: 'tnum' 1`).

### Hierarchy
- **Answer** (300 in light mode, 400 in dark; clamp(26px, 2.8vw, 34px); 1.25; −0.025em; at most 28ch): the sentence that leads each page. Bold counts step up to 500 in light and 600 in dark. Links in it are weight 500 with a 1.5px underline offset 6px. Condensed sets it at 21px, 1.3, up to 60ch. A record's own page shows its title at the same size in place of the hidden question.
- **Headline** (300 light / 400 dark, 22px, 1.4; 17px on phones, 15.5px in Condensed): the summary sentence of the "Since you last checked" card.
- **Title** (600, 18px, −0.015em; 15.5px in Condensed; `--t-card`): box titles, the Since card title and card heads on All repositories.
- **Group** (650, 15px, −0.01em, text colour; 14px in Condensed; `--t-group`): a heading over a run of rows inside a list, such as Stuck or Moving on All repositories, or Waiting on others in the Inbox list. It has more space above it than below. Where the rows carry no hairlines of their own, a rule-strong hairline runs from the heading to the edge.
- **Sub** (650, 13.5px, text-2; 12.5px in Condensed; `--t-sub`): a column heading inside a card (the briefing's Reported facts, the area drawer's columns) and the Inbox's "Answer as …" line. It sits a step above the 12.5–13px text-3 metadata, so a heading never wears the metadata's grey.
- **Tab** (600, 15px; 14px in Condensed and on phones): Dashboard tab labels. Need-you row titles use 600 at 15px too.
- **Body** (400, 14.5px; 13.5px in Condensed): all running text, task titles and tile names.
- **Label** (500, 12.5px): status words, lane-bar segment text (600 labels), tab status lines, column headings (600, text-3, sentence case) and meta lines.
- **Explain** (400, 13px, 1.45, text-3, up to 80ch; 12px in Condensed): the explanation under every box title.
- **Numeral** (Onest 300, 20px in light; Unbounded 300, 17px in dark): the counts in the Team tallies.
- **Mono** (12.5px; 11.5px on tiles and branches): file paths, branches, the folder under a tile name and technical identifiers. Nothing else.

### Named Rules
**The Plain Voice Rule.** Every word on screen is Onest. Unbounded appears only as a dark-mode tally numeral and JetBrains Mono only as a path, branch or identifier.

**The Answer Has No Eyebrow Rule.** Nothing visible sits above the answer sentence. The question lives under the active rail item and in a visually hidden h1.

**The Sentence Case Rule.** Headings, tab labels and column heads are sentence case. No tracked uppercase labels.

## Layout

The shell is a 236px frame rail (72px when collapsed to icons) beside the sheet, under a 62px frame top bar. At 960px and below the rail becomes a drawer over a scrim and the sheet loses its corner. Inside the sheet the page is one centred column with sections `--hub-gap` apart.

- **Spacious (default):** the page column is capped at 1160px. The sheet is padded 44px at the top and clamp(20px, 4vw, 60px) at the sides. Sections are 32px apart, the answer sits 40px above what follows, card insets are 26px, list rows have 16px of vertical padding, nav rows are 40px and lane bars 34px.
- **Condensed:** the column widens to 1440px. The sheet drops to 22px and clamp(14px, 2vw, 28px), the gap to 16px, the answer gap to 18px, card insets to 16px, rows to 9px, nav rows to 32px and lane bars to 28px. Body text steps down 1px. The track hides each task's "Next:" line and map tiles hide their activity line. Structure, order and components stay the same.
- **Mobile (760px and below, either density):** sections are 20px apart, the sheet is padded 26px at the top and 16px at the sides, the answer gap is 24px and card insets 18px. The track drops its column header and stacks each task over its lane bar. The map becomes two columns, the area drawer one, and the Dashboard tabs scroll sideways at their natural width.
- **Phones (640px and below):** the top bar keeps only what a person acts on (menu, repository, search, health, inbox, account) with 44px icon targets. Theme and density move to Setup › Preferences.

Two-column pages use a 1fr main column with a 280–340px side column (Team) or two equal columns (Where's contested files and pull requests); both stack to one column at 1100px.

**The Same Room, Fewer Steps Rule.** Density is a measurement switch, not a different layout. A component that exists in Spacious exists in Condensed in the same place. The view menu says so: "Condensed changes measurements only. Every page keeps the same parts in the same places."

## Elevation & Depth

Depth is mostly tone: the frame is darkest, then the canvas, then surface cards, with sunken wells inside cards. Cards add one very soft lift. In light mode that is a 1px ambient shadow plus a long, faint violet-tinted drop; in dark mode it is a hairline ring, an inset top highlight and a deep drop. The sheet has no shadow; tone and its corner separate it from the frame. Map tiles and lane segments are flat and change state with inset rings.

### Shadow Vocabulary
- **Card lift** (light: `0 1px 2px rgba(29,28,40,.03), 0 20px 40px -30px rgba(70,55,160,.22)`; dark: `inset 0 1px 0 rgba(255,255,255,.03), 0 0 0 1px rgba(255,255,255,.055), 0 24px 48px -32px rgba(0,0,0,.9)`): every box, the Since card and a record page's action grid.
- **Menu pop** (light: `0 24px 48px -20px rgba(23,20,46,.4), 0 2px 6px rgba(23,20,46,.06)`; dark: `0 30px 60px -24px rgba(0,0,0,.85), 0 0 0 1px rgba(255,255,255,.08)`): popover menus from the top bar, which float over both frame and sheet.
- **Thumb** (light: `0 1px 2px rgba(20,16,50,.08), 0 4px 10px -4px rgba(20,16,50,.18)`): the active segment of a segmented control only.
- **Drawer edge** (`24px 0 48px -24px rgba(0,0,0,.6)`): the rail when it opens as a drawer at 960px and below.

### Named Rules
**The Inset State Rule.** Tiles and lane segments change state with inset rings: 1px rule-strong on tile hover, 2px brand when a tile is selected, 2px coral on a tile with a crossing, 1.5px current colour on lane-segment hover. They never lift.

**The Flat Sheet Rule.** The sheet never casts a shadow and a box never sits inside another box. Inside a card, depth is a sunken well or a hairline rule.

## Shapes

Soft and round throughout. Boxes and the Since card use 20px corners (14px in Condensed). Map tiles, the area drawer and textareas use 16px (12px for tiles in Condensed). Notices use 14px, popover menus 18px with 12px items, and nav rows 12px (8px in Condensed). Every interactive control (buttons, fields, search, segmented controls, catch-up lane names, breadcrumbs) is a full pill. Status dots, avatars, icon buttons and need-you icons are circles. Lane-bar and inbox-bar segments have 5px and 3px inner corners with pill-rounded ends. The only square-shouldered marker is the 6px outlined muted tag (for example "Draft"), and inline paths sit on a 6px sunken chip. The sheet has a single 26px curve at its top left; its other corners are square against the frame.

## Components

### Answer
A sentence, not a header. There is no rule under it and nothing visible above it. The h1 holds the page's question and is clipped from view; the answer paragraph is `aria-live="polite"`. On the Dashboard it reads, for example, "2 agents are working on 3 tasks. 1 thing needs you. 1 crossing." The "needs you" and crossing counts are coral links: following one scrolls to its box and rings it once with a soft violet pulse. When nothing needs you, the sentence says "Nothing needs you." in plain text. An optional fact line under it is 14.5px text-2, up to 70ch.

### Box with frameless head
- **Corner Style:** 20px (14px in Condensed), card lift, no border.
- **Head:** transparent, with no divider, no icon and no tinted band. The title is 16px 600; the count sits plain on the right in 13px 500 text-3, or coral 700 when the box's tone is bad; the explanation spans the full width beneath.
- **Internal Padding:** head 22px 26px 6px and body 14px 26px 26px; Condensed 12px 16px 2px and 8px 16px 14px; mobile 18px 18px 4px and 10px 18px 20px.

### Intent review
On the local Work detail, keep the human request, amendments and agent focus distinct. Show candidate identity and source coverage beside criterion findings; make supported, mismatch and unknown states explicit, with evidence origin and assurance visible. Label findings stale when their intent or candidate binding differs. Keep execution status separate from semantic outcome, retain attempt history, and make incomplete coverage visible. A human disposition records a view; it never grants admission.

### Section tabs
The Dashboard (and any rail item with more than one page) shows its pages as equal columns over a 1px rule-strong baseline. Each tab is a label over its live status: "1 needs you" with a coral dot, "No crossings" with no dot, "Last event 4 min ago" with a violet dot, "2 of 3 awake" with a green dot. The open tab is text colour with a 2px brand underline; hover shows a rule-strong underline. Tabs are links (`#home`, `#now`, `#activity`, `#team`) with `aria-current="page"`.

### Status dot and muted tag
A 7px dot with a 3px soft halo in its tone, then a 12.5px 500 word in text-2. A bad dot is coral. Muted metadata (a client, a draft) is instead a 6px-cornered outlined tag with no dot.

### Buttons, fields and choices
- **Shape:** full pill, 14px side padding (12px for small, at 13px).
- **Primary:** brand fill with On Brand text; hover and press go to Brand Ink.
- **Outline (secondary):** transparent with a rule-strong border and text-colour label; on hover it fills brand with On Brand text.
- **Picked:** a picked choice in a group, a picked density, or a picked catch-up lane is the ink pill: text-colour fill with surface-colour text.
- **Fields:** pill on surface with a rule-strong border; textareas use 16px corners.
- **Focus:** a 2px brand outline offset 3px everywhere on the sheet; controls on the frame use a 2px Frame Active Icon outline offset 2px.

### Navigation
- **Rail:** frame background. Rows are `--hub-nav-h` tall (40px, 32px in Condensed) with 12px corners, Frame Dim icon and 500 label. Hover adds a 5% white wash. The active row takes a 13% white wash with a 1px inner ring, a white 600 label, a Frame Active Icon and the item's question beneath it in 12.5px Frame Dim; other rows keep their question for screen readers only, and the drawer shows every question. Badges are compact counts: bright coral with dark text for trouble, translucent amber for waiting. A collapse control reduces the rail to icons with tooltips.
- **Items:** Dashboard (Now, Where, Catch up, Team), Inbox, History (Finished, Changes, Change records, Files), Timeline, Skills (Skills, Checks, After merge, Library) and Setup (Agents, Hub, Access, Computers, Admin, Preferences).
- **Top bar:** frame background with a faint 6% white bottom rule. From left: the menu button (narrow screens only), the logo, the repository button (a pill that opens the repository menu), the search pill ("Search agents, files, work…" with its shortcut key), then on the right the health chip (a 7px green dot and "Hub live", or a coral dot with peach text for "Reconnecting" or "Degraded"), the page tour (a round question-mark button), the inbox bell with a coral or amber count, the view menu and the account avatar. The account menu holds who you are, the token in use, **Your computers**, Preferences and Forget token. Opened on a web sign-in instead of a pasted token, the repository button lists every repository that sign-in may open ("Repositories you can open"), with the Hub's sentence for one it just refused and **Sign out of this browser** at the end; the account menu says "Signed in on the web" instead of naming a token, and its last item is Sign out of this browser. While the page checks for a web sign-in it shows only the frame and the logo, so the token page never flashes. Frame icon buttons are 38px circles that take an 8% white wash on hover.
- **View menu:** Theme as Light ("A white sheet in the violet frame"), Dark ("A dusk sheet, the frame a step deeper") and Match this device; Density as Spacious or Condensed pills; then the density note.
- **Breadcrumbs:** only deep pages (a run, a step, a saved moment) carry a quiet pill trail above the page.

### Needs you
A box listing only decisions for a person: a blocker, a task whose agent went quiet, a handover, two agents holding one file, two tasks planning the same files, or a pull request touching held files. It comes before "Start a task" whenever it is not empty. Each row is one line: a 30px circle icon drawn as a 1.5px ring in its tone (Brand Ink, warn or coral), a 15px 600 title, one 13px text-3 line under it (the task and when, or where to look), a single primary action as an outline pill ("Open in <the viewer's agent>" when a launch link exists, otherwise "Copy prompt for <agent>"), a "⋯" menu for the other prompts, and a chevron. The chevron, or a click on the row's own text, opens the row in place with a height animation: the reason, "Decision for …", the line saying what to do next, the box for the person's own words and Hide, in one sunken well; the chevron button carries `aria-expanded`. Overlaps are settled in the Inbox, so they share one row instead of one each: "N overlaps to talk through" with the first path under it and an "Open the Inbox" pill, at the place the first overlap would sit. Rows are separated by hairlines, and the box's head counts every need, each overlap included (coral when the box is bad). When empty it says "All clear" beside a green check. Its explanation states that reading the page answers none of them.

### Start a task
One quiet pill-shaped line ("Start a task · Describe it, then hand it to your agent") that opens in place into a card: the pill's corners ease to the card's 20px as it grows. It sits after Needs you when something needs a person, and leads the page otherwise.

### Coach marks
A new viewer's first Dashboard visit carries up to three short pointers in the page, not over it: one under the headline, one above Needs you, one above the track. Each is a brand-soft note with a small tail toward what it points at, and a "Got it" pill; the first one still on the page also carries "Skip tips". A dismissed pointer folds away and the page glides up. Dismissals and "Skip tips" are saved in this browser with the welcome tour's own flags (finishing or switching off the welcome tour also removes them), nothing blocks the page, and the walkthrough stays one click away from the question-mark button.

### The track and its lane bar
Every unfinished task as one row: a task column (180–260px; 160–230px in Condensed) with the title link, its status dot and word, its branch in mono, and a check-in line, then the lane bar, then the agent's own last words in one ellipsised line with their time.
- **Lane bar:** four segments in a pill-ended capsule with 3px gaps, sized 1fr : 3fr : 1fr : 1fr, under one column header: **Claimed · Now · Next · Finished**.
- **Claimed:** Brand Soft with Brand Ink text and a check; its tooltip names who claimed it and when.
- **Now:** the one wide segment. Brand fill with On Brand text, the holder's 24px avatar (20px in Condensed) ringed in On Brand, then "In progress · note" in words. A handover is Brand Soft with a 1.5px brand ring. Paused work, and any claim gone quiet or unknown (the label reads "Quiet" or "Unknown"), is warn-soft with a 1.5px warn-fill ring. Blocked fills coral with On Coral text.
- **Next:** an outline in rule-strong; its tooltip gives the reported next step, or says none was reported and fades the icon.
- **Finished:** sunken with a muted flag. The track holds only unfinished work, so it is never filled; its tooltip says a finished report still doesn't prove review or landing.
- **Narrow** (the bar's container under 560px): the other segments become 34px icon chips and the Now segment takes the rest, stacking its label over its note so neither is cut off.
- **Check-in line:** a 7px dot and words: green "Checked in 3 min ago", or amber "No check-in since … · still assigned" or "Check-in unknown · still assigned".

### Repository map (Where)
Up to 12 tiles, one per top-level folder that an agent holds files in or a task declared it will touch, in an auto-fill grid (190px minimum, 116px rows, 10px gaps; 160px, 88px and 6px in Condensed; two columns on mobile). A tile is a sunken button whose background mixes in up to 22% brand by how many people are there. It shows the folder name (14.5px 600), its path in mono, up to five overlapping 26px faces ringed in sunken (then "+N"), and "N editing · N tasks planned". A crossing adds a coral pill at the top right and a 2px coral inset ring. Picking a tile rings it in brand and opens the area drawer below: a sunken 16px panel with three columns, Editing now, Tasks planned here and Crossings. Quieter folders beyond 12 are counted, not hidden silently.

### "Since you last checked" card (Catch up)
A sentence, then a picture of the time away. The head holds the title, the range ("08:00 → now") and an outline "Mark reviewed" button. The headline sentence follows with bold counts, and a coral "Needs a look: N blocked tasks." when something is blocked.
- **Lanes:** three rows, Merged, Work and Between agents, each a pill name with its count beside a 1px rule-strong rail, with one dot per retained record at its time. Work dots are 9px text-2 ink, merges 11px brand, agent messages amber, and a blocked task a 13px coral dot with a soft halo. Each dot is ringed in the card surface so clusters stay countable.
- **Lane names are filters:** picking one turns it into the ink pill, dims the other rails to 30% and narrows the list; picking it again shows everything.
- **List:** newest first, five items, then a link to show the rest. Each row has its toned icon and a collapsed technical disclosure.
- **Foot:** each source's coverage as a 7px dot and word (green observed, amber partial, coral unavailable), and a technical disclosure.
- **Honesty:** opening, refreshing or paging the card never marks anything reviewed, and the card says so. Marking is the explicit button. When retention has a gap, an amber note says the missing interval cannot be treated as quiet and the button reads "Mark available records reviewed". A partly loaded delivery must be loaded fully before it can be marked.

### Team
When nobody is active it says so with the last activity time ("No agents active right now · Last agent activity 20 hr. ago"); only a repository that has never had an agent shows "No agent has connected yet" with the set-up command under it. Otherwise: who is working right now, grouped by person ("jonny · 2 agents · leads 1 task"), each agent with a 10px corner dot on its avatar: green if it reported activity in the last five minutes, amber if quieter but still holding files. Beside it, the **Lately** tallies: a ruled list of counts (sessions started, work announced, edits refused, edits repeated and let through, handoffs asked, warnings sent) with the numeral on the right; zero rows go text-3.

### Inbox bar
Under the Inbox answer, one 10px-tall segmented bar (up to 260px wide) with one segment per item: coral for an overlap that needs a conversation, amber for a request waiting on another lead, rule-strong for another signal, and Brand Soft with a brand inset for a settled request. Beside it: "N overlaps · N open requests · N other signals".

### Notices and empty states
- **Notices:** a quiet tinted note, never an alarm band: sunken (or the tone's soft tint) with 14px corners and no border; a bad notice draws its icon in coral. Retry actions sit at the end as small outline pills.
- **Empty states:** left-aligned, with a 34px ring icon, a 600 title and a text-3 line saying what will make something appear.

### Hide (dismiss in this browser)
Rows that only a person can clear, but that nobody has touched for a day (Needs you, Inbox requests and other attention signals), carry a quiet **Hide** after their next-step line: 12.5px 500 text-3 with an eye-slash icon, no border, a sunken wash and Brand Ink on hover. Hiding is remembered in this browser only, keyed by the item and its last update, so anything that changes comes back by itself; the Hub is never told. Under the list, "Show N hidden" brings hidden rows back dimmed to 55% with **Unhide**; it reads "Hide N again" while open. Fresh rows never offer Hide.

### Refusal notice
When the Hub says no, the page shows the Hub's own sentence as the notice title, word for word, in the bad tone with a shield icon: "brandonfla has Read on public repository counterbranch/agentscm; ask a repository owner for Triage or higher". A bare code gets the sentence the CLI and the menu bar app print for it, always "what's wrong; the one fix". A fix that is one plain `agentscm` command becomes a command row with Copy; anything else stays text. Never "membership", JSON or an HTTP code.

### Set up this computer
One lead sentence in text-2 (60ch), the command in a sunken command row (terminal icon, 12.5px mono, Copy at the end), then one 12.5px text-3 line saying what it will do. It sits on the signed-out page in its own white box above the token form, and under the empty states of Setup › Agents and Team. The command is written in one place in the code.

### Agent status rows (Setup › Agents)
The answer counts what is already loaded: "1 of 2 agents is ready; maria's Codex needs its AgentSCM hooks approved." Only an empty list gets an instruction. Each row is the agent's face, the agent named the way the Dashboard names someone's agent ("maria's Codex", 15px 600) over one 13px text-3 line of its checks ("MCP ✓ · Edit hook not yet"), and a single state pill on the right. A row that is not Ready adds one 13.5px text-2 line, aligned under the name, saying what to do next. What Ready means and how long proof lasts is said once, in the box's explanation, never repeated on the page.

### Signed in (the browser's landing page)
The one browser moment of setup, told on the frame rather than the sheet: a 600px column, the logo, a 56px green mark with a dark check, "This computer is signed in." in 30–44px 300 white, then "<computer> is signed in to <host> as <login> on GitHub. You won't need to do this again." in 17px Frame Dim with Frame Ink bolds. Three hairline-ruled next steps follow (terminal, menu bar, revoke), then a pill **Open the Hub** in Frame Brand and "You can close this tab." A refused sign-in swaps the mark for coral with a cross and shows the refusal notice on a white 20px sheet. The page shows nothing written into its link except the login, provider and computer name.

### Computers and Admin lists
A computer is a ruled row: laptop icon in text-3, the name in 600 with a brand "This computer" pill or a muted "Revoked" pill, the owner's login (Admin only), then a 13px text-3 line ("Signed in 12 days ago · last used 5 min. ago · 2 repositories") and up to four repository chips in mono on sunken 6px corners. **Revoke** is a small outline-danger pill on the right; it turns into an inline confirm ("Its agents stop at once and their tokens are revoked." with a filled danger "Revoke <name>" and Cancel), never a modal. The computer the page runs on is never offered for revoke. Revoked rows are hidden behind "Show N revoked computers". Signed out in this browser, the box holds one line and a primary **Sign in with GitHub** (or GitLab) pill instead of a list; signed in, its head carries a small outline **Sign out of this browser**, which leaves every computer signed in. The Admin tab appears only for a browser signed in as a Hub admin; anyone who opens it anyway is told why, and the tabs stay. Admin adds **Turned away** (login · forge · how, the reason sentence in text-2 at 68ch, an amber "3×" pill for repeats, the time) and **Connected repositories** (a plain table with right-aligned tabular counts).

### Motion
- **The baton ride:** when a task's state, claim or holder changes between refreshes, its Now segment sweeps in from the left (a clip-path reveal, 0.7s, `cubic-bezier(.16, 1, .3, 1)`) and the holder's avatar slides in from 38px to the left over 0.9s, sharpening from blurred and faint. This is the world's one authored moment.
- **Feedback only:** a box rings once (1.4s) when an answer link jumps to it, the signed-in mark lets one green ring out (1.1s) when the landing page opens, the area drawer settles in over 0.4s, a Needs you row, the Start a task line and a coach mark grow and fold in height over 0.3–0.4s, and tile tint and rings ease over 0.3–0.5s.
- With `prefers-reduced-motion: reduce`, the ride, the ring and the drawer animation stop, smooth scrolling becomes instant, and tile, lane, tab, button and rail transitions are removed.

## Do's and Don'ts

### Do:
- **Do** lead every page with its answer sentence at clamp(26px, 2.8vw, 34px), or 21px in Condensed, and keep the question in the rail and the hidden h1.
- **Do** keep the frame deep violet in both themes and give the sheet its single 26px top-left corner.
- **Do** draw unfinished work as the four-part lane bar, Claimed · Now · Next · Finished, with the wide Now segment naming the state in words and showing the holder's face.
- **Do** show a quiet or unknown claim as amber and "still assigned", never as free work.
- **Do** show status as a 7px dot with a soft halo plus a word.
- **Do** make every control a full pill and every container a soft 20px card (14px in Condensed).
- **Do** switch density by changing measurements only, keyed on `html[data-density='condensed']`.
- **Do** self-host fonts from the @fontsource packages and inline them, so they load under `font-src 'self' data:`.
- **Do** honour `prefers-reduced-motion` for every animation and transition you add.

### Don't:
- **Don't** put an eyebrow, kicker or visible question above the answer.
- **Don't** use coral for anything except trouble, including links in the answer that point at trouble.
- **Don't** let a lane bar, tile or tally claim review, verification or landing. Finished means an agent reported it finished.
- **Don't** mark anything reviewed, acknowledged or answered because someone opened, refreshed or paged a view. Only an explicit button does that, and it names what it covers.
- **Don't** load fonts from Google Fonts or any other CDN.
- **Don't** set any word in Unbounded except dark-mode tally numerals, or any non-path text in JetBrains Mono.
- **Don't** add icons, dividers or tinted bands to box heads, or nest a box inside a box.
- **Don't** build a grid of separate KPI cards or a dense ruled table for the dashboard.
- **Don't** show tile or segment state by lifting with a shadow. Use inset rings.
- **Don't** add a second authored motion. The baton ride is the moment.
