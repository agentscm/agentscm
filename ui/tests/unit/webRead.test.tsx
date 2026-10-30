import { act, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mintWebReadToken, sessionRepositories, type BrowserSession } from '../../src/devices.ts'
import { ComputersPage } from '../../src/pages/Computers.tsx'
import { firstAllowed, RENEW_BEFORE_SECONDS, tryOrder, useWebRead } from '../../src/webRead.ts'

const SESSION: BrowserSession = { login: 'maria', provider: 'github', admin: false, csrf: 'csrf-token-0123456789' }
const SECOND_SESSION: BrowserSession = { ...SESSION, csrf: 'second-session-csrf-0123456789' }
const SESSION_BODY = { user: { id: 'usr_1', login: 'maria', provider: 'github', host: 'github.com', user_id: 1 }, admin: false, csrf_token: SESSION.csrf, expires_at: 1_900_000_000 }
const NOW = 1_800_000_000
const DEV = (repositories: string[], revoked: number | null = null) => ({ id: `dev_${'a'.repeat(32)}`, name: 'mbp', created_at: 1, last_used_at: null, revoked_at: revoked, current: false, repositories })
const TOKEN = (repository: string, extra: object = {}) => ({ id: 'tok', name: 'claude', repository, revoked: false, expires_at: NOW + 3600, read_only: false, created_at: 1, ...extra })
const REFUSED = { error: 'insufficient_role', login: 'maria', repository: 'acme/secret', visibility: 'private', has: 'read', needs: 'triage', message: 'maria has Read on private repository acme/secret; ask a repository owner for Triage or higher' }

type Call = { url: string; init: RequestInit }
type Reply = { status: number; body?: unknown }
function stubFetch(routes: Record<string, (init: RequestInit) => Reply | Promise<Reply>>) {
  const calls: Call[] = []
  vi.stubGlobal('fetch', async (url: string, init: RequestInit = {}) => {
    calls.push({ url, init })
    const route = routes[`${init.method ?? 'GET'} ${url}`]
    const { status, body } = route ? await route(init) : { status: 404, body: { error: 'not_found' } }
    return new Response(status === 204 ? null : JSON.stringify(body ?? {}), { status, headers: { 'content-type': 'application/json' } })
  })
  return calls
}
const minted = (calls: Call[]) => calls.filter((c) => c.url === '/v1/hub/session/read-token')

beforeEach(() => {
  vi.spyOn(Date, 'now').mockReturnValue(NOW * 1000)
  localStorage.clear()
  sessionStorage.clear()
})
afterEach(() => {
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
  vi.useRealTimers()
})

describe('sessionRepositories', () => {
  it('offers the union of live agent tokens and computers, sorted, without revoked or expired ones', async () => {
    stubFetch({
      'GET /v1/hub/tokens': () => ({ status: 200, body: { tokens: [TOKEN('acme/widgets'), TOKEN('acme/old', { revoked: true }), TOKEN('acme/expired', { expires_at: NOW - 1 }), TOKEN('not a repo')] } }),
      'GET /v1/hub/devices': () => ({ status: 200, body: { devices: [DEV(['acme/docs', 'acme/widgets']), DEV(['acme/gone'], NOW - 10)] } }),
    })
    expect(await sessionRepositories(SESSION)).toEqual(['acme/docs', 'acme/widgets'])
  })

  it('adds every connected repository for a Hub admin, and leaves out a source that fails', async () => {
    const calls = stubFetch({
      'GET /v1/hub/tokens': () => ({ status: 500 }),
      'GET /v1/hub/devices': () => ({ status: 200, body: { devices: [DEV(['acme/docs'])] } }),
      'GET /v1/hub/admin/repositories': () => ({ status: 200, body: { repositories: [{ full_name: 'acme/all', provider: 'github', host: 'github.com', devices: 0, agent_tokens: 0 }] } }),
    })
    expect(await sessionRepositories({ ...SESSION, admin: true })).toEqual(['acme/all', 'acme/docs'])
    await sessionRepositories(SESSION)
    expect(calls.filter((c) => c.url.includes('/admin/'))).toHaveLength(1)
  })
})

describe('mintWebReadToken', () => {
  it('posts only the repository, with the cookie and the CSRF header', async () => {
    const calls = stubFetch({ 'POST /v1/hub/session/read-token': () => ({ status: 201, body: { token: 'a'.repeat(64), expires_at: NOW + 1800 } }) })
    expect(await mintWebReadToken(SESSION, 'acme/widgets')).toEqual({ repository: 'acme/widgets', token: 'a'.repeat(64), expiresAt: NOW + 1800 })
    const { init } = calls[0]!
    expect(init.credentials).toBe('same-origin')
    expect((init.headers as Record<string, string>)['x-csrf-token']).toBe(SESSION.csrf)
    expect(JSON.parse(init.body as string)).toEqual({ repository: 'acme/widgets' })
  })

  it('refuses a reply it cannot read, and a repository name that isn’t one', async () => {
    stubFetch({ 'POST /v1/hub/session/read-token': () => ({ status: 201, body: { token: 'short', expires_at: NOW } }) })
    await expect(mintWebReadToken(SESSION, 'acme/widgets')).rejects.toThrow(/unreadable/)
    await expect(mintWebReadToken(SESSION, '../x')).rejects.toThrow()
  })
})

