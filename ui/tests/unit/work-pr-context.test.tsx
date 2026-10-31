import { fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { WorkUpdate } from '../../src/Catchup.tsx'
import { contextFixture } from '../../dev/prContext.ts'
import { decodeWorkPrLink, fetchWorkPrContext, WorkPrContext } from '../../src/WorkPrContext.tsx'
import type { WorkItem } from '../../src/standup.ts'
import { clearPollCache } from '../../src/usePoll.ts'

const item = { id: `wrk_${'b'.repeat(32)}`, repository: 'acme/widgets', generation: 2, updated_seq: 15 } as WorkItem
function relationship() {
  return { work_id: item.id, repository: item.repository, repository_id: 91, generation: 2, updated_seq: 15, snapshot_status: 'matches_work', head_status: 'not_compared', link: {
    target: { repository_id: 91, number: 42, head_sha: 'a'.repeat(40) }, author: 'owner_fixture', token: 'token_fixture', agent: 'codex', session: 'session_fixture', generation: 2, linked_at: 1_793_387_000, event_seq: 15,
  } }
}
const linkedContext = (number: number) => ({ ...contextFixture(number), repository_id: 91 })
const response = (value: unknown) => new Response(JSON.stringify(value))
afterEach(() => { vi.restoreAllMocks(); clearPollCache() })

describe('explicit Work PR relationship', () => {
  it('renders relationship changes without replaying a prior checkpoint or finish status', () => {
    const changed = { ...item, state: 'finished', lead_agent: 'codex', title: 'Fixture Work', current: { summary: 'Prior checkpoint text' } } as WorkItem
    render(<WorkUpdate event={{ seq: 16, at: 1_793_387_000, kind: 'work_pr_link', work_id: item.id, item: changed }} />)
    expect(screen.getByText('Relationship changed')).toBeTruthy()
    expect(screen.queryByText(/Prior checkpoint text/)).toBeNull()
    expect(screen.getByText('Relationship changed').closest('li')?.className).not.toContain('tone-ok')
  })
  it('fetches the exact Work sequence, linked number and caller-asserted head', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response(relationship())).mockResolvedValueOnce(response(linkedContext(42)))
    const data = await fetchWorkPrContext(item, 'synthetic', new AbortController().signal)
    expect(fetcher.mock.calls[0]![0]).toBe(`/v1/hub/standup/work/${item.id}/pr-link?expected_updated_seq=15`)
    expect(fetcher.mock.calls[1]![0]).toBe(`/v1/hub/repos/acme%2Fwidgets/pr-context?number=42&expected_head=${'a'.repeat(40)}`)
    expect(data.context?.pull_request?.number).toBe(42)
    expect(fetcher.mock.calls.every(([, init]) => init?.credentials === 'omit' && init?.cache === 'no-store')).toBe(true)
  })

  it('preserves a link created before takeover while matching the current Work sequence', async () => {
    const previous = relationship(); previous.link.generation = 1; previous.link.event_seq = 10
    const decoded = decodeWorkPrLink(previous, item)
    expect(decoded.snapshot_status).toBe('matches_work')
    expect(decoded.link?.generation).toBe(1)
    vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response(previous)).mockResolvedValueOnce(response(linkedContext(42)))
    const result = await fetchWorkPrContext(item, 'takeover', new AbortController().signal)
    expect(result.context?.pull_request?.number).toBe(42)
    const future = relationship(); future.link.generation = 3
    expect(() => decodeWorkPrLink(future, item)).toThrow(/target mismatch/)
  })
  it('requires a matching stable repository ID even without provider evidence', async () => {
    for (const repository_id of [undefined, 92]) {
      const context = { ...contextFixture(42), repository_id }
      const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response(relationship())).mockResolvedValueOnce(response(context))
      await expect(fetchWorkPrContext(item, 'repo-identity', new AbortController().signal)).rejects.toThrow(/repository identity mismatch/)
      fetcher.mockRestore()
    }
  })
  it('does not infer a relationship or fetch context for an unlinked Work item', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(response({ ...relationship(), link: null }))
    render(<WorkPrContext item={item} token="unlinked" refreshWork={() => {}} />)
    await screen.findByText('No explicit PR relationship')
    expect(fetcher).toHaveBeenCalledTimes(1)
  })
  it('rejects a different PR response instead of presenting it under the Work link', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response(relationship())).mockResolvedValueOnce(response(linkedContext(43)))
    await expect(fetchWorkPrContext(item, 'wrong-pr', new AbortController().signal)).rejects.toThrow(/identity mismatch/)
  })
  it('stops before PR reads when Work changed and offers a detail reload', async () => {
    const changed = { ...relationship(), updated_seq: 16, snapshot_status: 'work_changed' }
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(response(changed))
    const reload = vi.fn()
    render(<WorkPrContext item={item} token="changed" refreshWork={reload} />)
    await screen.findByText('Work changed since this detail loaded')
    expect(fetcher).toHaveBeenCalledTimes(1)
    expect(screen.queryByRole('link', { name: 'Inspect workflow record' })).toBeNull()
    fireEvent.click(screen.getByRole('button', { name: 'Reload Work detail' }))
    expect(reload).toHaveBeenCalledTimes(1)
  })
  it('rejects cross-Work, cross-repository, sequence and target identity mismatch', () => {
    const baseline = relationship()
    for (const invalid of [
      { ...baseline, work_id: `wrk_${'c'.repeat(32)}` },
      { ...baseline, repository: 'acme/other' },
      { ...baseline, repository_id: 0 },
      { ...baseline, updated_seq: 16 },
      { ...baseline, link: { ...baseline.link, target: { ...baseline.link.target, repository_id: 92 } } },
      { ...baseline, link: { ...baseline.link, target: { ...baseline.link.target, number: 0 } } },
      { ...baseline, link: { ...baseline.link, target: { ...baseline.link.target, head_sha: 'invalid' } } },
    ]) expect(() => decodeWorkPrLink(invalid, item)).toThrow()
  })
  it('labels an asserted link and a changed provider head separately', async () => {
    const link = relationship(); link.link.target.number = 43; link.link.target.head_sha = 'b'.repeat(40)
    vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response(link)).mockResolvedValueOnce(response(linkedContext(43)))
    render(<WorkPrContext item={item} token="stale-head" refreshWork={() => {}} />)
    await screen.findByText(/Caller-asserted relationship/)
    expect(screen.getByText('The observed head changed')).toBeTruthy()
    expect(screen.getByText(/Linked by owner_fixture · codex/)).toBeTruthy()
    expect(screen.queryByText('Reported blocker')).toBeNull()
  })
  it('hides prior relationship and context after read access is revoked', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response(relationship())).mockResolvedValueOnce(response(linkedContext(42))).mockResolvedValueOnce(new Response('{}', { status: 403 }))
    render(<WorkPrContext item={item} token="revoked-link" refreshWork={() => {}} />)
    await screen.findByRole('link', { name: 'Inspect workflow record' })
    fireEvent.click(screen.getByRole('button', { name: 'Refresh context' }))
    await screen.findByText('PR relationship is unavailable')
    expect(screen.queryByRole('link', { name: 'Inspect workflow record' })).toBeNull()
    expect(screen.queryByText(/Caller-asserted relationship/)).toBeNull()
  })
  it('does not reuse another token’s relationship while new access is loading', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response(relationship())).mockResolvedValueOnce(response(linkedContext(42)))
    const view = render(<WorkPrContext item={item} token="first-token" refreshWork={() => {}} />)
    await screen.findByRole('link', { name: 'Inspect workflow record' })
    fetcher.mockImplementation(() => new Promise<Response>(() => {}))
    view.rerender(<WorkPrContext item={item} token="second-token" refreshWork={() => {}} />)
    expect(screen.queryByRole('link', { name: 'Inspect workflow record' })).toBeNull()
  })
  it('shows unavailable trusted identity without guessing a branch relationship', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response('{}', { status: 400 }))
    const reload = vi.fn()
    render(<WorkPrContext item={item} token="missing-identity" refreshWork={reload} />)
    await screen.findByText('PR context request was rejected')
    expect(screen.queryByRole('button', { name: 'Try again' })).toBeNull()
    expect(screen.queryByText('No explicit PR relationship')).toBeNull()
    fireEvent.click(screen.getByRole('button', { name: 'Reload Work detail' }))
    expect(reload).toHaveBeenCalledTimes(1)
  })
  it('offers a Work reload without blaming the connection when the linked context request is rejected', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response(relationship())).mockResolvedValueOnce(new Response('{}', { status: 400 }))
    const reload = vi.fn()
    render(<WorkPrContext item={item} token="rejected-context" refreshWork={reload} />)
    await screen.findByText('PR context request was rejected')
    expect(screen.queryByText(/repository connection/)).toBeNull()
    expect(screen.queryByText(/Caller-asserted relationship/)).toBeNull()
    fireEvent.click(screen.getByRole('button', { name: 'Reload Work detail' }))
    expect(reload).toHaveBeenCalledTimes(1)
  })
})
