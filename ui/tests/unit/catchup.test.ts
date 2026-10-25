import { describe, expect, it, vi } from 'vitest'
import {
  acquireCatchupDeliveryClient,
  appendCatchupPage,
  catchupPath,
  decodeCatchupPage,
  newCatchupDeliveryClient,
  releaseCatchupDeliveryClient,
} from '../../src/catchup.ts'

const installWebLocks = () => {
  const held = new Set<string>()
  Object.defineProperty(navigator, 'locks', {
    configurable: true,
    value: {
      request: async (name: string, _options: LockOptions, callback: (lock: Lock | null) => unknown) => {
        if (held.has(name)) return callback(null)
        held.add(name)
        try {
          return await callback({ name, mode: 'exclusive' })
        } finally {
          held.delete(name)
        }
      },
    },
  })
}

const delivery = (digit = 'a') => `cup_${digit.repeat(32)}`
const merge = (over: Record<string, unknown> = {}) => ({
  id: 'merge-observation-1',
  seq: 1,
  source: 'forge_pull',
  source_id: 'github:128',
  occurred_at: 1_798_000_000,
  observed_at: 1_798_000_010,
  title: '<img src=x onerror=alert(1)>',
  branch: 'feat/catchup',
  base: 'main',
  head_commit: '1'.repeat(40),
  merge_commit: '2'.repeat(40),
  pull_number: 128,
  paths: ['src/hub/catchup.rs', 'ui/src/catchup.ts'],
  paths_complete: true,
  ...over,
})
const page = (over: Record<string, unknown> = {}) => ({
  subscriber: 'human',
  delivery_client: 'hub_test-client',
  delivery: delivery(),
  event_from: 3,
  event_through: 3,
  event_after: 3,
  next_event_after: 3,
  merge_from: 0,
  merge_through: 1,
  merge_after: 0,
  next_merge_after: 1,
  work_events: [],
  coordination_events: [],
  merges: [merge()],
  retention: { work_gap: false, coordination_gap: false, merge_gap: false, event_available_from: 0, merge_available_from: 0 },
  source_coverage: [
    { source: 'tracked_publication', state: 'observed_only', checked_at: 1_798_000_010, unread_pages: 0, deferred: 0 },
    { source: 'forge_pull', state: 'observed_only', checked_at: 1_798_000_010, unread_pages: 0, deferred: 0 },
  ],
  complete: true,
  ...over,
})

