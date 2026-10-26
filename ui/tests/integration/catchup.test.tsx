import { StrictMode } from 'react'
import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { CatchupPanel } from '../../src/Catchup.tsx'
import { DashboardPage } from '../../src/pages/Dashboard.tsx'
import type { WorkOverview } from '../../src/standup.ts'

const delivery = (digit = 'a') => `cup_${digit.repeat(32)}`
const page = (over: Record<string, unknown> = {}) => ({
  subscriber: 'human',
  delivery_client: 'hub_test-client',
  delivery: delivery(),
  event_from: 3,
  event_through: 3,
  event_after: 3,
  next_event_after: 3,
  merge_from: 1,
  merge_through: 1,
  merge_after: 1,
  next_merge_after: 1,
  work_events: [],
  coordination_events: [],
  merges: [],
  retention: { work_gap: false, coordination_gap: false, merge_gap: false, event_available_from: 0, merge_available_from: 0 },
  source_coverage: [
    { source: 'tracked_publication', state: 'observed_only', checked_at: 1_798_000_010, unread_pages: 0, deferred: 0 },
    { source: 'forge_pull', state: 'observed_only', checked_at: 1_798_000_010, unread_pages: 0, deferred: 0 },
  ],
  complete: true,
  ...over,
})
const response = (value: unknown) => new Response(JSON.stringify(value), { status: 200, headers: { 'content-type': 'application/json' } })
const narrative = () => ({
  id: 'briefing-test',
  digest: 'a'.repeat(64),
  status: 'pending',
  generated_at: null,
  generated_by: null,
  window: { event_from: 3, event_through: 3, merge_from: 1, merge_through: 1, subscriber: 'human', complete: true },
  sources: [],
  warnings: [],
  output: null,
})
const isNarrative = (input: RequestInfo | URL) => new URL(String(input), 'http://hub.test').pathname === '/v1/hub/standup/narrative'
const catchupCalls = (mock: { mock: { calls: Parameters<typeof fetch>[] } }) => mock.mock.calls.filter(([input]) => !isNarrative(input))
const mockCatchup = (...results: Array<Response | Promise<Response> | Error>) => {
  let index = 0
  return vi.spyOn(globalThis, 'fetch').mockImplementation((input) => {
    if (isNarrative(input)) return Promise.resolve(response(narrative()))
    const result = results[index++]
    if (result === undefined) return Promise.reject(new Error('Unexpected catch-up request'))
    return result instanceof Error ? Promise.reject(result) : Promise.resolve(result)
  })
}
const deferred = <T,>() => {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => { resolve = done })
  return { promise, resolve }
}

beforeEach(() => {
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
})

afterEach(() => {
  vi.restoreAllMocks()
  Reflect.deleteProperty(navigator, 'locks')
})

