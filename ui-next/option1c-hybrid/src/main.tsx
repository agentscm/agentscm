import 'bootstrap/dist/css/bootstrap.min.css'
import 'bootstrap-icons/font/bootstrap-icons.min.css'
import './styles.css'
import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { ApiError, fetchHealth, fetchWhoami } from '../../../ui/src/api.ts'
import { fragmentTokenAllowed, loadToken, saveToken } from '../../../ui/src/storage.ts'
import App from './App.tsx'
import { ErrorBoundary } from './ErrorBoundary.tsx'

/** `#token=…` (from `agentscm demo`): removed from the address bar before anything else runs. */
function takeFragmentToken(): string | null {
  const fragment = new URLSearchParams(window.location.hash.slice(1))
  const token = fragment.get('token')
  if (fragment.has('token')) history.replaceState(null, '', window.location.pathname + window.location.search)
  return token
}

// Same rules as the current status page: stored only once the hub confirms it is read-only,
// and never over a stored token unless the hub is the demo's.
async function adoptFragmentToken(): Promise<void> {
  const token = takeFragmentToken()
  if (!token) return
  const signal = new AbortController().signal
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      const health = await fetchHealth(signal)
      if (health.status !== 'ok') throw new ApiError(503, 'HTTP 503')
      if (!fragmentTokenAllowed(loadToken(), health.demo === true)) return
      if ((await fetchWhoami(token, signal)).token.read_only) saveToken(token, false)
      return
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) return
      await new Promise((resolve) => setTimeout(resolve, 500))
    }
  }
}

// A `#token=` link opened in an already-loaded tab only changes the hash; reload so it is stripped and checked like a fresh load.
window.addEventListener('hashchange', () => {
  if (new URLSearchParams(window.location.hash.slice(1)).has('token')) window.location.reload()
})

void adoptFragmentToken().then(() =>
  createRoot(document.getElementById('root')!).render(
    <StrictMode>
      <ErrorBoundary>
        <App />
      </ErrorBoundary>
    </StrictMode>,
  ),
)
