import { fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'
import type { Derived } from '../../src/derive.ts'
import { DashboardPage } from '../../src/pages/Dashboard.tsx'
import type { WorkItem, WorkOverview } from '../../src/standup.ts'
import { TourProvider } from '../../src/tour/Tour.tsx'
import type { HubData } from '../../src/useHub.ts'
import type { Poll } from '../../src/usePoll.ts'

const TOUR = 'agentscm.tour.v1'
const now = Math.floor(Date.now() / 1000)
const item = (id: string, login: string, over: Partial<WorkItem> = {}): WorkItem =>
  ({
    id: `wrk_${id.repeat(32)}`, title: `Task ${id}`, state: 'active', claim_status: 'active', updated_at: now - 60, last_checkin_at: now - 60, blocker: null, scope: ['src/a.rs'], next_action: null, current: null, branch: null,
    lead_agent: 'claude-code', lead_token: 't', lead_session: 's', owner_login: login, participants: [{ login, agent: 'claude-code', token: 't', session: 's', role: 'lead', owner: `github:github.com:${login}` }], ...over,
  }) as unknown as WorkItem
const overview = (current: WorkItem[]) => ({ current, recent: [], overlaps: [], current_complete: true, recent_complete: true, overlaps_complete: true }) as unknown as WorkOverview
const d = { agents: [], groups: [], filesHeld: 0, contested: [], prOverlaps: [], otherOpenPrs: 0, intentById: new Map() } as unknown as Derived
const data = { whoami: { owner: { login: 'maria', provider: 'github', host: 'github.com' }, token: { repository: 'acme/widgets' } }, failing: false, activity: null, fresh: new Set(), lastOk: null, health: null, work: null } as unknown as HubData
const ready = (ov: WorkOverview): Poll<WorkOverview> => ({ kind: 'ready', value: ov }) as Poll<WorkOverview>
// maria's own blocked task: the primary action is an Open link, and the blank prompt keeps Copy in the menu.
const blocked = (login = 'maria') => item('b', login, { state: 'blocked', blocker: { summary: 'Waiting on the API team', at: now - 60 } as WorkItem['blocker'] })
const page = (ov: WorkOverview) => (
  <TourProvider section="home" data={data} d={d} overview={ov}>
    <DashboardPage section="home" question="q" token="t" repo="acme/widgets" data={data} d={d} overview={ready(ov)} />
  </TourProvider>
)
afterEach(() => localStorage.setItem(TOUR, '{"welcomeSeen":true,"welcomeOff":true,"autoplay":[],"sectionsSeen":[]}'))
const before = (a: Element, b: Element) => !!(a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING)

describe('Needs you comes first', () => {
  it('leads the page when something needs a person, with Start a task as one collapsed line after it', () => {
    render(page(overview([blocked()])))
    const needs = document.getElementById('box-needs')!
    const start = document.getElementById('box-start')!
    expect(before(needs, start)).toBe(true)
    const toggle = within(start).getByRole('button', { name: /Start a task/ })
    expect(toggle.getAttribute('aria-expanded')).toBe('false')
    fireEvent.click(toggle)
    expect(toggle.getAttribute('aria-expanded')).toBe('true')
    expect(start.querySelector('.reveal')!.getAttribute('data-open')).toBe('true')
  })
  it('leaves Start a task first when nothing needs a person', () => {
    render(page(overview([item('a', 'dev')])))
    expect(before(document.getElementById('box-start')!, document.getElementById('box-needs')!)).toBe(true)
  })
})

describe('a Needs you row', () => {
  const row = () => document.querySelector<HTMLElement>('.need')!

  it('is one line with a single primary action, and opens in place for the reason, who decides and the words box', () => {
    render(page(overview([blocked()])))
    const chev = within(row()).getByRole('button', { name: /Show details/ })
    expect(chev.getAttribute('aria-expanded')).toBe('false')
    expect(row().querySelector('.reveal')!.getAttribute('data-open')).toBe('false')
    expect(row().querySelector('.reveal-in')!.hasAttribute('inert')).toBe(true) // closed: out of the tab order
    fireEvent.click(chev)
    expect(chev.getAttribute('aria-expanded')).toBe('true')
    expect(chev.getAttribute('aria-controls')).toBe(row().querySelector('.reveal-in')!.id)
    expect(row().querySelector('.reveal')!.getAttribute('data-open')).toBe('true')
    expect(row().querySelector('.reveal-in')!.hasAttribute('inert')).toBe(false)
    expect(within(row()).getByText(/“Waiting on the API team”/)).toBeTruthy()
    expect(within(row()).getByText('Decision for maria')).toBeTruthy()
    expect(within(row()).getByLabelText('Your decision')).toBeTruthy()
  })

  it('opens from a click on its own text, but a click on its link only follows the link', () => {
    render(page(overview([blocked()])))
    fireEvent.click(row().querySelector('.attn-title')!)
    expect(row().classList.contains('is-open')).toBe(true)
    fireEvent.click(row().querySelector('.attn-title')!)
    expect(row().classList.contains('is-open')).toBe(false)
    fireEvent.click(within(row()).getByRole('link', { name: 'Task b' }))
    expect(row().classList.contains('is-open')).toBe(false)
  })

  it('offers Copy as the primary action while a blank is unfilled, keeps Open in the menu, and promotes Open once the words are typed', () => {
    render(page(overview([blocked()])))
    const acts = () => row().querySelector<HTMLElement>('.need-acts')!
    // claude-code is the viewer's own agent: Open exists but the prompt still has its blank.
    expect(within(acts()).getByRole('button', { name: /^Copy prompt for maria’s claude-code$/ })).toBeTruthy()
    expect(within(acts()).queryByRole('link')).toBeNull()
    fireEvent.click(within(acts()).getByRole('button', { name: 'More actions' }))
    const open = within(acts()).getByRole('menuitem', { name: /Open in Claude Code/ }) as HTMLButtonElement
    expect(open.disabled).toBe(true)
    fireEvent.change(within(row()).getByLabelText('Your decision'), { target: { value: 'use backoff' } })
    const link = within(row()).getByRole('link', { name: /Open in Claude Code/ })
    expect(link.getAttribute('href')).toMatch(/^claude-cli:\/\/open\?repo=acme\/widgets&q=/)
    expect(link.classList.contains('need-primary')).toBe(true)
    expect(within(acts()).getByRole('menuitem', { name: /^Copy prompt for maria’s claude-code$/ })).toBeTruthy() // the menu is still open, and every action stays reachable
  })

  it('has no menu when there is only one action, and none for a decision with no prompt', () => {
    render(page(overview([blocked('dev')])))
    expect(within(row()).getByRole('button', { name: /^Copy prompt for dev’s claude-code$/ })).toBeTruthy()
    expect(within(row()).queryByRole('button', { name: 'More actions' })).toBeNull()
  })

  it('keeps Hide reachable inside the opened row, and Unhide on the row while it is hidden', () => {
    const stale = item('c', 'dev', { claim_status: 'stale', updated_at: now - 3 * 86400, last_checkin_at: now - 3 * 86400 })
    render(page(overview([stale])))
    const hide = within(row()).getByRole('button', { name: 'Hide this item in this browser' })
    expect(hide.closest('.reveal-in')!.hasAttribute('inert')).toBe(true) // inside the row, reachable once it is open
    fireEvent.click(within(row()).getByRole('button', { name: /Show details/ }))
    expect(hide.closest('.reveal-in')!.hasAttribute('inert')).toBe(false)
    fireEvent.click(hide)
    fireEvent.click(screen.getByRole('button', { name: 'Show 1 hidden' }))
    expect(within(row()).getByRole('button', { name: 'Show this item again' })).toBeTruthy()
    localStorage.removeItem('agentscm.hidden.v1')
  })
})

describe('coach marks', () => {
  const fresh = () => localStorage.removeItem(TOUR)
  const marks = () => [...document.querySelectorAll<HTMLElement>('[data-coach]')]
  const stored = () => JSON.parse(localStorage.getItem(TOUR) ?? 'null')

  it('point at the headline, the Needs you list and the track, in page order, without a dialog', () => {
    fresh()
    render(page(overview([blocked()])))
    expect(screen.queryByRole('dialog')).toBeNull()
    expect(marks().map((m) => m.dataset['coach'])).toEqual(['answer', 'needs', 'track'])
    expect(before(document.querySelector('.answer')!, marks()[0]!)).toBe(true)
    expect(before(marks()[1]!, document.getElementById('box-needs')!)).toBe(true)
    expect(before(marks()[2]!, document.getElementById('box-track')!)).toBe(true)
    expect(within(marks()[0]!).getByRole('button', { name: 'Skip tips' })).toBeTruthy()
    expect(within(marks()[1]!).queryByRole('button', { name: 'Skip tips' })).toBeNull() // one skip, on the first
  })

  // A dismissed pointer folds away first (a short animation), then leaves the page.
  it('are dismissed one at a time and remembered, and Skip moves to the next one', async () => {
    fresh()
    render(page(overview([blocked()])))
    fireEvent.click(within(marks()[0]!).getByRole('button', { name: 'Got it' }))
    await waitFor(() => expect(marks().map((m) => m.dataset['coach'])).toEqual(['needs', 'track']))
    expect(stored().sectionsSeen).toEqual(['coach:answer'])
    expect(within(marks()[0]!).getByRole('button', { name: 'Skip tips' })).toBeTruthy()
  })

  it('Skip tips removes the whole set for good, even for the next page load', async () => {
    fresh()
    const first = render(page(overview([blocked()])))
    fireEvent.click(within(marks()[0]!).getByRole('button', { name: 'Skip tips' }))
    await waitFor(() => expect(marks()).toHaveLength(0))
    expect(stored()).toMatchObject({ welcomeSeen: true, welcomeOff: true })
    first.unmount()
    render(page(overview([blocked()])))
    expect(marks()).toHaveLength(0)
  })

  it('never show for a viewer who already finished or switched off the old welcome tour', () => {
    render(page(overview([blocked()])))
    expect(marks()).toHaveLength(0)
  })
})
