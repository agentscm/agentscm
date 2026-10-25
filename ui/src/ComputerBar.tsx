import { useState } from 'react'
import { disconnectComputer, fetchIdentity, fetchRepositories, localSelection, switchRepository, takeConnectOutcome, type ConnectedRepository, type ConnectOutcome, type Identity } from './frontDoor.ts'
import { usePoll, type Poll } from './usePoll.ts'
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

/** This computer's identity (rechecked every minute) and its connected repositories. */
function useLocalComputer(token: string): { identity: Poll<Identity>; repositories: ConnectedRepository[] } {
  const identity = usePoll((signal) => fetchIdentity(token, signal), 60_000, token)
  const repositories = usePoll((signal) => fetchRepositories(token, signal), 0, token)
  return { identity, repositories: repositories.kind === 'ready' ? repositories.value : [] }
}

const selectorOf = (r: ConnectedRepository) => `${r.repository}@${r.hub}`

/**
 * The repository menu at the top of this computer's own Hub page: the connected computer, every
 * repository connected on it (the same repository on two Hubs is told apart by Hub), and Disconnect.
 */
export function LocalRepoSwitcher({ token, repo }: { token: string; repo: string }) {
  const [open, setOpen] = useState(false)
  const { identity, repositories } = useLocalComputer(token)
  const computer = identity.kind === 'ready' ? identity.value.computer : 'This computer'
  const repository = identity.kind === 'ready' ? identity.value.repository : repo
  const hub = identity.kind === 'ready' ? identity.value.hub : null
  const selection = localSelection(token) ?? repository ?? ''
  const current = repositories.find((r) => selectorOf(r) === selection) ?? repositories.find((r) => r.repository.toLowerCase() === repository?.toLowerCase() && r.hub === hub?.url)
  const repeated = (r: ConnectedRepository) => repositories.filter((o) => o.repository.toLowerCase() === r.repository.toLowerCase()).length > 1
  return (
    <div
      className="pop"
      onKeyDown={(e) => {
        if (e.key === 'Escape') setOpen(false)
      }}
      onBlur={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setOpen(false)
      }}
    >
      <button type="button" data-tour="repo" className={`repo-btn${open ? ' is-open' : ''}`} aria-haspopup="menu" aria-expanded={open} aria-label={`${repository ?? 'No repository'} on ${computer}`} onClick={() => setOpen((o) => !o)}>
        <span className="repo-mark is-computer" aria-hidden="true"><Icon name="laptop" /></span>
        <span className="repo-name">{repository ?? 'Choose a repository'}</span>
        <Icon name="chevron-expand" className="repo-chevron" />
      </button>
      {open && (
        <div className="pop-menu pop-left" role="menu">
          <div className="pop-head">
            <Icon name="laptop" /> On {computer}
          </div>
          {repositories.map((r) => {
            const isCurrent = current !== undefined && selectorOf(r) === selectorOf(current)
            return (
              <button
                key={selectorOf(r)}
                type="button"
                role="menuitemradio"
                aria-checked={isCurrent}
                className={`pop-item${isCurrent ? ' is-current' : ''}`}
                onClick={() => {
                  setOpen(false)
                  if (!isCurrent && switchRepository(token, selectorOf(r))) window.location.reload()
                }}
              >
                <span className="repo-mark" aria-hidden="true"><Icon name="git" /></span>
                <span className="pop-main">
                  <code>{r.repository}</code>
                  <span className="pop-sub">
                    {repeated(r) ? hostOf(r.hub) : r.folder && r.folder !== r.repository.split('/')[1] ? r.folder : 'Read-only team view'}
                  </span>
                </span>
                {isCurrent && <Icon name="check2" className="pop-check" />}
              </button>
            )
          })}
          <p className="pop-note">
            <Icon name="info-circle" /> Files and saved conversations come from this computer. Team work comes from each repository’s Hub, read-only.
          </p>
          <div className="pop-sep" />
          <button
            type="button"
            role="menuitem"
            className="pop-item"
            onClick={() => {
              setOpen(false)
              void disconnectComputer().then(() => window.location.reload())
            }}
          >
            <Icon name="box-arrow-right" />
            <span className="pop-main">
              Disconnect this browser<span className="pop-sub">Connect again with Open Hub or <code>agentscm open</code></span>
            </span>
          </button>
        </div>
      )}
    </div>
  )
}

/** Only what needs doing on this computer's own Hub page; nothing when all is well. */
export function ComputerNotices({ token }: { token: string }) {
  const { identity, repositories } = useLocalComputer(token)
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
export function ComputerSettings({ token }: { token: string }) {
  const { identity } = useLocalComputer(token)
  const [busy, setBusy] = useState(false)
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
