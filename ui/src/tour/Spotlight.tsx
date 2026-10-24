// The scrim and the cut-out. One SVG: a mask punches a rounded hole, crop marks sit at its corners,
// and between steps the hole glides from the old target to the new one (a rAF tween on SVG attributes, no layout).
import { useEffect, useId, useLayoutEffect, useRef } from 'react'
import type { Rect } from './place.ts'

const EASE = 'cubic-bezier(0.25, 1, 0.5, 1)' // ease-out-quart
const ease = (p: number) => 1 - Math.pow(1 - p, 4)
const GLIDE_MS = 320
const MARK = 13 // crop mark arm length
const OUT = 4 // marks sit this far outside the hole
const reduced = () => window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false

interface Box { x: number; y: number; w: number; h: number }
const same = (a: Box, b: Box) => a.x === b.x && a.y === b.y && a.w === b.w && a.h === b.h

/** The visible part of the target, so a tall target is cut out only where it is on screen. */
function visible(r: Rect): Box {
  const vw = window.innerWidth
  const vh = window.innerHeight
  const x = Math.max(2, r.left)
  const y = Math.max(2, r.top)
  return { x, y, w: Math.max(0, Math.min(vw - 2, r.left + r.width) - x), h: Math.max(0, Math.min(vh - 2, r.top + r.height) - y) }
}

const marksPath = ({ x, y, w, h }: Box) => {
  const l = x - OUT, t = y - OUT, r = x + w + OUT, b = y + h + OUT
  return `M${l} ${t + MARK}V${t}H${l + MARK}M${r - MARK} ${t}H${r}V${t + MARK}M${r} ${b - MARK}V${b}H${r - MARK}M${l + MARK} ${b}H${l}V${b - MARK}`
}

/** `step` changes when the tour moves; the first new rect after that is glided to rather than snapped to. */
export function Spotlight({ rect, step }: { rect: Rect | null; step: number }) {
  const id = `m${useId().replace(/\W/g, '')}`
  const cut = useRef<SVGRectElement>(null)
  const frame = useRef<SVGRectElement>(null)
  const ping = useRef<SVGRectElement>(null)
  const marks = useRef<SVGGElement>(null)
  const markPath = useRef<SVGPathElement>(null)
  const cur = useRef<Box | null>(null)
  const goal = useRef<Box | null>(null)
  const raf = useRef(0)
  const pending = useRef(false)
  const lastStep = useRef(step)

  const paint = (b: Box) => {
    for (const el of [cut.current, frame.current, ping.current]) {
      el?.setAttribute('x', String(b.x))
      el?.setAttribute('y', String(b.y))
      el?.setAttribute('width', String(b.w))
      el?.setAttribute('height', String(b.h))
    }
    markPath.current?.setAttribute('d', marksPath(b))
  }
  // Marks settle in, one ping ring, on arrival. Skipped under reduced motion.
  const arrive = () => {
    if (reduced()) return
    marks.current?.animate?.([{ transform: 'scale(1.15)' }, { transform: 'scale(1)' }], { duration: 220, easing: EASE })
    ping.current?.animate?.([{ opacity: 0.7, transform: 'scale(1)' }, { opacity: 0, transform: 'scale(1.08)' }], { duration: 650, easing: EASE })
  }

  useEffect(() => {
    if (lastStep.current === step) return
    lastStep.current = step
    pending.current = true
    const t = window.setTimeout(() => (pending.current = false), 700)
    return () => window.clearTimeout(t)
  }, [step])

  useLayoutEffect(() => {
    if (!rect) {
      cancelAnimationFrame(raf.current)
      raf.current = 0
      cur.current = goal.current = null
      return
    }
    const to = visible(rect)
    goal.current = to
    const from = cur.current
    if (!from) {
      cur.current = to
      paint(to)
      arrive()
      return
    }
    if (raf.current) return // a glide is under way; it reads the goal on every frame
    if (pending.current && !reduced() && !same(from, to)) {
      pending.current = false
      const start = { ...from }
      const t0 = performance.now()
      const tick = (now: number) => {
        const g = goal.current!
        const p = Math.min(1, (now - t0) / GLIDE_MS)
        const e = ease(p)
        const b = { x: start.x + (g.x - start.x) * e, y: start.y + (g.y - start.y) * e, w: start.w + (g.w - start.w) * e, h: start.h + (g.h - start.h) * e }
        cur.current = b
        paint(b)
        if (p < 1) raf.current = requestAnimationFrame(tick)
        else {
          raf.current = 0
          cur.current = { ...g }
          paint(g)
          arrive()
        }
      }
      raf.current = requestAnimationFrame(tick)
    } else {
      cur.current = to
      paint(to)
    }
  }, [rect])

  useEffect(() => () => cancelAnimationFrame(raf.current), [])

  return (
    <svg className="tour-scrim" width="100%" height="100%" aria-hidden="true" focusable="false">
      <defs>
        <mask id={id}>
          <rect width="100%" height="100%" fill="#fff" />
          {rect && <rect ref={cut} rx={14} fill="#000" />}
        </mask>
      </defs>
      <rect className="scrim" width="100%" height="100%" mask={`url(#${id})`} />
      {rect && (
        <>
          <rect ref={ping} className="tour-ping" rx={14} />
          <rect ref={frame} className="tour-hole" rx={14} />
          <path ref={markPath} id={`${id}-marks`} fill="none" />
          <g ref={marks} className="tour-marks">
            <use href={`#${id}-marks`} className="halo" />
            <use href={`#${id}-marks`} className="ink" />
          </g>
        </>
      )}
    </svg>
  )
}
