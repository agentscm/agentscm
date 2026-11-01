// The G layout reuses the Hub's own pieces: its command palette, its work links, its dashboard pages.
import { act, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Derived } from '../../src/derive.ts'
import { gAnswer } from '../../src/g/answers.tsx'
import { GLive, type LiveProps } from '../../src/g/GLive.tsx'
import { gRoute } from '../../src/g/layout.ts'
import { DashboardPage } from '../../src/pages/Dashboard.tsx'
import type { WorkOverview } from '../../src/standup.ts'
import { SECTIONS } from '../../src/App.tsx'
import type { HubEvent } from '../../src/types.ts'
import type { HubData } from '../../src/useHub.ts'
import type { Poll } from '../../src/usePoll.ts'

const W1 = `wrk_${'1'.repeat(32)}`
const nowS = Math.floor(Date.now() / 1000)
const agent = (name: string, owner: string, lastActive: number, files: string[]) => ({ id: `${owner}-${name}`, token: 't', name, owner, via: [], intent: null, task: null, lastActive, holdings: files.map((spec) => ({ spec, contested: false })) })
const agents = [agent('claude-ivy', 'maya', nowS - 30, ['ui/src/App.tsx', 'src/hub/mcp.rs']), agent('codex-orion', 'dev', nowS - 3600, ['src/hub/mcp.rs']), agent('cursor-kit', 'maya', nowS - 60, [])]
const d = { agents, groups: [{ owner: 'maya', agents: [agents[0], agents[2]] }, { owner: 'dev', agents: [agents[1]] }], filesHeld: 2, contested: [{ path: 'src/hub/mcp.rs', holders: [{ id: 'a', owner: 'maya', name: 'claude-ivy' }, { id: 'b', owner: 'dev', name: 'codex-orion' }], repeated: false, handoff: null }], prOverlaps: [], otherOpenPrs: 0, intentById: new Map() } as unknown as Derived
const work = { id: W1, title: 'Bound coordination history', state: 'active', claim_status: 'active', updated_at: nowS - 60, last_checkin_at: nowS - 60, blocker: null, scope: ['src/hub/mcp.rs'], next_action: null, current: null, branch: null, lead_agent: 'codex-orion', lead_token: 't', lead_session: 's', owner_login: 'dev', participants: [{ login: 'dev', agent: 'codex-orion', token: 't', session: 's', role: 'lead', owner: 'github:github.com:dev' }] }
const overview = { current: [work], recent: [], overlaps: [], current_complete: true, recent_complete: true, overlaps_complete: true } as unknown as WorkOverview
const poll = { kind: 'ready', value: overview, fetchedAt: 0, refreshError: null, retry: () => {} } as Poll<WorkOverview>
const data = { whoami: { owner: { login: 'maya', provider: 'github', host: 'github.com' }, token: { repository: 'acme/widgets' } }, failing: false, activity: null, fresh: new Set(), lastOk: nowS * 1000, health: null, work: null } as unknown as HubData
const text = (node: unknown) => render(<p>{node as never}</p>).container.textContent

describe('gRoute', () => {
  it('opens a change only for a valid work id', () => {
    expect(gRoute(`#work/${W1}`)).toEqual({ page: 'change', id: W1 })
    expect(gRoute(`#work/${W1}?tab=proof`)).toEqual({ page: 'change', id: W1 })
    expect(gRoute('#work/wrk_not-an-id')).toBeNull()
    expect(gRoute('#work/finished')).toBeNull()
  })
  it('names the G-only pages', () => {
    expect(gRoute('#changes')).toEqual({ page: 'changes' })
    expect(gRoute('#features')).toEqual({ page: 'features' })
    expect(gRoute('#team')).toBeNull()
  })
})

describe('gAnswer', () => {
  it('has no sentence for Now, which G draws itself, or before the data loads', () => {
    expect(gAnswer('home', d, [], nowS * 1000)).toBeUndefined()
    expect(gAnswer('team', null, null, null)).toBeUndefined()
    expect(gAnswer('activity', d, null, nowS * 1000)).toBeFalsy()
  })
  it('Catch up says when the latest event was', () => {
    expect(text(gAnswer('activity', d, [], nowS * 1000))).toBe('Nothing has been recorded here yet.')
    expect(text(gAnswer('activity', d, [{ at: nowS - 120 } as HubEvent], nowS * 1000))).toMatch(/^The latest event was .*ago\.$/)
  })
  it('Team counts the agents awake in the last five minutes', () => {
    expect(text(gAnswer('team', d, [], nowS * 1000))).toBe('2 of 3 agents awake, across 2 people.')
    expect(text(gAnswer('team', { ...d, agents: [], groups: [] }, [], nowS * 1000))).toBe('No agent is connected right now.')
  })
  it('Held files says how many are held and how many contested', () => {
    expect(text(gAnswer('now', d, [], nowS * 1000))).toBe('2 files held by 2 agents, 1 contested.')
    expect(text(gAnswer('now', { ...d, contested: [] }, [], nowS * 1000))).toBe('2 files held by 2 agents, none contested.')
    expect(text(gAnswer('now', { ...d, filesHeld: 0 }, [], nowS * 1000))).toBe('No agent holds a file right now.')
  })
})

