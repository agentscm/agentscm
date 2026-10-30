import { ApiError, fetchHealth, fetchWhoami } from './api.ts'
import { fragmentTokenAllowed, loadToken, saveToken } from './storage.ts'

/** `#token=…` (from `agentscm demo`): removed from the address bar before anything else runs. */
function takeFragmentToken(): string | null {
  const fragment = new URLSearchParams(window.location.hash.slice(1))
  const token = fragment.get('token')
  if (fragment.has('token')) history.replaceState(null, '', window.location.pathname + window.location.search)
  return token
}

const refusal = (e: unknown): boolean => e instanceof ApiError && (e.status === 401 || e.status === 403)

/** Shown on the front door when the hub turned a `#token=` link down. */
export const LINK_REFUSED = 'That read-only link isn’t valid any more. Ask for a new one.'
let linkRefused = false
/** Whether the last `adoptFragmentToken` ended because the hub refused the link's token (401/403). */
export const fragmentLinkRefused = (): boolean => linkRefused

// Stored only once the hub confirms the token is read-only. A working saved token is never swapped
// out (outside the demo hub); one the hub no longer accepts is replaced by the link's token.
export async function adoptFragmentToken(): Promise<boolean> {
  linkRefused = false
  const token = takeFragmentToken()
  if (!token) return false
  const signal = new AbortController().signal
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      const health = await fetchHealth(signal)
      if (health.status !== 'ok') throw new ApiError(503, 'HTTP 503')
      const stored = loadToken()
      if (!fragmentTokenAllowed(stored, health.demo === true) && !(await refusedStored(stored!, signal))) return false
      if (!(await fetchWhoami(token, signal)).token.read_only) return false
      saveToken(token, false)
      return true
    } catch (e) {
      if (refusal(e)) {
        linkRefused = true
        return false
      }
      await new Promise((resolve) => setTimeout(resolve, 500))
    }
  }
  return false
}

/** True when the hub no longer accepts the saved token; other failures throw so the caller retries. */
async function refusedStored(stored: string, signal: AbortSignal): Promise<boolean> {
  try {
    await fetchWhoami(stored, signal)
    return false
  } catch (e) {
    if (refusal(e)) return true
    throw e
  }
}
