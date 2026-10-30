import { useState } from 'react'
import { useInstall } from './install.ts'
import { loadInstallDismissed, saveInstallDismissed } from './storage.ts'
import { useTour } from './tour/Tour.tsx'
import { Icon } from './ui.tsx'

/** One quiet line at the top of the page, once, for a browser that can install the Hub as an app. */
export function InstallOffer() {
  const { kind, hint, install } = useInstall()
  const tour = useTour()
  const [gone, setGone] = useState(loadInstallDismissed)
  // The welcome tour plays first; this waits until it is done.
  const touring = !!tour && tour.remember && !tour.prefs.welcomeSeen && !tour.prefs.welcomeOff
  if (gone || touring || kind === 'none') return null
  return (
    <div className="install-offer" role="region" aria-label="Install as an app">
      <Icon name="download" />
      <span className="install-text">
        Get AgentSCM in your Dock: its own window, with a badge for what needs you.
        {hint && <> <span className="muted">{hint}</span></>}
      </span>
      {kind === 'chromium' && <button type="button" className="btn btn-sm btn-primary" onClick={install}>Install</button>}
      <button type="button" className="install-x" aria-label="Dismiss" onClick={() => { saveInstallDismissed(); setGone(true) }}>
        <Icon name="x-lg" />
      </button>
    </div>
  )
}
