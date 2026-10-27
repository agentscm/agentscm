import { act, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mintWebReadToken, sessionRepositories, type BrowserSession } from '../../src/devices.ts'
import { firstAllowed, RENEW_BEFORE_SECONDS, tryOrder, useWebRead } from '../../src/webRead.ts'

const SESSION: BrowserSession = { login: 'maria', provider: 'github', admin: false, csrf: 'csrf-token-0123456789' }
const SESSION_BODY = { user: { id: 'usr_1', login: 'maria', provider: 'github', host: 'github.com', user_id: 1 }, admin: false, csrf_token: SESSION.csrf, expires_at: 1_900_000_000 }
const NOW = 1_800_000_000
const DEV = (repositories: string[], revoked: number | null = null) => ({ id: `dev_${'a'.repeat(32)}`, name: 'mbp', created_at: 1, last_used_at: null, revoked_at: revoked, current: false, repositories })
const TOKEN = (repository: string, extra: object = {}) => ({ id: 'tok', name: 'claude', repository, revoked: false, expires_at: NOW + 3600, read_only: false, created_at: 1, ...extra })
const REFUSED = { error: 'insufficient_role', login: 'maria', repository: 'acme/secret', visibility: 'private', has: 'read', needs: 'triage', message: 'maria has Read on private repository acme/secret; ask a repository owner for Triage or higher' }

type Call = { url: string; init: RequestInit }
function stubFetch(routes: Record<string, (init: RequestInit) => { status: number; body?: unknown }>) {
  const calls: Call[] = []
  vi.stubGlobal('fetch', async (url: string, init: RequestInit = {}) => {
    calls.push({ url, init })
    const route = routes[`${init.method ?? 'GET'} ${url}`]
    const { status, body } = route ? route(init) : { status: 404, body: { error: 'not_found' } }
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
      {s.kind === 'ready' && (
        <>
          <output data-testid="repo">{s.read.repository}</output>
          <output data-testid="repos">{s.repos.join(',')}</output>
          <output data-testid="refusal">{s.refusal?.message ?? ''}</output>
          <button onClick={() => web.pick('acme/secret')}>pick secret</button>
          <button onClick={() => web.pick('acme/docs')}>pick docs</button>
          <button onClick={web.renew}>renew</button>
          <output data-testid="restarts">{s.restarts}</output>
        </>
      )}
      {s.kind === 'none' && <output data-testid="none">{s.empty ? 'empty' : (s.refusal?.message ?? 'signed out')}</output>}
    </div>
  )
}

function hub(extra: Record<string, (init: RequestInit) => { status: number; body?: unknown }> = {}) {
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

  it('falls back to the token page when there is no web sign-in, or nothing to open', async () => {
    hub({ 'GET /v1/hub/session': () => ({ status: 401 }) })
    const { unmount } = render(<Probe />)
    expect((await screen.findByTestId('none')).textContent).toBe('signed out')
    unmount()
    hub({ 'GET /v1/hub/tokens': () => ({ status: 200, body: { tokens: [] } }), 'GET /v1/hub/devices': () => ({ status: 200, body: { devices: [] } }) })
    render(<Probe />)
    expect((await screen.findByTestId('none')).textContent).toBe('empty')
  })
})
