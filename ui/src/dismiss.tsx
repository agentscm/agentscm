// Per-browser "Hide" for attention rows nobody has touched in a day. Only this browser's localStorage
// remembers it: the hub is never told, and an agent's records stay exactly as they are. A row is keyed by
// its id and its last-updated time, so anything that changes comes back by itself.
import { useState, useSyncExternalStore } from 'react'
import { Icon } from './ui.tsx'

const KEY = 'agentscm.hidden.v1'
const MAX = 500
/** A row can be hidden once nothing has happened to it for this long. */
export const STALE_SECONDS = 24 * 3600

export const hideKey = (id: string, updatedAt: number) => `${id}@${updatedAt}`
/** Old enough to hide; an unknown time (0) never is. */
export const isStale = (updatedAt: number, nowS: number) => updatedAt > 0 && nowS - updatedAt >= STALE_SECONDS

const listeners = new Set<() => void>()
let cache: { raw: string | null; keys: ReadonlySet<string> } = { raw: null, keys: new Set() }

function stored(): string | null {
  try {
    return window.localStorage.getItem(KEY)
  } catch {
    return null
  }
}

function read(): ReadonlySet<string> {
  const raw = stored()
  if (raw === cache.raw) return cache.keys
  let list: unknown = []
  try {
    list = JSON.parse(raw ?? '[]')
  } catch {
    /* unreadable: nothing hidden */
  }
  cache = { raw, keys: new Set(Array.isArray(list) ? list.filter((k): k is string => typeof k === 'string').slice(-MAX) : []) }
  return cache.keys
}

function subscribe(cb: () => void) {
  listeners.add(cb)
  window.addEventListener('storage', cb)
  return () => {
    listeners.delete(cb)
    window.removeEventListener('storage', cb)
  }
}

export function setHidden(key: string, hidden: boolean): void {
  const next = new Set(read())
  if (hidden) next.add(key)
  else next.delete(key)
  try {
    window.localStorage.setItem(KEY, JSON.stringify([...next].slice(-MAX)))
  } catch {
    /* not saved: hidden for this page load only */
  }
  cache = { raw: stored(), keys: next }
  for (const l of listeners) l()
}

/** The hidden keys, shared by every component that reads them. */
export function useHidden(): ReadonlySet<string> {
  return useSyncExternalStore(subscribe, read)
}

/** Rows to draw, hiding the hidden ones unless asked; `count` is how many are hidden. */
export function useHideable<T>(rows: T[], keyOf: (row: T) => string) {
  const hidden = useHidden()
  const [show, setShow] = useState(false)
  const count = rows.filter((r) => hidden.has(keyOf(r))).length
  return { rows: show ? rows : rows.filter((r) => !hidden.has(keyOf(r))), count, show: show && count > 0, setShow, isHidden: (r: T) => hidden.has(keyOf(r)) }
}

/** `k` may list several keys when one row stands for several items, such as the folded overlaps. */
export function HideButton({ k, hidden, what }: { k: string | string[]; hidden: boolean; what: string }) {
  return (
    <button type="button" className="hide-btn" onClick={() => [k].flat().forEach((x) => setHidden(x, !hidden))} aria-label={hidden ? `Show ${what} again` : `Hide ${what} in this browser`} title={hidden ? undefined : 'Hidden in this browser only. It comes back if it changes.'}>
      <Icon name={hidden ? 'eye' : 'eye-slash'} /> {hidden ? 'Unhide' : 'Hide'}
    </button>
  )
}

export function ShowHidden({ count, show, onShow }: { count: number; show: boolean; onShow: (v: boolean) => void }) {
  if (count === 0) return null
  return (
    <button type="button" className="show-hidden" aria-pressed={show} onClick={() => onShow(!show)}>
      <Icon name={show ? 'eye-slash' : 'eye'} /> {show ? `Hide ${count} again` : `Show ${count} hidden`}
    </button>
  )
}
