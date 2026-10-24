import { useEffect, useRef, useState } from 'react'

export type Poll<T> =
  | { kind: 'loading' }
  | { kind: 'error'; message: string; status?: number; retry: () => void }
  | { kind: 'ready'; value: T; fetchedAt: number; refreshError: string | null; retry: () => void }

const message = (e: unknown) => (e instanceof Error ? e.message : 'Unknown error')

/**
 * Loads `load` now and every `everyMs` (0 = once) while the tab is visible. A failed refresh keeps
 * and labels the last good value instead of replacing it. `key` restarts from scratch (loading);
 * `refresh` re-fetches now but keeps the shown value (stale-while-revalidate).
 */
export function usePoll<T>(load: (signal: AbortSignal) => Promise<T>, everyMs: number, key: string, refresh = 0): Poll<T> {
  // The state is tagged with the key it was loaded for; a result for another key reads as loading.
  const [tagged, setTagged] = useState<{ key: string; poll: Poll<T> }>({ key, poll: { kind: 'loading' } })
  const [attempt, setAttempt] = useState(0)
  const latest = useRef(load)
  useEffect(() => {
    latest.current = load
  })
  useEffect(() => {
    let disposed = false
    let running = false
    let timer: number | undefined
    let controller: AbortController | undefined
    const setState = (next: Poll<T> | ((s: Poll<T>) => Poll<T>)) => setTagged((t) => ({ key, poll: typeof next === 'function' ? next(t.key === key ? t.poll : { kind: 'loading' }) : next }))
    const retry = () => {
      setState({ kind: 'loading' })
      setAttempt((n) => n + 1)
    }
    const schedule = () => {
      if (everyMs > 0 && !disposed && !document.hidden) timer = window.setTimeout(() => void run(), everyMs)
    }
    const run = async () => {
      if (disposed || running || document.hidden) return
      running = true
      controller = new AbortController()
      try {
        const value = await latest.current(controller.signal)
        if (disposed) return
        setState({ kind: 'ready', value, fetchedAt: Date.now(), refreshError: null, retry })
      } catch (e) {
        if (disposed || controller.signal.aborted) return
        const status = typeof (e as { status?: unknown }).status === 'number' ? (e as { status: number }).status : undefined
        // A 401 on a refresh means the session is gone: show that, not a stale value.
        setState((s) => (s.kind === 'ready' && status !== 401 ? { ...s, refreshError: message(e) } : { kind: 'error', message: message(e), ...(status !== undefined ? { status } : {}), retry }))
      } finally {
        running = false
        schedule()
      }
    }
    const onVisibility = () => {
      window.clearTimeout(timer)
      if (!document.hidden) void run()
    }
    document.addEventListener('visibilitychange', onVisibility)
    void run()
    return () => {
      disposed = true
      window.clearTimeout(timer)
      controller?.abort()
      document.removeEventListener('visibilitychange', onVisibility)
    }
  }, [everyMs, key, attempt, refresh])
  return tagged.key === key ? tagged.poll : { kind: 'loading' }
}