describe('catch-up delivery decoder', () => {
  it('accepts fixed-watermark merge observations and preserves text as display data', () => {
    expect(decodeCatchupPage(page())).toMatchObject({
      subscriber: 'human',
      delivery_client: 'hub_test-client',
      complete: true,
      merges: [{ title: '<img src=x onerror=alert(1)>', source: 'forge_pull', seq: 1 }],
    })
  })

  it('accepts a server-sanitized family title and rejects raw invisible controls', () => {
    const sanitizedTitle = 'Family 👨👩👧👦'
    expect(decodeCatchupPage(page({ merges: [merge({ title: sanitizedTitle })] })).merges[0]?.title).toBe(sanitizedTitle)
    expect(() => decodeCatchupPage(page({ merges: [merge({ title: 'Family 👨‍👩‍👧‍👦' })] }))).toThrow(/Invalid catch-up response/)
    expect(() => decodeCatchupPage(page({ merges: [merge({ title: 'Family \u202ereversed' })] }))).toThrow(/Invalid catch-up response/)
  })

  it.each([
    ['invalid delivery', page({ delivery: '../../settings' })],
    ['missing delivery client', page({ delivery_client: undefined })],
    ['claimed complete before cursor reaches watermark', page({ next_merge_after: 0 })],
    ['row beyond page cursor', page({ next_merge_after: 0, complete: false })],
    ['unsafe commit', page({ merges: [merge({ merge_commit: 'javascript:alert(1)' })] })],
    ['duplicate paths', page({ merges: [merge({ paths: ['src/lib.rs', 'src/lib.rs'] })] })],
  ])('rejects %s', (_name, value) => {
    expect(() => decodeCatchupPage(value)).toThrow(/Invalid catch-up response/)
  })

  it('requires every fixed watermark and cursor in a continuation URL', () => {
    expect(catchupPath()).toBe('/v1/hub/standup/catchup?subscriber=human&limit=20')
    expect(() => catchupPath({ delivery: delivery() })).toThrow(/every cursor/)
    const path = catchupPath({ deliveryClient: 'hub_tab-a', delivery: delivery(), eventAfter: 3, eventThrough: 4, mergeAfter: 1, mergeThrough: 2 })
    expect(path).toContain('delivery_client=hub_tab-a')
    expect(path).toContain('event_through=4')
  })

  it('creates bounded opaque delivery clients', () => {
    const first = newCatchupDeliveryClient()
    const second = newCatchupDeliveryClient()
    expect(first).toMatch(/^hub_[A-Za-z0-9_-]+$/)
    expect(second).not.toBe(first)
    expect(first.length).toBeLessThanOrEqual(200)
  })

  it('persists eight independently locked slots and falls back safely when storage is unavailable', async () => {
    sessionStorage.clear()
    installWebLocks()
    const leases = []
    for (let index = 0; index < 9; index++) leases.push(await acquireCatchupDeliveryClient())
    expect(new Set(leases.map((lease) => lease.client)).size).toBe(9)
    expect(leases.filter((lease) => lease.slot !== null)).toHaveLength(8)
    expect(leases[8]?.slot).toBeNull()
    for (const lease of leases) releaseCatchupDeliveryClient(lease)
    await Promise.resolve()

    const first = await acquireCatchupDeliveryClient()
    releaseCatchupDeliveryClient(first)
    await Promise.resolve()
    const remounted = await acquireCatchupDeliveryClient()
    expect(remounted.client).toBe(first.client)
    releaseCatchupDeliveryClient(remounted)

    const getItem = vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => { throw new Error('storage denied') })
    const fallback = await acquireCatchupDeliveryClient()
    expect(fallback.slot).toBeNull()
    expect(fallback.client).toMatch(/^hub_/)
    releaseCatchupDeliveryClient(fallback)
    getItem.mockRestore()

    Reflect.deleteProperty(navigator, 'locks')
    sessionStorage.clear()
    const unsupported = await acquireCatchupDeliveryClient()
    expect(unsupported.slot).toBeNull()
    expect(sessionStorage.length).toBe(0)
  })

  it('appends only the exact next page of the same delivery', () => {
    const first = decodeCatchupPage(page({
      event_through: 4,
      next_event_after: 3,
      merge_through: 2,
      next_merge_after: 1,
      complete: false,
    }))
    const next = decodeCatchupPage(page({
      event_through: 4,
      event_after: 3,
      next_event_after: 4,
      merge_through: 2,
      merge_after: 1,
      next_merge_after: 2,
      merges: [merge({ id: 'merge-observation-2', seq: 2 })],
    }))
    expect(appendCatchupPage(first, next)).toMatchObject({ complete: true, merge_after: 0, merges: [{ seq: 1 }, { seq: 2 }] })
    expect(() => appendCatchupPage(first, { ...next, delivery: delivery('b') })).toThrow(/continuation/)
    expect(() => appendCatchupPage(first, { ...next, delivery_client: 'hub_other-tab' })).toThrow(/continuation/)
    const refreshedCoverage = next.source_coverage.map((source) => ({ ...source, checked_at: 200, state: 'unavailable' as const }))
    expect(appendCatchupPage(first, { ...next, source_coverage: refreshedCoverage }).source_coverage).toEqual(refreshedCoverage)
    expect(() => appendCatchupPage(first, { ...next, retention: { ...next.retention, work_gap: true } })).toThrow(/continuation/)
  })
})
