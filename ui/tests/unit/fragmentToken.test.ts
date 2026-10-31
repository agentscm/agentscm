import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { adoptFragmentToken, fragmentLinkRefused } from '../../src/fragmentToken.ts'
import { loadToken, saveToken } from '../../src/storage.ts'

const reply = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })
const stub = (whoami: (auth: string) => Response, demo = true) =>
  vi.stubGlobal('fetch', vi.fn(async (url: string, init?: RequestInit) => (String(url).includes('whoami') ? whoami(JSON.stringify(init?.headers ?? {})) : reply(200, { status: 'ok', demo }))))

beforeEach(() => {
  sessionStorage.clear()
  localStorage.clear()
  window.history.replaceState(null, '', '/#token=synthetic-demo-token')
})
afterEach(() => {
  vi.unstubAllGlobals()
  sessionStorage.clear()
  localStorage.clear()
})

describe('adoptFragmentToken', () => {
  it('strips the fragment, stores a confirmed read-only token and says so', async () => {
    stub(() => reply(200, { token: { read_only: true } }))
    expect(await adoptFragmentToken()).toBe(true)
    expect(window.location.hash).toBe('')
    expect(loadToken()).toBe('synthetic-demo-token')
  })

  it('keeps a rejected token out of storage', async () => {
    stub(() => reply(401, { error: 'unauthorized' }))
    expect(await adoptFragmentToken()).toBe(false)
    expect(loadToken()).toBeNull()
  })

  it('flags a link the hub refuses (401 or 403) so the front door can say so', async () => {
    stub(() => reply(403, { error: 'forbidden' }))
    expect(await adoptFragmentToken()).toBe(false)
    expect(fragmentLinkRefused()).toBe(true)
    stub(() => reply(200, { token: { read_only: true } }))
    window.history.replaceState(null, '', '/#token=synthetic-demo-token')
    await adoptFragmentToken()
    expect(fragmentLinkRefused()).toBe(false)
  })

  describe('on a hub that is not the demo', () => {
    const stale = (auth: string) => (auth.includes('stale-saved') ? reply(401, { error: 'unauthorized' }) : reply(200, { token: { read_only: true } }))

    it('replaces a saved token the hub no longer accepts', async () => {
      saveToken('stale-saved', true)
      stub(stale, false)
      expect(await adoptFragmentToken()).toBe(true)
      expect(loadToken()).toBe('synthetic-demo-token')
      expect(localStorage.length).toBe(0)
    })

    it('keeps a saved token the hub still accepts', async () => {
      saveToken('working-saved', true)
      stub(stale, false)
      expect(await adoptFragmentToken()).toBe(false)
      expect(loadToken()).toBe('working-saved')
      expect(fragmentLinkRefused()).toBe(false)
    })
  })
})
