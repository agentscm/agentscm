import { describe, expect, it, vi } from 'vitest'
import { decodeReconciliation, fetchReconciliation } from '../../src/workReconciliation.ts'
const item = { id: `wrk_${'a'.repeat(32)}`, generation: 2, updated_seq: 15 }
const current = { work_id: item.id, generation: 2, work_seq: 15, declaration_seq: 12, conditions: [{ id: 'first-pr', state: 'satisfied', obstructing: false, limits: [] }, { id: 'second-pr', state: 'unknown', obstructing: true, limits: ['coverage_unknown'] }], links: [], reassessments: [{ id: 'request-a', reason: 'Reassess the preserved blocker.' }], reservation_disposition: 'held_owner_decision_required', next_action: 'Reassess the blocker.' }
const view = () => ({ schema_version: 2, work_id: item.id, generation: 2, updated_seq: 15, declaration_seq: 12, retained: current, current, stale_reasons: [] })
describe('durable Work reconciliation', () => {
  it('keeps partial condition satisfaction and obstruction separate', () => { const decoded = decodeReconciliation(view(), item); expect(decoded.current?.conditions.map((c) => c.obstructing)).toEqual([false, true]); expect(decoded.current?.reservation_disposition).toBe('held_owner_decision_required') })
  it('does not promote retained history when current is stale or absent', () => {
    expect(decodeReconciliation({ ...view(), current: null, stale_reasons: ['provider_or_prerequisite_evidence_changed'] }, item).current).toBeNull()
    expect(decodeReconciliation({ ...view(), current: null, retained: null, stale_reasons: ['not_reconciled'] }, item).current).toBeNull()
    expect(decodeReconciliation(view(), { ...item, updated_seq: 16 }).current).toBeNull()
    expect(decodeReconciliation({ ...view(), stale_reasons: ['work_changed'] }, item).current).toBeNull()
  })
  it('rejects wrong identity, contradictory obstruction and unbounded lists', () => {
    expect(() => decodeReconciliation({ ...view(), work_id: 'other' }, item)).toThrow()
    expect(() => decodeReconciliation({ ...view(), current: { ...current, conditions: [{ id: 'bad', state: 'satisfied', obstructing: true, limits: [] }] } }, item)).toThrow()
    expect(() => decodeReconciliation({ ...view(), current: { ...current, conditions: Array(101).fill(current.conditions[0]) } }, item)).toThrow()
  })
  it('only performs a read and uses the exact Work identity', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response(JSON.stringify(view()), { status: 200 }))
    await fetchReconciliation(item, 'synthetic', new AbortController().signal)
    expect(fetcher).toHaveBeenCalledOnce()
    expect(fetcher.mock.calls[0]?.[0]).toBe(`/v1/hub/standup/work/${item.id}/reconciliation-v2`)
    expect(fetcher.mock.calls[0]?.[1]?.method ?? 'GET').toBe('GET')
    fetcher.mockRestore()
  })
  it('accepts full bounded historical evidence beyond the smaller PR-context response limit', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response(JSON.stringify({ ...view(), retained: { evidence: 'x'.repeat(60 * 1024) } }), { status: 200 }))
    expect((await fetchReconciliation(item, 'synthetic', new AbortController().signal)).current?.conditions).toHaveLength(2)
    fetcher.mockRestore()
  })
  it('refuses oversized evidence instead of silently truncating it', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response(JSON.stringify({ ...view(), retained: { evidence: 'x'.repeat(513 * 1024) } }), { status: 200 }))
    await expect(fetchReconciliation(item, 'synthetic', new AbortController().signal)).rejects.toThrow(/response limit/)
    fetcher.mockRestore()
  })

})
