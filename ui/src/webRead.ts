// Opening the hosted Hub's pages on a web sign-in alone. With no pasted token, the page asks the Hub
// for a 30-minute read-only token for one repository (POST /v1/hub/session/read-token) and keeps it
// in memory only: never in localStorage or sessionStorage, so a reload asks once more. Each ask
// stores a token on the Hub, so the page asks only when it has none, two minutes before it
// expires, or after a 401, and never twice at once.
import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError } from './api.ts'
import { fetchBrowserSession, mintWebReadToken, sessionRepositories, signOutBrowser, type BrowserSession, type WebReadToken } from './devices.ts'
import type { Refusal } from './refusal.ts'

export type WebRead =
  | { kind: 'off' }
  | { kind: 'checking' }
  /** No web sign-in, or nothing it may open; `refusal` says why when the Hub said no. */
  | { kind: 'none'; refusal: Refusal | null; empty: boolean }
  /** `restarts` counts renewals after a 401, which stopped the page's polling; the page remounts on it. */
  | { kind: 'ready'; session: BrowserSession; repos: string[]; read: WebReadToken; refusal: Refusal | null; restarts: number }

const PICKED_KEY = 'agentscm.web.repository'
export const RENEW_BEFORE_SECONDS = 120

// Only the repository name is remembered, to open the same one next time; never the token.
const picked = (): string | null => {
  try {
    return window.localStorage.getItem(PICKED_KEY)
  } catch {
    return null
  }
}
const remember = (repo: string) => {
  try {
    window.localStorage.setItem(PICKED_KEY, repo)
  } catch {
    /* opens the first repository next time */
  }
}

/** The order to try: the one picked last time first, then the rest. */
export const tryOrder = (repos: string[], last: string | null): string[] => (last && repos.includes(last) ? [last, ...repos.filter((r) => r !== last)] : repos)

/** Mints for the first repository the Hub allows, dropping the ones it refuses. */
export async function firstAllowed(session: BrowserSession, order: string[], signal?: AbortSignal): Promise<{ read: WebReadToken | null; refused: string[]; refusal: Refusal | null; ended: boolean }> {
  const refused: string[] = []
  let refusal: Refusal | null = null
  for (const repo of order) {
    try {
      return { read: await mintWebReadToken(session, repo, signal), refused, refusal, ended: false }
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) return { read: null, refused, refusal, ended: true }
      if (!(e instanceof ApiError) || e.status >= 500) throw e
      refused.push(repo)
      refusal = e.refusal ?? refusal
    }
  }
  return { read: null, refused, refusal, ended: false }
}

export function useWebRead(enabled: boolean) {
  const [state, setState] = useState<WebRead>({ kind: 'checking' })
  const [attempt, setAttempt] = useState(0)
  const flight = useRef<Promise<void> | null>(null)
  const current = useRef(state)
  useEffect(() => {
    current.current = state
  })

  useEffect(() => {
    if (!enabled) return
    // A cancelled check (React runs effects twice in development) must not ask the Hub for a token.
    const ctl = new AbortController()
    const gone = () => ctl.signal.aborted
    void (async () => {
      // A pasted token may have been forgotten since the last check: start from the session again.
      setState({ kind: 'checking' })
      try {
        const session = await fetchBrowserSession(ctl.signal)
        if (gone()) return
        if (!session) return setState({ kind: 'none', refusal: null, empty: false })
        const repos = await sessionRepositories(session, ctl.signal)
        if (gone()) return
        const { read, refused, refusal, ended } = await firstAllowed(session, tryOrder(repos, picked()), ctl.signal)
        if (gone()) return
        if (ended) return setState({ kind: 'none', refusal: null, empty: false })
        if (!read) return setState({ kind: 'none', refusal, empty: repos.length === 0 })
        remember(read.repository)
        setState({ kind: 'ready', session, repos: repos.filter((r) => !refused.includes(r)), read, refusal: null, restarts: 0 })
      } catch {
        if (!gone()) setState({ kind: 'none', refusal: null, empty: false })
      }
    })()
    return () => ctl.abort()
  }, [enabled, attempt])

  /** Asks again for `repo` (the current one when renewing); one ask at a time. */
  const ask = useCallback((repo?: string, after401 = false) => {
    if (flight.current) return flight.current
    const s = current.current
    if (s.kind !== 'ready') return Promise.resolve()
    const target = repo ?? s.read.repository
    flight.current = mintWebReadToken(s.session, target)
      .then(
        (read) => {
          remember(read.repository)
          setState((now) => (now.kind === 'ready' ? { ...now, read, refusal: null, restarts: now.restarts + (after401 ? 1 : 0) } : now))
        },
        (e: unknown) => {
          if (e instanceof ApiError && e.status === 401) return setState({ kind: 'none', refusal: null, empty: false })
          if (e instanceof ApiError && e.status < 500 && repo) {
            // Picking a repository the Hub refuses: say why, drop it, stay on the current one.
            return setState((now) => (now.kind === 'ready' ? { ...now, repos: now.repos.filter((r) => r !== repo), refusal: e.refusal } : now))
          }
          // Renewing failed: start over from the session.
          setState({ kind: 'checking' })
          setAttempt((n) => n + 1)
        },
      )
      .finally(() => {
        flight.current = null
      })
    return flight.current
  }, [])

  // Renew two minutes before the token expires.
  const expiresAt = state.kind === 'ready' ? state.read.expiresAt : null
  useEffect(() => {
    if (expiresAt === null) return
    const ms = Math.max(0, (expiresAt - RENEW_BEFORE_SECONDS) * 1000 - Date.now())
    const timer = window.setTimeout(() => void ask(), ms)
    return () => window.clearTimeout(timer)
  }, [expiresAt, ask])

  const signOut = useCallback(async () => {
    const s = current.current
    try {
      if (s.kind === 'ready') await signOutBrowser(s.session)
      setState({ kind: 'none', refusal: null, empty: false })
    } catch {
      // Say so rather than look signed out: a reload would open the Hub again.
      setState({ kind: 'none', refusal: { code: 'sign_out_failed', message: 'This browser is still signed in; the Hub didn’t confirm the sign-out. Reload the page and try again.', fix: null }, empty: false })
    }
  }, [])

  return {
    state: enabled ? state : ({ kind: 'off' } as const),
    /** After a 401: the page's polling stopped, so a new token remounts it. */
    renew: useCallback(() => void ask(undefined, true), [ask]), pick: useCallback((repo: string) => void ask(repo), [ask]), signOut }
}
