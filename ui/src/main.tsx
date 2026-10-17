import 'antd/dist/reset.css'
import './styles.css'
import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import App from './App.tsx'
import { ApiError, fetchHealth, fetchWhoami } from './api.ts'
import { fragmentTokenAllowed, loadToken, saveToken } from './storage.ts'

/** `#token=…` (from `agentscm demo`), removed from the address bar before anything else runs. */
function takeFragmentToken(): string | null {
  const token = new URLSearchParams(window.location.hash.slice(1)).get('token')
  if (window.location.hash) history.replaceState(null, '', window.location.pathname + window.location.search)
  return token
}

// Stored like a pasted token, only once the hub confirms it is read-only, and never over a
// stored token unless the hub is the demo's.
async function adoptFragmentToken(): Promise<void> {
  const token = takeFragmentToken()
  if (!token) return
  const signal = new AbortController().signal
  // A few tries: `agentscm demo` may open the page before its hub answers.
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      const health = await fetchHealth(signal)
      // Unavailable says nothing about demo mode: try again rather than give up.
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

void adoptFragmentToken().then(() =>
  createRoot(document.getElementById('root')!).render(
    <StrictMode>
      <App />
    </StrictMode>,
  ),
)
