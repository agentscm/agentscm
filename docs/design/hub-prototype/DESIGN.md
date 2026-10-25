---
name: AgentSCM Hub (Twilight)
description: A calm workroom for agent teams. A deep-violet frame holds a quiet sheet that answers one plain question in a sentence, then shows one picture of it.
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
  rule-dark: "rgba(255, 255, 255, .06)"
  rule-strong-dark: "rgba(255, 255, 255, .13)"
  brand-light: "#6d5ce0"
  brand-ink-light: "#5947cc"
  brand-soft-light: "#f1eefd"
  brand-dark: "#a796ff"
  brand-ink-dark: "#c6bbff"
  brand-soft-dark: "rgba(167, 150, 255, .13)"
  coral-light: "#c8432a"
  coral-dark: "#ff7a5c"
  ok-light: "#127a52"
  ok-soft-light: "#e7f6ef"
  ok-fill-light: "#2fb67c"
  warn-light: "#9a5a06"
  warn-soft-light: "#fdf2e2"
  warn-fill-light: "#eea341"
  bad-light: "#c0391f"
  bad-soft-light: "#fdeee9"
  ok-dark: "#6fdba6"
  ok-fill-dark: "#52c98f"
  warn-dark: "#f6bd7a"
  warn-fill-dark: "#f5aa5a"
  bad-dark: "#ff9c84"
  agent-claude-dark: "#ff8a6b"
  agent-codex-light: "#2fb67c"
  agent-codex-dark: "#52c98f"
  agent-cursor-light: "#6d5ce0"
  agent-cursor-dark: "#a796ff"
typography:
  answer:
    fontFamily: "Onest, system-ui, sans-serif"
    fontSize: "clamp(26px, 2.8vw, 34px)"
    fontWeight: 300
    lineHeight: 1.25
    letterSpacing: "-0.025em"
  answer-condensed:
    fontFamily: "Onest, system-ui, sans-serif"
    fontSize: "21px"
    fontWeight: 300
    lineHeight: 1.3
    letterSpacing: "-0.025em"
  numeral:
    fontFamily: "Unbounded, Onest, sans-serif"
    fontSize: "38px"
    fontWeight: 300
    lineHeight: 1.05
    letterSpacing: "-0.04em"
  headline:
    fontFamily: "Onest, system-ui, sans-serif"
    fontSize: "18px"
    fontWeight: 500
    letterSpacing: "-0.015em"
  title:
    fontFamily: "Onest, system-ui, sans-serif"
    fontSize: "16px"
    fontWeight: 600
    letterSpacing: "-0.01em"
  body:
    fontFamily: "Onest, system-ui, sans-serif"
    fontSize: "14.5px"
    fontWeight: 400
    lineHeight: 1.5
  body-condensed:
    fontFamily: "Onest, system-ui, sans-serif"
    fontSize: "13.5px"
    fontWeight: 400
    lineHeight: 1.5
  label:
    fontFamily: "Onest, system-ui, sans-serif"
    fontSize: "12px"
    fontWeight: 500
  mono:
    fontFamily: "JetBrains Mono, ui-monospace, Menlo, monospace"
    fontSize: "12.5px"
    fontWeight: 400
rounded:
  sheet-corner: "26px"
  card: "20px"
  card-condensed: "14px"
  tile: "16px"
  tile-condensed: "12px"
  nav: "12px"
  nav-condensed: "8px"
  tag: "6px"
  pill: "999px"
  round: "50%"
spacing:
  gap: "32px"
  gap-condensed: "16px"
  gap-mobile: "20px"
  sheet-top: "44px"
  sheet-top-condensed: "22px"
  sheet-side: "clamp(20px, 4vw, 60px)"
  sheet-side-condensed: "clamp(14px, 2vw, 28px)"
  card-inset: "26px"
  card-inset-condensed: "16px"
  card-inset-mobile: "18px"
  page-max: "1160px"
  page-max-condensed: "1440px"
