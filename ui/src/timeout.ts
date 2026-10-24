const FETCH_TIMEOUT_MS = 15_000

/** The caller's signal plus a 15 s limit, so a stalled hub fails like any other error instead of hanging. */
export function withTimeout(signal?: AbortSignal): AbortSignal {
  const limit = AbortSignal.timeout(FETCH_TIMEOUT_MS)
  if (!signal) return limit
  if (typeof AbortSignal.any === 'function') return AbortSignal.any([signal, limit])
  // Older browsers (Safari before 17.4): relay both by hand, dropping the listeners once either fires
  // (the limit always does within 15 s), so a long-lived caller signal does not collect one per fetch.
  const both = new AbortController()
  const relay = (from: AbortSignal) => () => {
    for (const [s, f] of relays) s.removeEventListener('abort', f)
    both.abort(from.reason)
  }
  const relays = [signal, limit].map((s) => [s, relay(s)] as const)
  for (const [s, f] of relays) {
    if (s.aborted) {
      f()
      break
    }
    s.addEventListener('abort', f, { once: true })
  }
  return both.signal
}
