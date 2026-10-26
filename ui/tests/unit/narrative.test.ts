import { describe, expect, it, vi } from 'vitest'
import { decodeNarrative, fetchNarrative, InvalidNarrativeResponse } from '../../src/narrative.ts'

const ready = (over: Record<string, unknown> = {}) => ({
  id: 'briefing-1',
  digest: 'a'.repeat(64),
  status: 'ready',
  generated_at: 1_798_000_100,
  generated_by: 'agent-a',
  window: { event_from: 2, event_through: 8, merge_from: 1, merge_through: 3, subscriber: 'human', complete: true },
  sources: [{ id: 'work-1', label: 'Build the briefing', work_id: `wrk_${'1'.repeat(32)}`, commit: null, pull_number: null }],
  warnings: [],
  output: {
    headline: 'One task changed.',
    headline_references: ['work-1'],
    facts: [{ text: 'The agent reported progress.', references: ['work-1'] }],
    implications: [],
  },
  ...over,
})

describe('agent briefing response', () => {
  it('accepts a bounded ready view and rejects unknown or duplicate references', () => {
    expect(decodeNarrative(ready()).output?.facts[0]?.text).toBe('The agent reported progress.')
    expect(() => decodeNarrative(ready({
      output: { headline: 'Changed', headline_references: ['work-1'], facts: [{ text: 'Unknown source', references: ['missing'] }], implications: [] },
    }))).toThrow(InvalidNarrativeResponse)
    expect(() => decodeNarrative(ready({
      output: { headline: 'Changed', headline_references: ['work-1'], facts: [{ text: 'Duplicate source', references: ['work-1', 'work-1'] }], implications: [] },
    }))).toThrow(InvalidNarrativeResponse)
    expect(() => decodeNarrative(ready({
      output: { headline: 'Unknown source', headline_references: ['missing'], facts: [{ text: 'A fact', references: ['work-1'] }], implications: [] },
    }))).toThrow(InvalidNarrativeResponse)
    expect(() => decodeNarrative(ready({
      output: { headline: 'Uncited claim', headline_references: [], facts: [{ text: 'A fact', references: ['work-1'] }], implications: [] },
    }))).toThrow(InvalidNarrativeResponse)
  })

  it('allows a cited-fact-free ready result only when its bounded window has no sources', () => {
    expect(decodeNarrative(ready({
      sources: [],
      output: { headline: 'No source records were retained.', headline_references: [], facts: [], implications: [] },
    })).output?.facts).toEqual([])
    expect(() => decodeNarrative(ready({
      output: { headline: 'Missing facts', headline_references: ['work-1'], facts: [], implications: [] },
    }))).toThrow(InvalidNarrativeResponse)
  })

  it('uses a bounded GET with bearer authorization and never posts', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(new Response(JSON.stringify(ready()), { status: 200 }))
    const view = await fetchNarrative('read-token', new AbortController().signal)

    expect(view.status).toBe('ready')
    expect(fetchMock).toHaveBeenCalledTimes(1)
    expect(fetchMock.mock.calls[0]?.[0]).toBe('/v1/hub/standup/narrative?subscriber=human')
    expect(fetchMock.mock.calls[0]?.[1]).toMatchObject({ method: 'GET', credentials: 'omit', headers: { Authorization: 'Bearer read-token' } })
  })

  it('rejects an oversized task before issuing a request', async () => {
    const fetchMock = vi.spyOn(globalThis, 'fetch')
    await expect(fetchNarrative('read-token', new AbortController().signal, { task: 'x'.repeat(501) })).rejects.toThrow(InvalidNarrativeResponse)
    expect(fetchMock).not.toHaveBeenCalled()
  })
})
