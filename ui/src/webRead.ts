// Opening the hosted Hub's pages on a web sign-in alone. With no pasted token, the page asks the Hub
// for a 30-minute read-only token for one repository (POST /v1/hub/session/read-token) and keeps it
// in memory only: never in localStorage or sessionStorage, so a reload asks once more. Each ask
// stores a token on the Hub, so the page asks only when it has none, two minutes before it
// expires, or after a 401, and never twice at once.
import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError } from './api.ts'
import { fetchBrowserSession, fetchRepositoryList, mintWebReadToken, sessionRepositories, signOutBrowser, type BrowserSession, type RepositoryList, type WebReadToken } from './devices.ts'
import type { Refusal } from './refusal.ts'
import { fetchOverview } from './standup.ts'

export type WebRead =
  | { kind: 'off' }
  /** `repos` is how many repositories the session can see, once that is known. */
  | { kind: 'checking'; session?: BrowserSession; repos?: number }
  /** A known browser session may lack a usable repository; a failed check leaves identity unknown. */
  | { kind: 'unavailable'; session: BrowserSession | null; reason: 'connection' | 'empty' | 'refused'; refusal: Refusal | null; /** How far a failed check got: the repositories seen so far. */ repos?: number }
  /** No browser session, or a confirmed sign-out. */
  | { kind: 'none'; refusal: Refusal | null; empty: boolean }
  /** `restarts` counts renewals after a 401; each new token restarts Hub polling. `readable` counts the repositories the Hub listed as readable now; `list` is that list, for All repositories to show at once. */
  | { kind: 'ready'; session: BrowserSession; repos: string[]; readable: number; list: { value: RepositoryList; fetchedAt: number } | null; read: WebReadToken; refusal: Refusal | null; restarts: number }

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
const sameSession = (a: BrowserSession, b: BrowserSession): boolean => a.csrf === b.csrf && a.login === b.login && a.provider === b.provider
const sessionKey = (s: BrowserSession): string => `${s.provider}\0${s.login}\0${s.csrf}`

/** Repositories probed for unfinished work when nothing was picked before; bounds the first-sign-in requests. */
export const PROBE_LIMIT = 5

/**
 * Mints for the first repository the Hub allows, dropping the ones it refuses. With `probe`, walks on through
 * the first PROBE_LIMIT allowed ones and opens the first with unfinished work, else the first allowed one.
 */
export async function firstAllowed(session: BrowserSession, order: string[], signal?: AbortSignal, probe = false): Promise<{ read: WebReadToken | null; refused: string[]; refusal: Refusal | null; ended: boolean }> {
  const refused: string[] = []
  let refusal: Refusal | null = null
  let first: WebReadToken | null = null
  let allowed = 0
  for (const repo of order) {
    if (allowed >= PROBE_LIMIT) break
    let read: WebReadToken
    try {
      read = await mintWebReadToken(session, repo, signal)
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) return { read: null, refused, refusal, ended: true }
      if (!(e instanceof ApiError) || e.status >= 500) {
        if (first && !signal?.aborted) break
        throw e
      }
      refused.push(repo)
      refusal = e.refusal ?? refusal
      continue
    }
    if (!probe) return { read, refused, refusal, ended: false }
    allowed += 1
    first ??= read
    try {
      if ((await fetchOverview(read.token, signal ?? new AbortController().signal, { limit: 1 })).current.length > 0) return { read, refused, refusal, ended: false }
    } catch {
      if (signal?.aborted) throw new DOMException('aborted', 'AbortError')
    }
  }
  return { read: first, refused, refusal, ended: false }
}