describe('DashboardPage answer', () => {
  beforeEach(() => vi.stubGlobal('fetch', async () => new Response('{}', { status: 404 })))
  afterEach(() => vi.unstubAllGlobals())
  const page = (answer?: React.ReactNode) => <DashboardPage section="team" question="Who is working right now?" token="t" repo="acme/widgets" data={data} d={d} overview={poll} answer={answer} />
  it('keeps the shared headline unless a layout supplies its own', () => {
    const { container, rerender } = render(page())
    expect(container.querySelector('.answer-text')!.textContent).toContain('1 unfinished task')
    rerender(page('Two agents are awake.'))
    expect(container.querySelector('.answer-text')!.textContent).toBe('Two agents are awake.')
  })
})

describe('G palette', () => {
  beforeEach(() => {
    vi.stubGlobal('fetch', async () => new Response('{}', { status: 404 }))
    // jsdom does not implement scrolling; the layout scrolls to the top on each page.
    vi.spyOn(window, 'scrollTo').mockImplementation(() => {})
  })
  afterEach(() => {
    vi.unstubAllGlobals()
    vi.restoreAllMocks()
  })
  const live = { token: 't', repo: 'acme/widgets', data, d, overview: poll, needs: [], handoff: { me: 'maya', repo: 'acme/widgets', originUrl: null } } as LiveProps
  const open = (showAdmin: boolean) => {
    render(<GLive live={live} section="home" sections={SECTIONS} page={() => null} showAdmin={showAdmin} hubSignedOut={false} onClassic={() => {}} />)
    fireEvent.keyDown(window, { key: 'k', ctrlKey: true })
    return screen.getByRole('dialog', { name: 'Search the hub' })
  }
  const labels = (dialog: HTMLElement) => within(dialog).getAllByRole('option').map((o) => o.querySelector('.palette-label')!.textContent)

  it('lists the rail’s pages, with the G-only ones', () => {
    const dialog = open(false)
    expect(within(dialog).getAllByRole('option').every((o) => o.closest('[role="listbox"]'))).toBe(true)
    const all = labels(dialog)
    for (const page of ['Needs you', 'Changes', 'Process', 'Code map', 'Held files', 'Contributions', 'Feature map', 'All repositories']) expect(all).toContain(page)
    expect(all).not.toContain('Admin')
  })
  it('keeps Admin for admins', () => {
    const dialog = open(true)
    fireEvent.change(within(dialog).getByRole('combobox'), { target: { value: 'admin' } })
    expect(labels(dialog)).toEqual(['Admin'])
  })
  it('finds agents, held files and work, grouped', () => {
    const dialog = open(false)
    fireEvent.change(within(dialog).getByRole('combobox'), { target: { value: 'mcp' } })
    expect(within(dialog).getByText('Held files', { selector: '.palette-group' })).toBeTruthy()
    expect(labels(dialog)).toEqual(['src/hub/mcp.rs'])
    fireEvent.change(within(dialog).getByRole('combobox'), { target: { value: 'coordination' } })
    expect(within(dialog).getByText('Work', { selector: '.palette-group' })).toBeTruthy()
  })
  it('hands focus back to the search button and keeps Tab on the input', async () => {
    render(<GLive live={live} section="home" sections={SECTIONS} page={() => null} showAdmin={false} hubSignedOut={false} onClassic={() => {}} />)
    const pill = screen.getByRole('button', { name: 'Search the Hub' })
    pill.focus()
    fireEvent.click(pill)
    const dialog = screen.getByRole('dialog', { name: 'Search the hub' })
    const input = within(dialog).getByRole('combobox')
    expect(document.activeElement).toBe(input)
    fireEvent.keyDown(dialog, { key: 'Tab' })
    expect(document.activeElement).toBe(input)
    fireEvent.keyDown(input, { key: 'Escape' })
    await act(async () => {})
    expect(screen.queryByRole('dialog')).toBeNull()
    expect(document.activeElement).toBe(pill)
  })
})
