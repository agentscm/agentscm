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
const data = { whoami: { owner: { login: 'maria', provider: 'github', host: 'github.com' }, token: { repository: 'acme/widgets' } }, failing: false, activity: null, fresh: new Set() } as unknown as HubData
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

  it('names the requester of a contested file from its handoff, and offers the words to paste', () => {
    const contested = [{ path: 'src/a.rs', holders: [{ id: 'm', owner: 'maria', name: 'claude-code' }, { id: 'd', owner: 'dev', name: 'cursor' }], handoff: { request: 'r', at: now, from: { owner: 'dev', name: 'cursor' }, to: { owner: 'maria', name: 'claude-code' }, note: null, answer: { kind: 'declined' as const, at: now } } }]
    render(<DashboardPage section="home" question="q" token="t" repo="acme/widgets" data={data} d={{ ...d, contested }} overview={ready(overview([]))} />)
    expect(needsBox().getByText(/Agree who keeps it · ask maria or dev/)).toBeTruthy()
    expect(needsBox().getByRole('button', { name: /^Copy prompt for dev’s / })).toBeTruthy()
    expect(needsBox().queryByText(/another agent/)).toBeNull()
  })

  it('a settled handoff is listed under the agents, and no count includes it', () => {
    const contested = [{ path: 'src/ledger.rs', holders: [{ id: 'm', owner: 'maria', name: 'claude-code' }], handoff: { request: 'r', at: now, from: { owner: 'dev', name: 'codex' }, to: { owner: 'maria', name: 'claude-code' }, note: null, answer: { kind: 'declined' as const, at: now, reason: 'Mid-refactor' } } }]
    const dd = { ...d, contested } as Derived
    expect(dashboardStatus(overview([]), dd, null, null).home).toEqual({ text: 'Nothing needs you', tone: 'ok' })
    render(<DashboardPage section="home" question="q" token="t" repo="acme/widgets" data={data} d={dd} overview={ready(overview([]))} />)
    expect(needsBox().getByText('Agents are sorting it out · 1')).toBeTruthy()
    expect(needsBox().getByText(/kept/).textContent).toMatch(/^maria’s .+ kept src\/ledger\.rs: “Mid-refactor”\.$/)
  })
})

describe('Headline', () => {
  it('counts only moving tasks as worked on, says why the rest are still, and matches the Needs you count', () => {
    const ov = overview([
      item('a', 'maria', { claim_status: 'active' }),
      item('b', 'dev', { claim_status: 'active' }),
      item('c', 'dev', { state: 'blocked' }),
      item('e', 'sam', { state: 'paused' }),
      item('f', 'sam', { state: 'handover', claim_status: 'active' }),
    ])
    render(page(ov))
    expect(document.querySelector('.answer-text')!.textContent).toBe('5 unfinished tasks: 2 moving, 3 stuck. 2 things need you.')
    expect(document.querySelector('.answer-fact')!.textContent).toBe('Stuck: 1 blocked, 1 paused, 1 waiting for another agent. 0 agents are connected.')
    expect(dashboardStatus(ov, d, null, null).home?.text).toBe('Waiting on you')
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
