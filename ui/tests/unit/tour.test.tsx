import { act, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest'
import { fetchActivity, fetchHealth, fetchWhoami, fetchWork } from '../../src/api.ts'
import { SECTIONS } from '../../src/App.tsx'
import { derive } from '../../src/derive.ts'
import type { WorkOverview } from '../../src/standup.ts'
import { SECTION_TOURS, WELCOME, tourFor, type TourContext, type TourDef } from '../../src/tour/content.ts'
import { DEFAULT_PREFS, canRemember, closeSection, closeWelcome, loadPrefs, savePrefs, setAutoplay, setWelcome, shouldAutoplay, shouldWelcome } from '../../src/tour/state.ts'
import { TourProvider, TourTrigger, useTour } from '../../src/tour/Tour.tsx'
import type { HubData } from '../../src/useHub.ts'
import { stubHub } from '../fixtures/mockFetch.ts'

const KEY = 'agentscm.tour.v1'
const memory = (init: Record<string, string> = {}) => {
  const m = new Map(Object.entries(init))
  return { getItem: (k: string) => m.get(k) ?? null, setItem: (k: string, v: string) => void m.set(k, v), removeItem: (k: string) => void m.delete(k), m }
}
const throwing = { getItem: () => { throw new Error('blocked') }, setItem: () => { throw new Error('blocked') } }

describe('tour preferences', () => {
  it('loads the default for missing, empty, garbage or wrongly typed data', () => {
    expect(loadPrefs(null)).toEqual(DEFAULT_PREFS)
    expect(loadPrefs(memory())).toEqual(DEFAULT_PREFS)
    for (const raw of ['', '{', 'null', '[]', '"x"', '12']) expect(loadPrefs(memory({ [KEY]: raw }))).toEqual(DEFAULT_PREFS)
    expect(loadPrefs(throwing)).toEqual(DEFAULT_PREFS)
    expect(loadPrefs(memory({ [KEY]: JSON.stringify({ welcomeSeen: 'yes', welcomeOff: 1, autoplay: 'now', sectionsSeen: [1, 'now', null] }) }))).toEqual({ ...DEFAULT_PREFS, sectionsSeen: ['now'] })
  })
  it('caps list length and does not alias the default', () => {
    const many = Array.from({ length: 200 }, (_, i) => `s${i}`)
    expect(loadPrefs(memory({ [KEY]: JSON.stringify({ autoplay: many }) })).autoplay).toHaveLength(64)
    loadPrefs(null).autoplay.push('x')
    expect(DEFAULT_PREFS.autoplay).toEqual([])
  })
  it('round-trips through save and load, and reports a failed write', () => {
    const s = memory()
    const p = { welcomeSeen: true, welcomeOff: false, autoplay: ['now'], sectionsSeen: ['work'] }
    expect(savePrefs(p, s)).toBe(true)
    expect(loadPrefs(s)).toEqual(p)
    expect(savePrefs(p, null)).toBe(false)
    expect(savePrefs(p, throwing)).toBe(false)
    expect(savePrefs(p, { getItem: () => null, setItem: () => {} })).toBe(false) // write silently dropped
  })
  it('canRemember probes storage and cleans up after itself', () => {
    const s = memory()
    expect(canRemember(s)).toBe(true)
    expect(s.m.size).toBe(0)
    expect(canRemember(null)).toBe(false)
    expect(canRemember(throwing)).toBe(false)
    expect(canRemember({ getItem: () => null, setItem: () => {} })).toBe(false)
  })
  it('shouldWelcome and shouldAutoplay need storage that remembers', () => {
    expect(shouldWelcome(DEFAULT_PREFS, true)).toBe(true)
    expect(shouldWelcome(DEFAULT_PREFS, false)).toBe(false)
    expect(shouldWelcome({ ...DEFAULT_PREFS, welcomeSeen: true }, true)).toBe(false)
    expect(shouldWelcome({ ...DEFAULT_PREFS, welcomeOff: true }, true)).toBe(false)
    const p = { ...DEFAULT_PREFS, autoplay: ['now'] }
    expect(shouldAutoplay(p, 'now', true)).toBe(true)
    expect(shouldAutoplay(p, 'now', false)).toBe(false)
    expect(shouldAutoplay(p, 'work', true)).toBe(false)
  })
  it('closeWelcome: closing without the box changes nothing; finishing or ticking it turns the tour off', () => {
    expect(closeWelcome(DEFAULT_PREFS, false)).toEqual(DEFAULT_PREFS)
    expect(shouldWelcome(closeWelcome(DEFAULT_PREFS, false), true)).toBe(true)
    expect(closeWelcome(DEFAULT_PREFS, true)).toMatchObject({ welcomeSeen: true, welcomeOff: true })
  })
  it('setWelcome brings the welcome tour back, or turns it off', () => {
    const done = closeWelcome(DEFAULT_PREFS, true)
    const back = setWelcome(done, true)
    expect(shouldWelcome(back, true)).toBe(true)
    expect(shouldWelcome(setWelcome(back, false), true)).toBe(false)
  })
  it('closeSection / setAutoplay: the checkbox decides, and the Settings switch is the same list', () => {
    const armed = setAutoplay(DEFAULT_PREFS, 'now', true)
    expect(shouldAutoplay(armed, 'now', true)).toBe(true)
    const closed = closeSection(armed, 'now', false)
    expect(closed.autoplay).toEqual(['now'])
    expect(shouldAutoplay(closed, 'now', true)).toBe(true) // closed without the box: plays again next visit
    const done = closeSection(armed, 'now', true)
    expect(done.autoplay).toEqual([]) // finished or ticked: off, and the Settings switch reads the same list
    expect(shouldAutoplay(setAutoplay(done, 'now', true), 'now', true)).toBe(true) // switching on again replays
    expect(setAutoplay(setAutoplay(armed, 'now', true), 'now', true).autoplay).toEqual(['now']) // no duplicates
    expect(setAutoplay(armed, 'now', false).autoplay).toEqual([])
    expect(armed.autoplay).toEqual(['now']) // pure
  })
})

const nullCtx: TourContext = { data: null, d: null, overview: null, count: () => 0, text: () => null }
const emptyData: HubData = { whoami: null, work: null, activity: null, health: null, fresh: new Set(), lastOk: null, failing: false }
let mockCtx: TourContext
beforeAll(async () => {
  stubHub()
  const sig = new AbortController().signal
  const whoami = await fetchWhoami('mock', sig)
  const [work, activity, health] = await Promise.all([fetchWork(whoami.token.repository, 'mock', sig), fetchActivity(whoami.token.repository, 'mock', sig), fetchHealth(sig)])
  const overview = { recent_since: 0, until: 1, current: [], recent: [{}, {}], overlaps: [], current_complete: true, recent_complete: true, overlaps_complete: true, history_retention_gap: false } as unknown as WorkOverview
  mockCtx = { data: { ...emptyData, whoami, work, activity, health, lastOk: Date.now() }, d: derive(work, activity), overview, count: () => 2, text: () => 'sample text' }
  vi.unstubAllGlobals()
})

const allTours = (): [string, TourDef][] => [['welcome', WELCOME], ...Object.entries(SECTION_TOURS).map(([k, t]) => [k, t!] as [string, TourDef])]
const nonEmpty = (v: unknown) => typeof v === 'string' && v.trim().length > 0

describe('tour content', () => {
  it('every key is a real section and tourFor resolves it', () => {
    const keys = SECTIONS.map((s) => s.key as string)
    for (const k of Object.keys(SECTION_TOURS)) expect(keys, k).toContain(k)
    expect(tourFor('welcome')).toBe(WELCOME)
    for (const [k, t] of allTours()) expect(tourFor(k as never)).toBe(t)
    for (const s of SECTIONS) expect(tourFor(s.key) === null).toBe(!(s.key in SECTION_TOURS))
  })
  for (const [key, tour] of allTours()) {
    describe(`${key} tour`, () => {
      it('has a title and steps with title, what, why and how', () => {
        expect(nonEmpty(tour.title)).toBe(true)
        expect(tour.steps.length).toBeGreaterThan(0)
        tour.steps.forEach((s, i) => {
          for (const f of ['title', 'what', 'why', 'how'] as const) expect(nonEmpty(s[f]), `step ${i} ${f}`).toBe(true)
        })
        expect(new Set(tour.steps.map((s) => s.title)).size).toBe(tour.steps.length)
      })
      it('targets are non-empty valid selectors', () => {
        for (const s of tour.steps) {
          if (s.target === undefined) continue
          expect(s.target.length).toBeGreaterThan(0)
          for (const sel of s.target) {
            expect(nonEmpty(sel)).toBe(true)
            expect(() => document.querySelector(sel), sel).not.toThrow()
          }
        }
      })
      // Empty copy shows when the target is missing or the tip finds no data, so it needs one of the two to ever appear.
      it('empty copy is non-empty and reachable', () => {
        for (const s of tour.steps) if (s.empty !== undefined) {
          expect(nonEmpty(s.empty)).toBe(true)
          expect(s.target !== undefined || s.tip !== undefined, s.title).toBe(true)
        }
      })
      it('tips return a non-empty string or null and never throw', () => {
        for (const s of tour.steps) {
          if (!s.tip) continue
          for (const ctx of [nullCtx, { ...nullCtx, data: emptyData }, mockCtx]) {
            const t = s.tip(ctx)
            expect(t === null || nonEmpty(t), s.title).toBe(true)
          }
          expect(s.tip(nullCtx), `${s.title} with nothing loaded`).toBeNull()
        }
      })
    })
  }
})

afterEach(() => {
  vi.restoreAllMocks()
  window.localStorage.setItem(KEY, JSON.stringify({ welcomeSeen: true, welcomeOff: true, autoplay: [], sectionsSeen: [] }))
})

const shell = () => (
  <TourProvider section="now" data={emptyData} d={null} overview={null}>
    <button>opener</button>
    <TourTrigger section="now" />
  </TourProvider>
)
const fresh = () => window.localStorage.removeItem(KEY)
const stored = () => JSON.parse(window.localStorage.getItem(KEY) ?? 'null')
const dialog = () => screen.queryByRole('dialog')

describe('TourProvider overlay', () => {
  it('opens the welcome tour on first render and steps with Next and Back', () => {
    fresh()
    render(shell())
    const d = screen.getByRole('dialog')
    expect(d.getAttribute('aria-modal')).toBe('true')
    expect(within(d).getByRole('heading', { level: 2 }).textContent).toBe(WELCOME.steps[0]!.title)
    expect(within(d).getByText(`1 / ${WELCOME.steps.length}`)).toBeTruthy()
    expect(within(d).queryByRole('button', { name: /Back/ })).toBeNull()
    fireEvent.click(within(d).getByRole('button', { name: /Next/ }))
    expect(within(d).getByRole('heading', { level: 2 }).textContent).toBe(WELCOME.steps[1]!.title)
    expect(within(d).getByText(`2 / ${WELCOME.steps.length}`)).toBeTruthy()
    fireEvent.click(within(d).getByRole('button', { name: /Back/ }))
    expect(within(d).getByText(`1 / ${WELCOME.steps.length}`)).toBeTruthy()
    // Arrow keys move too, and the last step offers Done.
    for (let n = 1; n < WELCOME.steps.length; n++) fireEvent.keyDown(document, { key: 'ArrowRight' })
    expect(within(d).getByRole('button', { name: 'Done' })).toBeTruthy()
    fireEvent.keyDown(document, { key: 'ArrowLeft' })
    expect(within(d).getByText(`${WELCOME.steps.length - 1} / ${WELCOME.steps.length}`)).toBeTruthy()
  })
  it('announces the step politely', () => {
    fresh()
    const { container } = render(shell())
    const live = container.ownerDocument.querySelector('[aria-live="polite"]')!
    expect(live.textContent).toContain(`Step 1 of ${WELCOME.steps.length}`)
  })
  it('Escape or Skip without the box closes the tour, and it comes back next time', () => {
    fresh()
    const first = render(shell())
    fireEvent.keyDown(document, { key: 'Escape' })
    expect(dialog()).toBeNull()
    expect(shouldWelcome(loadPrefs(), true)).toBe(true)
    first.unmount()
    render(shell())
    expect(dialog()).not.toBeNull()
    fireEvent.click(screen.getByRole('button', { name: 'Skip' }))
    expect(dialog()).toBeNull()
    expect(shouldWelcome(loadPrefs(), true)).toBe(true)
  })
  it('Done on the last step finishes the tour for good', () => {
    fresh()
    render(shell())
    for (let n = 1; n < WELCOME.steps.length; n++) fireEvent.keyDown(document, { key: 'ArrowRight' })
    fireEvent.click(screen.getByRole('button', { name: 'Done' }))
    expect(stored()).toMatchObject({ welcomeSeen: true, welcomeOff: true })
  })
  it('writes onto what storage holds now, so another tab’s change survives', () => {
    window.localStorage.setItem(KEY, JSON.stringify({ welcomeSeen: true, welcomeOff: true, autoplay: [], sectionsSeen: [] }))
    const Switch = () => {
      const tour = useTour()
      return <button onClick={() => tour!.update((p) => setAutoplay(p, 'now', true))}>arm now</button>
    }
    render(<TourProvider section="settings" data={emptyData} d={null} overview={null}><Switch /></TourProvider>)
    // Another tab switches the Work tour on after this one loaded.
    window.localStorage.setItem(KEY, JSON.stringify({ welcomeSeen: true, welcomeOff: true, autoplay: ['work'], sectionsSeen: [] }))
    fireEvent.click(screen.getByRole('button', { name: 'arm now' }))
    expect(stored().autoplay).toEqual(['work', 'now'])
  })
  it('does not start by itself while signed out, and still starts by hand', () => {
    fresh()
    render(<TourProvider section="now" signedIn={false} data={emptyData} d={null} overview={null}><TourTrigger section="now" /></TourProvider>)
    expect(dialog()).toBeNull()
    fireEvent.click(screen.getByRole('button'))
    expect(dialog()).not.toBeNull()
  })
  it('Back keeps focus on Back', async () => {
    fresh()
    render(shell())
    fireEvent.keyDown(document, { key: 'ArrowRight' })
    fireEvent.keyDown(document, { key: 'ArrowRight' })
    const back = screen.getByRole('button', { name: /Back/ })
    back.focus()
    fireEvent.click(back)
    await new Promise((r) => setTimeout(r, 10))
    expect(document.activeElement?.textContent).toMatch(/Back/)
  })
  it('“Don’t show again” persists as welcomeOff', () => {
    fresh()
    render(shell())
    fireEvent.click(screen.getByRole('checkbox', { name: 'Don’t show again' }))
    fireEvent.keyDown(document, { key: 'Escape' })
    expect(stored()).toMatchObject({ welcomeSeen: true, welcomeOff: true })
  })
  it('does not reopen once seen, and the trigger replays the page tour', () => {
    window.localStorage.setItem(KEY, JSON.stringify({ welcomeSeen: true, welcomeOff: false, autoplay: [], sectionsSeen: [] }))
    render(shell())
    expect(dialog()).toBeNull()
    fireEvent.click(screen.getByRole('button', { name: 'Tour this page' }))
    expect(screen.getByRole('heading', { level: 2 }).textContent).toBe(SECTION_TOURS.now!.steps[0]!.title)
    expect(screen.queryByRole('checkbox', { name: 'Don’t show again' })).toBeNull() // only offered when auto-play is on
    fireEvent.keyDown(document, { key: 'Escape' })
    expect(dialog()).toBeNull()
  })
  it('auto-plays a page tour switched on in Settings, once', () => {
    window.localStorage.setItem(KEY, JSON.stringify({ welcomeSeen: true, welcomeOff: true, autoplay: ['now'], sectionsSeen: [] }))
    render(shell())
    expect(screen.getByRole('dialog')).toBeTruthy()
    fireEvent.click(screen.getByRole('checkbox', { name: 'Don’t show again' }))
    fireEvent.keyDown(document, { key: 'Escape' })
    expect(stored().autoplay).toEqual([])
  })
  it('traps Tab inside the card', () => {
    fresh()
    render(shell())
    const d = screen.getByRole('dialog')
    const buttons = [...d.querySelectorAll<HTMLElement>('button, input')]
    buttons[buttons.length - 1]!.focus()
    fireEvent.keyDown(document.activeElement!, { key: 'Tab' })
    expect(d.contains(document.activeElement)).toBe(true)
    buttons[0]!.focus()
    fireEvent.keyDown(document.activeElement!, { key: 'Tab', shiftKey: true })
    expect(document.activeElement).toBe(buttons[buttons.length - 1])
  })
  it('with storage blocked: no auto-open, no crash, the trigger still starts a tour', () => {
    vi.spyOn(window, 'localStorage', 'get').mockImplementation(() => { throw new DOMException('blocked', 'SecurityError') })
    render(shell())
    expect(dialog()).toBeNull()
    act(() => screen.getByRole('button', { name: 'Tour this page' }).click())
    expect(screen.getByRole('dialog')).toBeTruthy()
    fireEvent.keyDown(document, { key: 'Escape' }) // closing must not throw either
    expect(dialog()).toBeNull()
  })
  it('renders no trigger for a section without a tour, and nothing outside a provider', () => {
    const missing = SECTIONS.find((s) => !(s.key in SECTION_TOURS))
    const { container } = render(<><TourTrigger section="now" />{missing && <TourTrigger section={missing.key} />}</>)
    expect(container.querySelector('.tour-trigger')).toBeNull() // no provider
  })
})
