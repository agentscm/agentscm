// A stubbed fetch that answers from ui/dev/mock.ts, the same synthetic hub `MOCK=1 npm run dev` uses,
// so fixture shapes are the ones the shared decoders already accept. Overrides add the states the mock
// does not produce (sign-in providers, sign-out, error statuses, malformed or oversized bodies).
import type { IncomingMessage, ServerResponse } from 'node:http'
import { vi } from 'vitest'
import { mockHub } from '../../dev/mock.ts'

type Handler = (req: IncomingMessage, res: ServerResponse) => void
export interface Sent {
  method: string
  url: string
  headers: Record<string, string>
  body?: Record<string, unknown>
}
/** Return a Response to answer a request yourself, or undefined to let the mock answer it. */
export type Override = (url: URL, init: RequestInit) => Response | Promise<Response> | undefined

export const json = (status: number, body: unknown, headers: Record<string, string> = {}) =>
  new Response(status === 204 ? null : JSON.stringify(body), { status, headers: { 'content-type': 'application/json', ...headers } })

function mockHandler(): Handler {
  let hub: Handler | undefined
  const plugin = mockHub('full')
  const configure = plugin.configureServer as unknown as (server: { middlewares: { use: (path: string, fn: Handler) => void } }) => void
  configure({ middlewares: { use: (path, fn) => void (path === '/v1/hub' && (hub = fn)) } })
  if (!hub) throw new Error('mock hub did not register /v1/hub')
  return hub
}

/** Rewrites one JSON response from the mock (for states it cannot produce on its own). */
export async function fromMock(url: URL, init: RequestInit, edit: (body: Record<string, unknown>) => unknown): Promise<Response> {
  const res = await answer(url, init)
  return json(res.status, edit((await res.json()) as Record<string, unknown>))
}

const handler = mockHandler()
function answer(url: URL, init: RequestInit): Promise<Response> {
  return new Promise((resolve) => {
    const headers: Record<string, string> = {}
    const req = { url: url.pathname.replace(/^\/v1\/hub/, '') + url.search, method: init.method ?? 'GET', headers: { authorization: new Headers(init.headers).get('authorization') ?? undefined } }
    const res = {
      statusCode: 200,
      setHeader: (k: string, v: string) => void (headers[k] = v),
      end: (body: string) => resolve(new Response(body, { status: res.statusCode, headers })),
    }
    handler(req as unknown as IncomingMessage, res as unknown as ServerResponse)
  })
}

/** Installs the stub as global fetch for this test; returns the list of requests it saw. */
export function stubHub(...overrides: Override[]): Sent[] {
  const sent: Sent[] = []
  vi.stubGlobal('fetch', async (input: RequestInfo | URL, init: RequestInit = {}) => {
    const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url, 'http://127.0.0.1:5184')
    sent.push({
      method: (init.method ?? 'GET').toUpperCase(),
      url: url.pathname + url.search,
      headers: Object.fromEntries(new Headers(init.headers).entries()),
      ...(typeof init.body === 'string' ? { body: JSON.parse(init.body) as Record<string, unknown> } : {}),
    })
    for (const o of overrides) {
      const r = await o(url, init)
      if (r) return r
    }
    return answer(url, init)
  })
  return sent
}
