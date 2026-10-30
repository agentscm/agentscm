// First sign-in with no remembered repository prefers one with unfinished work (webRead.firstAllowed probing).
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { BrowserSession } from '../../src/devices.ts'
import { fetchOverview, type WorkOverview } from '../../src/standup.ts'
import { firstAllowed, PROBE_LIMIT } from '../../src/webRead.ts'

vi.mock('../../src/standup.ts', async (orig) => ({ ...(await orig<typeof import('../../src/standup.ts')>()), fetchOverview: vi.fn() }))

const SESSION: BrowserSession = { login: 'maria', provider: 'github', admin: false, csrf: 'csrf-token-0123456789' }
const overview = (busy: boolean) => ({ current: busy ? [{}] : [] }) as unknown as WorkOverview
/** Repositories in `busy` have unfinished work; the token minted for a repository is `t:<repo>`. */
function hub(busy: string[], failOverview: string[] = []) {
  const minted: string[] = []
  vi.stubGlobal('fetch', async (_url: string, init: RequestInit) => {
    const repository = JSON.parse(init.body as string).repository as string
    minted.push(repository)
    return new Response(JSON.stringify({ token: 't'.padEnd(63, 'x') + repository.length, expires_at: 1_900_000_000 }), { status: 201, headers: { 'content-type': 'application/json' } })
  })
  const probed: string[] = []
  vi.mocked(fetchOverview).mockImplementation(async () => {
    const repo = minted[minted.length - 1]!
    probed.push(repo)
    if (failOverview.includes(repo)) throw new Error('down')
    return overview(busy.includes(repo))
  })
  return { minted, probed }
}

beforeEach(() => {
  vi.mocked(fetchOverview).mockReset()
})

describe('firstAllowed probing', () => {
  it('opens the first repository with unfinished work, reusing its minted token', async () => {
    const h = hub(['acme/widgets'])
    const r = await firstAllowed(SESSION, ['acme/billing-docs', 'acme/widgets', 'acme/zed'], undefined, true)
    expect(r.read?.repository).toBe('acme/widgets')
    expect(h.minted).toEqual(['acme/billing-docs', 'acme/widgets'])
  })

  it('opens the first allowed repository when none has work, or when the overview fails', async () => {
    hub([])
    expect((await firstAllowed(SESSION, ['acme/a', 'acme/b'], undefined, true)).read?.repository).toBe('acme/a')
    hub(['acme/b'], ['acme/a', 'acme/b'])
    expect((await firstAllowed(SESSION, ['acme/a', 'acme/b'], undefined, true)).read?.repository).toBe('acme/a')
  })

  it('does not probe without the flag (a remembered pick)', async () => {
    const h = hub(['acme/b'])
    expect((await firstAllowed(SESSION, ['acme/a', 'acme/b'])).read?.repository).toBe('acme/a')
    expect(h.minted).toEqual(['acme/a'])
    expect(h.probed).toEqual([])
  })

  it(`stops after ${PROBE_LIMIT} allowed repositories`, async () => {
    const h = hub(['acme/r6'])
    const order = ['acme/r1', 'acme/r2', 'acme/r3', 'acme/r4', 'acme/r5', 'acme/r6', 'acme/r7']
    expect((await firstAllowed(SESSION, order, undefined, true)).read?.repository).toBe('acme/r1')
    expect(h.minted).toHaveLength(PROBE_LIMIT)
    expect(h.probed).toHaveLength(PROBE_LIMIT)
  })

  it('stops probing when aborted', async () => {
    hub([], ['acme/a'])
    const ctl = new AbortController()
    ctl.abort()
    await expect(firstAllowed(SESSION, ['acme/a', 'acme/b'], ctl.signal, true)).rejects.toThrow()
  })
})