/** `commit` lets the page wrap the change from the opening screen to the app (a view transition); the default applies it at once. */
export function useWebRead(enabled: boolean, commit: (apply: () => void) => void = (apply) => apply()) {
  const [state, setState] = useState<WebRead>({ kind: 'checking' })
  const [failures, setFailures] = useState<Map<string, BrowserSession>>(() => new Map())
  const [attempt, setAttempt] = useState(0)
  const flight = useRef<Promise<void> | null>(null)
  const signOutFlight = useRef(new Map<string, Promise<void>>())
  const current = useRef(state)
  const commitRef = useRef(commit)
  useEffect(() => { commitRef.current = commit })
  const active = useRef(enabled)
  const generation = useRef(0)
  const bootstrap = useRef<{ generation: number; session: BrowserSession | null; controller: AbortController } | null>(null)
  const confirmedGone = useRef(new Map<string, BrowserSession>())
  const failedSignOut = useRef(new Map<string, BrowserSession>())
  const publishFailures = useCallback(() => setFailures(new Map(failedSignOut.current)), [])
  const clearFailures = useCallback((session?: BrowserSession) => {
    if (session) failedSignOut.current.delete(sessionKey(session))
    else failedSignOut.current.clear()
    publishFailures()
  }, [publishFailures])
  const keepFailuresFor = useCallback((session: BrowserSession) => {
    const key = sessionKey(session)
    for (const old of failedSignOut.current.keys()) if (old !== key) failedSignOut.current.delete(old)
    publishFailures()
  }, [publishFailures])
  useEffect(() => {
    generation.current += 1
    active.current = enabled
    if (!enabled) {
      flight.current = null
      signOutFlight.current.clear()
      bootstrap.current = null
      confirmedGone.current.clear()
      clearFailures()
    }
  }, [enabled, clearFailures])
  useEffect(() => {
    current.current = enabled ? state : { kind: 'checking' }
  }, [enabled, state])

  const endSession = useCallback((session: BrowserSession) => {
    confirmedGone.current.set(sessionKey(session), session)
    generation.current += 1
    flight.current = null
    bootstrap.current?.controller.abort()
    bootstrap.current = null
    const none = { kind: 'none', refusal: null, empty: false } as const
    current.current = none
    setState(none)
    clearFailures()
  }, [clearFailures])

  // A DELETE may finish during a session recheck. Defer it until that session is known;
  // a former browser session must never sign out a newer one.
  const confirmSignedOut = useCallback((session: BrowserSession) => {
    if (!active.current) return
    const now = current.current
    if (now.kind === 'ready') {
      if (sameSession(now.session, session)) endSession(session)
      return
    }
    if (now.kind === 'unavailable' && now.session) {
      if (sameSession(now.session, session)) endSession(session)
      return
    }
    if (now.kind !== 'checking' && now.kind !== 'unavailable') return
    const checking = now.kind === 'checking' ? bootstrap.current?.session : null
    if (checking && !sameSession(checking, session)) return
    confirmedGone.current.set(sessionKey(session), session)
    clearFailures(session)
    if (checking) endSession(session)
  }, [clearFailures, endSession])

  const reportSignOutFailure = useCallback((session: BrowserSession) => {
    if (!active.current) return
    const now = current.current
    const known = now.kind === 'ready' || now.kind === 'unavailable' ? now.session : now.kind === 'checking' ? bootstrap.current?.session : null
    if (known && !sameSession(known, session)) return
    if (now.kind === 'none' || now.kind === 'off' || confirmedGone.current.has(sessionKey(session))) return
    failedSignOut.current.set(sessionKey(session), session)
    publishFailures()
  }, [publishFailures])

  useEffect(() => {
    if (!enabled) {
      // A pasted token can replace a web read token. Never reuse that read token if it is forgotten.
      let cancelled = false
      queueMicrotask(() => {
        if (!cancelled) {
          setState({ kind: 'checking' })
          clearFailures()
        }
      })
      return () => { cancelled = true }
    }
    // A recheck scheduled before a confirmed sign-out must not start a fresh mint.
    if (confirmedGone.current.size > 0 && current.current.kind === 'none') return
    // A cancelled check (React runs effects twice in development) must not ask the Hub for a token.
    const ctl = new AbortController()
    const run = { generation: generation.current, session: null as BrowserSession | null, repos: undefined as number | undefined, controller: ctl }
    bootstrap.current = run
    const valid = () => !ctl.signal.aborted && active.current && generation.current === run.generation && bootstrap.current === run
    const allowed = (session: BrowserSession) => {
      if (!valid()) return false
      if (confirmedGone.current.has(sessionKey(session))) {
        endSession(session)
        return false
      }
      return true
    }
    void (async () => {
      // A pasted token may have been forgotten since the last check: start from the session again.
      const checking = { kind: 'checking' } as const
      current.current = checking
      setState(checking)
      try {
        const session = await fetchBrowserSession(ctl.signal)
        if (!valid()) return
        if (!session) {
          const confirmed = confirmedGone.current.values().next().value as BrowserSession | undefined
          if (confirmed) return endSession(confirmed)
          clearFailures()
          const none = { kind: 'none', refusal: null, empty: false } as const
          current.current = none
          return setState(none)
        }
        run.session = session
        const identified = { kind: 'checking', session } as const
        current.current = identified
        setState(identified)
        keepFailuresFor(session)
        if (!allowed(session)) return
        const [known, list] = await Promise.all([sessionRepositories(session, ctl.signal), fetchRepositoryList(session, ctl.signal).catch(() => null)])
        const listedAt = Date.now()
        if (!allowed(session)) return
        // What the Hub says this sign-in can read now comes first; what it only knew of may still be refused.
        const readable = list?.rows.map((r) => r.repository) ?? []
        const repos = [...new Set([...readable, ...known])]
        run.repos = repos.length
        const counted = { kind: 'checking', session, repos: repos.length } as const
        current.current = counted
        setState(counted)
        const last = picked()
        // First sign-in on this browser: prefer a repository with unfinished work over an empty one.
        const { read, refused, refusal, ended } = await firstAllowed(session, tryOrder(repos, last), ctl.signal, !(last && repos.includes(last)))
        if (!allowed(session)) return
        if (ended) return endSession(session)
        if (!read) {
          const unavailable = { kind: 'unavailable', session, reason: repos.length === 0 ? 'empty' : 'refused', refusal } as const
          current.current = unavailable
          return setState(unavailable)
        }
        remember(read.repository)
        const ready = { kind: 'ready', session, repos: repos.filter((r) => !refused.includes(r)), readable: readable.length, list: list && { value: list, fetchedAt: listedAt }, read, refusal, restarts: 0 } as const
        commitRef.current(() => {
          if (!valid()) return
          current.current = ready
          bootstrap.current = null
          setState(ready)
        })
      } catch (e) {
        if (!valid()) return
        if (run.session && confirmedGone.current.has(sessionKey(run.session))) return endSession(run.session)
        // Keep the Hub's own sentence when it gave one (e.g. "GitHub didn't answer the Hub's access check").
        const unavailable = { kind: 'unavailable', session: run.session, reason: 'connection', refusal: e instanceof ApiError ? e.refusal : null, repos: run.repos } as const
        current.current = unavailable
        setState(unavailable)
      }
    })()
    return () => { ctl.abort(); if (bootstrap.current === run) bootstrap.current = null }
  }, [enabled, attempt, endSession, clearFailures, keepFailuresFor])

  /** Asks again for `repo` (the current one when renewing); one ask at a time. */
  const ask = useCallback((repo?: string, after401 = false) => {
    if (flight.current) return flight.current
    const s = current.current
    if (s.kind !== 'ready') return Promise.resolve()
    const target = repo ?? s.read.repository
    const startedIn = generation.current
    const request = mintWebReadToken(s.session, target)
      .then(
        (read) => {
          if (!active.current || generation.current !== startedIn) return
          remember(read.repository)
          setState((now) => (now.kind === 'ready' ? { ...now, read, refusal: null, restarts: now.restarts + (after401 ? 1 : 0) } : now))
        },
        (e: unknown) => {
          if (!active.current || generation.current !== startedIn) return
          if (e instanceof ApiError && e.status === 401) return confirmSignedOut(s.session)
          if (e instanceof ApiError && e.status < 500 && repo) {
            // Picking a repository the Hub refuses: say why, drop it, stay on the current one.
            return setState((now) => (now.kind === 'ready' ? { ...now, repos: now.repos.filter((r) => r !== repo), refusal: e.refusal } : now))
          }
          // Renewing failed: start over from the session.
          const checking = { kind: 'checking' } as const
          current.current = checking
          setState(checking)
          setAttempt((n) => n + 1)
        },
      )
      .finally(() => {
        if (flight.current === request) flight.current = null
      })
    flight.current = request
    return request
  }, [confirmSignedOut])

  // Renew two minutes before the token expires.
  const expiresAt = enabled && state.kind === 'ready' ? state.read.expiresAt : null
  useEffect(() => {
    if (expiresAt === null) return
    const ms = Math.max(0, (expiresAt - RENEW_BEFORE_SECONDS) * 1000 - Date.now())
    const timer = window.setTimeout(() => void ask(), ms)
    return () => window.clearTimeout(timer)
  }, [expiresAt, ask])

  // A tab left signed out notices a sign-in made in another tab when it comes back into view.
  const signedOut = enabled && state.kind === 'none'
  useEffect(() => {
    if (!signedOut) return
    let ctl: AbortController | null = null
    const recheck = () => {
      if (document.hidden || ctl) return
      const mine = (ctl = new AbortController())
      fetchBrowserSession(mine.signal)
        .then((session) => {
          if (!session || mine.signal.aborted || confirmedGone.current.has(sessionKey(session)) || current.current.kind !== 'none') return
          // A new session: the ones signed out before can no longer come back under it.
          confirmedGone.current.clear()
          setAttempt((n) => n + 1)
        }, () => {})
        .finally(() => { if (ctl === mine) ctl = null })
    }
    window.addEventListener('focus', recheck)
    document.addEventListener('visibilitychange', recheck)
    return () => {
      ctl?.abort()
      window.removeEventListener('focus', recheck)
      document.removeEventListener('visibilitychange', recheck)
    }
  }, [signedOut])

  const signOut = useCallback(() => {
    const s = current.current
    const session = s.kind === 'ready' || s.kind === 'unavailable' ? s.session : s.kind === 'checking' ? s.session : null
    if (!session) return Promise.resolve()
    const key = sessionKey(session)
    const previous = signOutFlight.current.get(key)
    if (previous) return previous
    clearFailures(session)
    const request = (async () => {
      try {
        await signOutBrowser(session)
        confirmSignedOut(session)
      } catch {
        reportSignOutFailure(session)
      }
    })().finally(() => {
      if (signOutFlight.current.get(key) === request) signOutFlight.current.delete(key)
    })
    signOutFlight.current.set(key, request)
    return request
  }, [clearFailures, confirmSignedOut, reportSignOutFailure])

  const known = state.kind === 'ready' || state.kind === 'unavailable' ? state.session : state.kind === 'checking' ? state.session : null
  const signOutError = state.kind !== 'none' && state.kind !== 'off' && failures.size > 0 && (!known || failures.has(sessionKey(known)))
    ? 'Hub sign-out failed. This browser may still be signed in to Hub pages. Check the connection and try again.'
    : null

  return {
    state: enabled ? state : ({ kind: 'off' } as const),
    signOutError: enabled ? signOutError : null,
    /** After a 401: ask for a new token so Hub polling restarts. */
    renew: useCallback(() => void ask(undefined, true), [ask]), pick: useCallback((repo: string) => void ask(repo), [ask]), retry: useCallback(() => setAttempt((n) => n + 1), []), signOut, confirmSignedOut, reportSignOutFailure }
}
