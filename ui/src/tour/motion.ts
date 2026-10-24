// Card motion: the card glides between placements (FLIP, transform only) and new step content slides in from the direction of travel.
import { useLayoutEffect, useRef, type RefObject } from 'react'

const EASE = 'cubic-bezier(0.25, 1, 0.5, 1)'
const reduced = () => window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false

/** Call on every render: when the card's box moved since the last one, slide it from the old spot. */
export function useGlide(ref: RefObject<HTMLElement | null>) {
  const prev = useRef<DOMRect | null>(null)
  const anim = useRef<Animation | null>(null)
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return
    anim.current?.cancel()
    const r = el.getBoundingClientRect()
    const p = prev.current
    prev.current = r
    if (!p || reduced() || typeof el.animate !== 'function') return
    const dx = p.left - r.left
    const dy = p.top - r.top
    if (Math.abs(dx) + Math.abs(dy) < 2) return
    anim.current = el.animate([{ transform: `translate(${dx}px, ${dy}px)` }, { transform: 'none' }], { duration: 260, easing: EASE })
  })
}

/** Content enters from +12px going forward, -12px going back. Not on first paint. */
export function useSwap(ref: RefObject<HTMLElement | null>, i: number) {
  const prev = useRef(i)
  useLayoutEffect(() => {
    const from = prev.current
    prev.current = i
    const el = ref.current
    if (from === i || !el || reduced() || typeof el.animate !== 'function') return
    const dx = i > from ? 12 : -12
    el.animate([{ opacity: 0, transform: `translateX(${dx}px)` }, { opacity: 1, transform: 'none' }], { duration: 200, easing: EASE })
  }, [ref, i])
}