describe('Since you last checked', () => {
  it('keeps the last Work snapshot visible and labels a failed refresh while catch-up loads independently', () => {
    const overview: WorkOverview = {
      recent_since: 100,
      until: 200,
      current: [],
      recent: [],
      overlaps: [],
      current_complete: true,
      recent_complete: true,
      overlaps_complete: true,
      history_retention_gap: false,
    }
    vi.spyOn(globalThis, 'fetch').mockImplementation(() => new Promise(() => {}))
    window.location.hash = '#activity'
    const data = { whoami: null, work: null, activity: null, health: null, fresh: new Set<string>(), lastOk: null, failing: false }
    render(<DashboardPage section="activity" question="What happened since I last looked?" token="read-only" repo="acme/widgets" data={data} d={null} overview={{
      kind: 'ready',
      value: overview,
      fetchedAt: 1_000_000,
      refreshError: 'connection refused',
      retry: vi.fn(),
    }} />)

    expect(screen.getByText('Showing the last overview that loaded')).toBeTruthy()
    expect(screen.getByText('Since you last checked')).toBeTruthy()
    expect(screen.getByText(/connection refused/)).toBeTruthy()
  })

  it('renders the exact server-sanitized merge title as text', async () => {
    const sanitizedTitle = 'Family 👨👩👧👦'
    mockCatchup(response(page({
      merge_from: 0,
      merge_after: 0,
      merges: [{
        id: 'merge-family', seq: 1, source: 'forge_pull', source_id: 'github:144', occurred_at: null, observed_at: 1_798_000_010,
        title: sanitizedTitle, branch: 'feat/family', base: 'main', head_commit: '1'.repeat(40), merge_commit: '2'.repeat(40), pull_number: 144,
        paths: ['ui/src/catchup.ts'], paths_complete: true,
      }],
    })))

    render(<CatchupPanel token="read-token" />)
    expect(await screen.findByText(sanitizedTitle)).toBeTruthy()
    expect(document.querySelector('img')).toBeNull()
  })

  it('never acknowledges on load and marks only a complete fixed delivery after a click', async () => {
    const fetchMock = mockCatchup(
      response(page()),
      response({ status: 'advanced', event_through: 3, merge_through: 1, current_event_ack: 3, current_merge_ack: 1 }),
      response(page({ delivery: delivery('b'), event_from: 3, merge_from: 1 })),
    )

    render(<CatchupPanel token="read-token" />)
    const button = await screen.findByRole('button', { name: /Mark reviewed/ })
    expect(catchupCalls(fetchMock)).toHaveLength(1)
    expect(screen.getByText('No new retained AgentSCM records are in this delivery.')).toBeTruthy()
    expect(screen.getByText(/does not claim all forge history/)).toBeTruthy()

    fireEvent.click(button)
    await waitFor(() => expect(catchupCalls(fetchMock)).toHaveLength(3))
    const calls = catchupCalls(fetchMock)
    expect(calls[1]?.[0]).toBe('/v1/hub/standup/catchup/ack')
    expect(calls[1]?.[1]).toMatchObject({ method: 'POST', body: JSON.stringify({ delivery: delivery(), expected_event_from: 3, expected_merge_from: 1 }) })
    const initialClient = new URL(String(calls[0]?.[0]), 'http://hub.test').searchParams.get('delivery_client')
    const refreshedClient = new URL(String(calls[2]?.[0]), 'http://hub.test').searchParams.get('delivery_client')
    expect(initialClient).toMatch(/^hub_/)
    expect(refreshedClient).toBe(initialClient)
  })

  it('keeps incomplete, retention-gap and unavailable states distinct', async () => {
    const fetchMock = mockCatchup(response(page({
      event_through: 4,
      next_event_after: 3,
      complete: false,
      retention: { work_gap: true, coordination_gap: false, merge_gap: false, event_available_from: 2, merge_available_from: 0 },
    })))
    const { unmount } = render(<CatchupPanel token="read-token" />)
    expect(await screen.findByText('More changes remain')).toBeTruthy()
    expect(screen.getByText('Earlier records are no longer retained')).toBeTruthy()
    expect(screen.getByRole('button', { name: /Load more/ })).toBeTruthy()
    expect(screen.queryByRole('button', { name: /Mark reviewed/ })).toBeNull()
    unmount()

    fetchMock.mockRestore()
    mockCatchup(new Error('connection refused'))
    render(<CatchupPanel token="read-token" />)
    expect(await screen.findByText('Catch-up unavailable')).toBeTruthy()
    expect(screen.getByText(/not an empty result or an all-clear/)).toBeTruthy()
  })

  it('ignores a previous token page that finishes after the new token loads', async () => {
    const oldContinuation = deferred<Response>()
    const fetchMock = mockCatchup(
      response(page({ delivery: delivery('a'), event_through: 4, next_event_after: 3, complete: false })),
      oldContinuation.promise,
      response(page({ delivery: delivery('b') })),
    )

    const view = render(<CatchupPanel token="old-token" />)
    fireEvent.click(await screen.findByRole('button', { name: /Load more/ }))
    await waitFor(() => expect(catchupCalls(fetchMock)).toHaveLength(2))

    view.rerender(<CatchupPanel token="new-token" />)
    expect(await screen.findByText(delivery('b'))).toBeTruthy()
    oldContinuation.resolve(response(page({ delivery: delivery('a'), event_after: 3, next_event_after: 4 })))

    await waitFor(() => expect(screen.getByRole('button', { name: /Mark reviewed/ }).hasAttribute('disabled')).toBe(false))
    expect(screen.getByText(delivery('b'))).toBeTruthy()
    expect(screen.queryByText(delivery('a'))).toBeNull()
  })

  it('ignores a previous token acknowledgement that finishes after the new token loads', async () => {
    const oldAcknowledgement = deferred<Response>()
    const fetchMock = mockCatchup(
      response(page({ delivery: delivery('a') })),
      oldAcknowledgement.promise,
      response(page({ delivery: delivery('b') })),
    )

    const view = render(<CatchupPanel token="old-token" />)
    fireEvent.click(await screen.findByRole('button', { name: /Mark reviewed/ }))
    await waitFor(() => expect(catchupCalls(fetchMock)).toHaveLength(2))

    view.rerender(<CatchupPanel token="new-token" />)
    expect(await screen.findByText(delivery('b'))).toBeTruthy()
    oldAcknowledgement.resolve(response({ status: 'conflict', event_through: 3, merge_through: 1, current_event_ack: 3, current_merge_ack: 1 }))

    await waitFor(() => expect(screen.getByRole('button', { name: /Mark reviewed/ }).hasAttribute('disabled')).toBe(false))
    expect(catchupCalls(fetchMock)).toHaveLength(3)
    expect(screen.queryByText('Review position changed')).toBeNull()
    expect(screen.queryByText('Marked retained records reviewed')).toBeNull()
    expect(screen.getByText(delivery('b'))).toBeTruthy()
  })

  it('keeps same-person component delivery windows independent while sharing the human subscriber', async () => {
    const initialClients: string[] = []
    const deliveryByClient = new Map<string, string>()
    const fetchMock = vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => {
      if (isNarrative(input)) return response(narrative())
      const url = new URL(String(input), 'http://hub.test')
      const client = url.searchParams.get('delivery_client') ?? ''
      expect(url.searchParams.get('subscriber')).toBe('human')
      expect(client).toMatch(/^hub_/)
      if (!url.searchParams.has('delivery')) {
        initialClients.push(client)
        const fixedDelivery = delivery(initialClients.length === 1 ? 'a' : 'b')
        deliveryByClient.set(client, fixedDelivery)
        return response(page({ delivery_client: client, delivery: fixedDelivery, event_through: 4, next_event_after: 3, complete: false }))
      }
      expect(url.searchParams.get('delivery')).toBe(deliveryByClient.get(client))
      return response(page({
        delivery_client: client,
        delivery: deliveryByClient.get(client),
        event_through: 4,
        event_after: 3,
        next_event_after: 4,
        source_coverage: [
          { source: 'tracked_publication', state: 'observed_only', checked_at: 1_798_000_020, unread_pages: 0, deferred: 0 },
          { source: 'forge_pull', state: 'unavailable', checked_at: null, unread_pages: 0, deferred: 0 },
        ],
      }))
    })

    render(<><CatchupPanel token="shared-token" /><CatchupPanel token="shared-token" /></>)
    await waitFor(() => expect(catchupCalls(fetchMock)).toHaveLength(2))
    expect(new Set(initialClients).size).toBe(2)

    for (const button of await screen.findAllByRole('button', { name: /Load more/ })) fireEvent.click(button)
    await waitFor(() => expect(catchupCalls(fetchMock)).toHaveLength(4))
    expect(await screen.findAllByRole('button', { name: /Mark reviewed/ })).toHaveLength(2)
    expect(screen.getAllByText('Forge pull request source unavailable')).toHaveLength(2)
  })

  it('reuses the persisted tab slot across Strict Mode remounts without changing identity', async () => {
    sessionStorage.clear()
    const fetchMock = vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => isNarrative(input) ? response(narrative()) : response(page()))
    const strict = render(<StrictMode><CatchupPanel token="shared-token" /></StrictMode>)
    await screen.findByRole('button', { name: /Mark reviewed/ })
    const strictClients = catchupCalls(fetchMock).map((call) => new URL(String(call[0]), 'http://hub.test').searchParams.get('delivery_client'))
    expect(new Set(strictClients).size).toBe(1)
    const persistedClient = strictClients[0]

    strict.unmount()
    fetchMock.mockClear()
    render(<CatchupPanel token="shared-token" />)
    await screen.findByRole('button', { name: /Mark reviewed/ })
    const remountedClient = new URL(String(catchupCalls(fetchMock)[0]?.[0]), 'http://hub.test').searchParams.get('delivery_client')
    expect(remountedClient).toBe(persistedClient)
  })
})