components:
  answer-masthead:
    textColor: "{colors.text-light}"
    typography: "{typography.answer}"
    width: "28ch"
  answer-masthead-condensed:
    typography: "{typography.answer-condensed}"
    width: "60ch"
  answer-link:
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
    padding: "22px 26px 18px"
  since-card-condensed:
    rounded: "{rounded.card-condensed}"
    padding: "12px 16px 10px"
  metro-stop:
    backgroundColor: "{colors.surface-light}"
    textColor: "{colors.text-3-light}"
    rounded: "{rounded.round}"
    size: "20px"
  metro-stop-done:
    backgroundColor: "{colors.brand-light}"
    textColor: "{colors.surface-light}"
  metro-stop-fail:
    backgroundColor: "{colors.coral-light}"
    textColor: "{colors.surface-light}"
  relay-holder:
    rounded: "{rounded.round}"
    size: "32px"
  relay-holder-condensed:
    size: "26px"
  map-tile:
    backgroundColor: "{colors.sunken-light}"
    rounded: "{rounded.tile}"
    padding: "16px 18px 14px"
    height: "112px"
  map-tile-condensed:
    rounded: "{rounded.tile-condensed}"
    padding: "10px 12px"
    height: "84px"
  crossing-pill:
    backgroundColor: "{colors.coral-light}"
    textColor: "{colors.surface-light}"
    rounded: "{rounded.pill}"
    padding: "2px 8px"
  crossing-pill-settled:
    backgroundColor: "{colors.ok-soft-light}"
    textColor: "{colors.ok-light}"
  lens-track:
    backgroundColor: "{colors.sunken-light}"
    rounded: "{rounded.pill}"
    padding: "3px"
  lens-segment:
    textColor: "{colors.text-2-light}"
    rounded: "{rounded.pill}"
    height: "32px"
  lens-segment-active:
    backgroundColor: "{colors.surface-light}"
    textColor: "{colors.text-light}"
  button-pill:
    textColor: "{colors.text-light}"
    rounded: "{rounded.pill}"
    padding: "0 14px"
  button-pill-hover:
    backgroundColor: "{colors.brand-light}"
    textColor: "{colors.surface-light}"
  button-primary:
    backgroundColor: "{colors.brand-light}"
    textColor: "{colors.surface-light}"
    rounded: "{rounded.pill}"
  chip-active:
    backgroundColor: "{colors.text-light}"
    textColor: "{colors.surface-light}"
    rounded: "{rounded.pill}"
  status-dot:
    textColor: "{colors.text-2-light}"
    typography: "{typography.label}"
    size: "7px"
  avatar:
    textColor: "{colors.surface-light}"
    rounded: "{rounded.round}"
    size: "30px"
  nav-item:
    textColor: "{colors.frame-dim-light}"
    rounded: "{rounded.nav}"
    height: "40px"
  nav-item-active:
    backgroundColor: "rgba(255, 255, 255, .1)"
    textColor: "{colors.surface-light}"
---

# Design System: AgentSCM Hub (Twilight)

## Overview

**Creative North Star: "The Calm Workroom"**

A deep-violet frame (top bar, nav rail and story player) holds one quiet content sheet. The sheet's only shaped corner is a large 26px curve at its top left, where it meets the frame. Each home view opens with a sentence that answers the view's question. The question itself sits under the active nav item and in a visually hidden h1. Below the sentence is one picture of the answer: lane bars of who holds each task (Now), or a tiled map of where agents plan to work in the repo (Where). The world is a hybrid of two older skins. From Graphite (B) it takes status as a coloured dot plus a word. From Glance (C) it takes progress drawn as a metro line, pill-shaped controls and the Today activity strip.

The frame stays the same in both modes; only its depth changes. Inside the sheet, light mode uses the Mist palette (white sheet, soft violet, light font weights) and dark mode uses the Dusk palette (violet-grey sheet, lavender, the Unbounded numerals). The world is set with `html[data-skin="twilight"]`, `data-bs-theme="light|dark"` and `data-density` (no attribute means Spacious, the default; `"condensed"` means Condensed). It sits on Bootstrap 5.3.8 and the Hub's `--hub-*` custom properties. Graphite and Glance are still in the same stylesheet as non-default alternates. Nothing here describes them.

