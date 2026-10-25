import { useState } from 'react'
import { disconnectComputer, fetchIdentity, fetchRepositories, localSelection, switchRepository, takeConnectOutcome, type ConnectOutcome } from './frontDoor.ts'
import { usePoll } from './usePoll.ts'
import { Icon, Notice } from './ui.tsx'

// Read once per page load, after the first render waited for the link to be redeemed.
let outcome: ConnectOutcome | null | undefined
function connectOutcome(): ConnectOutcome | null {
  if (outcome === undefined) outcome = takeConnectOutcome()
  return outcome
}

const hostOf = (url: string) => {
  try {
    return new URL(url).host
  } catch {
    return url
  }
}

/**
 * On this computer's own Hub page: which computer and repository you are looking at, a switcher for
 * the other connected repositories, and the one thing to do when the computer cannot reach a Hub.
 */
export function ComputerBar({ token, allowDisconnect = false, onDisconnected }: { token: string; allowDisconnect?: boolean; onDisconnected?: () => void }) {
  const identity = usePoll((signal) => fetchIdentity(token, signal), 60_000, token)
  const repositories = usePoll((signal) => fetchRepositories(token, signal), 0, token)
  const [busy, setBusy] = useState(false)
  if (identity.kind !== 'ready') return null
  const { computer, repository, hub } = identity.value
  const others = repositories.kind === 'ready' ? repositories.value : []
  // The same repository can be connected to two Hubs; then the Hub tells them apart.
  const repeated = (r: { repository: string }) => others.filter((o) => o.repository.toLowerCase() === r.repository.toLowerCase()).length > 1
  const selection = localSelection(token) ?? repository
  const current = others.find((r) => `${r.repository}@${r.hub}` === selection) ?? others.find((r) => r.repository.toLowerCase() === repository?.toLowerCase() && r.hub === hub?.url)
  const connected = connectOutcome()
  return (
    <>
      {connected === 'used' && (
        <Notice tone="warn" icon="link-45deg" title="That link was already used">
          Each link from <code>agentscm open</code> or Open Hub works once. You are still connected with the link you used before.
        </Notice>
      )}
      <div className="computer-bar" role="status">
        <Icon name="laptop" />
        <span>
          Your computer: <strong>{computer}</strong>
        </span>
        {others.length > 1 || (repository === null && others.length > 0) ? (
          <label className="computer-bar-repo">
            <span className="visually-hidden">Repository</span>
            <select
              className="form-select form-select-sm"
              value={current ? `${current.repository}@${current.hub}` : ''}
              aria-describedby={repository === null ? 'computer-bar-gone' : undefined}
              onChange={(e) => {
                if (switchRepository(token, e.target.value)) window.location.reload()
              }}
            >
              {!current && <option value="">Choose a repository</option>}
              {others.map((r) => (
                <option key={`${r.repository}@${r.hub}`} value={`${r.repository}@${r.hub}`}>
                  {r.repository}
                  {repeated(r) ? ` · ${hostOf(r.hub)}` : ''}
                  {r.folder && r.folder !== r.repository.split('/')[1] ? ` (${r.folder})` : ''}
                </option>
              ))}
            </select>
          </label>
        ) : (
          repository && <code>{repository}</code>
        )}
        {allowDisconnect && (
          <button
            type="button"
            className="btn btn-sm btn-outline-secondary"
            disabled={busy}
            onClick={() => {
              setBusy(true)
              void disconnectComputer().then(() => onDisconnected?.())
            }}
          >
            Disconnect this browser
          </button>
        )}
      </div>
      {repository === null && (
        <Notice tone="warn" icon="folder-x" title="That repository is no longer connected here">
          <span id="computer-bar-gone">{others.length > 0 ? 'Choose another repository above, or run ' : 'Run '}<code>agentscm open</code> in its checkout. This browser stays connected to your computer.</span>
        </Notice>
      )}
      {hub && !hub.signed_in && (
        <Notice tone="warn" icon="box-arrow-in-right" title="This computer isn’t signed in to its Hub">
          Your files and saved conversations still work. To see team work here too, run <code>agentscm hub login --hub {hub.url}</code> on this computer, then reload.
        </Notice>
      )}
    </>
  )
}
