import { onFrontDoor } from '../frontDoor.ts'
import { useState, type FormEvent } from 'react'
import { AGENT_TOKEN_MESSAGE, ApiError, fetchWhoami } from '../api.ts'
import { Logo } from '../App.tsx'
import { CopyButton, Explain, Icon, Notice } from '../ui.tsx'
import { ConnectAgent, InstallCommand, installCommandHere } from './Connect.tsx'
import type { Refusal } from '../refusal.ts'
import { RefusalNotice } from '../Refusal.tsx'

const CREATE = 'agentscm hub token create --read-only --ttl 30d --name status-page --repo owner/name'

export function SignIn({ notice, refusal: opening = null, onSignedIn }: { notice: string | null; /** Why the web sign-in couldn't open a repository. */ refusal?: Refusal | null; onSignedIn: (token: string, keep: boolean) => void }) {
  const [value, setValue] = useState('')
  const [keep, setKeep] = useState(false)
  const [busy, setBusy] = useState(false)
  const [reveal, setReveal] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [refusal, setRefusal] = useState<Refusal | null>(opening)
  const [connect, setConnect] = useState(false)

  const submit = async (ev: FormEvent) => {
    ev.preventDefault()
    const token = value.trim()
    if (!token) return setError('Paste a read-only token to continue.')
    setBusy(true)
    setError(null)
    setRefusal(null)
    try {
      const whoami = await fetchWhoami(token, new AbortController().signal)
      if (!whoami.token.read_only) {
        // Never stored, and dropped from the form.
        setValue('')
        setError(AGENT_TOKEN_MESSAGE)
        setBusy(false)
        return
      }
      onSignedIn(token, keep)
    } catch (e) {
      // Say why the Hub said no; "could not reach" only when it didn't answer.
      if (e instanceof ApiError && e.refusal) setRefusal(e.refusal)
      else if (e instanceof ApiError && e.status === 401) setError('That token was not accepted. Check that it is current and for this hub.')
      else if (e instanceof ApiError && e.status === 403) setError('This Hub answered but won’t open that repository with this token.')
      else if (e instanceof ApiError) setError(`The Hub answered with an error (HTTP ${e.status}). Try again in a moment.`)
      else setError('Could not reach the hub. Try again in a moment.')
      setBusy(false)
    }
  }

  return (
    <div className="signin">
      <div className="signin-intro">
        <Logo dark height={40} />
        <h1>See what every agent is working on.</h1>
        <p className="lead">The Hub shows which agent holds which files, who owns each task, and where two agents need to coordinate. It only reads; it never changes your work.</p>
        <ul className="signin-points">
          <li>
            <Icon name="broadcast" /> Live file ownership across agents
          </li>
          <li>
            <Icon name="kanban" /> Tasks, blockers and handovers by owner
          </li>
          <li>
            <Icon name="sign-intersection-y" /> Overlaps that need a conversation
          </li>
        </ul>
      </div>
      <div className="signin-panel">
        {installCommandHere() && (
          <section className="box mb-3" aria-labelledby="install-title">
            <header className="box-head">
              <span className="box-badge tone-brand">
                <Icon name="laptop" />
              </span>
              <h2 id="install-title" className="box-title">Set up this computer</h2>
            </header>
            <div className="box-body">
              <InstallCommand lead="Run this in a checkout of a repository on this Hub. It connects your agents there and signs you in." />
            </div>
          </section>
        )}
        {onFrontDoor() ? (
          <section className="box" aria-labelledby="connect-title">
            <header className="box-head">
              <span className="box-badge tone-brand">
                <Icon name="laptop" />
              </span>
              <h2 id="connect-title" className="box-title">Connect this computer</h2>
            </header>
            <div className="box-body">
              {notice && <Notice tone="brand" icon="info-circle" title={notice} />}
              <p>
                In a terminal, run <code>agentscm open</code> in your repository, or choose <strong>Open Hub</strong> in the AgentSCM menu bar app. This page opens connected to your files, saved conversations and your team’s Hub.
              </p>
              <p className="small muted mb-0">Each connection lasts while AgentSCM keeps running on this computer, for up to a week.</p>
            </div>
          </section>
        ) : (
        <form className="box" onSubmit={submit} noValidate>
          <header className="box-head">
            <span className="box-badge tone-brand">
              <Icon name="key" />
            </span>
            <h2 className="box-title">Open a repository</h2>
            <Explain text="A read-only token belongs to one repository. It can read status but cannot claim files, change work, or answer requests." />
          </header>
          <div className="box-body">
            {notice && (
              <Notice tone="brand" icon="info-circle" title={notice} />
            )}
            {refusal && <RefusalNotice refusal={refusal} />}
            <label htmlFor="token" className="form-label">
              Read-only token
            </label>
            <div className="input-group input-group-lg">
              <span className="input-group-text">
                <Icon name="lock" />
              </span>
              <input id="token" className={`form-control ${error ? 'is-invalid' : ''}`} type={reveal ? 'text' : 'password'} value={value} onChange={(e) => setValue(e.target.value)} autoComplete="off" spellCheck={false} placeholder="Paste token" aria-describedby={error ? 'token-error' : 'token-help'} />
              <button type="button" className="btn btn-outline-secondary" onClick={() => setReveal((r) => !r)} aria-label={reveal ? 'Hide token' : 'Show token'}>
                <Icon name={reveal ? 'eye-slash' : 'eye'} />
              </button>
            </div>
            {error ? (
              <p id="token-error" className="field-error" role="alert">
                <Icon name="exclamation-circle" /> {error}
              </p>
            ) : (
              <p id="token-help" className="form-text">
                Kept only in this browser.
              </p>
            )}
            <div className="form-check my-3">
              <input id="keep" className="form-check-input" type="checkbox" checked={keep} onChange={(e) => setKeep(e.target.checked)} />
              <label className="form-check-label" htmlFor="keep">
                Keep me signed in on this browser
              </label>
              <Explain text="Unchecked keeps the token only until this tab closes. Checked keeps it in this browser until you forget it." />
            </div>
            <button type="submit" className="btn btn-primary btn-lg w-100" disabled={busy}>
              {busy ? <span className="spinner-border spinner-border-sm me-2" aria-hidden="true" /> : <Icon name="box-arrow-in-right" />} Open status
            </button>
            <div className="create-token">
              <span className="small muted">Don’t have one? Create it from a terminal:</span>
              <div className="command">
                <code>{CREATE}</code>
                <CopyButton text={CREATE} />
              </div>
            </div>
          </div>
        </form>
        )}
        {!onFrontDoor() && (
        <div className="signin-alt">
          <button type="button" className="btn btn-outline-secondary" onClick={() => setConnect((c) => !c)} aria-expanded={connect}>
            <Icon name="plug" /> Connect an agent
          </button>
          <a className="btn btn-outline-secondary" href="#workspace">
            <Icon name="people" /> Sign in to workspace
          </a>
        </div>
        )}
        {connect && (
          <section className="box mt-3" aria-label="Connect an agent">
            <div className="box-body">
              <ConnectAgent />
            </div>
          </section>
        )}
      </div>
    </div>
  )
}
