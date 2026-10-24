import { ApiError, fetchHealth, fetchWhoami } from './api.ts'
import { fragmentTokenAllowed, loadToken, saveToken } from './storage.ts'

/** `#token=…` (from `agentscm demo`): removed from the address bar before anything else runs. */
function takeFragmentToken(): string | null {
  const fragment = new URLSearchParams(window.location.hash.slice(1))
  const token = fragment.get('token')
  if (fragment.has('token')) history.replaceState(null, '', window.location.pathname + window.location.search)
  return token
}

// Same rules as the current status page: stored only once the hub confirms it is read-only,
// and never over a stored token unless the hub is the demo's.
export async function adoptFragmentToken(): Promise<boolean> {
  const token = takeFragmentToken()
  if (!token) return false
  const signal = new AbortController().signal
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      const health = await fetchHealth(signal)
      if (health.status !== 'ok') throw new ApiError(503, 'HTTP 503')
      if (!fragmentTokenAllowed(loadToken(), health.demo === true)) return false
      if (!(await fetchWhoami(token, signal)).token.read_only) return false
      saveToken(token, false)
      return true
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) return false
      await new Promise((resolve) => setTimeout(resolve, 500))
    }
  }
  return false
}
