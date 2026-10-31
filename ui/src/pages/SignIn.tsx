import { onFrontDoor } from '../frontDoor.ts'
import { useEffect, useState, type FormEvent, type ReactNode } from 'react'
import { AGENT_TOKEN_MESSAGE, ApiError, fetchWhoami } from '../api.ts'
import { Logo } from '../App.tsx'
import { fetchLoginProviders, loginHref, PROVIDER_NAMES, type LoginProvider } from '../hubLogin.ts'
import { agentSetupPrompt, signInHelpPrompt } from '../connectAgent.ts'
import { RollCall, openingSteps } from '../RollCall.tsx'
import { leaveToProvider } from '../viewTransition.ts'
import { CopyButton, Explain, Icon, Notice } from '../ui.tsx'
import type { Refusal } from '../refusal.ts'
import { RefusalNotice } from '../Refusal.tsx'
import { AskAgent } from '../AskAgent.tsx'

const CREATE = 'agentscm hub token create --read-only --ttl 30d --name status-page --repo owner/name'

/** A pasted read-only link works like its bare token; a link to another hub is refused before its token is sent anywhere. */
export function pastedToken(text: string, origin = window.location.origin): { token: string } | { error: string } {
  let url: URL
  try { url = new URL(text) } catch { return { token: text } }
  const token = new URLSearchParams(url.hash.slice(1)).get('token')
  if (!token) return { error: 'That link has no read-only token in it. Paste the whole link, or just the token.' }
  if (url.origin !== origin) return { error: `That link is for another hub (${url.origin}). Open the link itself instead.` }
  return { token }
}

function ProviderButtons({ providers, onLeave }: { providers: LoginProvider[]; onLeave: (p: LoginProvider) => void }) {
  return (
    <div className="signin-providers">
      {providers.map((p) => (
        // A plain click morphs the sheet to "Taking you to GitHub" before leaving; cmd/ctrl/middle clicks stay native.
        <a key={p} className="btn btn-primary btn-lg" href={loginHref(p)} onClick={(e) => {
          if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey || e.defaultPrevented) return
          e.preventDefault()
          onLeave(p)
        }}>
          <Icon name={p} /> Sign in with {PROVIDER_NAMES[p]}
        </a>
      ))}
    </div>
  )
}

const AccountHint = ({ names }: { names: string }) => <p className="signin-fine">Use the {names} account that can see your team’s repository.</p>

/** Where a person with no access goes: ask for a link, or make one for someone else. */
function NoAccess() {
  return (
    <>
      <p className="signin-fine signin-rule">No access? Ask a teammate for a read-only link, then open it in this browser.</p>
      <details className="disclosure signin-make">
        <summary>Make a read-only link for someone</summary>
        <p className="signin-fine">
          Sign in with <code>agentscm hub login</code>, then run:
        </p>
        <div className="command">
          <code>{CREATE}</code>
          <CopyButton text={CREATE} />
        </div>
      </details>
    </>
  )
}

const NoSignIn = () => <p className="signin-fine signin-rule">This hub has no GitHub or GitLab sign-in, so it opens with a read-only token. Ask whoever runs this hub for one.</p>

interface TokenCtl {
  value: string
  setValue: (v: string) => void
  keep: boolean
  setKeep: (k: boolean) => void
  busy: boolean
  reveal: boolean
  setReveal: (f: (r: boolean) => boolean) => void
  error: string | null
  submit: (ev: FormEvent) => void
  messages: ReactNode
}

