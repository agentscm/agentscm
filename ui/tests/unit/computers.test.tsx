import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { decodeDevice, decodeRefusal, deviceAuth, fetchBrowserSession, fetchDevices, revokeDevice, type DeviceAuth } from '../../src/devices.ts'
import { AdminPage, ComputersPage, useDeviceAuth } from '../../src/pages/Computers.tsx'

// As the Shell wires them: one auth read shared by both pages.
function Computers({ token }: { token: string }) {
  const { auth, reload } = useDeviceAuth(token)
  return <ComputersPage auth={auth} provider="github" onSignedOut={reload} />
}
function Admin({ token }: { token: string }) {
  const { auth } = useDeviceAuth(token)
  return <AdminPage auth={auth} provider="github" />
}

const ID = `dev_${'a'.repeat(32)}`
const DEVICE = { id: ID, name: 'jonny-mbp', created_at: 1_790_000_000, last_used_at: 1_790_100_000, revoked_at: null, current: false, repositories: ['counterbranch/agentscm', 'not a repo'] }
const SESSION = { user: { id: 'usr_1', login: 'jhawlwut', provider: 'github' }, admin: true, csrf_token: 'csrf-token-0123456789', expires_at: 1_800_000_000 }

type Call = { url: string; init: RequestInit }
function stubFetch(routes: Record<string, (init: RequestInit) => { status: number; body?: unknown }>) {
  const calls: Call[] = []
  vi.stubGlobal('fetch', async (url: string, init: RequestInit = {}) => {
    calls.push({ url, init })
    const key = `${init.method ?? 'GET'} ${url}`
    const route = routes[key]
    const { status, body } = route ? route(init) : { status: 404, body: { error: 'not_found' } }
    return new Response(status === 204 ? null : JSON.stringify(body ?? {}), { status, headers: { 'content-type': 'application/json' } })
  })
  return calls
}
afterEach(() => vi.unstubAllGlobals())

describe('decoders', () => {
  it('keeps only valid ids, repositories and owners', () => {
    expect(decodeDevice({ ...DEVICE, id: 'dev_short' })).toBeNull()
    expect(decodeDevice(DEVICE)).toMatchObject({ id: ID, repositories: ['counterbranch/agentscm'], owner: null })
    expect(decodeDevice({ ...DEVICE, name: 'evil\u0000\nname', owner: { login: 'maria', provider: 'gitlab' } })).toMatchObject({ name: 'evilname', owner: { login: 'maria', provider: 'gitlab' } })
    expect(decodeRefusal({ at: 1, count: 0, provider: 'github', login: 'b', reason: 'not_allowed', via: 'x' })).toEqual({ at: 1, count: 1, provider: 'github', login: 'b', reason: 'not_allowed', via: 'sign_in' })
    // #162: refusals while the Hub's pages asked for a read token.
    expect(decodeRefusal({ at: 1, count: 1, provider: 'github', login: 'b', reason: 'insufficient_role', via: 'web_read_token' })?.via).toBe('web_read_token')
  })
})

describe('auth and requests', () => {
  it('uses the local page grant as a header and sends no cookies', async () => {
    const calls = stubFetch({ 'GET /v1/hub/devices': () => ({ status: 200, body: { devices: [DEVICE] } }) })
    const auth = await deviceAuth('local:abc:counterbranch/agentscm')
    expect(auth).toEqual({ kind: 'local', token: 'local:abc:counterbranch/agentscm' })
    await fetchDevices(auth!)
    expect(calls[0]!.init.credentials).toBe('omit')
    expect((calls[0]!.init.headers as Record<string, string>)['Authorization']).toBe('Bearer local:abc:counterbranch/agentscm')
  })

  it('uses the browser session cookie with CSRF on revoke, and never sends a read-only token', async () => {
    const calls = stubFetch({ 'GET /v1/hub/session': () => ({ status: 200, body: SESSION }), [`DELETE /v1/hub/devices/${ID}`]: () => ({ status: 204 }) })
    const auth = (await deviceAuth('read-only-token'))!
    expect(auth.kind).toBe('browser')
    expect(await revokeDevice(auth, ID)).toBe('revoked')
    const del = calls.find((c) => c.init.method === 'DELETE')!
    expect(del.init.credentials).toBe('same-origin')
    expect((del.init.headers as Record<string, string>)['x-csrf-token']).toBe(SESSION.csrf_token)
    expect(calls.every((c) => !JSON.stringify(c.init.headers ?? {}).includes('read-only-token'))).toBe(true)
  })

  it('reads the optional profile name as bounded text and drops anything else', async () => {
    const withUser = (name: unknown) => stubFetch({ 'GET /v1/hub/session': () => ({ status: 200, body: { ...SESSION, user: { ...SESSION.user, name } } }) })
    withUser(' Maria Ruiz ')
    expect((await fetchBrowserSession())?.name).toBe('Maria Ruiz')
    for (const bad of [null, '', '   ', 42, 'x'.repeat(101)]) {
      withUser(bad)
      expect((await fetchBrowserSession())?.name).toBeUndefined()
    }
  })

  it('has no way in on the hosted Hub without a browser session', async () => {
    stubFetch({ 'GET /v1/hub/session': () => ({ status: 401, body: { error: 'sign_in_required' } }) })
    expect(await deviceAuth('read-only-token')).toBeNull()
  })

  it('treats an unknown computer as already gone', async () => {
    stubFetch({})
    const auth: DeviceAuth = { kind: 'local', token: 'local:x:o/r' }
    expect(await revokeDevice(auth, ID)).toBe('gone')
    await expect(revokeDevice(auth, '../admin')).rejects.toThrow()
  })
})

