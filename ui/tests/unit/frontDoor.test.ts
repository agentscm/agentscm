import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { adoptFrontDoorLink, disconnectComputer, isLocalCredential, localRepository, onFrontDoor, switchRepository, takeConnectOutcome } from '../../src/frontDoor.ts'
import { localCredential } from '../../src/localApi.ts'
import { loadToken, saveToken, signInAfterRefusal } from '../../src/storage.ts'

const CODE = 'c'.repeat(64)
const GRANT = 'a'.repeat(64)
const INSTANCE = '1'.repeat(32)
const reply = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })

type Call = { url: string; init: RequestInit | undefined }
function stub(handler: (url: string, init?: RequestInit) => Response): Call[] {
  const calls: Call[] = []
  vi.stubGlobal('fetch', vi.fn(async (url: string, init?: RequestInit) => {
    calls.push({ url: String(url), init })
    return handler(String(url), init)
  }))
  return calls
}
const auth = (call: Call) => new Headers(call.init?.headers).get('authorization')
const frontDoor = (redeem: () => Response) => stub((url) => (url === '/v1/local/identity' ? reply(200, { version: 1, instance: INSTANCE }) : url === '/v1/local/pairings/redeem' ? redeem() : reply(404, {})))

beforeEach(() => {
  sessionStorage.clear()
  localStorage.clear()
  window.history.replaceState(null, '', `/ui/#pair=${CODE}&repo=acme/widgets`)
})
afterEach(() => {
  vi.unstubAllGlobals()
  sessionStorage.clear()
  localStorage.clear()
})

describe('this computer’s Hub page', () => {
  it('leaves history before the code is sent, redeems it once and keeps one credential', async () => {
    const calls = frontDoor(() => reply(200, { version: 1, grant: GRANT, expires_at: 1, instance: INSTANCE, computer: 'synthetic' }))
    expect(await adoptFrontDoorLink()).toBe(true)
    expect(onFrontDoor()).toBe(true)
    expect(window.location.hash).toBe('#explorer')
    expect(window.location.href).not.toContain(CODE)
    const redeem = calls.find((c) => c.url === '/v1/local/pairings/redeem')!
    expect(redeem.init?.method).toBe('POST')
    expect(redeem.init?.credentials).toBe('omit')
    expect(auth(redeem)).toBe(`Bearer ${CODE}`)
    const token = loadToken()!
    expect(token).toBe(`local:${GRANT}:acme/widgets`)
    expect(localStorage.getItem('agentscm.token')).toBe(token)
    expect(isLocalCredential(token) && localRepository(token)).toBe('acme/widgets')
    expect(localCredential()).toBe(token)
    expect(takeConnectOutcome()).toBe('connected')
  })

  it('reports a used link and keeps the existing connection', async () => {
    saveToken(`local:${GRANT}:acme/widgets`, true)
    frontDoor(() => reply(401, { error: 'code_invalid' }))
    expect(await adoptFrontDoorLink()).toBe(false)
    expect(loadToken()).toBe(`local:${GRANT}:acme/widgets`)
    expect(takeConnectOutcome()).toBe('used')
  })

  it('never redeems on another site, or with a malformed repository', async () => {
    const calls = stub(() => reply(404, { error: 'not_found' }))
    expect(await adoptFrontDoorLink()).toBe(false)
    expect(onFrontDoor()).toBe(false)
    expect(calls.some((c) => c.url.includes('redeem'))).toBe(false)
    expect(window.location.href).not.toContain(CODE)

    window.history.replaceState(null, '', `/ui/#pair=${CODE}&repo=../../etc`)
    const again = frontDoor(() => reply(200, { grant: GRANT }))
    expect(await adoptFrontDoorLink()).toBe(false)
    expect(again.some((c) => c.url.includes('redeem'))).toBe(false)
  })

  it('switches repository with the same connection and disconnects it', async () => {
    saveToken(`local:${GRANT}:acme/widgets`, true)
    expect(switchRepository(loadToken()!, 'acme/gadgets')).toBe(`local:${GRANT}:acme/gadgets`)
    expect(loadToken()).toBe(`local:${GRANT}:acme/gadgets`)
    expect(switchRepository(loadToken()!, 'acme/gadgets@https://hub.example')).toBe(`local:${GRANT}:acme/gadgets@https://hub.example`)
    expect(localRepository(loadToken()!)).toBe('acme/gadgets')
    expect(switchRepository(loadToken()!, 'not a repo')).toBeNull()
    expect(switchRepository('a-hub-token', 'acme/gadgets')).toBeNull()

    const calls = stub(() => new Response(null, { status: 204 }))
    await disconnectComputer()
    expect(calls[0]!.init?.method).toBe('DELETE')
    expect(auth(calls[0]!)).toBe(`Bearer local:${GRANT}:acme/gadgets@https://hub.example`)
    expect(loadToken()).toBeNull()
  })

  it('sends no local credential on the hosted Hub', () => {
    saveToken('ordinary-read-only-hub-token', false)
    expect(localCredential()).toBe('')
  })

  it('a stale tab adopts a newer connection instead of erasing it', () => {
    expect(signInAfterRefusal('local:old', 'local:new')).toBe('local:new')
    expect(signInAfterRefusal('local:old', 'local:old')).toBeNull()
    expect(signInAfterRefusal('local:old', null)).toBeNull()
  })
})
