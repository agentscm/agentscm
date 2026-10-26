import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { NarrativePanel } from '../../src/Narrative.tsx'

const source = {
  id: 'source-1',
  label: 'Reported work <img src=x onerror=alert(1)>',
  work_id: `wrk_${'1'.repeat(32)}`,
  commit: 'a'.repeat(40),
  pull_number: 42,
}
const view = (over: Record<string, unknown> = {}) => ({
  id: 'briefing-1',
  digest: 'b'.repeat(64),
  status: 'ready',
  generated_at: 1_798_000_100,
  generated_by: 'agent-a',
  window: { event_from: 2, event_through: 8, merge_from: 1, merge_through: 3, subscriber: 'human', complete: true },
  sources: [source],
  warnings: [],
  output: {
    headline: 'Reported progress <script>alert(1)</script>',
    headline_references: ['source-1'],
    facts: [{ text: 'A checkpoint was reported.', references: ['source-1'] }],
    implications: [{ text: 'Inspect the current scope before editing.', references: ['source-1'] }],
  },
  ...over,
})
const response = (value: unknown) => new Response(JSON.stringify(value), { status: 200 })
const deferred = <T,>() => {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => { resolve = done })
  return { promise, resolve }
}

afterEach(() => {
  vi.useRealTimers()
  vi.restoreAllMocks()
})