describe('choosing the repository', () => {
  it('tries the one picked last time first', () => {
    expect(tryOrder(['a/a', 'b/b', 'c/c'], 'b/b')).toEqual(['b/b', 'a/a', 'c/c'])
    expect(tryOrder(['a/a'], 'gone/gone')).toEqual(['a/a'])
  })

  it('skips repositories the Hub refuses, keeps the reason, and stops when the sign-in ended', async () => {
    stubFetch({
      'POST /v1/hub/session/read-token': (init) => (JSON.parse(init.body as string).repository === 'acme/secret' ? { status: 403, body: REFUSED } : { status: 201, body: { token: 'b'.repeat(64), expires_at: NOW + 1800 } }),
    })
    const r = await firstAllowed(SESSION, ['acme/secret', 'acme/widgets'])
    expect(r).toMatchObject({ read: { repository: 'acme/widgets' }, refused: ['acme/secret'], refusal: { message: REFUSED.message } })
    stubFetch({ 'POST /v1/hub/session/read-token': () => ({ status: 401, body: { error: 'sign_in_required' } }) })
    expect(await firstAllowed(SESSION, ['acme/widgets'])).toMatchObject({ read: null, ended: true })
  })
})

function Probe({ enabled = true }: { enabled?: boolean }) {
  const web = useWebRead(enabled)
  const s = web.state
  return (
    <div>
      <output data-testid="kind">{s.kind}</output>
      <output data-testid="sign-out-error">{web.signOutError ?? ''}</output>
      <button onClick={() => web.confirmSignedOut(SESSION)}>confirmed elsewhere</button>
      <button onClick={() => web.confirmSignedOut(SECOND_SESSION)}>confirmed second session</button>
      <button onClick={() => web.reportSignOutFailure(SESSION)}>failed elsewhere</button>
      <button onClick={() => web.retry()}>retry check</button>
      {((s.kind === 'checking' || s.kind === 'unavailable') && s.session) && <button onClick={() => void web.signOut()}>retry sign out</button>}
      {s.kind === 'ready' && (
        <>
          <output data-testid="repo">{s.read.repository}</output>
          <output data-testid="token">{s.read.token}</output>
          <output data-testid="repos">{s.repos.join(',')}</output>
          <output data-testid="refusal">{s.refusal?.message ?? ''}</output>
          <button onClick={() => web.pick('acme/secret')}>pick secret</button>
          <button onClick={() => web.pick('acme/docs')}>pick docs</button>
          <button onClick={web.renew}>renew</button>
          <button onClick={() => void web.signOut()}>sign out</button>
          <output data-testid="restarts">{s.restarts}</output>
        </>
      )}
      {s.kind === 'none' && <output data-testid="none">{s.empty ? 'empty' : (s.refusal?.message ?? 'signed out')}</output>}
      {s.kind === 'unavailable' && <output data-testid="unavailable">{s.reason}</output>}
    </div>
  )
}

function ComputersProbe() {
  const web = useWebRead(true)
  return <div>
    <output data-testid="kind">{web.state.kind}</output>
    <output data-testid="sign-out-error">{web.signOutError ?? ''}</output>
    <button onClick={web.renew}>renew</button>
    {web.state.kind === 'ready' && <ComputersPage auth={{ kind: 'ready', value: { kind: 'browser', session: web.state.session } }} provider="github" onSignedOut={web.confirmSignedOut} onSignOutFailed={web.reportSignOutFailure} />}
  </div>
}

function hub(extra: Record<string, (init: RequestInit) => Reply | Promise<Reply>> = {}) {
  let n = 0
  return stubFetch({
    'GET /v1/hub/session': () => ({ status: 200, body: SESSION_BODY }),
    'GET /v1/hub/tokens': () => ({ status: 200, body: { tokens: [TOKEN('acme/widgets')] } }),
    'GET /v1/hub/devices': () => ({ status: 200, body: { devices: [DEV(['acme/docs', 'acme/secret'])] } }),
    'POST /v1/hub/session/read-token': (init) => {
      const repository = JSON.parse(init.body as string).repository
      if (repository === 'acme/secret') return { status: 403, body: REFUSED }
      n++
      return { status: 201, body: { token: `${'t'.repeat(60)}${String(n).padStart(4, '0')}`, expires_at: NOW + 1800 } }
    },
    ...extra,
  })
}

