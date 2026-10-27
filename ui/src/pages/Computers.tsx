import { useCallback, useEffect, useState } from 'react'
import { ApiError } from '../api.ts'
import { deviceAuth, fetchAdmin, fetchDevices, revokeDevice, signOutBrowser, type AdminRepository, type Device, type DeviceAuth, type SignInRefusal } from '../devices.ts'
import { reasonSentence } from '../refusal.ts'
import { RefusalNotice } from '../Refusal.tsx'
import { SectionTabs } from '../TopNav.tsx'
import { Ago, Box, Empty, Icon, Notice, Pill, Skeleton } from '../ui.tsx'

export type Load<T> = { kind: 'loading' } | { kind: 'ready'; value: T } | { kind: 'error'; error: unknown }
const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`

function Failure({ error, what }: { error: unknown; what: string }) {
  if (error instanceof ApiError && error.refusal) return <RefusalNotice refusal={error.refusal} />
  if (error instanceof ApiError && error.status === 401) return <Notice tone="warn" icon="person-lock" title="Your browser sign-in on this Hub has ended">Sign in again to see {what}.</Notice>
  if (error instanceof ApiError && error.status === 403) return <Notice tone="warn" icon="shield-lock" title={`Only Hub admins can see ${what}`} />
  return <Notice tone="bad" icon="wifi-off" title={`Could not load ${what}`}>The Hub didn’t answer. This page tries again when you reload it.</Notice>
}

/** How this page reaches the computer and admin routes. Read once per signed-in page; `reload` after signing out. */
export function useDeviceAuth(token: string) {
  const [auth, setAuth] = useState<Load<DeviceAuth | null>>({ kind: 'loading' })
  const [tick, setTick] = useState(0)
  useEffect(() => {
    const ctl = new AbortController()
    deviceAuth(token, ctl.signal).then(
      (value) => setAuth({ kind: 'ready', value }),
      (error) => !ctl.signal.aborted && setAuth({ kind: 'error', error }),
    )
    return () => ctl.abort()
  }, [token, tick])
  return { auth, reload: useCallback(() => setTick((t) => t + 1), []) }
}

const FORGE = { github: 'GitHub', gitlab: 'GitLab' } as const

/** Signed out in this browser: one way in, through the forge. */
function SignInHere({ provider, what }: { provider: 'github' | 'gitlab'; what: string }) {
  return (
    <div className="computer-signin">
      <p>Sign in to this Hub in your browser to see {what}. A read-only repository token can’t.</p>
      <a className="btn btn-primary" href={`/v1/hub/login/${provider}?web=1`}>
        <Icon name={provider} /> Sign in with {FORGE[provider]}
      </a>
    </div>
  )
}

function RevokeButton({ device, auth, onDone }: { device: Device; auth: DeviceAuth; onDone: (outcome: 'revoked' | 'gone') => void }) {
  const [step, setStep] = useState<'idle' | 'confirm' | 'busy'>('idle')
  const [error, setError] = useState<unknown>(null)
  if (device.revokedAt) return null
  if (device.current) return <span className="computer-note">Sign this computer out from the menu bar app.</span>
  if (step === 'idle')
    return (
      <button type="button" className="btn btn-sm btn-outline-danger" onClick={() => setStep('confirm')}>
        Revoke
      </button>
    )
  return (
    <div className="computer-confirm" role="group" aria-label={`Revoke ${device.name}`}>
      <span>Its agents stop at once and their tokens are revoked.</span>
      <button
        type="button"
        className="btn btn-sm btn-danger"
        disabled={step === 'busy'}
        onClick={async () => {
          setStep('busy')
          setError(null)
          try {
            onDone(await revokeDevice(auth, device.id))
          } catch (e) {
            setError(e)
            setStep('confirm')
          }
        }}
      >
        {step === 'busy' && <span className="spinner-border spinner-border-sm me-1" aria-hidden="true" />}Revoke {device.name}
      </button>
      <button type="button" className="btn btn-sm btn-link" disabled={step === 'busy'} onClick={() => setStep('idle')}>
        Cancel
      </button>
      {error !== null && (
        <span className="computer-error" role="alert">
          {error instanceof ApiError && error.refusal
            ? error.refusal.message
            : error instanceof ApiError && error.status === 400
              ? 'This computer can’t remove itself here; use Sign Out in its menu bar app.'
              : error instanceof ApiError && error.status === 403
                ? 'Your browser sign-in needs a refresh; reload the page and try again.'
                : 'The Hub didn’t confirm the revoke. Nothing changed; try again.'}
        </span>
      )}
    </div>
  )
}

function ComputerList({ devices, auth, showOwner = false, onChange }: { devices: Device[]; auth: DeviceAuth; showOwner?: boolean; onChange: () => void }) {
  const [showRevoked, setShowRevoked] = useState(false)
  const [notice, setNotice] = useState<string | null>(null)
  const active = devices.filter((d) => !d.revokedAt)
  const revoked = devices.filter((d) => d.revokedAt)
  const shown = showRevoked ? devices : active
  return (
    <>
      {notice && <Notice tone="ok" icon="check-circle" title={notice} />}
      {active.length === 0 && <Empty icon="laptop" title="No computers are signed in" />}
      <ul className="plain-list computers">
        {shown.map((d) => (
          <li key={d.id} className={`computer${d.revokedAt ? ' is-revoked' : ''}`}>
            <Icon name="laptop" className="computer-icon" />
            <div className="computer-main">
              <div className="computer-name">
                <strong>{d.name}</strong>
                {d.current && <Pill tone="brand">This computer</Pill>}
                {d.revokedAt && <Pill tone="muted">Revoked</Pill>}
                {showOwner && d.owner && <span className="computer-owner">{d.owner.login}</span>}
              </div>
              <div className="computer-meta">
                Signed in <Ago at={d.createdAt} />
                {d.revokedAt ? (
                  <>
                    {' '}· revoked <Ago at={d.revokedAt} />
                  </>
                ) : d.lastUsedAt ? (
                  <>
                    {' '}· last used <Ago at={d.lastUsedAt} />
                  </>
                ) : (
                  ' · not used yet'
                )}
                {d.repositories.length > 0 && <> · {plural(d.repositories.length, 'repository', 'repositories')}</>}
              </div>
              {d.repositories.length > 0 && (
                <div className="computer-repos">
                  {d.repositories.slice(0, 4).map((r) => (
                    <code key={r}>{r}</code>
                  ))}
                  {d.repositories.length > 4 && <span className="muted">+{d.repositories.length - 4} more</span>}
                </div>
              )}
            </div>
            <div className="computer-act">
              <RevokeButton
                device={d}
                auth={auth}
                onDone={(outcome) => {
                  setNotice(outcome === 'revoked' ? `${d.name} was revoked. Its agents can no longer act for ${d.owner?.login ?? 'you'}.` : `${d.name} was already removed.`)
                  onChange()
                }}
              />
            </div>
          </li>
        ))}
      </ul>
      {revoked.length > 0 && (
        <button type="button" className="btn btn-link btn-sm px-0" onClick={() => setShowRevoked((s) => !s)} aria-expanded={showRevoked}>
          {showRevoked ? 'Hide revoked computers' : `Show ${plural(revoked.length, 'revoked computer')}`}
        </button>
      )}
    </>
  )
}

export function ComputersPage({ auth, provider, onSignedOut }: { auth: Load<DeviceAuth | null>; provider: 'github' | 'gitlab'; onSignedOut: () => void }) {
  const [devices, setDevices] = useState<Load<{ devices: Device[]; hub: string | null }>>({ kind: 'loading' })
  const [signingOut, setSigningOut] = useState(false)
  const [signOutError, setSignOutError] = useState(false)
  const [tick, setTick] = useState(0)
  const reload = useCallback(() => setTick((t) => t + 1), [])
  const ready = auth.kind === 'ready' ? auth.value : null
  useEffect(() => {
    if (!ready) return
    const ctl = new AbortController()
    fetchDevices(ready, ctl.signal).then(
      (value) => setDevices({ kind: 'ready', value }),
      (error) => !ctl.signal.aborted && setDevices({ kind: 'error', error }),
    )
    return () => ctl.abort()
  }, [ready, tick])
  const active = devices.kind === 'ready' ? devices.value.devices.filter((d) => !d.revokedAt).length : null
  // The local page reads one Hub at a time (the current repository's); say which.
  const hub = devices.kind === 'ready' && devices.value.hub ? new URL(devices.value.hub).host : 'this Hub'
  const session = ready?.kind === 'browser' ? ready.session : null
  return (
    <div className="page">
      <div className="answer">
        <h1 className="page-question">Which computers can act for me?</h1>
        <p className="answer-text">{active === null ? `Your computers signed in to ${hub}.` : active === 0 ? `No computer is signed in as you on ${hub}.` : <><b>{plural(active, 'computer')}</b> can start agents as you on {hub}.</>}</p>
      </div>
      <SectionTabs />
      <Box
        icon="laptop"
        title="Your computers"
        count={active ?? undefined}
        explain="Each computer signs in once and renews its own agent tokens. Revoking one stops its agents at once."
        actions={
          session && (
            <button
              type="button"
              className="btn btn-sm btn-outline-secondary"
              disabled={signingOut}
              onClick={async () => {
                setSigningOut(true)
                setSignOutError(false)
                try {
                  await signOutBrowser(session)
                  onSignedOut()
                } catch {
                  setSignOutError(true)
                } finally {
                  setSigningOut(false)
                }
              }}
            >
              Sign out of this browser
            </button>
          )
        }
      >
        {signOutError && <Notice tone="bad" icon="box-arrow-right" title="This browser is still signed in">The Hub didn’t confirm the sign-out. Reload the page and try again.</Notice>}
        {auth.kind === 'loading' || (ready && devices.kind === 'loading') ? (
          <Skeleton rows={2} />
        ) : auth.kind === 'error' ? (
          <Failure error={auth.error} what="your computers" />
        ) : !ready ? (
          <SignInHere provider={provider} what="your computers" />
        ) : devices.kind === 'error' ? (
          <Failure error={devices.error} what="your computers" />
        ) : devices.kind === 'ready' ? (
          <ComputerList devices={devices.value.devices} auth={ready} onChange={reload} />
        ) : null}
      </Box>
    </div>
  )
}

const VIA = { sign_in: 'browser sign-in', device_mint: 'agent connecting', web_read_token: 'opening the Hub’s pages' } as const

function Refusals({ rows }: { rows: SignInRefusal[] }) {
  if (rows.length === 0) return <Empty icon="shield-check" title="Nobody was turned away in the last 30 days" />
  return (
    <ul className="plain-list refusals">
      {rows.map((r) => (
        <li key={`${r.provider}:${r.login}:${r.reason}`} className="refusal-row">
          <div>
            <strong>{r.login}</strong> <span className="muted">· {r.provider === 'gitlab' ? 'GitLab' : 'GitHub'} · {VIA[r.via]}</span>
            <p className="refusal-why">{reasonSentence(r.reason, r.provider) ?? `Refused (${r.reason}).`}</p>
          </div>
          <div className="refusal-when">
            {r.count > 1 && <Pill tone="warn">{r.count}×</Pill>} <Ago at={r.at} />
          </div>
        </li>
      ))}
    </ul>
  )
}

function Repositories({ rows }: { rows: AdminRepository[] }) {
  if (rows.length === 0) return <Empty icon="plug" title="No repositories are connected yet" />
  return (
    <div className="table-responsive">
      <table className="table admin-table">
        <thead>
          <tr>
            <th scope="col">Repository</th>
            <th scope="col" className="num">Computers</th>
            <th scope="col" className="num">Agent tokens</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.fullName}>
              <td>
                <Icon name={r.provider === 'gitlab' ? 'gitlab' : 'github'} /> <code>{r.fullName}</code>
                {r.host && !['github.com', 'gitlab.com'].includes(r.host) && <span className="muted small"> · {r.host}</span>}
              </td>
              <td className="num">{r.devices}</td>
              <td className="num">{r.agentTokens}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

export function AdminPage({ auth, provider }: { auth: Load<DeviceAuth | null>; provider: 'github' | 'gitlab' }) {
  const [data, setData] = useState<Load<Awaited<ReturnType<typeof fetchAdmin>>>>({ kind: 'loading' })
  const [tick, setTick] = useState(0)
  const browser = auth.kind === 'ready' && auth.value?.kind === 'browser' && auth.value.session.admin ? auth.value : null
  useEffect(() => {
    if (!browser) return
    const ctl = new AbortController()
    fetchAdmin(browser, ctl.signal).then(
      (value) => setData({ kind: 'ready', value }),
      (error) => !ctl.signal.aborted && setData({ kind: 'error', error }),
    )
    return () => ctl.abort()
  }, [browser, tick])
  const v = data.kind === 'ready' ? data.value : null
  return (
    <div className="page">
      <div className="answer">
        <h1 className="page-question">Which repositories are connected, and who was turned away?</h1>
        <p className="answer-text">
          {v ? (
            <>
              <b>{plural(v.repositories.length, 'repository', 'repositories')}</b> connected · {v.refusals.length === 0 ? 'nobody turned away' : <b className="tone-warn">{plural(v.refusals.reduce((n, r) => n + r.count, 0), 'refusal')}</b>} in the last 30 days.
            </>
          ) : (
            'Connected repositories, refusals and every signed-in computer.'
          )}
        </p>
      </div>
      <SectionTabs />
      {auth.kind === 'loading' || (browser && data.kind === 'loading') ? (
        <Skeleton rows={4} />
      ) : auth.kind === 'error' ? (
        <Failure error={auth.error} what="the admin lists" />
      ) : auth.kind === 'ready' && !auth.value ? (
        <Box icon="shield-lock" title="Hub admins only">
          <SignInHere provider={provider} what="who was turned away and which repositories are connected" />
        </Box>
      ) : !browser ? (
        <Notice tone="muted" icon="shield-lock" title="Only Hub admins see this page">
          {auth.kind === 'ready' && auth.value?.kind === 'local' ? 'Open the Hub’s own page and sign in there as a Hub admin. This computer’s sign-in never opens it.' : 'Your account isn’t a Hub admin on this Hub.'}
        </Notice>
      ) : data.kind === 'error' ? (
        <Failure error={data.error} what="the admin lists" />
      ) : v ? (
        <div className="stack">
          <Box icon="person-x" tone={v.refusals.length ? 'warn' : 'ok'} title="Turned away" count={v.refusals.length} explain="Sign-ins and agent connections the Hub refused in the last 30 days, one row per person and reason.">
            <Refusals rows={v.refusals} />
          </Box>
          <Box icon="plug" title="Connected repositories" count={v.repositories.length}>
            <Repositories rows={v.repositories} />
          </Box>
          <Box icon="laptop" title="Every signed-in computer" count={v.devices.filter((d) => !d.revokedAt).length} explain="Revoking a computer stops its agents at once and revokes the agent tokens it made.">
            <ComputerList devices={v.devices} auth={browser} showOwner onChange={() => setTick((t) => t + 1)} />
          </Box>
        </div>
      ) : null}
    </div>
  )
}