describe('Agent briefing', () => {
  it('keeps a pending briefing distinct and polls after 15 seconds without posting', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    const pending = view({ status: 'pending', generated_at: null, generated_by: null, output: null })
    const fetchMock = vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(response(pending))
      .mockResolvedValueOnce(response(pending))

    render(<NarrativePanel token="read-token" />)
    expect(await screen.findByText('Waiting for a connected agent to generate this briefing.')).toBeTruthy()
    await vi.advanceTimersByTimeAsync(15_000)
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2))
    expect(fetchMock.mock.calls.every((call) => call[1]?.method === 'GET')).toBe(true)
  })

  it('retries temporary capture unavailability without posting', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    const fetchMock = vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(new Response('{}', { status: 503 }))
      .mockResolvedValueOnce(response(view()))
    render(<NarrativePanel token="read-token" />)
    expect(await screen.findByText('Agent briefing unavailable')).toBeTruthy()
    await vi.advanceTimersByTimeAsync(30_000)
    expect(await screen.findByText('Reported facts')).toBeTruthy()
    expect(fetchMock).toHaveBeenCalledTimes(2)
    expect(fetchMock.mock.calls.every((call) => call[1]?.method === 'GET')).toBe(true)
  })

  it('keeps polling a failed job so a recovered result becomes visible', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    const fetchMock = vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(response(view({ status: 'failed', generated_at: null, generated_by: null, output: null, warnings: ['Generation cooldown.'] })))
      .mockResolvedValueOnce(response(view()))
    render(<NarrativePanel token="read-token" />)
    expect(await screen.findByText('Agent briefing generation failed')).toBeTruthy()
    await vi.advanceTimersByTimeAsync(30_000)
    expect(await screen.findByText('Reported facts')).toBeTruthy()
    expect(fetchMock).toHaveBeenCalledTimes(2)
    expect(fetchMock.mock.calls.every((call) => call[1]?.method === 'GET')).toBe(true)
  })

  it('renders ready text, incomplete scope, warnings and only trusted local Work links', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response(view({
      window: { event_from: 2, event_through: 8, merge_from: 1, merge_through: 3, subscriber: 'human', complete: false },
      warnings: ['Some source records were omitted.'],
    })))

    render(<NarrativePanel token="read-token" />)
    expect(await screen.findByText('Reported facts')).toBeTruthy()
    expect(screen.getByText('What this may mean for your work')).toBeTruthy()
    expect(screen.getByText('The source window is incomplete')).toBeTruthy()
    expect(screen.getByText('Some source records were omitted.')).toBeTruthy()
    expect(screen.getByText('Reported progress <script>alert(1)</script>')).toBeTruthy()
    const workLinks = screen.getAllByRole('link', { name: source.label })
    expect(workLinks).toHaveLength(3)
    expect(workLinks.every((link) => link.getAttribute('href') === `#work/${source.work_id}`)).toBe(true)
    expect(screen.getAllByText(/PR #42/).length).toBeGreaterThan(0)
    expect(document.querySelector('img')).toBeNull()
    expect(document.querySelector('script')).toBeNull()
  })

  it('hides an old token result even when it finishes after the new token', async () => {
    const oldResult = deferred<Response>()
    const fetchMock = vi.spyOn(globalThis, 'fetch')
      .mockReturnValueOnce(oldResult.promise)
      .mockResolvedValueOnce(response(view({ output: { headline: 'New token briefing', headline_references: ['source-1'], facts: [{ text: 'New token fact', references: ['source-1'] }], implications: [] } })))

    const rendered = render(<NarrativePanel token="old-token" />)
    rendered.rerender(<NarrativePanel token="new-token" />)
    expect(await screen.findByText('New token briefing')).toBeTruthy()

    oldResult.resolve(response(view({ output: { headline: 'Old token briefing', headline_references: ['source-1'], facts: [{ text: 'Old token fact', references: ['source-1'] }], implications: [] } })))
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2))
    expect(screen.queryByText('Old token briefing')).toBeNull()
    expect(screen.getByText('New token briefing')).toBeTruthy()
  })

  it('requests a task only on submit and hides a superseded task result', async () => {
    const oldTask = deferred<Response>()
    const fetchMock = vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(response(view({ output: { headline: 'General briefing', headline_references: ['source-1'], facts: [{ text: 'General fact', references: ['source-1'] }], implications: [] } })))
      .mockReturnValueOnce(oldTask.promise)
      .mockResolvedValueOnce(response(view({ output: { headline: 'Parser briefing', headline_references: ['source-1'], facts: [{ text: 'Parser fact', references: ['source-1'] }], implications: [] } })))

    render(<NarrativePanel token="read-token" />)
    expect(await screen.findByText('General briefing')).toBeTruthy()
    const taskInput = screen.getByLabelText('Task to consider')
    fireEvent.change(taskInput, { target: { value: 'Review storage' } })
    expect(fetchMock).toHaveBeenCalledTimes(1)
    fireEvent.click(screen.getByRole('button', { name: 'Brief this task' }))
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2))

    fireEvent.change(screen.getByLabelText('Task to consider'), { target: { value: 'Review parser' } })
    fireEvent.click(screen.getByRole('button', { name: 'Brief this task' }))
    expect(await screen.findByText('Parser briefing')).toBeTruthy()
    expect(new URL(String(fetchMock.mock.calls[2]?.[0]), 'http://hub.test').searchParams.get('task')).toBe('Review parser')
    expect(fetchMock.mock.calls.every((call) => call[1]?.method === 'GET')).toBe(true)

    oldTask.resolve(response(view({ output: { headline: 'Storage briefing', headline_references: ['source-1'], facts: [{ text: 'Storage fact', references: ['source-1'] }], implications: [] } })))
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(3))
    expect(screen.queryByText('Storage briefing')).toBeNull()
    expect(screen.getByText('Parser briefing')).toBeTruthy()
  })

  it('shows an invalid response as unavailable, never as an all-clear', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch')
      .mockResolvedValueOnce(response(view({
        output: { headline: 'Invalid', headline_references: ['source-1'], facts: [{ text: 'Bad reference', references: ['unknown'] }], implications: [] },
      })))
      .mockResolvedValueOnce(response(view()))

    render(<NarrativePanel token="read-token" />)
    expect(await screen.findByText('Agent briefing unavailable')).toBeTruthy()
    expect(screen.getByText(/not an empty result or an all-clear/)).toBeTruthy()
    fireEvent.click(screen.getByRole('button', { name: 'Retry briefing' }))
    expect(await screen.findByText('Reported progress <script>alert(1)</script>')).toBeTruthy()
    expect(fetchMock).toHaveBeenCalledTimes(2)
  })
})
