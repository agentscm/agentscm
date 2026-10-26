// Where the browser lands after "sign in once per computer". It only reads its own fragment:
// `#signed-in?login=…&computer=…`, or `#signed-in?error=<code>[&login&repo&role&needs&visibility]`
// when the Hub said no. Nothing here is a secret, and nothing is stored or sent.
// Anyone can craft this link, so it never shows free text or a command from it: the Hub is always
// this page's own origin, a refusal is composed from validated fields, and its fix is a fixed local
// sentence. A forged "signed in" state is possible but only changes what this tab displays.
import { useState } from 'react'
import { Logo } from '../App.tsx'
import { safeHubOrigin } from '../connectAgent.ts'
import { hashPath, hashQuery } from '../Pager.tsx'
import { refusalFrom, type Refusal } from '../refusal.ts'
import { RefusalNotice } from '../Refusal.tsx'
import { Icon } from '../ui.tsx'

export type SignedInState =
  | { kind: 'ok'; hub: string; host: string; login: string; computer: string | null }
  | { kind: 'refused'; hub: string; refusal: Refusal }
  | { kind: 'invalid'; hub: string }

const LOGIN = /^[A-Za-z0-9](?:[A-Za-z0-9._-]{0,98}[A-Za-z0-9])?$/
const printable = (v: string | null, max: number): string | null => {
  // eslint-disable-next-line no-control-regex
  const t = v?.replace(/[\u0000-\u001f\u007f]+/g, ' ').trim()
  return t ? t.slice(0, max) : null
}

export const isSignedInHash = (hash = window.location.hash) => hashPath(hash) === '#signed-in'

const AFTER_REFUSAL = 'When that’s sorted, choose Sign In in the AgentSCM menu bar app.'

export function signedInFromHash(hash = window.location.hash, origin = window.location.origin): SignedInState {
  const q = hashQuery(hash)
  const hub = safeHubOrigin(origin) ?? origin
  const error = q.get('error')
  if (error !== null) {
    // Never `message` or `fix` from the link: only the code and fields the sentence is built from.
    const refusal = refusalFrom({ error, login: q.get('login'), repo: q.get('repo'), role: q.get('role'), needs: q.get('needs'), visibility: q.get('visibility') })
    return refusal ? { kind: 'refused', hub, refusal: { ...refusal, fix: AFTER_REFUSAL } } : { kind: 'invalid', hub }
  }
  const login = q.get('login')
  if (!login || !LOGIN.test(login)) return { kind: 'invalid', hub }
  return { kind: 'ok', hub, host: new URL(hub).host, login, computer: printable(q.get('computer'), 80) }
}

const NEXT = [
  { icon: 'terminal', title: 'Start your coding agent in any checkout.', body: 'It connects by itself and tells you in one line, for example “AgentSCM: coordinating on owner/repo”.' },
  { icon: 'menu-button-wide', title: 'The menu bar app shows Connected.', body: 'If something needs you, it says what and how to fix it.' },
  { icon: 'shield-check', title: 'Remove this computer any time.', body: 'Account menu → Your computers → Revoke.' },
]

export function SignedInPage() {
  const [state] = useState(() => signedInFromHash())
  const hubUi = `${state.hub}/ui/`
  return (
    <main className="done">
      <div className="done-top">
        <Logo dark height={32} />
      </div>
      <section className="done-body" aria-labelledby="done-title">
        {state.kind === 'ok' ? (
          <>
            <span className="done-mark tone-ok" aria-hidden="true">
              <Icon name="check2" />
            </span>
            <h1 id="done-title" className="done-title">This computer is signed in.</h1>
            <p className="done-lead">
              {state.computer ? <strong>{state.computer}</strong> : 'This computer'} is signed in to <strong>{state.host}</strong> as <strong>{state.login}</strong>. You won’t need to do this again.
            </p>
            <ol className="done-next">
              {NEXT.map((n) => (
                <li key={n.title}>
                  <Icon name={n.icon} />
                  <span>
                    <strong>{n.title}</strong> {n.body}
                  </span>
                </li>
              ))}
            </ol>
          </>
        ) : (
          <>
            <span className="done-mark tone-bad" aria-hidden="true">
              <Icon name="x-lg" />
            </span>
            <h1 id="done-title" className="done-title">This computer isn’t signed in yet.</h1>
            {state.kind === 'refused' ? (
              <div className="done-sheet">
                <RefusalNotice refusal={state.refusal} />
              </div>
            ) : (
              <p className="done-lead">This sign-in link is incomplete. Choose Sign In in the AgentSCM menu bar app to start again.</p>
            )}
          </>
        )}
        <div className="done-actions">
          <a className="btn btn-primary btn-lg" href={hubUi}>
            Open the Hub <Icon name="arrow-right" />
          </a>
          {state.kind === 'ok' && <span className="done-close">You can close this tab.</span>}
        </div>
      </section>
    </main>
  )
}
