// Where the browser lands after "sign in once per computer". The CLI's loopback page redirects to
// `#/signed-in?status=signed_in&login=…&provider=github|gitlab&computer=…&auto_connect=on|off`, or
// `#/signed-in?status=refused&reason=<kind>`. Nothing here is a secret, and nothing is stored or sent.
// Anyone can craft this link, so it never shows free text or a command from it: the Hub is always
// this page's own origin, and a refusal is the shared sentence for a known reason code. A forged
// "signed in" state is possible but only changes what this tab displays.
import { useState } from 'react'
import { Logo } from '../App.tsx'
import { safeHubOrigin } from '../connectAgent.ts'
import { hashPath, hashQuery } from '../Pager.tsx'
import { clean } from '../publications.ts'
import { reasonSentence, type Refusal } from '../refusal.ts'
import { RefusalNotice } from '../Refusal.tsx'
import { Icon } from '../ui.tsx'

export type SignedInState =
  | { kind: 'ok'; hub: string; host: string; login: string; provider: 'GitHub' | 'GitLab' | null; computer: string | null; autoConnect: boolean | null }
  | { kind: 'refused'; hub: string; refusal: Refusal }
  | { kind: 'invalid'; hub: string }

const LOGIN = /^[A-Za-z0-9](?:[A-Za-z0-9._-]{0,98}[A-Za-z0-9])?$/

export const isSignedInHash = (hash = window.location.hash) => ['#/signed-in', '#signed-in'].includes(hashPath(hash))

export function signedInFromHash(hash = window.location.hash, origin = window.location.origin): SignedInState {
  const q = hashQuery(hash)
  const hub = safeHubOrigin(origin) ?? origin
  if (q.get('status') === 'refused') {
    const reason = q.get('reason') ?? ''
    if (!/^[a-z_]{1,40}$/.test(reason)) return { kind: 'invalid', hub }
    // The reason is whatever the sign-in exchange answered; one without its own sentence still means sign-in didn't finish.
    const code = reasonSentence(reason) ? reason : 'login_failed'
    return { kind: 'refused', hub, refusal: { code, message: reasonSentence(code)!, fix: null } }
  }
  const login = q.get('login')
  if (q.get('status') !== 'signed_in' || !login || !LOGIN.test(login)) return { kind: 'invalid', hub }
  const provider = q.get('provider') === 'github' ? 'GitHub' : q.get('provider') === 'gitlab' ? 'GitLab' : null
  return { kind: 'ok', hub, host: new URL(hub).host, login, provider, computer: clean(q.get('computer'), 80).trim() || null, autoConnect: q.get('auto_connect') === 'on' ? true : q.get('auto_connect') === 'off' ? false : null }
}

// The terminal asks about automatic connection after this page opens, so the redirect may not say.
const START = {
  on: { icon: 'terminal', title: 'Start your coding agent in any checkout.', body: 'It connects by itself and tells you in one line, for example “AgentSCM: coordinating on owner/repo”.' },
  off: { icon: 'terminal', title: 'Connect a checkout from the menu bar app.', body: 'Automatic connection is off on this computer, so each repository is connected once from the AgentSCM menu bar app.' },
  ask: { icon: 'terminal', title: 'Start your coding agent in a checkout.', body: 'With automatic connection on (the terminal asks once), it connects by itself and tells you in one line; otherwise connect each repository once from the AgentSCM menu bar app.' },
}

function next(autoConnect: boolean | null) {
  return [
    START[autoConnect === null ? 'ask' : autoConnect ? 'on' : 'off'],
    { icon: 'menu-button-wide', title: 'The menu bar app shows Connected.', body: 'If something needs you, it says what and how to fix it.' },
    { icon: 'shield-check', title: 'Remove this computer any time.', body: 'Account menu → Your computers → Revoke.' },
  ]
}

export function SignedInPage() {
  const [state] = useState(() => signedInFromHash())
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
              {state.computer ? <strong>{state.computer}</strong> : 'This computer'} is signed in to <strong>{state.host}</strong> as <strong>{state.login}</strong>
              {state.provider && <> on {state.provider}</>}. You won’t need to do this again.
            </p>
            <ol className="done-next">
              {next(state.autoConnect).map((n) => (
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
          {/* The CLI signed in this computer, not this browser: open the Hub through the web sign-in, which the forge passes straight through right after authorizing. */}
          <a className="btn btn-primary btn-lg" href={state.kind === 'ok' && state.provider ? `${state.hub}/v1/hub/login/${state.provider.toLowerCase()}?web=1` : `${state.hub}/ui/`}>
            Open the Hub <Icon name="arrow-right" />
          </a>
          {state.kind === 'ok' && <span className="done-close">You can close this tab.</span>}
        </div>
      </section>
    </main>
  )
}
