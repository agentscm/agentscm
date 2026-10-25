import { useState } from 'react'
import { fetchSharing, isSharing, setSharing, type SharingCheckout } from './captureSharing.ts'
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
  const state = usePoll((signal) => fetchSharing(token, signal), 0, `${token}|${nonce}`)
  const [confirming, setConfirming] = useState<string | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const key = (c: SharingCheckout) => `${c.repository}@${c.hub}`
  const change = async (checkout: SharingCheckout, on: boolean) => {
    setBusy(key(checkout))
    setError(null)
    try {
      await setSharing(token, checkout, on)
      setConfirming(null)
    } catch (e) {
      setError(e instanceof Error ? e.message : 'The change failed')
    } finally {
      setBusy(null)
      setNonce((n) => n + 1)
    }
  }
  return (
    <Box tour="settings-sharing" icon="chat-square-text" title="Conversation sharing" explain="Off by default, one checkout at a time. Turning it on sets up capture for that checkout if it was never set up.">
      {state.kind === 'loading' && <Skeleton rows={2} />}
      {state.kind === 'error' && <p className="small">Could not read the sharing settings on this computer.</p>}
      {error && <div className="alert alert-danger small" role="alert">{error}</div>}
      {state.kind === 'ready' && state.value.length === 0 && <p className="muted small">No checkout is connected on this computer.</p>}
      {state.kind === 'ready' &&
        state.value.map((checkout) => {
          const on = isSharing(checkout)
          const id = `share-${key(checkout).replace(/[^A-Za-z0-9_-]/g, '_')}`
          return (
            <div key={key(checkout)} className="settings-row">
              <div>
                <label className="settings-label" htmlFor={id}>Share agent conversations with Hub</label>
                <div className="small"><code>{checkout.repository}</code> <span className="muted">· {checkout.folder} · {checkout.hub}</span></div>
                <div className="mt-1">
                  {on ? <Pill tone="brand" icon="cloud-upload">Shared with Hub</Pill> : <Pill tone="muted">Metadata only</Pill>}
                </div>
                {confirming === key(checkout) && (
                  <div className="mt-2" role="group" aria-label={`Confirm sharing for ${checkout.repository}`}>
                    <div className="settings-label">Share conversations from this checkout?</div>
                    <SharingConsequences hub={checkout.hub} />
                    <button type="button" className="btn btn-sm btn-primary me-2" disabled={busy !== null} onClick={() => void change(checkout, true)}>Share conversations</button>
                    <button type="button" className="btn btn-sm btn-outline-secondary" disabled={busy !== null} onClick={() => setConfirming(null)}>Cancel</button>
                  </div>
                )}
              </div>
              <div className="form-check form-switch m-0">
                <input
                  id={id}
                  className="form-check-input"
                  type="checkbox"
                  role="switch"
                  checked={on}
                  disabled={busy !== null || !checkout.readable}
                  onChange={(e) => {
                    setError(null)
                    if (e.target.checked) setConfirming(key(checkout))
                    else void change(checkout, false)
                  }}
                />
                <Icon name={busy === key(checkout) ? 'hourglass-split' : 'chat-square-text'} />
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
