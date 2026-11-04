import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { ReconciliationFacts, WorkReconciliation } from '../../src/WorkReconciliation.tsx'
import type { WorkItem } from '../../src/standup.ts'
import { clearPollCache } from '../../src/usePoll.ts'
afterEach(() => { cleanup(); vi.restoreAllMocks(); clearPollCache() })
describe('Work reconciliation panel', () => {
  it('shows partial satisfaction and preserves the separate task/blocker decision', () => {
    render(<ReconciliationFacts value={{ stale_reasons: [], current: { conditions: [{ id: 'API change', state: 'satisfied', obstructing: false, limits: [] }, { id: 'UI change', state: 'unknown', obstructing: true, limits: ['provider_unavailable'] }], links: [], reassessments: [{ id: 'request', reason: 'Reassess' }], reservation_disposition: 'held_owner_decision_required', next_action: 'Ask the owner to reassess.' } }} />)
    expect(screen.getByText('Satisfied')).toBeTruthy(); expect(screen.getByText('Unknown')).toBeTruthy()
    expect(screen.getByText(/original blocker is preserved below/)).toBeTruthy()
    expect(screen.getByText(/owner must decide/)).toBeTruthy()
    expect(screen.queryByRole('button')).toBeNull()
  })
  it('shows unavailable current results without rendering historical success', () => {
    render(<ReconciliationFacts value={{ current: null, stale_reasons: ['provider_or_prerequisite_evidence_changed'] }} />)
    expect(screen.getByText(/Prerequisite evidence needs updating/)).toBeTruthy(); expect(screen.queryByText('Satisfied')).toBeNull()
    expect(screen.queryByRole('button')).toBeNull()
  })
  it('offers a Work reload when a newer apply advanced the sequence', async () => {
    const item = { id: `wrk_${'a'.repeat(32)}`, generation: 2, updated_seq: 15 } as WorkItem
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response(JSON.stringify({ schema_version: 2, work_id: item.id, generation: 2, updated_seq: 16, declaration_seq: 12, current: null, retained: null, stale_reasons: ['not_reconciled'] }), { status: 200 }))
    const refreshWork = vi.fn()
    render(<WorkReconciliation item={item} token="synthetic" refreshWork={refreshWork} />)
    fireEvent.click(await screen.findByRole('button', { name: 'Reload Work detail' }))
    expect(refreshWork).toHaveBeenCalledOnce()
  })

})