describe('ComputersPage', () => {
  it('lists computers and revokes one after a confirm', async () => {
    let revoked = false
    stubFetch({
      'GET /v1/hub/session': () => ({ status: 200, body: SESSION }),
      'GET /v1/hub/devices': () => ({ status: 200, body: { devices: [{ ...DEVICE, revoked_at: revoked ? 1_790_200_000 : null }] } }),
      [`DELETE /v1/hub/devices/${ID}`]: () => {
        revoked = true
        return { status: 204 }
      },
    })
    render(<Computers token="read-only-token" />)
    fireEvent.click(await screen.findByRole('button', { name: 'Revoke' }))
    expect(screen.getByRole('group', { name: 'Revoke jonny-mbp' }).textContent).toContain('agent tokens it made for counterbranch/agentscm. Those agents lose access to this Hub. It does not stop them running, change their GitHub or GitLab access, or release files they announced; those expire on their own.')
    fireEvent.click(screen.getByRole('button', { name: 'Revoke jonny-mbp' }))
    expect(await screen.findByText(/jonny-mbp was revoked/)).toBeTruthy()
    await waitFor(() => expect(screen.getByText('No computers are signed in')).toBeTruthy())
  })

  it('offers a forge sign-in when this browser has no Hub session', async () => {
    stubFetch({ 'GET /v1/hub/session': () => ({ status: 401, body: { error: 'sign_in_required' } }) })
    render(<Computers token="read-only-token" />)
    expect((await screen.findByRole('link', { name: /Sign in with GitHub/ })).getAttribute('href')).toBe('/v1/hub/login/github?web=1')
  })

  it('signs the browser out with CSRF, then offers sign-in again', async () => {
    let signedIn = true
    const calls = stubFetch({
      'GET /v1/hub/session': () => (signedIn ? { status: 200, body: SESSION } : { status: 401 }),
      'GET /v1/hub/devices': () => ({ status: 200, body: { devices: [DEVICE] } }),
      'DELETE /v1/hub/session': () => {
        signedIn = false
        return { status: 204 }
      },
    })
    render(<Computers token="read-only-token" />)
    expect(await screen.findByText('Workspace access and computers stay signed in.')).toBeTruthy()
    fireEvent.click(await screen.findByRole('button', { name: 'Sign out of Hub pages' }))
    expect(await screen.findByRole('link', { name: /Sign in with GitHub/ })).toBeTruthy()
    const del = calls.find((c) => c.init.method === 'DELETE')!
    expect((del.init.headers as Record<string, string>)['x-csrf-token']).toBe(SESSION.csrf_token)
  })

  it('names the Hub the local page read from', async () => {
    stubFetch({ 'GET /v1/hub/devices': () => ({ status: 200, body: { hub: 'https://dogfood.agentscm.dev', devices: [DEVICE] } }) })
    render(<Computers token="local:abc:o/r" />)
    expect((await screen.findByText(/can start agents as you on/)).textContent).toContain('dogfood.agentscm.dev')
  })

  it('does not offer to revoke the computer the page runs on', async () => {
    stubFetch({ 'GET /v1/hub/devices': () => ({ status: 200, body: { devices: [{ ...DEVICE, current: true }] } }) })
    render(<Computers token="local:abc:o/r" />)
    expect(await screen.findByText('This computer')).toBeTruthy()
    expect(screen.queryByRole('button', { name: 'Revoke' })).toBeNull()
  })
})

describe('AdminPage', () => {
  it('shows refusals in the shared words, connected repositories and every computer', async () => {
    stubFetch({
      'GET /v1/hub/session': () => ({ status: 200, body: SESSION }),
      'GET /v1/hub/admin/repositories': () => ({ status: 200, body: { repositories: [{ full_name: 'counterbranch/agentscm', provider: 'github', host: 'github.com', connection: 'con_1', devices: 2, agent_tokens: 3 }] } }),
      'GET /v1/hub/admin/sign-in-refusals': () => ({ status: 200, body: { refusals: [{ at: 1_790_000_000, count: 3, provider: 'github', login: 'brandonfla', forge_user_id: 1, reason: 'insufficient_role', via: 'device_mint' }] } }),
      'GET /v1/hub/admin/devices': () => ({ status: 200, body: { devices: [{ ...DEVICE, owner: { id: 'usr_2', login: 'maria', provider: 'github' } }] } }),
    })
    render(<Admin token="read-only-token" />)
    expect(await screen.findByText('brandonfla')).toBeTruthy()
    expect(screen.getByText(/role on the repositories connected to this Hub is too low/)).toBeTruthy()
    expect(screen.getByText('3×')).toBeTruthy()
    expect(screen.getByText('counterbranch/agentscm', { selector: 'td code' })).toBeTruthy()
    expect(screen.getByText('maria')).toBeTruthy()
  })

  it('stays closed to people who aren’t Hub admins', async () => {
    const calls = stubFetch({ 'GET /v1/hub/session': () => ({ status: 200, body: { ...SESSION, admin: false } }) })
    render(<Admin token="read-only-token" />)
    expect(await screen.findByText('Only Hub admins see this page')).toBeTruthy()
    expect(calls.some((c) => c.url.includes('/admin/'))).toBe(false)
  })
})
