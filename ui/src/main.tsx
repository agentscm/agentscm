import 'bootstrap/dist/css/bootstrap.min.css'
import 'bootstrap-icons/font/bootstrap-icons.min.css'
import './styles.css'
import './density.css'
import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { adoptFragmentToken } from './fragmentToken.ts'
import { adoptFrontDoorLink } from './frontDoor.ts'
import { loadDensity } from './storage.ts'
import App from './App.tsx'
import { ErrorBoundary } from './ErrorBoundary.tsx'

// Before the first render, so the page never paints at the wrong density.
document.documentElement.dataset['density'] = loadDensity()

// A `#token=` or `#pair=` link opened in an already-loaded tab only changes the hash; reload so it is stripped and checked like a fresh load.
window.addEventListener('hashchange', () => {
  const fragment = new URLSearchParams(window.location.hash.slice(1))
  if (fragment.has('token') || fragment.has('pair')) window.location.reload()
})

// A slow hub must not hold the first render: give the link check a few seconds, then show the page anyway.
// If the check then finishes with a stored token, reload so the page picks it up instead of staying on sign-in.
let slow = false
// On this computer's own page, a `#pair=` link connects it; elsewhere `#token=` signs in as before.
const adopting = adoptFrontDoorLink().then((connected) => connected || adoptFragmentToken())
void Promise.race([adopting, new Promise((resolve) => setTimeout(() => { slow = true; resolve(false) }, 6000))]).then(() => {
  createRoot(document.getElementById('root')!).render(
    <StrictMode>
      <ErrorBoundary>
        <App />
      </ErrorBoundary>
    </StrictMode>,
  )
  if (slow) void adopting.then((saved) => saved && window.location.reload())
})
