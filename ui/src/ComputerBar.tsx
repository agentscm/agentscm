import { createContext, useContext, useMemo, useState, type ReactNode } from 'react'
import { disconnectComputer, fetchIdentity, fetchRepositories, takeConnectOutcome, type ConnectedRepository, type ConnectOutcome, type Identity } from './frontDoor.ts'
import { usePoll, type Poll } from './usePoll.ts'
import { Icon, Notice } from './ui.tsx'

// Read once per page load, after the first render waited for the link to be redeemed.
let outcome: ConnectOutcome | null | undefined
function connectOutcome(): ConnectOutcome | null {
  if (outcome === undefined) outcome = takeConnectOutcome()
  return outcome
}

export const hostOf = (url: string) => {
  try {
    return new URL(url).host
  } catch {
    return url
  }
}

interface LocalComputer {
  identity: Poll<Identity | null>
  repositories: ConnectedRepository[]
}

const LocalComputerContext = createContext<LocalComputer | null>(null)

/**
 * Reads this computer's identity (rechecked every minute) and its connected repositories once for
 * the whole page, for the repository menu, the notices and Settings.
 */
export function LocalComputerProvider({ token, children }: { token: string | null; children: ReactNode }) {
  const identity = usePoll((signal) => token ? fetchIdentity(token, signal) : Promise.resolve(null), token ? 60_000 : 0, token ?? '')
  const repositories = usePoll((signal) => token ? fetchRepositories(token, signal) : Promise.resolve([]), 0, token ?? '')
  const value = useMemo(() => token ? { identity, repositories: repositories.kind === 'ready' ? repositories.value : [] } : null, [token, identity, repositories])
  return <LocalComputerContext.Provider value={value}>{children}</LocalComputerContext.Provider>
}

/** What `LocalComputerProvider` read; `null` outside this computer's own Hub page. */
export const useLocalComputer = () => useContext(LocalComputerContext)

export const selectorOf = (r: ConnectedRepository) => `${r.repository}@${r.hub}`

/** Only what needs doing on this computer's own Hub page; nothing when all is well. */
export function ComputerNotices() {
  const local = useLocalComputer()
  if (!local) return null
  const { identity, repositories } = local
  const used = connectOutcome() === 'used'
  const value = identity.kind === 'ready' ? identity.value : null
  return (
    <>
      {used && (
        <Notice tone="warn" icon="link-45deg" title="That link was already used">
          Each link from <code>agentscm open</code> or Open Hub works once. You are still connected with the link you used before.
        </Notice>
      )}
      {value && value.repository === null && (
        <Notice tone="warn" icon="folder-x" title="That repository is no longer connected here">
          {repositories.length > 0 ? 'Choose another from the repository menu at the top, or run ' : 'Run '}
          <code>agentscm open</code> in its checkout. This browser stays connected to your computer.
        </Notice>
      )}
      {value?.hub && !value.hub.signed_in && (
        <Notice tone="warn" icon="box-arrow-in-right" title="This computer isn’t signed in to its Hub">
          Your files and saved conversations still work. To see team work here too, run <code>agentscm hub login --hub {value.hub.url}</code> on this computer, then reload.
        </Notice>
      )}
    </>
  )
}

/** Settings → This computer: what is connected, and Disconnect. */
export function ComputerSettings() {
  const local = useLocalComputer()
  const [busy, setBusy] = useState(false)
  if (!local) return null
  const { identity } = local
  const value = identity.kind === 'ready' ? identity.value : null
  return (
    <div className="settings-row">
      <div>
        <div className="settings-label">
          <Icon name="laptop" /> {value?.computer ?? 'This computer'}
        </div>
        <div className="muted small">
          {value?.repository ? <code>{value.repository}</code> : 'No repository selected'}
          {value?.expires_at ? ` · connected until ${new Date(value.expires_at * 1000).toLocaleDateString()}, or until AgentSCM stops on this computer` : ''}
        </div>
      </div>
      <button
        type="button"
        className="btn btn-outline-secondary"
        disabled={busy}
        onClick={() => {
          setBusy(true)
          void disconnectComputer().then(() => window.location.reload())
        }}
      >
        <Icon name="box-arrow-right" /> Disconnect this browser
      </button>
    </div>
  )
}
