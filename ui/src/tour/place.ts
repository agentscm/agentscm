// Where the tour card sits. Pure, so the decisions (compact card, phone sheet, docking) can be tested without a browser.
export interface Rect {
  top: number
  left: number
  width: number
  height: number
}

export interface Placement {
  /** Drops the illustration and tucks "why" and "how" behind a disclosure. */
  compact: boolean
  /** A phone sheet: full width, half the screen at most. */
  sheet: boolean
  style: { top?: number; left?: number; right?: number; bottom?: number; width?: number; maxHeight?: string; inset?: number; margin?: string; height?: string }
}

export const CARD_W = 400
export const HERO_W = 480
const GAP = 12
const EDGE = 16
const COMPACT_H = 300

/**
 * `fullH` is the card's height with its illustration (last measured, or an estimate); `h` its height right now.
 * The compact decision only reads `fullH`, so shrinking the card cannot flip the decision back.
 */
export function placeCard(rect: Rect | null, card: { fullH: number; h: number; hero?: boolean }, vp: { w: number; h: number }): Placement {
  const narrow = vp.w <= 640
  if (!rect) {
    const w = Math.min(card.hero ? HERO_W : CARD_W, vp.w - 32)
    return { compact: vp.h < card.fullH + 32, sheet: false, style: { inset: 0, margin: 'auto', height: 'fit-content', width: w } }
  }
  if (narrow) {
    const top = Math.max(0, rect.top)
    const bottom = Math.min(vp.h, rect.top + rect.height)
    // The half of the screen the target's visible part leaves free.
    const sheetAtTop = vp.h - bottom < top
    // Never taller than the free half, so the sheet does not cover the part of the target that is showing.
    const free = (sheetAtTop ? top : vp.h - bottom) - 12 - 8
    const maxHeight = free >= vp.h * 0.45 ? '45dvh' : `${Math.max(190, Math.floor(free))}px`
    return { compact: true, sheet: true, style: sheetAtTop ? { top: 12, left: 12, right: 12, maxHeight } : { bottom: 12, left: 12, right: 12, maxHeight } }
  }
  const w = Math.min(CARD_W, vp.w - 32)
  const left = Math.max(EDGE, Math.min(rect.left, vp.w - w - EDGE))
  const near = (h: number) => {
    const below = rect.top + rect.height + GAP
    const above = rect.top - GAP - h
    const right = rect.left + rect.width + GAP
    const side = right + w <= vp.w - EDGE ? right : rect.left - GAP - w >= EDGE ? rect.left - GAP - w : null
    // Clamped, so a target still scrolling into view never drags the card off screen.
    const fit = (t: number) => Math.min(Math.max(EDGE, t), Math.max(EDGE, vp.h - h - EDGE))
    if (below + h <= vp.h - EDGE) return { top: fit(below), left, width: w }
    if (above >= EDGE) return { top: fit(above), left, width: w }
    if (side !== null) return { top: Math.max(EDGE, Math.min(rect.top, vp.h - h - EDGE)), left: side, width: w }
    return null
  }
  // A target taller than the room the full card would leave gets the compact card instead.
  const tall = rect.height > vp.h - card.fullH - 48
  if (!tall && near(card.fullH)) return { compact: false, sheet: false, style: near(card.fullH)! }
  // Compact: beside or around the target when there is room, else docked in the corner so it covers the least of it.
  const spot = near(card.h > 0 && card.h < card.fullH ? card.h : COMPACT_H)
  return { compact: true, sheet: false, style: spot ?? { right: EDGE, bottom: EDGE, width: w } }
}