The world rejects the KPI-card grid and the dense Jira-style table. Boxes are soft, frameless cards with lots of air. Lists use hairline separators. Colour is reserved for state.

**Key Characteristics:**
- Deep-violet frame, light or dark sheet, and one 26px corner where they meet.
- A sentence answer leads the dashboard. No eyebrow, kicker or visible question sits above it.
- Brand violet marks progress and the current selection. Coral marks trouble and the links in the answer.
- Onest for every word, Unbounded for big dark-mode numerals only, JetBrains Mono for paths and skill names only.
- Two densities share every component. Condensed changes measurements, never structure.
- One authored motion: a task's new stage sweeps in on its lane bar, carrying the holder's avatar.

## Colors

Violet carries the structure, near-neutral violet-greys carry the reading, and a single warm coral is held back for trouble.

### Primary
- **Twilight Frame** (frame-light / frame-dark): the top bar, nav rail and story player in both modes. It is darker in dark mode so the sheet still lifts off it. Text on the frame uses **Frame Ink** for names and **Frame Dim** for secondary text and resting nav. The active nav icon uses **Frame Active Icon**.
- **Mist Violet / Dusk Lavender** (brand-light / brand-dark): metro lines filling as stages finish, done stops, the ring around the current holder, the selected map tile, focus outlines, primary buttons and the hover fill on pill buttons. **Brand Ink** is the text-weight version of the same violet. **Brand Soft** is the tint behind ready and active workflow pills and selection.

### Secondary
- **Coral** (coral-light / coral-dark): links in the answer sentence, failed stops, lap badges, the crossing pill and crossing tile ring on Map, and the count of a box whose tone is bad. This is the one warm colour in the world.

### Tertiary
- **Agent hues** (agent-*; Claude uses coral in light mode): fills behind the avatar initials that tell Claude, Codex and Cursor apart. They identify agents, not state.
- **Status** (ok / warn / bad, each with a soft and a fill version): the fill is the status dot, the soft version is the dot's halo and the settled-pill background, and the plain token is status text.

### Neutral
- **Canvas** (canvas-light / canvas-dark): the sheet's own ground.
- **Surface** (surface-light / surface-dark): cards, active lens segments and the disc of a metro stop.
- **Sunken** (sunken-light / sunken-dark): the lens track, map tiles at rest and the area drawer.
- **Ink ramp** (text / text-2 / text-3, in both modes): primary reading, secondary reading, then counts, times and explanations.
- **Rules** (rule / rule-strong): hairline separators between list rows, then empty metro rails, button outlines and stop rings.

### Named Rules
**The Coral Is Trouble Rule.** Coral means "look here, something needs a person". It is used for links in the answer sentence and for failure, laps and crossings, and nowhere else. Never use it for brand, decoration or neutral time markers.

**The Frame Never Changes Hue Rule.** Light and dark swap the sheet, not the world. The frame is deep violet in both modes and only changes depth.

**The Readable Coral Rule.** Light-mode coral is the deeper #c8432a: 4.69:1 as text on the canvas and 4.89:1 under white text, so it passes AA at every size the build uses. Dark-mode coral (#ff7a5c) is bright, so text and glyphs on a coral fill go dark (#1c0f0b) instead of white. The Claude avatar hue stays #f06a4c because it identifies an agent, not trouble.

## Typography

**Body Font:** Onest (with system-ui, sans-serif)
**Numeral Font:** Unbounded (with Onest), for big dark-mode numerals only
**Mono Font:** JetBrains Mono (with ui-monospace, Menlo), for paths and skill names only

**Character:** Onest is a soft, plain grotesque. Light mode sets it at light weights, so the sheet reads like a calm note rather than a dashboard. Tabular figures are on everywhere.

