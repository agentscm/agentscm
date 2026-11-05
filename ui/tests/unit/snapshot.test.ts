import { afterEach, describe, expect, it, vi } from 'vitest'
import { fetchSnapshot } from '../../src/api.ts'

const signal = () => new AbortController().signal
const body = { work: { intents: [], connected: true, pull_requests: [] }, activity: { sessions: [], events: [], next_before: null } }

describe('fetchSnapshot', () => {
  afterEach(() => vi.unstubAllGlobals())

  it('sends the previous ETag and treats 304 as "nothing changed"', async () => {
    const seen: (string | null)[] = []
    vi.stubGlobal('fetch', async (_url: string, init: RequestInit) => {
      const tag = new Headers(init.headers).get('if-none-match')
      seen.push(tag)
      return tag === '"v1"' ? new Response(null, { status: 304 }) : new Response(JSON.stringify(body), { status: 200, headers: { etag: '"v1"' } })
    })
    const first = await fetchSnapshot('acme/widgets', 'tok', signal(), null)
    expect(first?.etag).toBe('"v1"')
    expect(first?.work.connected).toBe(true)
    expect(first?.activity.events).toEqual([])
    expect(await fetchSnapshot('acme/widgets', 'tok', signal(), first!.etag)).toBeNull()
    expect(seen).toEqual([null, '"v1"'])
  })

  it('reports a refusal as an ApiError', async () => {
    vi.stubGlobal('fetch', async () => new Response(JSON.stringify({ error: 'unauthorized' }), { status: 401 }))
    await expect(fetchSnapshot('acme/widgets', 'tok', signal(), null)).rejects.toMatchObject({ status: 401 })
  })
})
