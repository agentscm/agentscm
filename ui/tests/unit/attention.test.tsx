import { act, fireEvent, render, within } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'
import type { Derived } from '../../src/derive.ts'
import { STALE_SECONDS } from '../../src/dismiss.tsx'
import { needHideKey, needsYou } from '../../src/needs.ts'
import { DashboardPage, dashboardStatus } from '../../src/pages/Dashboard.tsx'
import type { WorkItem, WorkOverview } from '../../src/standup.ts'
import type { HubData } from '../../src/useHub.ts'
import type { Poll } from '../../src/usePoll.ts'

const now = Math.floor(Date.now() / 1000)
const twoDays = now - 2 * 86400
const item = (id: string, login: string, over: Partial<WorkItem> = {}): WorkItem =>
  ({
    id: `wrk_${id.repeat(32)}`, title: `Task ${id}`, state: 'active', claim_status: 'stale', updated_at: twoDays, last_checkin_at: twoDays, blocker: null, scope: ['src/a.rs'], next_action: null, current: null, branch: null,
    lead_agent: 'codex', lead_token: 't', lead_session: 's', owner_login: login, participants: [{ login, agent: 'codex', token: 't', session: 's', role: 'lead', owner: `github:github.com:${login}` }], ...over,
  }) as unknown as WorkItem
const overview = (current: WorkItem[]) => ({ current, recent: [], overlaps: [], current_complete: true, recent_complete: true, overlaps_complete: true }) as unknown as WorkOverview
const d = { agents: [], groups: [], filesHeld: 0, contested: [], prOverlaps: [], otherOpenPrs: 0, intentById: new Map() } as unknown as Derived
const data = { whoami: { owner: { login: 'maria' } }, failing: false, activity: null, fresh: new Set() } as unknown as HubData
const ready = (ov: WorkOverview): Poll<WorkOverview> => ({ kind: 'ready', value: ov }) as Poll<WorkOverview>

afterEach(() => localStorage.removeItem('agentscm.hidden.v1'))

const page = (ov: WorkOverview) => <DashboardPage section="home" question="q" token="t" repo="acme/widgets" data={data} d={d} overview={ready(ov)} />
const needsBox = () => within(document.getElementById('box-needs')!)

describe('Needs you', () => {
  it('ages a quiet check-in like the track does, never as a bare clock time', () => {
    render(page(overview([item('a', 'dev')])))
    const body = needsBox().getByText(/Last check-in/).textContent
    expect(body).toContain('Last check-in 2 days ago.')
    expect(body).not.toMatch(/\d:\d\d/)
  })

  it('tells the viewer to resume their own quiet task, and to ask the lead about anyone else’s', () => {
    render(page(overview([item('a', 'maria'), item('b', 'dev', { last_checkin_at: twoDays - 60 })])))
    expect(needsBox().getByText('Resume it or hand it over')).toBeTruthy()
    expect(needsBox().getByText('Ask dev before picking it up')).toBeTruthy()
    expect(needsBox().queryByText(/Ask maria/)).toBeNull()
  })
})

describe('Hide in this browser', () => {
  it('offers Hide only on rows untouched for a day, and hides without asking the hub', () => {
    const fresh = item('f', 'dev', { updated_at: now - 60, last_checkin_at: now - 60 })
    render(page(overview([item('a', 'dev'), fresh])))
    expect(needsBox().getAllByRole('button', { name: /^Hide / })).toHaveLength(1)
    fireEvent.click(needsBox().getByRole('button', { name: /^Hide / }))
    expect(needsBox().queryByText('Task a')).toBeNull()
    expect(needsBox().getByText('Task f')).toBeTruthy()
    expect(localStorage.getItem('agentscm.hidden.v1')).toContain(`quiet:${'wrk_' + 'a'.repeat(32)}@${twoDays}`)
    fireEvent.click(needsBox().getByRole('button', { name: 'Show 1 hidden' }))
    expect(needsBox().getByText('Task a')).toBeTruthy()
    fireEvent.click(needsBox().getByRole('button', { name: 'Show this item again' }))
    expect(needsBox().getByText('Task a')).toBeTruthy()
    expect(needsBox().queryByRole('button', { name: /hidden|again$/ })).toBeNull()
  })

  it('a hidden row comes back by itself once its task changes, and no count includes it while hidden', () => {
    const quiet = item('a', 'dev')
    const ov = overview([quiet])
    const [need] = needsYou(ov, d)
    localStorage.setItem('agentscm.hidden.v1', JSON.stringify([needHideKey(need!)]))
    const { rerender } = render(page(ov))
    expect(needsBox().queryByText('Task a')).toBeNull()
    expect(dashboardStatus(ov, d, null, null, new Set([needHideKey(need!)])).home).toEqual({ text: 'Nothing needs you', tone: 'ok' })
    act(() => rerender(page(overview([{ ...quiet, updated_at: twoDays + STALE_SECONDS / 2 }]))))
    expect(needsBox().getByText('Task a')).toBeTruthy()
  })
})