### Hierarchy
- **Answer** (light 300 in light mode, regular 400 in dark mode; clamp(26px, 2.8vw, 34px); line-height 1.25; −0.025em): the sentence that leads each home view, at most 28ch wide. Bold words step up to 500 in light mode and 600 in dark. Answer links are coral at weight 500 with a 1.5px underline offset 6px. In Condensed the answer is 21px, line-height 1.3, up to 60ch wide.
- **Numeral** (Unbounded light 300, 38px; 26px in Condensed, 28px on mobile): big counts in dark mode only. Light mode shows the same numerals in Onest at weight 300 and 42px.
- **Headline** (light 300 in light mode, 400 in dark, with 500–600 bold counts; 22px, 15.5px in Condensed): the summary sentence in the "Since you last checked" card.
- **Title** (semibold 600, 16px; 14px in Condensed): box titles and map tile names.
- **Body** (regular 400, 14.5px; 13.5px in Condensed): all running text.
- **Label** (medium 500, 12px): status words, metro stage labels (12px bold 700; 11.5px in Condensed, 11px on mobile), counts and explanations (13px in text-3).
- **Mono** (12.5px): file paths, the repo name and skill names. Nothing else.

### Named Rules
**The Plain Voice Rule.** Every word on screen is Onest. Unbounded appears only as a big dark-mode numeral, and JetBrains Mono only as a path or skill name. If a string is not one of those, it is set in Onest.

**The Answer Has No Eyebrow Rule.** Nothing visible sits above the answer sentence. The question lives under the active nav item and in a visually hidden h1.

## Layout

The shell is a 236px frame rail beside the sheet, under a 62px frame top bar, with a 64px frame story player pinned at the bottom. Above 992px the rail is sticky. Below that it becomes a Bootstrap off-canvas drawer. Inside the sheet, the page is a centred column.

- **Spacious (default):** the page column is capped at 1160px. The sheet is padded 44px at the top and clamp(20px, 4vw, 60px) at the sides. The gap between boxes is 32px. The answer sits 40px above the first box, and card insets are 26px. Two-column tabs are the content plus a 300–340px side column.3fr : 1fr).
- **Condensed:** the page column widens to 1440px. Sheet padding drops to 22px and clamp(14px, 2vw, 28px), the gap to 16px, card insets to 16px and the space under the answer to 18px. The body text steps down 1px. Row padding roughly halves, and track notes hide except at the current stop and at failed stops. Structure, order and components stay identical.
- **Mobile (≤760px):** everything stacks into one column. The sheet is padded 26px at the top and 16px at the sides, the gap is 20px and card insets are 18px. The track's column headers hide and each row shows its stage labels inline. The lens becomes a three-column grid without icons.

**The Same Room, Fewer Steps Rule.** Density is a measurement switch, not a different layout. A component that exists in Spacious exists in Condensed in the same place.

## Elevation & Depth

Depth comes mostly from tone: the frame is darkest, then the canvas, then a surface card, and sunken wells sit inside cards. Cards add one very soft lift. In light mode that is a 1px ambient shadow plus a long violet-tinted drop that is barely visible. In dark mode it is a hairline ring, an inset top highlight and a deep drop shadow. Map tiles are flat, and their states are drawn with inset rings, not shadows. The sheet itself has no shadow. It is separated from the frame by tone and its corner.

### Shadow Vocabulary
- **Card lift** (light: `0 1px 2px rgba(29,28,40,.03), 0 20px 40px -30px rgba(70,55,160,.22)`; dark: `inset 0 1px 0 rgba(255,255,255,.03), 0 0 0 1px rgba(255,255,255,.055), 0 24px 48px -32px rgba(0,0,0,.9)`): every box, the Since card and the activity strip.
- **Lens thumb** (`0 1px 2px rgba(20,16,50,.08), 0 4px 10px -4px rgba(20,16,50,.18)`): the active segment of the lens only.
- **Holder halo** (`0 0 0 3px surface, 0 0 0 5px brand, 0 6px 14px -6px rgba(20,16,60,.45)`): the holder's avatar inside the current lane-bar segment. The ring is red-coral (bad-fill) when that stage failed.

**The Inset State Rule.** Tiles and stops change state with inset rings: 1px rule-strong on hover, 2px brand when selected, 2px coral when there is a crossing. They never change state by lifting.

## Shapes

