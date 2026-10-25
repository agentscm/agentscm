import { useEffect, useRef, useState } from 'react'
import { fetchSharing, isSharing, setSharing, SharingError, sharingState, type SharingCheckout } from './captureSharing.ts'
import { Box, Icon, Pill, Skeleton } from './ui.tsx'
import { usePoll } from './usePoll.ts'

/** What turning sharing on does, in the words the menu bar app uses. */
export function SharingConsequences({ hub }: { hub: string }) {
  return (
    <ul className="small">
      <li>Prompts, replies and tool output are redacted for secrets on this computer first. Redaction is best effort, so avoid putting secrets in conversations.</li>
      <li>The redacted conversation is uploaded to <code>{hub}</code>.</li>
      <li>It is kept 30 days by default.</li>
      <li>You and Hub admins can read it.</li>
      <li>Turning sharing off stops future uploads. Copies already uploaded stay until they expire or you delete them.</li>
    </ul>
  )
}

/** Conversation sharing for this computer's checkouts. Only the local service can change it. */
export function CaptureSharingBox({ token }: { token: string }) {
  const [nonce, setNonce] = useState(0)
  // A re-read after a change keeps the list mounted, so the keyboard position survives it.
  const state = usePoll((signal) => fetchSharing(token, signal), 0, token, nonce)
  const [confirming, setConfirming] = useState<string | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [announcement, setAnnouncement] = useState('')
  const confirmButton = useRef<HTMLButtonElement>(null)
  const restoreFocus = useRef<string | null>(null)
  const switchId = (c: SharingCheckout) => `share-${c.id}`
  const confirmId = (c: SharingCheckout) => `share-confirm-${c.id}`
  const aboutId = (c: SharingCheckout) => `share-about-${c.id}`
  useEffect(() => {
    if (confirming) confirmButton.current?.focus()
  }, [confirming])
  useEffect(() => {
    if (busy === null && restoreFocus.current) {
      document.getElementById(restoreFocus.current)?.focus()
      restoreFocus.current = null
    }
  }, [busy])
  const change = async (checkout: SharingCheckout, on: boolean) => {
    setBusy(checkout.id)
    setError(null)
    setAnnouncement(`Updating conversation sharing for ${checkout.repository}…`)
    try {
      await setSharing(token, checkout, on)
      setConfirming(null)
      setAnnouncement(`Sharing ${on ? 'turned on' : 'turned off'} for ${checkout.repository} in ${checkout.folder}`)
    } catch (e) {
      // The list is re-read below either way; a checkout that vanished from it needs a fresh look.
      setError(e instanceof SharingError && e.code === 'unknown_checkout' ? 'The checkout list changed; review and try again' : e instanceof Error ? e.message : 'The change failed')
      setAnnouncement('')
    } finally {
      restoreFocus.current = switchId(checkout)
      setBusy(null)
      setNonce((n) => n + 1)
    }
  }
  const cancel = (checkout: SharingCheckout) => {
    setConfirming(null)
    document.getElementById(switchId(checkout))?.focus()
  }
  return (
    <Box tour="settings-sharing" icon="chat-square-text" title="Conversation sharing" explain="Off by default, one checkout at a time. Turning it on sets up capture for that checkout if it was never set up.">
      <div className="visually-hidden" role="status">{announcement}</div>
      {state.kind === 'loading' && <Skeleton rows={2} />}
      {state.kind === 'error' && <p className="small">Could not read the sharing settings on this computer.</p>}
      {error && <div className="alert alert-danger small" role="alert">{error}</div>}
      {state.kind === 'ready' && state.value.length === 0 && <p className="muted small">No checkout is connected on this computer.</p>}
      {state.kind === 'ready' &&
        state.value.map((checkout) => {
          const on = isSharing(checkout)
          const state = sharingState(checkout)
          const open = confirming === checkout.id
          return (
            <div key={checkout.id} className="settings-row">
              <div>
                <label className="settings-label" htmlFor={switchId(checkout)}>Share agent conversations with Hub</label>
                <div id={aboutId(checkout)} className="small"><code>{checkout.repository}</code> <span className="muted">· {checkout.folder} · {checkout.hub}</span></div>
                <div className="mt-1">
                  {on ? <Pill tone="brand" icon="cloud-upload">{state}</Pill> : <Pill tone="muted">{state}</Pill>}
                </div>
                {checkout.lineage_mismatch && (
                  <p className="small mt-1 mb-0" role="note">
                    Sharing was set for {checkout.saved_hub}; this checkout is now connected to {checkout.hub}. {on ? 'It can only be turned off here.' : 'It cannot be turned on here.'} To start fresh, run <code>agentscm change-capture unconfigure</code> and then <code>agentscm change-capture forget</code> for this checkout.
                  </p>
                )}
                <div id={confirmId(checkout)} hidden={!open}>
                  {open && (
                    <div className="mt-2" role="group" aria-label={`Confirm sharing for ${checkout.repository} in ${checkout.folder}`}>
                      <div className="settings-label">Share conversations from this checkout?</div>
                      <SharingConsequences hub={checkout.hub} />
                      <button ref={confirmButton} type="button" className="btn btn-sm btn-primary me-2" disabled={busy !== null} onClick={() => void change(checkout, true)}>Share conversations</button>
                      <button type="button" className="btn btn-sm btn-outline-secondary" disabled={busy !== null} onClick={() => cancel(checkout)}>Cancel</button>
                    </div>
                  )}
                </div>
              </div>
              <div className="form-check form-switch m-0">
                <input
                  id={switchId(checkout)}
                  className="form-check-input"
                  type="checkbox"
                  role="switch"
                  checked={on}
                  aria-describedby={aboutId(checkout)}
                  aria-controls={confirmId(checkout)}
                  disabled={busy !== null || !checkout.readable || (checkout.lineage_mismatch && !on)}
                  onChange={(e) => {
                    setError(null)
                    if (e.target.checked) setConfirming(checkout.id)
                    else void change(checkout, false)
                  }}
                />
                <Icon name={busy === checkout.id ? 'hourglass-split' : 'chat-square-text'} />
              </div>
            </div>
          )
        })}
    </Box>
  )
}

/** The setting lives on this computer, so a Hub opened elsewhere only says where to find it. */
export function CaptureSharingElsewhere() {
  return (
    <Box tour="settings-sharing" icon="chat-square-text" title="Conversation sharing" explain="Off by default.">
      <p className="small m-0">Sharing agent conversations with the Hub is a setting of each checkout on your computer. Change it in the AgentSCM menu bar app (Share agent conversations with Hub) or on the Hub page opened from this computer with <code>agentscm open</code>.</p>
    </Box>
  )
}
