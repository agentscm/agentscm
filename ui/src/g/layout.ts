// The G layout: a second chrome over the same pages and routes. Opening `#g` (or `#g/<route>`) turns it on for this browser;
// "Classic layout" in its top bar turns it off. Routes stay the Hub's own, plus a few G-only pages.
import { useEffect, useState } from 'react'
import { workIdFromHash } from '../standup.ts'

const KEY = 'agentscm.layout'

export function loadLayout(): 'g' | 'classic' {
  try {
    return window.localStorage.getItem(KEY) === 'g' ? 'g' : 'classic'
  } catch {
    return 'classic'
  }
}

export function saveLayout(v: 'g' | 'classic'): void {
  try {
    window.localStorage.setItem(KEY, v)
  } catch {
    // Private windows can refuse storage; the choice then lasts for this page only.
  }
}

/**
 * `#g` turns G on and lands on Now; `#g/<route>` turns it on and opens that route, so a link can share any page in G.
 * Returns whether the hash was such a link. main.tsx calls it before the first render, so the section router never
 * sees the `#g` prefix.
 */
export function takeGLink(): boolean {
  const m = /^#g(?:\/(.*))?$/.exec(window.location.hash)
  if (!m) return false
  saveLayout('g')
  window.history.replaceState(null, '', `${window.location.pathname}${window.location.search}#${m[1] || 'home'}`)
  return true
}

/** The layout for this page; a `#g` link typed into an open tab switches to G too. */
export function useLayout(): ['g' | 'classic', (v: 'g' | 'classic') => void] {
  const [layout, setLayout] = useState(loadLayout)
  useEffect(() => {
    const on = () => {
      if (!takeGLink()) return
      setLayout('g')
      // replaceState fires no event, so tell the section router the route changed.
      window.dispatchEvent(new HashChangeEvent('hashchange'))
    }
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  return [layout, (v) => { saveLayout(v); setLayout(v) }]
}

/** G-only pages, by hash. Everything else falls through to the Hub's own section router. */
export type GPage = 'now' | 'changes' | 'change' | 'process' | 'code' | 'contributions' | 'features'

export function gRoute(hash: string): { page: GPage; id?: string } | null {
  const h = hash.replace(/^#/, '').split('?')[0] ?? ''
  const [first = '', second] = h.split('/')
  if (first === '' || first === 'home' || first === 'coordination') return { page: 'now' }
  if (first === 'work' && (!second || second === 'working')) return { page: 'now' }
  const id = workIdFromHash(`#${h}`)
  if (id) return { page: 'change', id }
  if (first === 'changes') return { page: 'changes' }
  if (first === 'process') return { page: 'process' }
  if (first === 'code') return { page: 'code' }
  if (first === 'contributions') return { page: 'contributions' }
  if (first === 'features') return { page: 'features' }
  return null
}

export function useHash(): string {
  const [hash, setHash] = useState(window.location.hash)
  useEffect(() => {
    const on = () => setHash(window.location.hash)
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  return hash
}