The shapes are soft and round throughout. Cards use 20px corners (14px in Condensed). Map tiles, chat bubbles and inbox choices use 16px (12px in Condensed). Nav items use 12px (8px in Condensed). Every interactive control (buttons, lens, filters, search, player speed) is a full pill. Stops, status dots, avatars and tile marks are circles. The only square-shouldered tag is the 6px-radius outlined "Proposed" / "In review" marker. The sheet has a single 26px curve at its top left. Its other corners are square against the frame.

## Components

### Answer masthead
A sentence, not a header. There is no rule under it and nothing visible above it. The h1 holds the question and is clipped from view. The answer paragraph is `aria-live="polite"`, so changes are announced. The "needs you" link and the crossing link inside it are coral, and they are the dashboard's primary action.

### Box with frameless head
- **Corner Style:** 20px (14px in Condensed).
- **Background:** surface with the card lift. There is no top border.
- **Head:** transparent, with no divider and no icon. The title is 16px semibold, the count sits plain on the right in text-3 (coral bold 700 when the box's tone is bad), and a 13px text-3 explanation spans the full width beneath.
- **Internal Padding:** 22px 26px 6px for the head and 14px 26px 26px for the body. Condensed uses 12px 16px 2px and 8px 16px 14px.

### The other pages: one picture each
- **Inbox:** the answer sentence, then a segmented bar of every decision (coral stopped, amber waiting, grey for the team, violet-tinted done) with "N decided · N on you". Catch-up lives on the dashboard; the Inbox only links to it.
- **History:** sessions grouped by day. Each row's time column has a thin ribbon showing when that session ran within its day.
- **Skills:** the post-merge workflow as one five-step bar (Merged, Ready, Checked in, Claimed, Saved) with a caption under each step, then one thin track per task lit to the step it reached (a ring marks the current step).
- **Insights:** one row per measure: a large number, the measure and what was seen, a check line ("how we know") and a cross line ("what it doesn't mean"), and a dot strip of when it happened today (coral for failures).
- **Setup:** a chain of three nodes (this computer → Hub → GitHub) joined by labelled links ("hooks report", "reads every 5 min"); a broken link is dashed coral. Details follow in boxes.
- **Task page:** the same lane bar as the dashboard, the four "is it done" facts, then the thread.
- **Session page:** the chat if it's yours; otherwise one plain list of what the session did, shown once.
- **Timeline:** every checkpoint, commit and handover as a dot on one line (violet, ink, amber). Pick one to read it, step earlier or later, compare it with now or the moment before, or copy the restore command.
- **Sub-tabs on pages:** History (Sessions, Files), Skills (Skills, Checks, Recipes) and Setup (Connection, Agents, Access) use the same tab strip as the dashboard, each tab with its live status.
- **Honest catch-up:** coverage is listed per source with a status dot. A retention gap shows an amber note and the button reads "Mark available records reviewed". A mark moved on another tab shows a violet note. Each track row carries a start check: clear so far, review first, or can't tell.
- **Top bar:** the repository name opens a menu of connected repositories and unconnected checkouts on this computer. A question-mark button opens "This page in three steps".

### Dashboard tabs
Home is one Dashboard. Its answer sentence ("2 agents are working on 3 tasks. 1 thing needs you. 1 crossing in Invoices.") stays on top, then four tabs split it by question, each linkable: **Now** (`#/home/now`: Needs you, then the track: every task as a grid lane bar with its next step in the task column and its agent's last words under the bar), **Where** (`#/home/where`: the repo map with the area drawer, then heads-ups that worked out), **Catch up** (`#/home/catchup`: the catch-up card, the full log and baton passes) and **Team** (`#/home/team`: who is awake and who ran when, the crew and today's tallies; **Crew** for solo). Each tab is a label over its live status ("1 needs you" with a coral dot, "1 crossing", "22 new since 08:00" with a violet dot, "1 of 6 awake"), four across with a 2px brand underline on the open one. Each fact lives in exactly one tab: a new section goes where its question is, never into a second copy.

### "Since you last checked" card
A sentence, then a picture of the time you were away. The head holds the title, the "In review" tag, the range ("08:00 → now · marked 16 h ago") and a pill "Mark reviewed" button. The summary sentence follows at 22px (17px in a narrow column, 15.5px in Condensed), light weight in light mode with bold counts, and coral "Needs a look: …" when something failed.
- **Lanes:** three rows (Merged, Work, Between agents) across the window from your last mark to now, each a 1px rule with one dot per saved record at its time. Merges are 11px brand dots, work is ink (ok-green when something finished), agent messages are amber, and a failure is a 13px coral dot with a soft halo. Each dot is ringed in the card surface so clusters stay countable.
- **Lane names are filters:** pill buttons with counts. Picking one dims the other lanes and narrows the list below; picking it again shows everything.
- **List:** newest first, five items (three in a narrow column), then "N more in your inbox".
- **Looking back:** after "Mark reviewed", "See what you reviewed at 18:20" shows that window again on a sunken card titled "Already reviewed". It never moves the review position.

### Lane bar
Progress is one segmented capsule per task (`laneBar` in core.js, `.lb-*`): five segments with 3px gaps, rounded ends on the first and last, 5px corners inside. Read left to right like a sentence.
- **Done** segments fill brand-soft with brand-ink text and a check. **Now** is the one wide segment: brand fill with on-brand text (white in light mode, #16132a in dark), the holder's 24px avatar inside it, and "Stage · note" in words. **Waiting, paused or stale** turn amber (warn-soft with a 1.5px warn-fill inset). **Failed** fills coral. **Not yet** is sunken with a muted icon. **Not tracked or unknown** is an outline only. A "2×" chip counts extra attempts; a loop reads "Going in circles".
- **Fit** (task lists): the current segment gets 3fr, the others 1fr and show only their stage name. 34px tall (28px in Condensed). **Mini** (Map list): 26px, 11.5px text.
- **Grid** (the dashboard track): equal columns that line up under one stage header, so a lead can scan a column; every cell says its own fact ("1 commit", "Approved by Tomás", "Failed 1×") and is a button into the task. 40px tall (30px in Condensed).
- **Narrow** (container under 560px, e.g. phones): segments not in progress shrink to 34px icon chips, and the current one takes the rest, stacking its stage name over its note instead of cutting either off.
- A dashed pill under a track row (or a "Then" line under a bar) says which workflow runs after merge.

### Motion: the baton ride
When a task reaches a new stage, that segment sweeps in from the left (`.lb-seg.is-new`, a clip-path reveal over 0.7s on `cubic-bezier(.16, 1, .3, 1)`), and the holder's avatar slides into it from 38px to the left over 0.9s, sharpening from blurred and faint. A stage that just finished fills the same way. This is the world's one authored moment. With `prefers-reduced-motion: reduce`, the ride, the drawer, the crossing and the new-fact animations stop, the stop pulses stop, and the tile, rail and stop transitions are removed. Everything arrives in its final state.

## Do's and Don'ts

### Do:
- **Do** lead the dashboard with the answer sentence at clamp(26px, 2.8vw, 34px), or 21px in Condensed, and keep the question in the nav and the hidden h1.
- **Do** keep the frame deep violet in both modes and give the sheet its single 26px top-left corner.
- **Do** draw progress as a lane bar: filled for done, one wide segment that names where the task is now with the holder's face, quiet for not yet.
- **Do** show status as a 7px dot with a soft halo plus a word.
- **Do** make every control a full pill and every container a soft 20px card (14px in Condensed).
- **Do** switch density by changing measurements only, so both densities keep the same components in the same places.
- **Do** honour `prefers-reduced-motion` for every animation and transition you add.

### Don't:
- **Don't** put an eyebrow, kicker or visible question above the answer.
- **Don't** use coral for anything except trouble and links in the answer sentence.
- **Don't** set any word in Unbounded, or any non-path, non-skill text in JetBrains Mono.
- **Don't** add icons, dividers or tinted bands to box heads.
- **Don't** build a grid of separate KPI cards or a dense ruled table for the dashboard.
- **Don't** show tile or stop state by lifting with a shadow. Use inset rings.
- **Don't** add a second authored motion. The baton ride is the moment.
