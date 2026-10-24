import { createContext, useCallback, useContext, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type ReactNode } from 'react'
import type { Section } from '../App.tsx'
import type { Derived } from '../derive.ts'
import type { WorkOverview } from '../standup.ts'
import type { HubData } from '../useHub.ts'
import { Icon } from '../ui.tsx'
import { tourFor, type TourContext, type TourKey } from './content.ts'
import { canRemember, closeSection, closeWelcome, loadPrefs, savePrefs, shouldAutoplay, shouldWelcome, type TourPrefs } from './state.ts'

interface TourApi {
  prefs: TourPrefs
  /** False when this browser cannot store preferences; tours then only start by hand. */
  remember: boolean
  start: (key: TourKey) => void
  update: (f: (p: TourPrefs) => TourPrefs) => void
}

const Ctx = createContext<TourApi | null>(null)

/** Null outside a signed-in shell (sign-in page, tests that render one page). */
export const useTour = (): TourApi | null => useContext(Ctx)

export function TourProvider({ section, data, d, overview, children }: { section: Section; data: HubData; d: Derived | null; overview: WorkOverview | null; children: ReactNode }) {
  // Read once per load. Auto-play happens at most once per page open: the welcome tour first,
  // otherwise a page tour the viewer switched on in Settings.
  const [init] = useState(() => {
    const remember = canRemember()
    const prefs = loadPrefs()
    const first: TourKey | null = shouldWelcome(prefs, remember) ? 'welcome' : shouldAutoplay(prefs, section, remember) && tourFor(section) ? section : null
    return { remember, prefs, first }
  })
  const remember = init.remember
  const [prefs, setPrefs] = useState(init.prefs)
  const [active, setActive] = useState<TourKey | null>(init.first)
  const update = useCallback((f: (p: TourPrefs) => TourPrefs) => {
    setPrefs((p) => {
      const next = f(p)
      savePrefs(next)
      return next
    })
  }, [])
  const start = useCallback((key: TourKey) => {
    if (tourFor(key)) setActive(key)
  }, [])

  const [visited, setVisited] = useState(section)
  if (visited !== section) {
    setVisited(section)
    if (!active && shouldAutoplay(prefs, section, remember) && tourFor(section)) setActive(section)
  }

  const close = useCallback(
    (dontShow: boolean) => {
      if (active) update((p) => (active === 'welcome' ? closeWelcome(p, dontShow) : closeSection(p, active, dontShow)))
      setActive(null)
    },
    [active, update],
  )
  const api = useMemo(() => ({ prefs, remember, start, update }), [prefs, remember, start, update])
  return (
    <Ctx.Provider value={api}>
      {children}
      {active && <TourOverlay key={active} tourKey={active} data={data} d={d} overview={overview} onClose={close} autoplayOn={active !== 'welcome' && prefs.autoplay.includes(active)} />}
    </Ctx.Provider>
  )
}

/** The page trigger: small, beside the breadcrumbs, so it never competes with the page's answer. */
export function TourTrigger({ section }: { section: Section }) {
  const tour = useTour()
  if (!tour || !tourFor(section)) return null
  return (
    <button type="button" data-tour="tour-trigger" className="tour-trigger" onClick={() => tour.start(section)} aria-label="Tour this page" title="Tour this page">
      <Icon name="signpost-split" />
      <span className="tour-trigger-text">Tour this page</span>
    </button>
  )
}