/** The read-only token form; `children` follow the submit. */
function TokenForm({ c, children }: { c: TokenCtl; children?: ReactNode }) {
  return (
    <form onSubmit={c.submit} noValidate aria-label="Open with a read-only token">
      {c.messages}
      <div className="signin-label">
        <label htmlFor="token" className="form-label">
          Read-only link or token
        </label>
        <Explain text="A read-only link belongs to one repository and expires (30 days at most). Anyone holding it can read that repository’s tasks, notes and file paths; it cannot claim files, change work or answer requests. It is kept only in this browser. Whoever made it can revoke it with agentscm hub token revoke." />
      </div>
      <div className="input-group input-group-lg">
        <span className="input-group-text">
          <Icon name="lock" />
        </span>
        <input id="token" className={`form-control ${c.error ? 'is-invalid' : ''}`} type={c.reveal ? 'text' : 'password'} value={c.value} onChange={(e) => c.setValue(e.target.value)} autoComplete="off" spellCheck={false} placeholder="Paste it here" aria-describedby={c.error ? 'token-error' : undefined} />
        <button type="button" className="btn btn-outline-secondary" onClick={() => c.setReveal((r) => !r)} aria-label={c.reveal ? 'Hide token' : 'Show token'}>
          <Icon name={c.reveal ? 'eye-slash' : 'eye'} />
        </button>
      </div>
      {c.error && (
        <p id="token-error" className="field-error" role="alert">
          <Icon name="exclamation-circle" /> {c.error}
        </p>
      )}
      <div className="form-check signin-keep">
        <input id="keep" className="form-check-input" type="checkbox" checked={c.keep} onChange={(e) => c.setKeep(e.target.checked)} />
        <label className="form-check-label" htmlFor="keep">
          Keep me signed in on this browser
        </label>
      </div>
      <button type="submit" className="btn btn-primary btn-lg w-100" disabled={c.busy}>
        {c.busy ? <span className="spinner-border spinner-border-sm me-2" aria-hidden="true" /> : <Icon name="box-arrow-in-right" />} Open repository
      </button>
      {children}
    </form>
  )
}

const HEADLINE = 'See what every agent is working on.'
const LEAD = 'Who owns each task, which agent is editing which files, and where agents need to coordinate.'

/** An illustration of what the Hub shows, not live data. */
function Example() {
  const rows = [
    { who: 'maria’s claude-code', what: 'Fixing invoice rounding', initials: 'MC', hue: 262, state: 'Editing', bad: false },
    { who: 'ken’s codex', what: 'Migrating the billing tables', initials: 'KC', hue: 172, state: 'Editing', bad: false },
    { who: 'dev’s cursor', what: 'Updating webhook retries', initials: 'DC', hue: 22, state: 'Blocked', bad: true },
  ]
  return (
    <div className="signin-example" aria-hidden="true">
      <div className="signin-example-caption">Example</div>
      {rows.map((r) => (
        <div key={r.who} className="signin-example-row">
          <span className="signin-example-avatar" style={{ background: `hsl(${r.hue} 42% 40%)` }}>{r.initials}</span>
          <div className="signin-example-main">
            <span className="signin-example-who">{r.who}</span>
            <span className="signin-example-what">{r.what}</span>
          </div>
          <div className="signin-example-lane">
            <span className="seg-claimed" />
            <span className={`seg-now${r.bad ? ' is-blocked' : ''}`}>{r.state}</span>
            <span className="seg-next" />
            <span className="seg-done" />
          </div>
        </div>
      ))}
    </div>
  )
}

interface View {
  ready: boolean
  signInOnly: boolean
  providers: LoginProvider[]
  names: string
  messages: ReactNode
  form: TokenCtl
  agent: { title: string; prompt: string | null }
  /** The provider the person is being sent to, if they clicked its button. */
  leaving: LoginProvider | null
  onLeave: (p: LoginProvider) => void
}

/** Frame and sheet, the app's own anatomy: the promise on the frame, the way in (or the progress) on the sheet. */
export function SheetedFrame({ headline = HEADLINE, className = '', children }: { headline?: string; className?: string; children?: ReactNode }) {
  return (
    <div className={`signin signin-sheeted${className ? ` ${className}` : ''}`}>
      <div className="signin-logo">
        <Logo dark height={36} />
      </div>
      <div className="signin-pane-frame">
        <div className="signin-pane-frame-inner">
          <h1>{headline}</h1>
          <p className="lead">{LEAD}</p>
          <Example />
        </div>
      </div>
      <div className="signin-pane-sheet">{children}</div>
    </div>
  )
}

function SignInSheet({ v }: { v: View }) {
  return (
    <SheetedFrame>
      {v.leaving ? (
        <RollCall steps={[{ done: `Sent to ${PROVIDER_NAMES[v.leaving]}`, now: `Taking you to ${PROVIDER_NAMES[v.leaving]}` }, ...openingSteps()]} at={0} />
      ) : v.ready && (
        <div className="signin-sheet-inner">
          <section className="signin-card" aria-labelledby="sheet-title">
            <h2 id="sheet-title" className="signin-sheet-title">{v.signInOnly ? 'Sign in' : 'Open a repository'}</h2>
            {v.signInOnly ? (
              <>
                {v.messages}
                <ProviderButtons providers={v.providers} onLeave={v.onLeave} />
                <AccountHint names={v.names} />
                <NoAccess />
              </>
            ) : (
              <TokenForm c={v.form}>
                <NoSignIn />
              </TokenForm>
            )}
          </section>
          <AskAgent title={v.agent.title} prompt={v.agent.prompt} />
        </div>
      )}
    </SheetedFrame>
  )
}