describe('useWebRead', () => {
  it('opens the first allowed repository with one mint, and never stores the token', async () => {
    const calls = hub()
    render(<Probe />)
    expect((await screen.findByTestId('repo')).textContent).toBe('acme/docs')
    expect(minted(calls)).toHaveLength(1)
    // acme/docs opened first, so acme/secret wasn't tried yet and is still offered; picking it is the next test.
    expect(screen.getByTestId('repos').textContent).toBe('acme/docs,acme/secret,acme/widgets')
    const stored = JSON.stringify({ ...localStorage }) + JSON.stringify({ ...sessionStorage })
    expect(stored).not.toContain('tttt')
    expect(localStorage.getItem('agentscm.web.repository')).toBe('acme/docs')
  })

  it('stays off when a token was pasted, and asks nothing', async () => {
    const calls = hub()
    render(<Probe enabled={false} />)
    expect(screen.getByTestId('kind').textContent).toBe('off')
    await act(async () => {})
    expect(calls).toHaveLength(0)
  })

  it('says why when picking a refused repository, drops it and stays on the current one', async () => {
    localStorage.setItem('agentscm.web.repository', 'acme/widgets')
    hub()
    render(<Probe />)
    expect((await screen.findByTestId('repo')).textContent).toBe('acme/widgets')
    await act(async () => screen.getByText('pick secret').click())
    await waitFor(() => expect(screen.getByTestId('refusal').textContent).toBe(REFUSED.message))
    expect(screen.getByTestId('repo').textContent).toBe('acme/widgets')
    expect(screen.getByTestId('repos').textContent).not.toContain('acme/secret')
  })

  it('asks once for a renew even when called twice at the same moment', async () => {
    const calls = hub()
    render(<Probe />)
    await screen.findByTestId('repo')
    await act(async () => {
      screen.getByText('renew').click()
      screen.getByText('renew').click()
    })
    await waitFor(() => expect(minted(calls)).toHaveLength(2))
  })

  it('counts a renewal after a 401 as a restart, and a planned renewal not', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const calls = hub()
    render(<Probe />)
    await vi.waitFor(() => expect(minted(calls)).toHaveLength(1))
    await act(async () => vi.advanceTimersByTime((1800 - RENEW_BEFORE_SECONDS + 1) * 1000))
    await vi.waitFor(() => expect(minted(calls)).toHaveLength(2))
    expect(screen.getByTestId('restarts').textContent).toBe('0')
    await act(async () => screen.getByText('renew').click())
    await vi.waitFor(() => expect(screen.getByTestId('restarts').textContent).toBe('1'))
  })

  it('renews two minutes before the token expires', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    const calls = hub()
    render(<Probe />)
    await vi.waitFor(() => expect(minted(calls)).toHaveLength(1))
    await act(async () => vi.advanceTimersByTime((1800 - RENEW_BEFORE_SECONDS) * 1000 - 1000))
    expect(minted(calls)).toHaveLength(1)
    await act(async () => vi.advanceTimersByTime(2000))
    await vi.waitFor(() => expect(minted(calls)).toHaveLength(2))
  })

  it('skips a refused repository while opening, and does not offer it', async () => {
    localStorage.setItem('agentscm.web.repository', 'acme/secret')
    hub()
    render(<Probe />)
    expect((await screen.findByTestId('repo')).textContent).toBe('acme/docs')
    expect(screen.getByTestId('repos').textContent).toBe('acme/docs,acme/widgets')
  })

  it('offers what the Hub says is readable now first, even with no agent token or computer', async () => {
    const calls = hub({
      'GET /v1/hub/tokens': () => ({ status: 200, body: { tokens: [] } }),
      'GET /v1/hub/devices': () => ({ status: 200, body: { devices: [] } }),
      'GET /v1/hub/session/repositories': () => ({ status: 200, body: { repositories: [{ repository: 'acme/zeta' }, { repository: 'acme/alpha' }], unchecked: 0, complete: true } }),
    })
    render(<Probe />)
    expect((await screen.findByTestId('repo')).textContent).toBe('acme/zeta')
    expect(screen.getByTestId('repos').textContent).toBe('acme/zeta,acme/alpha')
    expect(minted(calls)).toHaveLength(1)
  })

  it('distinguishes no web sign-in from an identified session with nothing to open', async () => {
    hub({ 'GET /v1/hub/session': () => ({ status: 401 }) })
    const { unmount } = render(<Probe />)
    expect((await screen.findByTestId('none')).textContent).toBe('signed out')
    unmount()
    hub({ 'GET /v1/hub/tokens': () => ({ status: 200, body: { tokens: [] } }), 'GET /v1/hub/devices': () => ({ status: 200, body: { devices: [] } }) })
    render(<Probe />)
    expect((await screen.findByTestId('unavailable')).textContent).toBe('empty')
    expect(screen.getByText('retry sign out')).toBeTruthy()
  })

  it('ignores a mint response from before web sign-in was disabled and reopened', async () => {
    let mintNumber = 0
    let releaseOld!: (reply: Reply) => void
    hub({ 'POST /v1/hub/session/read-token': () => {
      mintNumber++
      if (mintNumber === 2) return new Promise<Reply>((resolve) => { releaseOld = resolve })
      return { status: 201, body: { token: String(mintNumber).repeat(64), expires_at: NOW + 1800 } }
    } })
    const view = render(<Probe />)
    await waitFor(() => expect(screen.getByTestId('token').textContent).toBe('1'.repeat(64)))
    act(() => screen.getByText('pick docs').click())
    await waitFor(() => expect(mintNumber).toBe(2))
    view.rerender(<Probe enabled={false} />)
    view.rerender(<Probe />)
    await waitFor(() => expect(screen.getByTestId('token').textContent).toBe('3'.repeat(64)))
    await act(async () => releaseOld({ status: 201, body: { token: '2'.repeat(64), expires_at: NOW + 1800 } }))
    expect(screen.getByTestId('token').textContent).toBe('3'.repeat(64))
  })

  it.each([204, 503])('applies a delayed HTTP %s sign-out reply to the same reopened browser session', async (status) => {
    let mintNumber = 0
    let releaseOld!: (reply: Reply) => void
    hub({
      'POST /v1/hub/session/read-token': () => ({ status: 201, body: { token: String(++mintNumber).repeat(64), expires_at: NOW + 1800 } }),
      'DELETE /v1/hub/session': () => new Promise<Reply>((resolve) => { releaseOld = resolve }),
    })
    const view = render(<Probe />)
    await waitFor(() => expect(screen.getByTestId('token').textContent).toBe('1'.repeat(64)))
    act(() => screen.getByText('sign out').click())
    await waitFor(() => expect(releaseOld).toBeTypeOf('function'))
    view.rerender(<Probe enabled={false} />)
    view.rerender(<Probe />)
    await waitFor(() => expect(screen.getByTestId('token').textContent).toBe('2'.repeat(64)))
    await act(async () => releaseOld({ status }))
    expect(screen.getByTestId('kind').textContent).toBe(status === 204 ? 'none' : 'ready')
    if (status === 204) expect(screen.queryByTestId('token')).toBeNull()
    else {
      expect(screen.getByTestId('token').textContent).toBe('2'.repeat(64))
      expect(screen.getByTestId('sign-out-error').textContent).toContain('sign-out failed')
    }
  })

  it.each([204, 503])('does not apply an old HTTP %s sign-out reply to a different reopened session', async (status) => {
    let csrf = SESSION.csrf
    let mintNumber = 0
    let releaseOld!: (reply: Reply) => void
    hub({
      'GET /v1/hub/session': () => ({ status: 200, body: { ...SESSION_BODY, csrf_token: csrf } }),
      'POST /v1/hub/session/read-token': () => ({ status: 201, body: { token: String(++mintNumber).repeat(64), expires_at: NOW + 1800 } }),
      'DELETE /v1/hub/session': () => new Promise<Reply>((resolve) => { releaseOld = resolve }),
    })
    const view = render(<Probe />)
    await waitFor(() => expect(screen.getByTestId('token').textContent).toBe('1'.repeat(64)))
    act(() => screen.getByText('sign out').click())
    await waitFor(() => expect(releaseOld).toBeTypeOf('function'))
    view.rerender(<Probe enabled={false} />)
    csrf = SECOND_SESSION.csrf
    view.rerender(<Probe />)
    await waitFor(() => expect(screen.getByTestId('token').textContent).toBe('2'.repeat(64)))
    await act(async () => releaseOld({ status }))
    expect(screen.getByTestId('kind').textContent).toBe('ready')
    expect(screen.getByTestId('token').textContent).toBe('2'.repeat(64))
    expect(screen.getByTestId('sign-out-error').textContent).toBe('')
  })

  it.each([204, 503])('ignores a delayed HTTP %s sign-out reply while web sign-in is disabled', async (status) => {
    let releaseOld!: (reply: Reply) => void
    hub({ 'DELETE /v1/hub/session': () => new Promise<Reply>((resolve) => { releaseOld = resolve }) })
    const view = render(<Probe />)
    await screen.findByTestId('token')
    act(() => screen.getByText('sign out').click())
    await waitFor(() => expect(releaseOld).toBeTypeOf('function'))
    view.rerender(<Probe enabled={false} />)
    await act(async () => releaseOld({ status }))
    expect(screen.getByTestId('kind').textContent).toBe('off')
    expect(screen.getByTestId('sign-out-error').textContent).toBe('')
  })

  it('remembers a delayed same-session confirmation while the reopened session identity is unknown', async () => {
    let gets = 0
    let mints = 0
    let releaseSession!: (reply: Reply) => void
    let releaseDelete!: (reply: Reply) => void
    hub({
      'GET /v1/hub/session': () => (++gets === 1 ? { status: 200, body: SESSION_BODY } : new Promise<Reply>((resolve) => { releaseSession = resolve })),
      'POST /v1/hub/session/read-token': () => ({ status: 201, body: { token: String(++mints).repeat(64), expires_at: NOW + 1800 } }),
      'DELETE /v1/hub/session': () => new Promise<Reply>((resolve) => { releaseDelete = resolve }),
    })
    const view = render(<Probe />)
    await screen.findByTestId('token')
    act(() => screen.getByText('sign out').click())
    await waitFor(() => expect(releaseDelete).toBeTypeOf('function'))
    view.rerender(<Probe enabled={false} />)
    view.rerender(<Probe />)
    await waitFor(() => expect(releaseSession).toBeTypeOf('function'))
    expect(screen.getByTestId('kind').textContent).toBe('checking')
    await act(async () => releaseDelete({ status: 204 }))
    await act(async () => releaseSession({ status: 200, body: SESSION_BODY }))
    expect(screen.getByTestId('kind').textContent).toBe('none')
    expect(mints).toBe(1)
  })

  it('serializes sign-out clicks and clears a failed attempt after a confirmed retry', async () => {
    let release!: (reply: Reply) => void
    let deletes = 0
    const calls = hub({ 'DELETE /v1/hub/session': () => {
      deletes++
      return deletes === 1 ? new Promise<Reply>((resolve) => { release = resolve }) : { status: 204 }
    } })
    render(<Probe />)
    await screen.findByTestId('token')
    act(() => { screen.getByText('sign out').click(); screen.getByText('sign out').click() })
    expect(calls.filter((c) => c.init.method === 'DELETE')).toHaveLength(1)
    await act(async () => release({ status: 503 }))
    expect(screen.getByTestId('kind').textContent).toBe('ready')
    expect(screen.getByTestId('sign-out-error').textContent).toContain('may still be signed in')
    await act(async () => screen.getByText('sign out').click())
    await waitFor(() => expect(screen.getByTestId('kind').textContent).toBe('none'))
    expect(calls.filter((c) => c.init.method === 'DELETE')).toHaveLength(2)
    expect(screen.getByTestId('sign-out-error').textContent).toBe('')
  })

  it('ignores an old menu failure after Computers confirms the same session gone', async () => {
    let release!: (reply: Reply) => void
    hub({ 'DELETE /v1/hub/session': () => new Promise<Reply>((resolve) => { release = resolve }) })
    render(<Probe />)
    await screen.findByTestId('token')
    act(() => screen.getByText('sign out').click())
    await waitFor(() => expect(release).toBeTypeOf('function'))
    act(() => screen.getByText('confirmed elsewhere').click())
    expect(screen.getByTestId('kind').textContent).toBe('none')
    await act(async () => release({ status: 503 }))
    expect(screen.getByTestId('kind').textContent).toBe('none')
    expect(screen.getByTestId('sign-out-error').textContent).toBe('')
  })

  it('does not let an old Computers response clear a newer browser session', async () => {
    let csrf = SESSION.csrf
    let mints = 0
    hub({
      'GET /v1/hub/session': () => ({ status: 200, body: { ...SESSION_BODY, csrf_token: csrf } }),
      'POST /v1/hub/session/read-token': () => ({ status: 201, body: { token: String(++mints).repeat(64), expires_at: NOW + 1800 } }),
    })
    const view = render(<Probe />)
    await waitFor(() => expect(screen.getByTestId('token').textContent).toBe('1'.repeat(64)))
    view.rerender(<Probe enabled={false} />)
    csrf = 'new-browser-session-csrf'
    view.rerender(<Probe />)
    await waitFor(() => expect(screen.getByTestId('token').textContent).toBe('2'.repeat(64)))
    act(() => screen.getByText('confirmed elsewhere').click())
    expect(screen.getByTestId('kind').textContent).toBe('ready')
    expect(screen.getByTestId('token').textContent).toBe('2'.repeat(64))
  })

  it('clears a failed sign-out alert when renewal finds the session gone', async () => {
    let mints = 0
    hub({
      'DELETE /v1/hub/session': () => ({ status: 503 }),
      'POST /v1/hub/session/read-token': () => (++mints === 1
        ? { status: 201, body: { token: 'a'.repeat(64), expires_at: NOW + 1800 } }
        : { status: 401, body: { error: 'sign_in_required' } }),
    })
    render(<Probe />)
    await screen.findByTestId('token')
    await act(async () => screen.getByText('sign out').click())
    await waitFor(() => expect(screen.getByTestId('sign-out-error').textContent).toContain('may still be signed in'))
    await act(async () => screen.getByText('renew').click())
    await waitFor(() => expect(screen.getByTestId('kind').textContent).toBe('none'))
    expect(screen.getByTestId('sign-out-error').textContent).toBe('')
  })

  it.each(['menu', 'computers'] as const)('does not restore the confirmed session after a paused recheck mint (%s)', async (caller) => {
    let mintCount = 0
    let releaseMint!: (reply: Reply) => void
    let releaseDelete!: (reply: Reply) => void
    hub({
      'POST /v1/hub/session/read-token': () => {
        mintCount++
        if (mintCount === 2) return { status: 503 }
        if (mintCount === 3) return new Promise<Reply>((resolve) => { releaseMint = resolve })
        return { status: 201, body: { token: 'a'.repeat(64), expires_at: NOW + 1800 } }
      },
      'DELETE /v1/hub/session': () => new Promise<Reply>((resolve) => { releaseDelete = resolve }),
    })
    render(<Probe />)
    await screen.findByTestId('token')
    if (caller === 'menu') act(() => screen.getByText('sign out').click())
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(releaseMint).toBeTypeOf('function'))
    expect(screen.getByTestId('kind').textContent).toBe('checking')
    if (caller === 'menu') await act(async () => releaseDelete({ status: 204 }))
    else act(() => screen.getByText('confirmed elsewhere').click())
    expect(screen.getByTestId('kind').textContent).toBe('none')
    await act(async () => releaseMint({ status: 201, body: { token: 'b'.repeat(64), expires_at: NOW + 1800 } }))
    expect(screen.getByTestId('kind').textContent).toBe('none')
    expect(screen.queryByTestId('token')).toBeNull()
  })

  it('applies confirmation after a bootstrap queues ready but before React renders it', async () => {
    let mints = 0
    let releaseMint!: (reply: Reply) => void
    const stored = vi.spyOn(Storage.prototype, 'setItem')
    hub({ 'POST /v1/hub/session/read-token': () => {
      mints++
      if (mints === 2) return { status: 503 }
      if (mints === 3) return new Promise<Reply>((resolve) => { releaseMint = resolve })
      return { status: 201, body: { token: 'a'.repeat(64), expires_at: NOW + 1800 } }
    } })
    render(<Probe />)
    await screen.findByTestId('token')
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(releaseMint).toBeTypeOf('function'))
    await act(async () => {
      releaseMint({ status: 201, body: { token: 'b'.repeat(64), expires_at: NOW + 1800 } })
      for (let i = 0; i < 50 && stored.mock.calls.filter(([key]) => key === 'agentscm.web.repository').length < 2; i++) await Promise.resolve()
      expect(stored.mock.calls.filter(([key]) => key === 'agentscm.web.repository')).toHaveLength(2)
      expect(screen.getByTestId('kind').textContent).toBe('checking')
      screen.getByText('confirmed elsewhere').click()
    })
    expect(screen.getByTestId('kind').textContent).toBe('none')
  })

  it('keeps a newer session when an older DELETE is confirmed during an unknown recheck', async () => {
    let gets = 0
    let mints = 0
    let releaseSession!: (reply: Reply) => void
    hub({
      'GET /v1/hub/session': () => (++gets === 1
        ? { status: 200, body: SESSION_BODY }
        : new Promise<Reply>((resolve) => { releaseSession = resolve })),
      'POST /v1/hub/session/read-token': () => (++mints === 2
        ? { status: 503 }
        : { status: 201, body: { token: String(mints).repeat(64), expires_at: NOW + 1800 } }),
    })
    render(<Probe />)
    await screen.findByTestId('token')
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(releaseSession).toBeTypeOf('function'))
    act(() => screen.getByText('confirmed elsewhere').click())
    await act(async () => releaseSession({ status: 200, body: { ...SESSION_BODY, csrf_token: SECOND_SESSION.csrf } }))
    await waitFor(() => expect(screen.getByTestId('token').textContent).toBe('3'.repeat(64)))
    act(() => screen.getByText('confirmed elsewhere').click())
    expect(screen.getByTestId('kind').textContent).toBe('ready')
  })

  it('retains both confirmed sessions when their DELETE replies arrive in reverse order', async () => {
    let gets = 0
    let mints = 0
    let releaseSession!: (reply: Reply) => void
    hub({
      'GET /v1/hub/session': () => (++gets === 1
        ? { status: 200, body: SESSION_BODY }
        : new Promise<Reply>((resolve) => { releaseSession = resolve })),
      'POST /v1/hub/session/read-token': () => (++mints === 2
        ? { status: 503 }
        : { status: 201, body: { token: String(mints).repeat(64), expires_at: NOW + 1800 } }),
    })
    render(<Probe />)
    await screen.findByTestId('token')
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(releaseSession).toBeTypeOf('function'))
    act(() => { screen.getByText('confirmed second session').click(); screen.getByText('confirmed elsewhere').click() })
    await act(async () => releaseSession({ status: 200, body: { ...SESSION_BODY, csrf_token: SECOND_SESSION.csrf } }))
    expect(screen.getByTestId('kind').textContent).toBe('none')
    expect(mints).toBe(2)
  })

  it.each([401, 503])('does not restore a confirmed session if rechecking ends with HTTP %s', async (status) => {
    let gets = 0
    let mints = 0
    let releaseSession!: (reply: Reply) => void
    hub({
      'GET /v1/hub/session': () => (++gets === 1
        ? { status: 200, body: SESSION_BODY }
        : new Promise<Reply>((resolve) => { releaseSession = resolve })),
      'POST /v1/hub/session/read-token': () => (++mints === 2
        ? { status: 503 }
        : { status: 201, body: { token: String(mints).repeat(64), expires_at: NOW + 1800 } }),
    })
    render(<Probe />)
    await screen.findByTestId('token')
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(releaseSession).toBeTypeOf('function'))
    act(() => screen.getByText('confirmed elsewhere').click())
    await act(async () => releaseSession({ status }))
    expect(screen.getByTestId('kind').textContent).toBe(status === 401 ? 'none' : 'unavailable')
    expect(mints).toBe(2)
  })

  it.each(['delete-first', 'renew-first'] as const)('keeps a failed menu DELETE visible across an unknown recheck (%s)', async (order) => {
    let mints = 0
    let gets = 0
    let deletes = 0
    let releaseDelete!: (reply: Reply) => void
    let releaseCheck!: (reply: Reply) => void
    hub({
      'GET /v1/hub/session': () => (++gets === 1 ? { status: 200, body: SESSION_BODY } : new Promise<Reply>((resolve) => { releaseCheck = resolve })),
      'POST /v1/hub/session/read-token': () => (++mints === 2 ? { status: 503 } : { status: 201, body: { token: 'a'.repeat(64), expires_at: NOW + 1800 } }),
      'DELETE /v1/hub/session': () => (++deletes === 1 ? new Promise<Reply>((resolve) => { releaseDelete = resolve }) : { status: 204 }),
    })
    render(<Probe />)
    await screen.findByTestId('token')
    act(() => screen.getByText('sign out').click())
    await waitFor(() => expect(releaseDelete).toBeTypeOf('function'))
    if (order === 'delete-first') await act(async () => releaseDelete({ status: 503 }))
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(releaseCheck).toBeTypeOf('function'))
    if (order === 'renew-first') await act(async () => releaseDelete({ status: 503 }))
    expect(screen.getByTestId('kind').textContent).toBe('checking')
    expect(screen.getByTestId('sign-out-error').textContent).toContain('sign-out failed')
    await act(async () => releaseCheck({ status: 503 }))
    expect(screen.getByTestId('kind').textContent).toBe('unavailable')
    expect(screen.getByTestId('sign-out-error').textContent).toContain('sign-out failed')
    act(() => screen.getByText('retry check').click())
    await waitFor(() => expect(gets).toBe(3))
    await act(async () => releaseCheck({ status: 200, body: SESSION_BODY }))
    await waitFor(() => expect(screen.getByTestId('kind').textContent).toBe('ready'))
    expect(screen.getByTestId('sign-out-error').textContent).toContain('sign-out failed')
    await act(async () => screen.getByText('sign out').click())
    expect(screen.getByTestId('kind').textContent).toBe('none')
    expect(deletes).toBe(2)
  })

  it('shows a Computers DELETE failure after its page unmounts, then restores only a freshly identified session', async () => {
    let mints = 0
    let gets = 0
    let releaseCheck!: (reply: Reply) => void
    hub({
      'GET /v1/hub/session': () => (++gets === 1 ? { status: 200, body: SESSION_BODY } : new Promise<Reply>((resolve) => { releaseCheck = resolve })),
      'POST /v1/hub/session/read-token': () => (++mints === 2 ? { status: 503 } : { status: 201, body: { token: String(mints).repeat(64), expires_at: NOW + 1800 } }),
    })
    render(<Probe />)
    await screen.findByTestId('token')
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(releaseCheck).toBeTypeOf('function'))
    act(() => screen.getByText('failed elsewhere').click())
    expect(screen.getByTestId('sign-out-error').textContent).toContain('sign-out failed')
    await act(async () => releaseCheck({ status: 200, body: SESSION_BODY }))
    await waitFor(() => expect(screen.getByTestId('token').textContent).toBe('3'.repeat(64)))
    expect(screen.getByTestId('sign-out-error').textContent).toContain('sign-out failed')
  })

  it('keeps a failed menu DELETE through a same-session recheck and permits a confirmed retry', async () => {
    let mints = 0
    let deletes = 0
    let releaseCheck!: (reply: Reply) => void
    let gets = 0
    hub({
      'GET /v1/hub/session': () => (++gets === 1 ? { status: 200, body: SESSION_BODY } : new Promise<Reply>((resolve) => { releaseCheck = resolve })),
      'POST /v1/hub/session/read-token': () => (++mints === 2 ? { status: 503 } : { status: 201, body: { token: String(mints).repeat(64), expires_at: NOW + 1800 } }),
      'DELETE /v1/hub/session': () => (++deletes === 1 ? { status: 503 } : { status: 204 }),
    })
    render(<Probe />)
    await screen.findByTestId('token')
    await act(async () => screen.getByText('sign out').click())
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(releaseCheck).toBeTypeOf('function'))
    await act(async () => releaseCheck({ status: 200, body: SESSION_BODY }))
    await waitFor(() => expect(screen.getByTestId('token').textContent).toBe('3'.repeat(64)))
    expect(screen.getByTestId('sign-out-error').textContent).toContain('sign-out failed')
    await act(async () => screen.getByText('sign out').click())
    expect(screen.getByTestId('kind').textContent).toBe('none')
    expect(deletes).toBe(2)
  })

  it('does not attach A’s failed DELETE to identified B when B’s later mint fails', async () => {
    let gets = 0
    let mints = 0
    let releaseSession!: (reply: Reply) => void
    let releaseMint!: (reply: Reply) => void
    hub({
      'GET /v1/hub/session': () => (++gets === 1 ? { status: 200, body: SESSION_BODY } : new Promise<Reply>((resolve) => { releaseSession = resolve })),
      'POST /v1/hub/session/read-token': () => {
        mints++
        if (mints === 2) return { status: 503 }
        if (mints === 3) return new Promise<Reply>((resolve) => { releaseMint = resolve })
        return { status: 201, body: { token: 'a'.repeat(64), expires_at: NOW + 1800 } }
      },
    })
    render(<Probe />)
    await screen.findByTestId('token')
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(releaseSession).toBeTypeOf('function'))
    act(() => screen.getByText('failed elsewhere').click())
    expect(screen.getByTestId('sign-out-error').textContent).toContain('sign-out failed')
    await act(async () => releaseSession({ status: 200, body: { ...SESSION_BODY, csrf_token: SECOND_SESSION.csrf } }))
    await waitFor(() => expect(releaseMint).toBeTypeOf('function'))
    expect(screen.getByTestId('sign-out-error').textContent).toBe('')
    await act(async () => releaseMint({ status: 503 }))
    expect(screen.getByTestId('kind').textContent).toBe('unavailable')
    expect(screen.getByTestId('sign-out-error').textContent).toBe('')
  })

  it.each(['empty', 'refused', 'later-5xx'] as const)('retries DELETE for identified A without a read token after %s repository discovery', async (outcome) => {
    let gets = 0
    let mints = 0
    let deletes = 0
    const calls = hub({
      'GET /v1/hub/session': () => { gets++; return { status: 200, body: SESSION_BODY } },
      'GET /v1/hub/tokens': () => ({ status: 200, body: { tokens: [] } }),
      'GET /v1/hub/devices': () => ({ status: 200, body: { devices: [DEV(gets === 1 ? ['acme/docs'] : outcome === 'empty' ? [] : outcome === 'refused' ? ['acme/secret'] : ['acme/docs'])] } }),
      'POST /v1/hub/session/read-token': () => {
        mints++
        if (mints === 2 || (mints === 3 && outcome === 'later-5xx')) return { status: 503 }
        if (mints === 3 && outcome === 'refused') return { status: 403, body: REFUSED }
        return { status: 201, body: { token: 'a'.repeat(64), expires_at: NOW + 1800 } }
      },
      'DELETE /v1/hub/session': () => ({ status: ++deletes === 1 ? 503 : 204 }),
    })
    render(<Probe />)
    await screen.findByTestId('token')
    await act(async () => screen.getByText('sign out').click())
    expect(screen.getByTestId('sign-out-error').textContent).toContain('sign-out failed')
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(screen.getByTestId('kind').textContent).toBe('unavailable'))
    expect(screen.queryByTestId('token')).toBeNull()
    expect(screen.getByTestId('sign-out-error').textContent).toContain('sign-out failed')
    const mintedBeforeRetry = minted(calls).length
    await act(async () => screen.getByText('retry sign out').click())
    expect(screen.getByTestId('kind').textContent).toBe('none')
    expect(deletes).toBe(2)
    expect(minted(calls)).toHaveLength(mintedBeforeRetry)
  })

  it('can sign out an identified checking session while its read-token mint is still pending', async () => {
    let mints = 0
    let releaseMint!: (reply: Reply) => void
    let deletes = 0
    hub({
      'POST /v1/hub/session/read-token': () => {
        mints++
        if (mints === 2) return { status: 503 }
        if (mints === 3) return new Promise<Reply>((resolve) => { releaseMint = resolve })
        return { status: 201, body: { token: 'a'.repeat(64), expires_at: NOW + 1800 } }
      },
      'DELETE /v1/hub/session': () => { deletes++; return { status: 204 } },
    })
    render(<Probe />)
    await screen.findByTestId('token')
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(releaseMint).toBeTypeOf('function'))
    expect(screen.getByTestId('kind').textContent).toBe('checking')
    await act(async () => screen.getByText('retry sign out').click())
    expect(screen.getByTestId('kind').textContent).toBe('none')
    expect(deletes).toBe(1)
    await act(async () => releaseMint({ status: 201, body: { token: 'b'.repeat(64), expires_at: NOW + 1800 } }))
    expect(screen.getByTestId('kind').textContent).toBe('none')
  })

  it.each([
    ['empty', 'menu', 204], ['empty', 'menu', 503], ['empty', 'computers', 204], ['empty', 'computers', 503],
    ['refused', 'menu', 204], ['refused', 'menu', 503], ['refused', 'computers', 204], ['refused', 'computers', 503],
  ] as const)('handles delayed %s-repository %s sign-out HTTP %s after discovery', async (outcome, caller, status) => {
    let gets = 0
    let mints = 0
    let deletes = 0
    let releaseDelete!: (reply: Reply) => void
    const calls = hub({
      'GET /v1/hub/session': () => { gets++; return { status: 200, body: SESSION_BODY } },
      'GET /v1/hub/tokens': () => ({ status: 200, body: { tokens: [] } }),
      'GET /v1/hub/devices': () => ({ status: 200, body: { devices: [DEV(gets === 1 ? ['acme/docs'] : outcome === 'empty' ? [] : ['acme/secret'])] } }),
      'POST /v1/hub/session/read-token': () => (++mints === 2 ? { status: 503 } : mints === 3 ? { status: 403, body: REFUSED } : { status: 201, body: { token: 'a'.repeat(64), expires_at: NOW + 1800 } }),
      'DELETE /v1/hub/session': () => (++deletes === 1 && caller === 'menu' ? new Promise<Reply>((resolve) => { releaseDelete = resolve }) : { status: 204 }),
    })
    render(<Probe />)
    await screen.findByTestId('token')
    if (caller === 'menu') {
      act(() => screen.getByText('sign out').click())
      await waitFor(() => expect(releaseDelete).toBeTypeOf('function'))
    }
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(screen.getByTestId('unavailable').textContent).toBe(outcome))
    expect(screen.queryByTestId('token')).toBeNull()
    const mintsBeforeReply = minted(calls).length
    if (caller === 'menu') await act(async () => releaseDelete({ status }))
    else act(() => screen.getByText(status === 204 ? 'confirmed elsewhere' : 'failed elsewhere').click())
    if (status === 204) {
      expect(screen.getByTestId('kind').textContent).toBe('none')
      expect(screen.getByTestId('sign-out-error').textContent).toBe('')
    } else {
      expect(screen.getByTestId('unavailable').textContent).toBe(outcome)
      expect(screen.getByTestId('sign-out-error').textContent).toContain('sign-out failed')
      await act(async () => screen.getByText('retry sign out').click())
      if (caller === 'menu') expect(deletes).toBe(2)
      expect(screen.getByTestId('kind').textContent).toBe('none')
    }
    expect(minted(calls)).toHaveLength(mintsBeforeReply)
  })

  it.each([204, 503])('keeps identified B with no repository when older A DELETE returns %s', async (status) => {
    let gets = 0
    let mints = 0
    let releaseA!: (reply: Reply) => void
    hub({
      'GET /v1/hub/session': () => ({ status: 200, body: { ...SESSION_BODY, csrf_token: ++gets === 1 ? SESSION.csrf : SECOND_SESSION.csrf } }),
      'GET /v1/hub/tokens': () => ({ status: 200, body: { tokens: [] } }),
      'GET /v1/hub/devices': () => ({ status: 200, body: { devices: [DEV(gets === 1 ? ['acme/docs'] : [])] } }),
      'POST /v1/hub/session/read-token': () => (++mints === 2 ? { status: 503 } : { status: 201, body: { token: 'a'.repeat(64), expires_at: NOW + 1800 } }),
      'DELETE /v1/hub/session': () => new Promise<Reply>((resolve) => { releaseA = resolve }),
    })
    render(<Probe />)
    await screen.findByTestId('token')
    act(() => screen.getByText('sign out').click())
    await waitFor(() => expect(releaseA).toBeTypeOf('function'))
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(screen.getByTestId('unavailable').textContent).toBe('empty'))
    await act(async () => releaseA({ status }))
    expect(screen.getByTestId('unavailable').textContent).toBe('empty')
    expect(screen.getByTestId('sign-out-error').textContent).toBe('')
  })

  it.each([['empty', 503], ['refused', 204]] as const)('keeps the actual Computers DELETE callback after %s discovery and HTTP %s', async (outcome, status) => {
    let gets = 0
    let mints = 0
    let releaseDelete!: (reply: Reply) => void
    hub({
      'GET /v1/hub/session': () => { gets++; return { status: 200, body: SESSION_BODY } },
      'GET /v1/hub/tokens': () => ({ status: 200, body: { tokens: [] } }),
      'GET /v1/hub/devices': () => ({ status: 200, body: { devices: [DEV(gets === 1 ? ['acme/docs'] : outcome === 'empty' ? [] : ['acme/secret'])] } }),
      'POST /v1/hub/session/read-token': () => (++mints === 2 ? { status: 503 } : mints === 3 ? { status: 403, body: REFUSED } : { status: 201, body: { token: 'a'.repeat(64), expires_at: NOW + 1800 } }),
      'DELETE /v1/hub/session': () => new Promise<Reply>((resolve) => { releaseDelete = resolve }),
    })
    render(<ComputersProbe />)
    const signOut = await screen.findByRole('button', { name: 'Sign out of Hub pages' })
    act(() => signOut.click())
    await waitFor(() => expect(releaseDelete).toBeTypeOf('function'))
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(screen.getByTestId('kind').textContent).toBe('unavailable'))
    await act(async () => releaseDelete({ status }))
    if (status === 204) {
      expect(screen.getByTestId('kind').textContent).toBe('none')
      expect(screen.getByTestId('sign-out-error').textContent).toBe('')
    } else {
      expect(screen.getByTestId('kind').textContent).toBe('unavailable')
      expect(screen.getByTestId('sign-out-error').textContent).toContain('sign-out failed')
    }
  })

  it.each([204, 503])('lets session B sign out while session A DELETE is stalled; late A %s cannot change B', async (lateStatus) => {
    let csrf = SESSION.csrf
    let mints = 0
    let releaseA!: (reply: Reply) => void
    const calls = hub({
      'GET /v1/hub/session': () => ({ status: 200, body: { ...SESSION_BODY, csrf_token: csrf } }),
      'POST /v1/hub/session/read-token': () => (++mints === 2 ? { status: 503 } : { status: 201, body: { token: String(mints).repeat(64), expires_at: NOW + 1800 } }),
      'DELETE /v1/hub/session': (init) => (init.headers as Record<string, string>)['x-csrf-token'] === SESSION.csrf
        ? new Promise<Reply>((resolve) => { releaseA = resolve })
        : { status: 204 },
    })
    render(<Probe />)
    await screen.findByTestId('token')
    act(() => screen.getByText('sign out').click())
    await waitFor(() => expect(releaseA).toBeTypeOf('function'))
    csrf = SECOND_SESSION.csrf
    act(() => screen.getByText('renew').click())
    await waitFor(() => expect(screen.getByTestId('token').textContent).toBe('3'.repeat(64)))
    await act(async () => screen.getByText('sign out').click())
    expect(calls.filter((c) => c.init.method === 'DELETE')).toHaveLength(2)
    expect(screen.getByTestId('kind').textContent).toBe('none')
    await act(async () => releaseA({ status: lateStatus }))
    expect(screen.getByTestId('kind').textContent).toBe('none')
    expect(screen.getByTestId('sign-out-error').textContent).toBe('')
  })
})
