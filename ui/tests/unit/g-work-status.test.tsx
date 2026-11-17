import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { decodeStatus, statusForWork, type ActionableStatus, type StatusState } from '../../src/actionableStatus.ts'
import type { LiveProps } from '../../src/g/GLive.tsx'
import { LiveChange, LiveChanges } from '../../src/g/LiveChanges.tsx'
import type { WorkItem, WorkOverview } from '../../src/standup.ts'
import type { Poll } from '../../src/usePoll.ts'

const fixture = JSON.parse(readFileSync(resolve(process.cwd(), '../src/hub/fixtures/actionable-status.json'), 'utf8')) as ActionableStatus
const ready = <T,>(value: T): Poll<T> => ({ kind: 'ready', value, fetchedAt: 0, refreshError: null, retry: () => {} })
const item = (id: string): WorkItem => ({ id: `wrk_${Array.from(id).map((c) => c.charCodeAt(0).toString(16)).join('').padEnd(32, '0').slice(0, 32)}`, title: `Change ${id}`, repository: 'acme/widgets', owner: 'owner', owner_login: 'maya', source: { kind: 'native', request_key: id }, state: 'paused', claim_status: 'stale', generation: 2, updated_seq: 12, last_checkin_at: 100, updated_at: 100, created_at: 90, created_seq: 1, lead_token: 't', lead_agent: 'codex', lead_session: 's', participants: [{ role: 'lead', token: 't', agent: 'codex', session: 's', login: 'maya', owner: 'owner' }], scope: [], branch: 'feature/test', worktree: null, next_action: null, current: null, blocker: null, depends_on: [], related_work: [] })
function status(items: WorkItem[], states: StatusState[] = items.map(() => 'active')): ActionableStatus {
  const shared = decodeStatus(fixture)
  shared.work = items.map((w, i) => ({ ...shared.work[0]!, work_id: w.id, generation: w.generation, updated_seq: w.updated_seq, state: states[i]!, needs_you: false }))
  return shared
}
function live(items: WorkItem[], shared: ActionableStatus | null): LiveProps {
  return { token: 'synthetic', repo: 'acme/widgets', data: { work: null, whoami: null, activity: null, health: null, fresh: new Set(), failing: false, lastOk: null }, d: null, needs: [], inbox: { n: 0, tip: '' }, handoff: { me: 'maya', repo: 'acme/widgets', originUrl: null }, overview: ready({ current: items, recent: [], overlaps: [], current_complete: true } as unknown as WorkOverview), status: shared ? ready(shared) : { kind: 'error', message: 'Unavailable', retry: () => {} } } as LiveProps
}

describe('shared Work status in G changes', () => {
  beforeEach(() => vi.stubGlobal('fetch', async () => new Response('{}', { status: 404 })))
  afterEach(() => vi.unstubAllGlobals())

  it('joins only the exact Work id, generation and update sequence', () => {
    const w = item('one'), shared = status([w])
    expect(statusForWork(w, shared)?.state).toBe('active')
    for (const changed of [{ id: 'other' }, { generation: 3 }, { updated_seq: 13 }]) expect(statusForWork({ ...w, ...changed }, shared)).toBeNull()
    expect(statusForWork(w, null)).toBeNull()
  })

  it('shows quiet paused Work with a shared active state in the active filter and lane', () => {
    const w = item('one')
    const { container } = render(<LiveChanges live={live([w], status([w]))} />)
    expect(screen.getByRole('button', { name: 'Active 1' })).toBeTruthy()
    fireEvent.click(screen.getByRole('button', { name: 'Active 1' }))
    const row = container.querySelector('.g-row')!
    expect(within(row as HTMLElement).getByText('Active')).toBeTruthy()
    expect(row.querySelector('.px-dot.tone-brand')).toBeTruthy()
    expect(screen.queryByText('Quiet · still assigned')).toBeNull()
    expect(screen.queryByText(/checks pass|being reviewed/i)).toBeNull()
  })

  it('keeps blockers, named waits and reassessment separate from active Work', () => {
    const items = ['active', 'blocked', 'awaiting_party', 'reassessment_needed'].map(item)
    const states: StatusState[] = ['active', 'blocked', 'awaiting_party', 'reassessment_needed']
    const { container } = render(<LiveChanges live={live(items, status(items, states))} />)
    for (const label of ['Active', 'Blocked', 'Awaiting a named party', 'Reassessment needed']) {
      fireEvent.click(screen.getByRole('button', { name: `${label} 1` }))
      expect(container.querySelectorAll('.g-row')).toHaveLength(1)
      expect(container.querySelector('.px-status')?.textContent).toBe(label)
    }
  })

  it('labels cached shared states after a failed refresh in the change list', () => {
    const w = item('one'), view = live([w], status([w]))
    view.status = { kind: 'ready', value: status([w]), fetchedAt: 0, refreshError: 'offline', retry: () => {} }
    const { container } = render(<LiveChanges live={view} />)
    expect(screen.getByText('Showing the last shared status that loaded')).toBeTruthy()
    expect(screen.getByText('Shared status could not refresh; these states may have changed.')).toBeTruthy()
    expect(container.querySelector('.px-status')?.textContent).toBe('Active')
  })

  it('labels cached shared state after a failed refresh on the change detail', async () => {
    const w = item('one'), view = live([w], status([w]))
    view.status = { kind: 'ready', value: status([w]), fetchedAt: 0, refreshError: 'offline', retry: () => {} }
    vi.stubGlobal('fetch', async (input: RequestInfo | URL) => new Response(JSON.stringify(w), { status: String(input) === `/v1/hub/standup/work/${w.id}` ? 200 : 404 }))
    const { container } = render(<LiveChange id={w.id} live={view} />)
    expect(await screen.findByText('Showing the last shared status that loaded')).toBeTruthy()
    expect(screen.getByText('Shared status could not refresh; this state may have changed.')).toBeTruthy()
    expect(container.querySelector('.g-status-text > b')?.textContent).toBe('Active')
  })

  it('keeps unavailable, omitted and mismatched status unknown rather than using raw state', () => {
    const w = item('one'), shared = status([w])
    const { container, rerender } = render(<LiveChanges live={live([w], null)} />)
    const unknown = () => {
      expect(screen.getByRole('button', { name: 'Status unknown 1' })).toBeTruthy()
      expect(container.querySelector('.px-status')?.textContent).toBe('Shared status unknown')
      expect(screen.getByRole('button', { name: 'Active 0' })).toBeTruthy()
    }
    unknown()
    shared.work[0]!.updated_seq++
    rerender(<LiveChanges live={live([w], shared)} />)
    unknown()
    shared.work = []
    rerender(<LiveChanges live={live([w], shared)} />)
    unknown()
  })
})
