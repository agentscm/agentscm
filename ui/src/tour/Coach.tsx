// The Dashboard's first-visit introduction: a short pointer beside the headline, the Needs you list and the track.
// In the page, not over it: nothing is blocked, each pointer is dismissed on its own, "Skip tips" ends the set, and both
// stay gone (the welcome tour's flags in tour/state.ts). The tour itself is still one click away from the ? button.
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { coachLeft, dismissCoach, skipCoach, type CoachId } from './state.ts'
import { useTour } from './Tour.tsx'

const LEAVE_MS = 320
const reduced = () => window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false

/** `side` is where the target is: the pointer's tail points that way. Pointers are in page order; the first one still on the page carries "Skip tips". */
export function Coach({ id, side, children }: { id: CoachId; side: 'above' | 'below'; children: ReactNode }) {
  const tour = useTour()
  const [leaving, setLeaving] = useState(false)
  const root = useRef<HTMLDivElement>(null)
  const timer = useRef<number>()
  useEffect(() => () => window.clearTimeout(timer.current), [])
  if (!tour) return null
  const left = coachLeft(tour.prefs, tour.remember)
  if (!left.includes(id)) return null
  const first = left[0] === id

  const done = (f: Parameters<typeof tour.update>[0]) => {
    // The pointer folds away (the page below glides up), then the choice is saved; with reduced motion it is immediate.
    // Focus moves on to what the pointer was about, so a keyboard user is not dropped at the top of the page.
    const next = root.current?.nextElementSibling ?? root.current?.previousElementSibling
    next?.querySelector<HTMLElement>('a[href], button:not([disabled])')?.focus({ preventScroll: true })
    if (reduced()) return tour.update(f)
    setLeaving(true)
    timer.current = window.setTimeout(() => tour.update(f), LEAVE_MS)
  }
  return (
    <div ref={root} className={`coach is-${side}${leaving ? ' is-leaving' : ''}`} role="note" aria-label="Tip" data-coach={id}>
      <div className="coach-clip">
        <div className="coach-card">
          <p className="coach-text">{children}</p>
          <div className="coach-acts">
            <button type="button" className="coach-btn" onClick={() => done((p) => dismissCoach(p, id))}>Got it</button>
            {first && <button type="button" className="coach-btn is-quiet" onClick={() => done(skipCoach)}>Skip tips</button>}
          </div>
        </div>
      </div>
    </div>
  )
}