interface Rect {
  top: number
  left: number
  width: number
  height: number
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

function TourOverlay({ tourKey, data, d, overview, onClose, autoplayOn }: { tourKey: TourKey; data: HubData; d: Derived | null; overview: WorkOverview | null; onClose: (dontShow: boolean) => void; autoplayOn: boolean }) {
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
    return () => opener?.focus({ preventScroll: true })
  }, [])
  useEffect(() => {
    // Deferred a task: started from ⌘K with Enter, the key's own press would otherwise land on the freshly focused Next and skip step one.
    const t = window.setTimeout(() => card.current?.querySelector<HTMLElement>('.tour-primary')?.focus({ preventScroll: true }), 0)
    return () => window.clearTimeout(t)
  }, [i])

  // Bring the target into view, then follow it through scrolls, resizes and data refreshes.
  useEffect(() => {
    const el = findTarget(step.target)
    el?.scrollIntoView({ block: 'center', inline: 'nearest', behavior: reducedMotion() ? 'auto' : 'smooth' })
    let frame = 0
    const measure = () => {
      cancelAnimationFrame(frame)
      frame = requestAnimationFrame(() => {
        const t = findTarget(step.target)
        const r = t?.getBoundingClientRect()
        setRect(r ? { top: r.top - PAD, left: r.left - PAD, width: r.width + PAD * 2, height: r.height + PAD * 2 } : null)
        setCardH(card.current?.offsetHeight ?? 0)
      })
    }
    measure()
    const poll = window.setInterval(measure, 500)
    window.addEventListener('resize', measure)
    window.addEventListener('scroll', measure, true)
    return () => {
      cancelAnimationFrame(frame)
      window.clearInterval(poll)
      window.removeEventListener('resize', measure)
      window.removeEventListener('scroll', measure, true)
    }
  }, [step])

  const next = useCallback(() => (last ? onClose(dontShow) : setI((n) => n + 1)), [last, onClose, dontShow])
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

  // Card placement: below the target when it fits, else above, else beside it, else pinned to the bottom (phones: always a sheet).
  const vw = window.innerWidth
  const vh = window.innerHeight
  const narrow = vw <= 640
  const cardW = Math.min(380, vw - 32)
  let style: CSSProperties
  if (!rect) style = { inset: 0, margin: 'auto', height: 'fit-content', width: cardW } // no transform: the entrance animation owns it
  else if (narrow) style = rect.top + rect.height / 2 > vh / 2 ? { top: 12, left: 12, right: 12 } : { bottom: 12, left: 12, right: 12 }
  else {
    const h = cardH || 280
    const left = Math.max(16, Math.min(rect.left, vw - cardW - 16))
    const below = rect.top + rect.height + 12
    const above = rect.top - 12 - h
    const right = rect.left + rect.width + 12
    const beside = right + cardW <= vw - 16 ? right : rect.left - 12 - cardW >= 16 ? rect.left - 12 - cardW : null
    const sideTop = Math.max(16, Math.min(rect.top, vh - h - 16))
    // A target taller than the room above and below it gets the card beside it, so the card never covers it.
    if (below + h <= vh - 16) style = { top: below, left, width: cardW }
    else if (above >= 16) style = { top: above, left, width: cardW }
    else if (beside !== null) style = { top: sideTop, left: beside, width: cardW }
    else style = { top: Math.max(16, vh - h - 16), left, width: cardW }
  }

  const titleId = `${ids}-title`
  return (
    <div className="tour-layer">
      {/* The cutout: one box whose huge shadow dims everything else. Clicks behind the tour are blocked. */}
      <div className="tour-block" onMouseDown={(e) => e.preventDefault()} />
      {rect ? <div className="tour-hole" style={{ top: rect.top, left: rect.left, width: rect.width, height: rect.height }} aria-hidden="true" /> : <div className="tour-dim" aria-hidden="true" />}
      <div ref={card} className={`tour-card${narrow ? ' is-sheet' : ''}`} style={style} role="dialog" aria-modal="true" aria-labelledby={titleId} aria-describedby={`${ids}-body`}>
        <div className="tour-head">
          <span className="tour-kicker">{tour.title}</span>
          <button type="button" className="tour-x" onClick={() => onClose(dontShow)} aria-label="Close tour">
            <Icon name="x-lg" />
          </button>
        </div>
        <div className="tour-progress" aria-hidden="true">
          {steps.map((_, n) => (
            <span key={n} className={n === i ? 'on' : n < i ? 'done' : ''} />
          ))}
        </div>
        <p className="visually-hidden" aria-live="polite" aria-atomic="true">
          Step {i + 1} of {steps.length}: {step.title}
        </p>
        <h2 id={titleId} className="tour-title">{step.title}</h2>
        <dl id={`${ids}-body`} className="tour-body">
          <div><dt><Icon name="eye" /> What it shows</dt><dd>{step.what}</dd></div>
          <div><dt><Icon name="exclamation-circle" /> Why it matters</dt><dd>{step.why}</dd></div>
          <div><dt><Icon name="hand-index" /> How to use it</dt><dd>{step.how}</dd></div>
        </dl>
        {shownTip && (
          <p className={`tour-tip${tip ? ' is-live' : ''}`}>
            <Icon name={tip ? 'lightning-charge' : 'hourglass'} />
            <span><strong>{tip ? 'On your screen now: ' : 'Nothing here yet: '}</strong>{shownTip}</span>
          </p>
        )}
        <div className="tour-foot">
          {(isWelcome || autoplayOn) && (
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
