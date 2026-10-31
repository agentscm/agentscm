import { useState } from 'react'
import { dockTip, isStandalone, useInstall } from './install.ts'
import { loadDockTipDismissed, loadInstallDismissed, saveDockTipDismissed, saveInstallDismissed } from './storage.ts'
import { useTour } from './tour/Tour.tsx'
import { Icon } from './ui.tsx'

/**
 * One quiet line at the top of the page, once: in a browser that can install the Hub as an app,
 * the offer; in the installed app, how to keep it in the Dock or on the taskbar.
 */
export function InstallOffer() {
  const tour = useTour()
  // The welcome tour plays first; this waits until it is done.
  const touring = !!tour && tour.remember && !tour.prefs.welcomeSeen && !tour.prefs.welcomeOff
  if (touring) return null
  return isStandalone() ? <DockTip /> : <Offer />
}

function Offer() {
  const { kind, hint, install } = useInstall()
  const [gone, setGone] = useState(loadInstallDismissed)
  if (gone || kind === 'none') return null
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

function DockTip() {
  const [gone, setGone] = useState(loadDockTipDismissed)
  const tip = dockTip(navigator.userAgent, navigator.maxTouchPoints)
  if (gone || !tip) return null
  return (
    <div className="install-offer" role="region" aria-label="Keep the app handy">
      <Icon name="pin-angle" />
      <span className="install-text">{tip}</span>
      <button type="button" className="install-x" aria-label="Dismiss" onClick={() => { saveDockTipDismissed(); setGone(true) }}>
        <Icon name="x-lg" />
      </button>
    </div>
  )
}
