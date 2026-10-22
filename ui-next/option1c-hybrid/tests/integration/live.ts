// Points the browser data layer's relative `/v1/...` fetches at the live hub and records every request.
import { afterAll, beforeAll } from 'vitest'
import { HUB } from '../hub.ts'

export const requests: { method: string; url: string }[] = []

const realFetch = globalThis.fetch
beforeAll(() => {
  globalThis.fetch = ((input: RequestInfo | URL, init?: RequestInit) => {
    const raw = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
    const url = raw.startsWith('/') ? `${HUB}${raw}` : raw
    requests.push({ method: (init?.method ?? 'GET').toUpperCase(), url })
    // jsdom's AbortSignal is not the one Node's fetch accepts; forward aborts onto a native one.
    const ctrl = new AbortController()
    init?.signal?.addEventListener('abort', () => ctrl.abort())
    return realFetch(url, { ...init, signal: ctrl.signal })
  }) as typeof fetch
})
afterAll(() => {
  globalThis.fetch = realFetch
})
