// One pager for every list that can grow, plus a hook that keeps the current cursor in the hash query
// (`#landed?before=<cursor>`) so a page can be linked to and Back works.
import { useCallback, useEffect, useRef, useState } from 'react'
import { Icon } from './ui.tsx'

/** The hash without its `?query`: route parsers use this so a cursor never breaks routing. */
export const hashPath = (hash = window.location.hash): string => hash.split('?')[0]!
export const hashQuery = (hash = window.location.hash): URLSearchParams => new URLSearchParams(hash.includes('?') ? hash.slice(hash.indexOf('?') + 1) : '')

export interface HashCursor {
  /** The cursor in the URL, or null on the first page. */
  cursor: string | null
  /** Go to an older page: pushes a history entry and remembers where we came from. */
  older: (next: string) => void
  /** Step back through the stack kept in history.state; opened from a link, this returns to the first page. */
  newer: () => void
  canNewer: boolean
  /** Back to the first page (after something new was created). */
  first: () => void
}

export function useHashCursor(param = 'before'): HashCursor {
  const [hash, setHash] = useState(() => window.location.hash)
  useEffect(() => {
    const on = () => setHash(window.location.hash)
    window.addEventListener('hashchange', on)
    window.addEventListener('popstate', on)
    return () => {
      window.removeEventListener('hashchange', on)
      window.removeEventListener('popstate', on)
    }
  }, [])
  const key = `pager:${param}`
  const cursor = hashQuery(hash).get(param) || null
  const stored: unknown = (window.history.state as Record<string, unknown> | null)?.[key]
  const stack = Array.isArray(stored) ? stored.filter((x): x is string => typeof x === 'string') : []
  const go = useCallback(
    (to: string | null, nextStack: string[]) => {
      const q = hashQuery()
      if (to) q.set(param, to)
      else q.delete(param)
      const qs = q.toString()
      const state = { ...(window.history.state as Record<string, unknown> | null), [key]: nextStack }
      window.history.pushState(state, '', `${window.location.pathname}${window.location.search}${hashPath() || '#'}${qs ? `?${qs}` : ''}`)
      setHash(window.location.hash)
      window.dispatchEvent(new HashChangeEvent('hashchange'))
    },
    [param, key],
  )
  return {
    cursor,
    older: (next) => go(next, [...stack, cursor ?? '']),
    newer: () => go(stack.at(-1) || null, stack.slice(0, -1)),
    canNewer: cursor !== null,
    first: () => {
      if (cursor !== null) go(null, [])
    },
  }
}

/** Newer / Older buttons with an "everything" end state. Pass `onNewer`/`onOlder` only when that move exists. */
export function Pager({ onNewer, onOlder, complete = false, newerLabel = 'Newer', olderLabel = 'Older', status, tour = 'pager' }: { onNewer?: (() => void) | undefined; onOlder?: (() => void) | undefined; complete?: boolean; newerLabel?: string; olderLabel?: string; status?: string; tour?: string }) {
  const nav = useRef<HTMLElement>(null)
  // A clicked button can vanish (last page, or the list shows a skeleton while the next page loads); this nav then takes focus, once.
  const refocusUntil = useRef(0)
  useEffect(() => {
    if (Date.now() < refocusUntil.current && (!document.activeElement || document.activeElement === document.body)) {
      refocusUntil.current = 0
      nav.current?.focus({ preventScroll: true })
    }
  })
  if (!onNewer && !onOlder && !complete) return null
  const keepFocus = (go: () => void) => () => {
    refocusUntil.current = Date.now() + 5000
    go()
  }
  return (
    <nav ref={nav} tabIndex={-1} className="pager" aria-label="Pages" data-tour={tour}>
      {(onNewer || onOlder) && (
        <button type="button" className="btn btn-sm btn-outline-secondary" disabled={!onNewer} onClick={onNewer && keepFocus(onNewer)}>
          <Icon name="chevron-left" /> {newerLabel}
        </button>
      )}
      {status && <span className="small muted">{status}</span>}
      {onOlder ? (
        <button type="button" className="btn btn-sm btn-outline-secondary" onClick={keepFocus(onOlder)}>
          {olderLabel} <Icon name="chevron-right" />
        </button>
      ) : complete ? (
        <span className="small muted"><Icon name="check2" /> That’s everything.</span>
      ) : onNewer ? (
        <button type="button" className="btn btn-sm btn-outline-secondary" disabled>
          {olderLabel} <Icon name="chevron-right" />
        </button>
      ) : null}
    </nav>
  )
}
