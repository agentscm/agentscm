import { act, fireEvent, render, within } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'
import type { Derived } from '../../src/derive.ts'
import { STALE_SECONDS } from '../../src/dismiss.tsx'
import { needHideKey, legacyNeeds as needsYou } from '../../src/needs.ts'
import { DashboardPage, dashboardStatus } from '../../src/pages/Dashboard.tsx'
import type { CoordinationRequest, WorkItem, WorkOverview } from '../../src/standup.ts'
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
const pair = (l: WorkItem, r: WorkItem, path: string) => ({ left: l, right: r, reasons: [{ left_path: path, right_path: path }], reasons_complete: true })
const ready = (ov: WorkOverview): Poll<WorkOverview> => ({ kind: 'ready', value: ov }) as Poll<WorkOverview>

afterEach(() => localStorage.removeItem('agentscm.hidden.v1'))

const page = (ov: WorkOverview) => <DashboardPage section="home" question="q" token="t" repo="acme/widgets" data={data} d={d} overview={ready(ov)} />
const needsBox = () => within(document.getElementById('box-needs')!)

describe('Needs you', () => {
  it('ages a blocker report like the track does, never as a bare clock time', () => {
    render(page(overview([item('a', 'dev', { state: 'blocked', blocker: { at: twoDays, summary: 'Needs a key' } as WorkItem['blocker'] })])))
    const gist = needsBox().getByText(/reported/).textContent
    expect(gist).toContain('reported 2 days ago')
    expect(gist).not.toMatch(/\d:\d\d/)
  })

  it('a stale, paused or handover task is no row, whoever owns it', () => {
    render(page(overview([item('a', 'maria'), item('b', 'dev', { state: 'paused' }), item('c', 'dev', { state: 'handover', claim_status: 'active' })])))
    expect(needsBox().queryByRole('listitem')).toBeNull()
    expect(needsBox().getByText('All clear')).toBeTruthy()
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

  it('tells the owner of a paused task that another agent asked for its file, and counts it', () => {
    const mine = item('a', 'maria', { state: 'paused' })
    const agent = (login: string) => ({ token: `t-${login}`, agent: 'codex', session: 's', owner: login, login })
    const request = { id: 'crq_1', requester: agent('dev'), status: 'pending', responses: [], acknowledgements: [], created_at: now,
      targets: [{ work_id: mine.id, lead: agent('maria'), generation: 1, reasons: [{ your_path: 'ui/src/pages/Catchup.tsx', their_path: 'ui/src/pages/Catchup.tsx' }], reasons_complete: true }] } as unknown as CoordinationRequest
    const asked = { requests: [request], me: 'maria' }
    expect(dashboardStatus(overview([mine]), d, null, null, new Set(), asked).needs.map((n) => n.kind)).toEqual(['asked'])
    render(<DashboardPage section="home" question="q" token="t" repo="acme/widgets" data={data} d={d} overview={ready(overview([mine]))} asked={asked} />)
    expect(needsBox().getByText(/asked for/).textContent).toBe('dev’s Codex asked for ui/src/pages/Catchup.tsx you hold, and your agent isn’t running')
    expect(needsBox().getByText(/Answer for it in the Inbox/)).toBeTruthy()
  })

  it('folds overlaps into one row that leads to the Inbox, still counting each', () => {
    const a = item('a', 'maria', { claim_status: 'active' })
    const b = item('b', 'dev', { claim_status: 'active' })
    const c = item('c', 'sam', { claim_status: 'active' })
    const ov = { ...overview([a, b, c]), overlaps: [pair(a, b, 'src/billing.rs'), pair(a, c, 'src/tax.rs')] } as unknown as WorkOverview
    render(page(ov))
    const box = needsBox()
    expect(box.getAllByRole('listitem')).toHaveLength(1)
    expect(box.getByRole('link', { name: '2 overlaps to talk through' }).getAttribute('href')).toBe('#inbox')
    expect(box.getByRole('link', { name: 'Open the Inbox' }).getAttribute('href')).toBe('#inbox')
    expect(box.getByText('2')).toBeTruthy()
  })

  it('names a single overlap in the singular', () => {
    const [a, b] = [item('a', 'maria', { claim_status: 'active' }), item('b', 'dev', { claim_status: 'active' })]
    render(page({ ...overview([a, b]), overlaps: [pair(a, b, 'src/billing.rs')] } as unknown as WorkOverview))
    expect(needsBox().getByRole('link', { name: '1 overlap to talk through' })).toBeTruthy()
    expect(needsBox().getByText(/^Two tasks plan/)).toBeTruthy()
  })

  it('leaves hidden overlaps out of the folded row and the count, and brings them back dimmed with Unhide', () => {
    const [a, b, c] = [item('a', 'maria', { claim_status: 'active' }), item('b', 'dev', { claim_status: 'active' }), item('c', 'sam', { claim_status: 'active' })]
    const ov = { ...overview([a, b, c]), overlaps: [pair(a, b, 'src/billing.rs'), pair(a, c, 'src/tax.rs')] } as unknown as WorkOverview
    const overlaps = needsYou(ov, d)
    localStorage.setItem('agentscm.hidden.v1', JSON.stringify([needHideKey(overlaps[0]!)]))
    const { unmount } = render(page(ov))
    expect(needsBox().getByRole('link', { name: '1 overlap to talk through' })).toBeTruthy()
    expect(needsBox().getByText('1')).toBeTruthy()
    fireEvent.click(needsBox().getByRole('button', { name: 'Show 1 hidden' }))
    const rows = needsBox().getAllByRole('listitem')
    expect(rows).toHaveLength(2)
    expect(rows.filter((r) => r.classList.contains('is-hidden'))).toHaveLength(1)
    expect(needsBox().getByText('2')).toBeTruthy()
    fireEvent.click(needsBox().getByRole('button', { name: 'Show this overlap again' }))
    expect(needsBox().getByRole('link', { name: '2 overlaps to talk through' })).toBeTruthy()
    unmount()
    localStorage.setItem('agentscm.hidden.v1', JSON.stringify(overlaps.map(needHideKey)))
    render(page(ov))
    expect(needsBox().queryByRole('listitem')).toBeNull()
    expect(needsBox().getByText('All clear')).toBeTruthy()
  })

  it('offers to show all of the needs the box counts, overlaps included', () => {
    const quiet = '012345'.split('').map((id) => item(id, 'dev', { state: 'blocked' }))
    const [a, b, c] = [item('a', 'maria', { claim_status: 'active' }), item('b', 'dev', { claim_status: 'active' }), item('c', 'sam', { claim_status: 'active' })]
    render(page({ ...overview([...quiet, a, b, c]), overlaps: [pair(a, b, 'src/billing.rs'), pair(a, c, 'src/tax.rs')] } as unknown as WorkOverview))
    expect(needsBox().getByText('8')).toBeTruthy()
    fireEvent.click(needsBox().getByRole('button', { name: 'Show all 8' }))
    expect(needsBox().getAllByRole('listitem')).toHaveLength(7)
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
    expect(document.querySelector('.answer-text')!.textContent).toBe('5 unfinished tasks: 2 moving, 3 awaiting review or blocked. 1 thing needs you.')
    expect(document.querySelector('.answer-fact')!.textContent).toBe('Reported: 1 blocked, 1 paused, 1 awaiting handover review. 0 agents are connected.')
    expect(dashboardStatus(ov, d, null, null).home?.text).toBe('Waiting on you')
  })
})

describe('Live overlaps', () => {
  it('only an overlap between live tasks reaches the Needs you count and the Now status', () => {
    const [a, b] = [item('a', 'maria', { claim_status: 'active' }), item('b', 'dev', { claim_status: 'active' })]
    const staleB = item('b', 'dev')
    const live = { ...overview([a, b]), overlaps: [pair(a, b, 'src/a.rs')] } as unknown as WorkOverview
    const quiet = { ...overview([a, staleB]), overlaps: [pair(a, staleB, 'src/a.rs')] } as unknown as WorkOverview
    expect(dashboardStatus(live, d, null, null).home?.text).toBe('Waiting on you')
    expect(dashboardStatus(live, d, null, null).now?.text).toBe('0 contested files · 1 overlap')
    expect(dashboardStatus(quiet, d, null, null).home).toEqual({ text: 'Nothing needs you', tone: 'ok' })
    expect(dashboardStatus(quiet, d, null, null).now).toEqual({ text: 'No contested files', tone: 'muted' })
  })
})

describe('Hide in this browser', () => {
  it('offers Hide only on rows untouched for a day, and hides without asking the hub', () => {
    const fresh = item('f', 'dev', { state: 'blocked', updated_at: now - 60, last_checkin_at: now - 60 })
    render(page(overview([item('a', 'dev', { state: 'blocked' }), fresh])))
    expect(needsBox().getAllByRole('button', { name: /^Hide / })).toHaveLength(1)
    fireEvent.click(needsBox().getByRole('button', { name: /^Hide / }))
    expect(needsBox().queryByText('Task a')).toBeNull()
    expect(needsBox().getByText('Task f')).toBeTruthy()
    expect(localStorage.getItem('agentscm.hidden.v1')).toContain(`blocked:${'wrk_' + 'a'.repeat(32)}@${twoDays}`)
    fireEvent.click(needsBox().getByRole('button', { name: 'Show 1 hidden' }))
    expect(needsBox().getByText('Task a')).toBeTruthy()
    fireEvent.click(needsBox().getByRole('button', { name: 'Show this item again' }))
    expect(needsBox().getByText('Task a')).toBeTruthy()
    expect(needsBox().queryByRole('button', { name: /hidden|again$/ })).toBeNull()
  })

  it('a hidden row comes back by itself once its task changes, and no count includes it while hidden', () => {
    const quiet = item('a', 'dev', { state: 'blocked' })
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
