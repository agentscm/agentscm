// Per-viewer walkthrough preferences, kept in this browser only: the hub UI never writes them to the hub.
// Pure and DOM-free apart from the guarded storage handle, so tests can pass their own.

const KEY = 'agentscm.tour.v1'

export interface TourPrefs {
  /** The welcome tour has been shown here once (finished, skipped or closed). */
  welcomeSeen: boolean
  /** "Don't show again" was checked on the welcome tour. */
  welcomeOff: boolean
  /** Sections whose page tour plays by itself the first time the page opens. */
  autoplay: string[]
  /** Sections whose page tour has been shown here at least once. */
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

export const shouldAutoplay = (p: TourPrefs, section: string, remember: boolean): boolean =>
  remember && p.autoplay.includes(section) && !p.sectionsSeen.includes(section)

const toggle = (list: string[], v: string, on: boolean): string[] => (on ? [...new Set([...list, v])] : list.filter((x) => x !== v))

/** Closing the welcome tour, any way: it counts as seen; the checkbox decides whether it may return. */
export const closeWelcome = (p: TourPrefs, dontShow: boolean): TourPrefs => ({ ...p, welcomeSeen: true, welcomeOff: dontShow })

/** Settings switch: on brings the welcome tour back at the next page load. */
export const setWelcome = (p: TourPrefs, on: boolean): TourPrefs => (on ? { ...p, welcomeSeen: false, welcomeOff: false } : { ...p, welcomeSeen: true, welcomeOff: true })

/** Closing a page tour; "Don't show again" there turns that page's auto-play off. */
export const closeSection = (p: TourPrefs, section: string, dontShow: boolean): TourPrefs => ({
  ...p,
  sectionsSeen: toggle(p.sectionsSeen, section, true),
  autoplay: dontShow ? toggle(p.autoplay, section, false) : p.autoplay,
})

/** Turning auto-play on also forgets "seen", so the tour plays on the next visit. */
export const setAutoplay = (p: TourPrefs, section: string, on: boolean): TourPrefs => ({
  ...p,
  autoplay: toggle(p.autoplay, section, on),
  sectionsSeen: on ? toggle(p.sectionsSeen, section, false) : p.sectionsSeen,
})
