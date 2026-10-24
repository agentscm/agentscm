const FETCH_TIMEOUT_MS = 15_000

/** The caller's signal plus a 15 s limit, so a stalled hub fails like any other error instead of hanging. */
export function withTimeout(signal?: AbortSignal): AbortSignal {
  const limit = AbortSignal.timeout(FETCH_TIMEOUT_MS)
  if (!signal) return limit
  if (typeof AbortSignal.any === 'function') return AbortSignal.any([signal, limit])
  // Older browsers (Safari before 17.4): relay both by hand.
  const both = new AbortController()
  for (const s of [signal, limit]) {
    if (s.aborted) both.abort(s.reason)
    else s.addEventListener('abort', () => both.abort(s.reason), { once: true })
  }
  return both.signal
}