export function SignIn({ notice, refusal: opening = null, onSignedIn }: { notice: string | null; /** Why the web sign-in couldn't open a repository. */ refusal?: Refusal | null; onSignedIn: (token: string, keep: boolean) => void }) {
  const [value, setValue] = useState('')
  const [keep, setKeep] = useState(false)
  const [busy, setBusy] = useState(false)
  const [reveal, setReveal] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [refusal, setRefusal] = useState<Refusal | null>(opening)
  // null until the Hub answers; the token form shows only when there is no other way in.
  const [providers, setProviders] = useState<LoginProvider[] | null>(null)
  useEffect(() => {
    if (onFrontDoor()) return
    const ctl = new AbortController()
    void fetchLoginProviders(ctl.signal).then((list) => {
      if (ctl.signal.aborted) return
      setProviders(list)
    })
    return () => ctl.abort()
  }, [])

  // Set on a click on a provider button; the page leaves for the provider once the sheet has morphed.
  const [leaving, setLeaving] = useState<LoginProvider | null>(null)
  const leave = (p: LoginProvider) => {
    if (leaving) return
    leaveToProvider(() => setLeaving(p), () => { window.location.href = loginHref(p) })
  }
  // Back from the provider through the bfcache restores this page as it was left: show the sign-in view again.
  useEffect(() => {
    const back = (e: PageTransitionEvent) => { if (e.persisted) setLeaving(null) }
    window.addEventListener('pageshow', back)
    return () => window.removeEventListener('pageshow', back)
  }, [])

  // With a sign-in provider, the sign-in buttons are the whole front door.
  const signInOnly = !onFrontDoor() && !!providers && providers.length > 0
  const submit = async (ev: FormEvent) => {
    ev.preventDefault()
    const text = value.trim()
    if (!text) return setError('Paste a read-only token to continue.')
    const pasted = pastedToken(text)
    if ('error' in pasted) return setError(pasted.error)
    const { token } = pasted
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
      else if (e instanceof ApiError) setError(`The Hub answered with an error. Try again in a moment.`)
      else setError('Could not reach the hub. Try again in a moment.')
      setBusy(false)
    }
  }

  const names = providers?.map((p) => PROVIDER_NAMES[p]).join(' or ') ?? ''
  const messages = (
    <>
      {notice && <Notice tone="brand" icon="info-circle" title={notice} />}
      {refusal && <RefusalNotice refusal={refusal} />}
    </>
  )

  if (onFrontDoor()) {
    return (
      <div className="signin">
        <div className="signin-logo">
          <Logo dark height={26} />
        </div>
        <div className="signin-column">
          <div className="signin-intro">
            <h1>{HEADLINE}</h1>
            <p className="lead">{LEAD}</p>
          </div>
          <section className="box signin-card" aria-labelledby="connect-title">
            <header className="box-head">
              <h2 id="connect-title" className="box-title">Connect this computer</h2>
            </header>
            <div className="box-body">
              {notice && <Notice tone="brand" icon="info-circle" title={notice} />}
              <p>
                In a terminal, run <code>agentscm open</code> in your repository, or choose <strong>Open Hub</strong> in the AgentSCM menu bar app. This page opens connected to your files, saved conversations and your team’s Hub.
              </p>
              <p className="signin-fine">Each connection lasts while AgentSCM keeps running on this computer, for up to a week.</p>
            </div>
          </section>
        </div>
      </div>
    )
  }

  // While the provider list is pending, only the intro shows, so nothing flashes in and out.
  const view: View = {
    ready: providers !== null,
    signInOnly,
    providers: providers ?? [],
    names,
    messages,
    form: { value, setValue, keep, setKeep, busy, reveal, setReveal, error, submit, messages },
    leaving,
    onLeave: leave,
    agent: signInOnly ? { title: 'Or let your agent set this up', prompt: agentSetupPrompt(window.location.origin) } : { title: 'Or ask your agent for help', prompt: signInHelpPrompt(window.location.origin) },
  }
  return <SignInSheet v={view} />
}
