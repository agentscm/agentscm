import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { adoptFragmentToken } from '../../src/fragmentToken.ts'
import { loadToken } from '../../src/storage.ts'

const reply = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })
const stub = (whoami: () => Response) =>
  vi.stubGlobal('fetch', vi.fn(async (url: string) => (String(url).includes('whoami') ? whoami() : reply(200, { status: 'ok', demo: true }))))

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
})
