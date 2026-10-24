// Per-viewer walkthrough preferences, kept in this browser only: the hub UI never writes them to the hub.
// Pure and DOM-free apart from the guarded storage handle, so tests can pass their own.

export const TOUR_KEY = 'agentscm.tour.v1'
const KEY = TOUR_KEY

export interface TourPrefs {
  /** The welcome tour was finished or turned off; kept equal to welcomeOff, read for older saved prefs. */
  welcomeSeen: boolean
  /** "Don't show again" was checked on the welcome tour. */
  welcomeOff: boolean
  /** Sections whose page tour plays by itself the first time the page opens. */
  autoplay: string[]
  /** Unused since the checkbox decides; kept so older saved prefs still parse. */
  sectionsSeen: string[]
}

export const DEFAULT_PREFS: TourPrefs = { welcomeSeen: false, welcomeOff: false, autoplay: [], sectionsSeen: [] }

// Fresh arrays each time: a shallow copy of DEFAULT_PREFS would share (and let callers mutate) its lists.
const defaults = (): TourPrefs => ({ ...DEFAULT_PREFS, autoplay: [], sectionsSeen: [] })

type Store = Pick<Storage, 'getItem' | 'setItem'>

function store(): Store | null {
  try {
    return window.localStorage
  } catch {
    return null
  }
}

const strings = (v: unknown): string[] => (Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string').slice(0, 64) : [])

/** Reads and validates the saved preferences; anything unreadable is the default. */
export function loadPrefs(s: Store | null = store()): TourPrefs {
  try {
    const raw = s?.getItem(KEY)
    if (!raw) return defaults()
    const v = JSON.parse(raw) as Record<string, unknown>
    return { welcomeSeen: v['welcomeSeen'] === true, welcomeOff: v['welcomeOff'] === true, autoplay: strings(v['autoplay']), sectionsSeen: strings(v['sectionsSeen']) }
  } catch {
    return defaults()
  }
}

/** Returns whether the write stuck; a failed write leaves the in-memory copy as the only record. */
export function savePrefs(p: TourPrefs, s: Store | null = store()): boolean {
  try {
    if (!s) return false
    s.setItem(KEY, JSON.stringify(p))
    return s.getItem(KEY) !== null
  } catch {
    return false
  }
}

/**
 * Whether this browser can remember a dismissal. Auto-play needs it: without storage the tour
 * would open again on every load, so it stays manual (the page trigger and ⌘K still work).
 */
export function canRemember(s: Store | null = store()): boolean {
  try {
    if (!s) return false
    const probe = `${KEY}.probe`
    s.setItem(probe, '1')
    const ok = s.getItem(probe) === '1'
    ;(s as Storage).removeItem?.(probe)
    return ok
  } catch {
    return false
  }
}

export const shouldWelcome = (p: TourPrefs, remember: boolean): boolean => remember && !p.welcomeSeen && !p.welcomeOff

export const shouldAutoplay = (p: TourPrefs, section: string, remember: boolean): boolean => remember && p.autoplay.includes(section)

const toggle = (list: string[], v: string, on: boolean): string[] => (on ? [...new Set([...list, v])] : list.filter((x) => x !== v))

/** `done`: finished, or closed with "Don't show again" ticked. Closing it any other way brings it back next time. */
export const closeWelcome = (p: TourPrefs, done: boolean): TourPrefs => (done ? { ...p, welcomeSeen: true, welcomeOff: true } : p)

/** Settings switch: on brings the welcome tour back at the next page load. */
export const setWelcome = (p: TourPrefs, on: boolean): TourPrefs => (on ? { ...p, welcomeSeen: false, welcomeOff: false } : { ...p, welcomeSeen: true, welcomeOff: true })

/** Same rule for a page tour that plays by itself: finishing it or ticking the box switches its auto-play off (the Settings switch follows). */
export const closeSection = (p: TourPrefs, section: string, done: boolean): TourPrefs => (done ? { ...p, autoplay: toggle(p.autoplay, section, false) } : p)

export const setAutoplay = (p: TourPrefs, section: string, on: boolean): TourPrefs => ({ ...p, autoplay: toggle(p.autoplay, section, on) })
