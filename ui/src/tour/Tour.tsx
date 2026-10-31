import { createContext, useCallback, useContext, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import type { Section } from '../App.tsx'
import type { Derived } from '../derive.ts'
import type { WorkOverview } from '../standup.ts'
import type { HubData } from '../useHub.ts'
import { Icon } from '../ui.tsx'
import { tourFor, type TourContext, type TourKey } from './content.ts'
import './tour.css'
import { Glyph, TourArt, WelcomeHero } from './art.tsx'
import { useGlide, useSwap } from './motion.ts'
import { WIDE_FROM, placeCard, type Rect } from './place.ts'
import { Spotlight } from './Spotlight.tsx'
import { Trail } from './Trail.tsx'
import { TOUR_KEY, canRemember, closeSection, closeWelcome, helpPulse, loadPrefs, savePrefs, seeHelp, shouldAutoplay, shouldWelcome, type TourPrefs } from './state.ts'

interface TourApi {
  prefs: TourPrefs
  /** False when this browser cannot store preferences; tours then only start by hand. */
  remember: boolean
  /** The "?" trigger invites a first look: a new account that has not used it yet. */
  pulse: boolean
  start: (key: TourKey) => void
  update: (f: (p: TourPrefs) => TourPrefs) => void
}

const Ctx = createContext<TourApi | null>(null)

/** Null outside a signed-in shell (sign-in page, tests that render one page). */
export const useTour = (): TourApi | null => useContext(Ctx)

export function TourProvider({ section, data, d, overview, signedIn = true, joinedAt, children }: { section: Section; /** Unix seconds of the account's first sign-in to this Hub (BrowserSession.joinedAt); unknown means no pulse. */ joinedAt?: number | null; /** False on the signed-out shell: nothing starts by itself there. */ signedIn?: boolean; data: HubData; d: Derived | null; overview: WorkOverview | null; children: ReactNode }) {
  // Read once per load. Auto-play happens at most once per page open, and only for a page tour the viewer
  // switched on in Settings. The welcome tour no longer opens by itself: the Dashboard introduces itself
  // with coach marks (Coach.tsx), and the tour stays one click away.
  const [init] = useState(() => {
    const remember = canRemember()
    const prefs = loadPrefs()
    const first: TourKey | null = signedIn && shouldAutoplay(prefs, section, remember) && tourFor(section) ? section : null
    return { remember, prefs, first }
  })
  const remember = init.remember
  const [prefs, setPrefs] = useState(init.prefs)
  const [active, setActive] = useState<TourKey | null>(init.first)
  // Applied to what storage holds now, so a stale tab never overwrites another tab's change.
  const update = useCallback(
    (f: (p: TourPrefs) => TourPrefs) => {
      if (!remember) return setPrefs(f)
      const next = f(loadPrefs())
      savePrefs(next)
      setPrefs(next)
    },
    [remember],
  )
  useEffect(() => {
    const on = (e: StorageEvent) => {
      if (e.key === null || e.key === TOUR_KEY) setPrefs(loadPrefs())
    }
    window.addEventListener('storage', on)
    return () => window.removeEventListener('storage', on)
  }, [])
  // Using the trigger (or ⌘K, or a Settings button) to open a tour answers the invitation to look around, for this browser.
  const start = useCallback(
    (key: TourKey) => {
      if (!tourFor(key)) return
      update(seeHelp)
      setActive(key)
    },
    [update],
  )
  // Read once per load: the pulse ends at the next page load once the account is a day old.
  const [openedAt] = useState(() => Date.now() / 1000)
  const pulse = signedIn && helpPulse(prefs, joinedAt, openedAt, remember)

  const [visited, setVisited] = useState(section)
  if (visited !== section) {
    setVisited(section)
    // A page tour belongs to its page: leaving the page closes it (without counting as finished).
    const stays = active === 'welcome' || active === section
    if (!stays) setActive(shouldAutoplay(prefs, section, remember) && tourFor(section) ? section : null)
  }

  const close = useCallback(
    (dontShow: boolean) => {
      if (active) update((p) => seeHelp(active === 'welcome' ? closeWelcome(p, dontShow) : closeSection(p, active, dontShow)))
      setActive(null)
    },
    [active, update],
  )
  const api = useMemo(() => ({ prefs, remember, pulse, start, update }), [prefs, remember, pulse, start, update])
  return (
    <Ctx.Provider value={api}>
      {children}
      {active && <TourOverlay key={active} tourKey={active} data={data} d={d} overview={overview} onClose={close} stoppable={active === 'welcome' ? shouldWelcome(prefs, remember) : prefs.autoplay.includes(active)} />}
    </Ctx.Provider>
  )
}

/**
 * The page trigger: small, beside the breadcrumbs, so it never competes with the page's answer.
 * A page without its own tour offers the welcome tour, so the top bar keeps one shape on every page.
 */
export function TourTrigger({ section }: { section: Section }) {
  const tour = useTour()
  if (!tour) return null
  const key: TourKey = tourFor(section) ? section : 'welcome'
  const label = key === 'welcome' ? 'Tour the hub' : 'Tour this page'
  return (
    <button type="button" data-tour="tour-trigger" className={`tour-trigger${tour.pulse ? ' is-pulsing' : ''}`} onClick={() => tour.start(key)} aria-label={label} title={label}>
      <Icon name="question-lg" />
      <span className="tour-trigger-text">{label}</span>
    </button>
  )
}

const PAD = 6
const reducedMotion = () => window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false

/** The first selector whose element is on screen; a drawer pushed off the left edge does not count. */
function findTarget(selectors: string[] | undefined): HTMLElement | null {
  for (const s of selectors ?? []) {
    for (const el of document.querySelectorAll<HTMLElement>(s)) {
      if (el.closest('.tour-layer')) continue
      const r = el.getBoundingClientRect()
      if (r.width > 0 && r.height > 0 && r.right > 0 && r.left < window.innerWidth && getComputedStyle(el).visibility !== 'hidden') return el
    }
  }
  return null
}

const visibleCount = (selector: string): number => {
  try {
    return [...document.querySelectorAll<HTMLElement>(selector)].filter((el) => !el.closest('.tour-layer') && el.getClientRects().length > 0).length
  } catch {
    return 0
  }
}

const firstText = (selector: string): string | null => {
  try {
    const el = [...document.querySelectorAll<HTMLElement>(selector)].find((e) => !e.closest('.tour-layer') && e.getClientRects().length > 0)
    const t = el?.textContent?.replace(/\s+/g, ' ').trim()
    return t ? t.slice(0, 200) : null
  } catch {
    return null
  }
}

const FOCUSABLE = 'button:not([disabled]), input:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])'

function TourOverlay({ tourKey, data, d, overview, onClose, stoppable }: { tourKey: TourKey; data: HubData; d: Derived | null; overview: WorkOverview | null; onClose: (dontShow: boolean) => void; /** "Don’t show again" means something: the tour is set to open by itself. */ stoppable: boolean }) {
  const tour = tourFor(tourKey)!
  const steps = tour.steps
  const [i, setI] = useState(0)
  const [dontShow, setDontShow] = useState(false)
  const [rect, setRect] = useState<Rect | null>(null)
  const [cardH, setCardH] = useState(0)
  const card = useRef<HTMLDivElement>(null)
  const ids = useId()
  const step = steps[i]!
  const last = i === steps.length - 1
  const isWelcome = tourKey === 'welcome'

  // Focus moves into the card and returns to where it was when the tour closes.
  useLayoutEffect(() => {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null
    // The opener may be gone (Settings "Run now" changes page); the page itself is the next best place.
    return () => (opener?.isConnected ? opener : document.getElementById('main'))?.focus({ preventScroll: true })
  }, [])
  useEffect(() => {
    // Deferred a task: started from ⌘K with Enter, the key's own press would otherwise land on the freshly focused Next and skip step one.
    // Keep focus where it is when it is still on a control in the card (Back stays on Back).
    const t = window.setTimeout(() => {
      const here = document.activeElement
      if (!(here instanceof HTMLElement && here !== card.current && card.current?.contains(here))) card.current?.querySelector<HTMLElement>('.tour-primary')?.focus({ preventScroll: true })
    }, 0)
    return () => window.clearTimeout(t)
  }, [i])

  // Bring the target into view, then follow it through scrolls, resizes and data refreshes.
  useEffect(() => {
    const el = findTarget(step.target)
    // A target too tall to sit beside the card scrolls to its top (below the sticky top bar), so its start stays visible above the card.
    const tall = !!el && el.getBoundingClientRect().height > window.innerHeight - (card.current?.offsetHeight ?? 300) - 48
    const bring = (behavior: ScrollBehavior) => {
      if (!el) return
      if (!tall) return el.scrollIntoView({ block: 'center', inline: 'nearest', behavior })
      const bar = document.querySelector<HTMLElement>('[data-tour="topbar"]')?.offsetHeight ?? 0
      window.scrollTo({ top: window.scrollY + el.getBoundingClientRect().top - bar - 16, behavior })
    }
    bring(reducedMotion() ? 'auto' : 'smooth')
    // Moving through steps quickly can cancel a smooth scroll; if the target is still off screen, jump to it.
    const settle = window.setTimeout(() => {
      const r = el?.getBoundingClientRect()
      if (r && (r.bottom < 0 || r.top > window.innerHeight)) bring('auto')
    }, 600)
    let frame = 0
    const measure = () => {
      cancelAnimationFrame(frame)
      frame = requestAnimationFrame(() => {
        const t = findTarget(step.target)
        const r = t?.getBoundingClientRect()
        const next = r ? { top: r.top - PAD, left: r.left - PAD, width: r.width + PAD * 2, height: r.height + PAD * 2 } : null
        // Only a changed number re-renders the overlay; the 500ms poll is otherwise free.
        setRect((p) => (p && next && p.top === next.top && p.left === next.left && p.width === next.width && p.height === next.height ? p : next))
        setCardH((p) => { const h = card.current?.offsetHeight ?? 0; return p === h ? p : h })
      })
    }
    measure()
    const poll = window.setInterval(measure, 500)
    window.addEventListener('resize', measure)
    window.addEventListener('scroll', measure, true)
    return () => {
      cancelAnimationFrame(frame)
      window.clearInterval(poll)
      window.clearTimeout(settle)
      window.removeEventListener('resize', measure)
      window.removeEventListener('scroll', measure, true)
    }
  }, [step])

  const next = useCallback(() => (last ? onClose(true) : setI((n) => n + 1)), [last, onClose])
  const back = useCallback(() => setI((n) => Math.max(0, n - 1)), [])

  // Capture phase, so the shell's own shortcuts (⌘K, [, Escape for the drawer) stay quiet while a tour is open.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      e.stopPropagation()
      const inCheckbox = e.target instanceof HTMLInputElement
      if (e.key === 'Escape') {
        e.preventDefault()
        onClose(dontShow)
      } else if (e.key === 'ArrowRight' && !inCheckbox) {
        e.preventDefault()
        next()
      } else if (e.key === 'ArrowLeft' && !inCheckbox) {
        e.preventDefault()
        back()
      } else if (e.key === 'Tab') {
        const list = [...(card.current?.querySelectorAll<HTMLElement>(FOCUSABLE) ?? [])]
        if (list.length === 0) return
        const at = list.indexOf(document.activeElement as HTMLElement)
        const to = e.shiftKey ? (at <= 0 ? list.length - 1 : at - 1) : at === list.length - 1 ? 0 : at + 1
        e.preventDefault()
        list[to]?.focus()
      } else if ((e.metaKey || e.ctrlKey) && e.key === 'k') e.preventDefault()
    }
    document.addEventListener('keydown', onKey, true)
    return () => document.removeEventListener('keydown', onKey, true)
  }, [next, back, onClose, dontShow])

  const ctx: TourContext = { data, d, overview, count: visibleCount, text: firstText }
  let tip: string | null
  try {
    tip = rect || !step.target ? (step.tip?.(ctx) ?? null) : null
  } catch {
    tip = null // a tip must never break the tour; fall back to the empty-state copy
  }
  // Empty-state copy only when there is nothing to show: the target is missing, or its tip found no data.
  const missing = !!step.target && !rect
  const shownTip = tip ?? (missing || step.tip ? (step.empty ?? null) : null)

  // Placement is a pure decision (place.ts): beside, below or above the target; compact when narrow or the target is tall.
  const [fullH, setFullH] = useState(470)
  const [moreAt, setMoreAt] = useState<number | null>(null) // the step whose "More" is open
  const more = moreAt === i
  const hero = isWelcome && i === 0
  const place = placeCard(rect, { fullH, h: cardH, hero }, { w: window.innerWidth, h: window.innerHeight }, document.documentElement.dataset['density'] !== 'condensed')
  const compact = place.compact
  // Measure the card as it renders: its full height (with the illustration) feeds the compact decision.
  useLayoutEffect(() => {
    const h = card.current?.offsetHeight ?? 0
    setCardH((p) => (p === h ? p : h))
    if (!compact && h > 0) setFullH((p) => (p === h ? p : h))
  }, [compact, i, more, rect, shownTip])
  useGlide(card)
  const swap = useRef<HTMLDivElement>(null)
  useSwap(swap, i)
  const showMore = !compact || more

  const titleId = `${ids}-title`
  return (
    <div className="tour-layer">
      {/* Clicks behind the tour are blocked; the scrim itself is a picture. */}
      <div className="tour-block" onMouseDown={(e) => e.preventDefault()} />
      <Spotlight rect={rect} step={i} />
      <div ref={card} className={`tour-card${place.sheet ? ' is-sheet' : ''}${compact ? ' is-compact' : ''}${!compact && (place.style.width ?? 0) >= WIDE_FROM ? ' is-wide' : ''}`} style={place.style} role="dialog" aria-modal="true" aria-labelledby={titleId} aria-describedby={`${ids}-body`}>
        <div className="tour-scroll">
        {!compact && <div className={`tour-band${hero ? ' is-hero' : ''}`}>{hero ? <WelcomeHero /> : <TourArt tourKey={tourKey} step={i} key={i} />}</div>}
        <div className="tour-main">
          <div className="tour-head">
            <span className="tour-kicker">{tour.title}</span>
            <button type="button" className="tour-x" onClick={() => onClose(dontShow)} aria-label="Close tour">
              <Icon name="x-lg" />
            </button>
          </div>
          <Trail i={i} n={steps.length} />
          <p className="visually-hidden" aria-live="polite" aria-atomic="true">
            Step {i + 1} of {steps.length}: {step.title}
          </p>
          <div ref={swap} className="tour-swap">
            <h2 id={titleId} className="tour-title">{step.title}</h2>
            <div id={`${ids}-body`} className="tour-swap tour-cols">
              <p className="tour-what">{step.what}</p>
              {compact && (
                <button type="button" className="tour-more" aria-expanded={more} aria-controls={`${ids}-more`} onClick={() => setMoreAt(more ? null : i)}>
                  {more ? 'Less' : 'More'}
                </button>
              )}
              {showMore && (
                <dl id={`${ids}-more`} className="tour-body">
                  <div className="tour-row"><Glyph kind="why" /><div><dt>Why it matters</dt><dd>{step.why}</dd></div></div>
                  <div className="tour-row"><Glyph kind="how" /><div><dt>How to use it</dt><dd>{step.how}</dd></div></div>
                </dl>
              )}
            </div>
            {shownTip && (
              <p className={`tour-tip${tip ? ' is-live' : ''}`}>
                {tip ? <span className="tour-live-dot" aria-hidden="true" /> : <Glyph kind="wait" />}
                <span><strong>{tip ? 'On your screen now: ' : 'Nothing here yet: '}</strong>{shownTip}</span>
              </p>
            )}
          </div>
        </div>
        </div>
        <div className="tour-foot">
          {stoppable && (
            <label className="tour-check">
              <input type="checkbox" checked={dontShow} onChange={(e) => setDontShow(e.target.checked)} /> Don’t show again
            </label>
          )}
          <span className="tour-count">{i + 1} / {steps.length}</span>
          <div className="tour-buttons">
            {i > 0 ? (
              <button type="button" className="btn btn-sm btn-outline-secondary" onClick={back}>
                <Icon name="arrow-left" /> Back
              </button>
            ) : (
              <button type="button" className="btn btn-sm btn-link" onClick={() => onClose(dontShow)}>Skip</button>
            )}
            <button type="button" className="btn btn-sm btn-primary tour-primary" onClick={next}>
              {last ? 'Done' : <>Next <Icon name="arrow-right" /></>}
            </button>
          </div>
        </div>
        <p className="tour-keys" aria-hidden="true"><kbd>←</kbd><kbd>→</kbd> move · <kbd>esc</kbd> close</p>
      </div>
    </div>
  )
}
