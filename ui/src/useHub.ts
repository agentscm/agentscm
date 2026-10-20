import { useEffect, useRef, useState, useSyncExternalStore } from 'react'
import { AGENT_TOKEN_MESSAGE, ApiError, fetchActivity, fetchHealth, fetchWhoami, fetchWork } from './api.ts'
import type { Activity, Health, Whoami, Work } from './types.ts'

const MAX_BACKOFF_MS = 60_000

/**
 * Sequential polling loop: one request in flight at a time, paused while the tab
 * is hidden, refreshed on becoming visible, exponential backoff after errors.
 */
export function useLoop(tick: (signal: AbortSignal) => Promise<void>, baseMs: number, onFailure: (e: unknown) => void) {
  const latest = useRef({ tick, onFailure })
  useEffect(() => {
    latest.current = { tick, onFailure }
  })
  useEffect(() => {
    let timer: number | undefined
    let ctrl: AbortController | undefined
    let fails = 0
    let running = false
    let stopped = false
    const schedule = (ms: number) => {
      window.clearTimeout(timer)
      if (!stopped && !document.hidden) timer = window.setTimeout(run, ms)
    }
    const run = async () => {
      window.clearTimeout(timer)
      if (stopped || running || document.hidden) return
      running = true
      ctrl = new AbortController()
      try {
        await latest.current.tick(ctrl.signal)
        fails = 0
      } catch (e) {
        if (stopped) return
        fails++
        latest.current.onFailure(e)
        if (e instanceof ApiError && e.status === 401) return
      } finally {
        running = false
      }
      schedule(Math.min(MAX_BACKOFF_MS, baseMs * 2 ** fails))
    }
    const onVisibility = () => {
      if (document.hidden) window.clearTimeout(timer)
      else void run()
    }
    document.addEventListener('visibilitychange', onVisibility)
    void run()
    return () => {
      stopped = true
      window.clearTimeout(timer)
      ctrl?.abort()
      document.removeEventListener('visibilitychange', onVisibility)
    }
  }, [baseMs])
}

export interface HubData {
  whoami: Whoami | null
  work: Work | null
  activity: Activity | null
  health: Health | null
  /** Event ids that appeared since the previous poll (drives the entrance highlight). */
  fresh: ReadonlySet<string>
  /** Unix ms of the last successful work+activity poll. */
  lastOk: number | null
  failing: boolean
}

export function useHub(token: string, onUnauthorized: (message?: string) => void): HubData {
  const [data, setData] = useState<HubData>({ whoami: null, work: null, activity: null, health: null, fresh: new Set(), lastOk: null, failing: false })
  const whoamiRef = useRef<Whoami | null>(null)
  const unauthorized = useRef(onUnauthorized)
  useEffect(() => {
    unauthorized.current = onUnauthorized
  })

  useLoop(
    async (signal) => {
      const whoami = whoamiRef.current ?? (await fetchWhoami(token, signal))
      if (!whoami.token.read_only) {
        throw new ApiError(401, AGENT_TOKEN_MESSAGE)
      }
      if (!whoamiRef.current) setData((d) => ({ ...d, whoami }))
      whoamiRef.current = whoami
      const repo = whoami.token.repository
      // If one of the pair fails, cancel the other instead of letting it run on.
      const pair = new AbortController()
      const cancel = () => pair.abort()
      signal.addEventListener('abort', cancel, { once: true })
      let work: Work, activity: Activity
      try {
        ;[work, activity] = await Promise.all([fetchWork(repo, token, pair.signal), fetchActivity(repo, token, pair.signal)])
      } catch (e) {
        pair.abort()
        throw e
      } finally {
        signal.removeEventListener('abort', cancel)
      }
      setData((d) => {
        const seen = new Set(d.activity?.events.map((e) => e.id))
        const fresh = d.activity ? new Set(activity.events.filter((e) => !seen.has(e.id)).map((e) => e.id)) : new Set<string>()
        return { ...d, whoami, work, activity, fresh, lastOk: Date.now(), failing: false }
      })
    },
    5_000,
    (e) => {
      if (e instanceof ApiError && e.status === 401) unauthorized.current(e.message === AGENT_TOKEN_MESSAGE ? e.message : undefined)
      else setData((d) => ({ ...d, failing: true }))
    },
  )

  useLoop(
    async (signal) => {
      const health = await fetchHealth(signal)
      setData((d) => ({ ...d, health }))
    },
    15_000,
    // A hub that does not answer at all is unavailable too, not still healthy.
    () => setData((d) => ({ ...d, health: { status: 'unavailable', ...(d.health?.demo ? { demo: true as const } : {}) } })),
  )

  return data
}

// One shared 1 s clock for every relative timestamp on the page.
const clock = {
  listeners: new Set<() => void>(),
  now: Date.now(),
  timer: undefined as number | undefined,
  subscribe(l: () => void) {
    clock.listeners.add(l)
    if (clock.listeners.size === 1) {
      clock.now = Date.now()
      clock.timer = window.setInterval(() => {
        clock.now = Date.now()
        clock.listeners.forEach((f) => f())
      }, 1000)
    }
    return () => {
      clock.listeners.delete(l)
      if (clock.listeners.size === 0) window.clearInterval(clock.timer)
    }
  },
  snapshot: () => clock.now,
}

export const useNow = (): number => useSyncExternalStore(clock.subscribe, clock.snapshot)
