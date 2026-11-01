import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { DeviceAuth } from '../../src/devices.ts'
import { RepositoriesPage } from '../../src/pages/Repositories.tsx'
import type { WorkItem, WorkOverview } from '../../src/standup.ts'

const AUTH = { kind: 'ready', value: { kind: 'browser', session: { login: 'example-user', provider: 'github', admin: false, csrf: 'csrf-token-0123456789' } } } as const satisfies { kind: 'ready'; value: DeviceAuth }
const ROW = (repository: string, extra: Record<string, unknown> = {}) => ({ repository, repository_id: 1, provider: 'github', unfinished: { total: 0, moving: 0, stuck: 0 }, needs_you: 0, agents: 0, last_activity_at: null, complete: true, ...extra })

const ITEM = (id: string, extra: Record<string, unknown> = {}) => ({ id, owner_login: 'maria', title: `Task ${id}`, state: 'active', claim_status: 'active', last_checkin_at: Math.floor(Date.now() / 1000) - 300, lead_agent: 'claude-code', ...extra }) as unknown as WorkItem
const OVERVIEW = (current: WorkItem[]) => ({ current, recent: [], overlaps: [], recent_since: 0, until: 86_400, recent_complete: true, history_retention_gap: false }) as unknown as WorkOverview

function stubList(body: unknown) {
  const calls: string[] = []
  vi.stubGlobal('fetch', async (url: string) => {
    calls.push(url)
    return new Response(JSON.stringify(body), { status: url === '/v1/hub/session/repositories' ? 200 : 404, headers: { 'content-type': 'application/json' } })
  })
  return calls
}
afterEach(() => {
  vi.unstubAllGlobals()
  window.location.hash = ''
})

describe('All repositories', () => {
  const now = () => Math.floor(Date.now() / 1000)

  it('names the repositories that need you, leads with the one with most waiting, and groups the rest', async () => {
    stubList({ repositories: [
      ROW('example-org/ops', { needs_you: 2, unfinished: { total: 1, moving: 0, stuck: 1 }, agents: 1, last_activity_at: now() - 3600 }),
      ROW('example-org/api', { needs_you: 2, unfinished: { total: 3, moving: 1, stuck: 2 }, agents: 4, last_activity_at: now() - 60 }),
      ROW('example-org/docs', { complete: false, agents: 1 }),
      ROW('example-org/slow', { unfinished: { total: 2, moving: 0, stuck: 1 }, last_activity_at: now() - 86_400 }),
      ROW('example-org/busy', { complete: false, unfinished: { total: 4, moving: 3, stuck: 0 }, agents: 6 }),
      ROW('example-org/old', { last_activity_at: now() - 30 * 86_400 }),
      ROW('example-org/empty'),
      ROW('not a repo'),
    ], unchecked: 1, complete: true })
    render(<RepositoriesPage auth={AUTH} web={{ repos: [], pick: vi.fn(), refusal: null }} current={null} />)
    const focus = await screen.findByRole('region', { name: /Needs you most/ })
    // A tie on waiting-on-you goes to the most recent activity, and the card is for that repository only.
    expect(focus.getAttribute('aria-label')).toBe('Needs you most: example-org/api')
    expect(focus.querySelector('h2')?.textContent).toBe('2 things are waiting on you.')
    expect(focus.querySelector('.rp-facts')?.textContent).toContain('2 stuck')
    expect(focus.querySelector('.rp-facts')?.textContent).toContain('1 moving')
    expect(focus.querySelector('.rp-facts')?.textContent).toContain('4 agents connected')
    expect(focus.querySelector('.rp-facts')?.textContent).toContain('active')
    expect(document.querySelector('.answer-text')?.textContent).toBe('api and ops need you.')
    expect(within(document.querySelector('.answer-text') as HTMLElement).getAllByRole('button').map((b) => b.textContent)).toEqual(['api', 'ops'])
    expect(document.querySelector('.answer-fact')?.textContent).toBe('Start with api: it has the most waiting on you.')
    const rowsOf = (title: string) => within(screen.getByRole('list', { name: title })).getAllByRole('listitem')
    const names = (title: string) => rowsOf(title).map((r) => r.querySelector('.rp-rname')?.textContent)
    expect(names('Also needs you')).toEqual(['example-org/ops'])
    expect(rowsOf('Also needs you')[0]!.querySelector('.rp-say')?.textContent).toBe('2 waiting on you, 1 stuck')
    expect(names('Stuck')).toEqual(['example-org/slow'])
    expect(names('Moving')).toEqual(['example-org/docs', 'example-org/busy'])
    // Lower bounds say so; a connected repository with nothing in progress says that, not a list of zeros.
    expect(rowsOf('Moving')[0]!.querySelector('.rp-say')?.textContent).toBe('Connected, nothing in progress')
    expect(rowsOf('Moving')[1]!.querySelector('.rp-say')?.textContent).toBe('at least 3 moving')
    // Each group head carries its count; the repositories in the focus card are not listed twice.
    expect(screen.getByRole('heading', { name: 'Also needs you' }).parentElement?.querySelector('.rp-n')?.textContent).toBe('1')
    expect(screen.queryByRole('button', { name: 'Open example-org/api' })).toBeNull()
    expect(screen.getByRole('button', { name: /2 quiet repositories/ }).getAttribute('aria-expanded')).toBe('false')
    expect(screen.getByText('1 repository could not be checked')).toBeTruthy()
    expect(document.getElementById('repo-example-org/busy')?.textContent).toBe('Nothing needs you; at least 4 unfinished, 0 stuck; 6 agents connected.')
  })

  it('says nothing needs you, names where agents work, and lists the active repositories as one group', async () => {
    stubList({ repositories: [ROW('example-org/api', { agents: 2, unfinished: { total: 3, moving: 3, stuck: 0 }, last_activity_at: now() - 90 }), ROW('example-org/docs', { agents: 1 }), ROW('example-org/recent', { last_activity_at: now() - 86_400 }), ROW('example-org/old', { last_activity_at: now() - 30 * 86_400 })], unchecked: 0, complete: true })
    render(<RepositoriesPage auth={AUTH} web={{ repos: [], pick: vi.fn(), refusal: null }} current={null} />)
    await screen.findByRole('list', { name: 'Moving on their own' })
    expect(document.querySelector('.answer-text')?.textContent).toBe('Nothing needs you.')
    expect(document.querySelector('.answer-fact')?.textContent).toBe('Agents are working in api and docs.')
    expect(document.querySelector('.rp-checked')?.textContent).toBe('Counts only, checked against your role on the forge.')
    expect(screen.queryByRole('region', { name: /Needs you most/ })).toBeNull()
    expect(screen.queryByRole('list', { name: 'Stuck' })).toBeNull()
    const rows = within(screen.getByRole('list', { name: 'Moving on their own' })).getAllByRole('listitem')
    expect(rows.map((r) => r.querySelector('.rp-rname')?.textContent)).toEqual(['example-org/api', 'example-org/recent', 'example-org/docs'])
    expect(rows[0]!.textContent).toContain('3 moving')
    expect(rows[1]!.textContent).toContain('No agent connected')
    expect(screen.getByRole('button', { name: /1 quiet repository/ })).toBeTruthy()
  })

  it('opens the focus repository from its button through the page’s own open path', async () => {
    stubList({ repositories: [ROW('example-org/api', { needs_you: 1, agents: 1 }), ROW('example-org/docs', { agents: 1 })], unchecked: 0, complete: true })
    const pick = vi.fn()
    const web = { repos: ['example-org/api', 'example-org/docs'], pick, refusal: null }
    const { rerender } = render(<RepositoriesPage auth={AUTH} web={web} current="example-org/docs" />)
    const focus = await screen.findByRole('region', { name: /Needs you most/ })
    expect(focus.querySelector('h2')?.textContent).toBe('1 thing is waiting on you.')
    const go = within(focus).getByRole('button', { name: 'Open what needs you in example-org/api' })
    expect(go.textContent).toBe('Open what needs you')
    fireEvent.click(go)
    expect(pick).toHaveBeenCalledWith('example-org/api')
    expect(go.getAttribute('aria-busy')).toBe('true')
    expect(go.textContent).toBe('Opening its Dashboard…')
    rerender(<RepositoriesPage auth={AUTH} web={web} current="example-org/api" />)
    await waitFor(() => expect(window.location.hash).toBe('#home'))
  })

  it('opens the quiet line into a list the filter narrows, and opens a repository from it', async () => {
    stubList({ repositories: [ROW('example-org/api', { agents: 1 }), ROW('example-org/docs'), ROW('example-org/design'), ROW('other-org/notes')], unchecked: 0, complete: true })
    const pick = vi.fn()
    render(<RepositoriesPage auth={AUTH} web={{ repos: [], pick, refusal: null }} current="example-org/api" />)
    const toggle = await screen.findByRole('button', { name: /3 quiet repositories/ })
    expect(toggle.textContent).toContain('Nothing in progress')
    const drawer = document.getElementById(toggle.getAttribute('aria-controls')!)!
    expect(drawer.hasAttribute('inert')).toBe(true)
    fireEvent.click(toggle)
    expect(toggle.getAttribute('aria-expanded')).toBe('true')
    expect(drawer.hasAttribute('inert')).toBe(false)
    const names = () => within(drawer).queryAllByRole('button').map((b) => b.getAttribute('aria-label'))
    expect(names()).toEqual(['Open example-org/docs', 'Open example-org/design', 'Open other-org/notes'])
    fireEvent.change(screen.getByRole('searchbox', { name: 'Filter quiet repositories' }), { target: { value: ' DE ' } })
    expect(names()).toEqual(['Open example-org/design'])
    fireEvent.change(screen.getByRole('searchbox', { name: 'Filter quiet repositories' }), { target: { value: 'nothing' } })
    expect(names()).toEqual([])
    expect(within(drawer).getByText('No quiet repository matches “nothing”.')).toBeTruthy()
    fireEvent.change(screen.getByRole('searchbox', { name: 'Filter quiet repositories' }), { target: { value: 'notes' } })
    fireEvent.click(within(drawer).getByRole('button', { name: 'Open other-org/notes' }))
    expect(pick).toHaveBeenCalledWith('other-org/notes')
    expect(within(drawer).getByText('Opening…')).toBeTruthy()
  })

  it('with no agent anywhere, leads with connecting one: a repository picker and the agent row for it', async () => {
    stubList({ repositories: [ROW('example-org/api', { unfinished: { total: 1, moving: 1, stuck: 0 } }), ROW('example-org/docs'), ROW('example-org/web')], unchecked: 0, complete: true })
    render(<RepositoriesPage auth={AUTH} web={{ repos: [], pick: vi.fn(), refusal: null }} current="example-org/docs" />)
    expect((await screen.findByRole('region', { name: 'Connect an agent' })).textContent).toContain('Open it in your agent for example-org/docs')
    expect(document.querySelector('.answer-text')?.textContent).toBe('No agent is working in any of these yet.')
    expect(document.querySelector('.answer-fact')?.textContent).toBe('Connect one and it shows up here, with what needs you first.')
    expect(screen.getByRole('button', { name: /2 quiet repositories/ }).textContent).toContain('No agents or unfinished work yet')
    // The repository with unfinished work still gets its row.
    expect(screen.getByRole('list', { name: 'Not connected yet' })).toBeTruthy()
    expect(screen.getByRole('button', { name: 'Open example-org/api' })).toBeTruthy()
    expect(screen.queryByText('Catch up with your agent')).toBeNull()
    const claude = () => screen.getAllByRole('link', { name: /Claude Code/ })[0]!.getAttribute('href')!
    expect(decodeURIComponent(claude())).toContain('Work in my checkout of example-org/docs')
    for (const name of ['Codex', 'Cursor', 'Copilot', 'Gemini', 'Grok']) expect(screen.getByText(name)).toBeTruthy()
    const picker = screen.getByRole('combobox', { name: 'Repository' })
    fireEvent.focus(picker)
    fireEvent.change(picker, { target: { value: 'web' } })
    fireEvent.keyDown(picker, { key: 'ArrowDown' })
    fireEvent.keyDown(picker, { key: 'Enter' })
    await waitFor(() => expect(screen.getByRole('region', { name: 'Connect an agent' }).textContent).toContain('Open it in your agent for example-org/web'))
    expect(decodeURIComponent(claude())).toContain('Work in my checkout of example-org/web')
  })

  it('opens a row through the web sign-in and goes to its Dashboard once its token is in', async () => {
    stubList({ repositories: [ROW('example-org/api', { agents: 1 }), ROW('example-org/docs', { agents: 1 })], unchecked: 0, complete: true })
    const pick = vi.fn()
    const web = { repos: ['example-org/api', 'example-org/docs'], pick, refusal: null }
    const { rerender } = render(<RepositoriesPage auth={AUTH} web={web} current="example-org/api" />)
    fireEvent.click(await screen.findByRole('button', { name: 'Open example-org/docs' }))
    expect(pick).toHaveBeenCalledWith('example-org/docs')
    expect(window.location.hash).toBe('')
    rerender(<RepositoriesPage auth={AUTH} web={web} current="example-org/docs" />)
    await waitFor(() => expect(window.location.hash).toBe('#home'))
  })

  it('stays on the page and says why when the Hub refuses the token', async () => {
    stubList({ repositories: [ROW('example-org/api', { agents: 1 }), ROW('example-org/docs', { agents: 1 })], unchecked: 0, complete: true })
    const web = { repos: ['example-org/api', 'example-org/docs'], pick: vi.fn(), refusal: null }
    const { rerender } = render(<RepositoriesPage auth={AUTH} web={web} current="example-org/api" />)
    fireEvent.click(await screen.findByRole('button', { name: 'Open example-org/docs' }))
    const refusal = { code: 'insufficient_role', message: 'You no longer have read access to example-org/docs.', fix: null }
    rerender(<RepositoriesPage auth={AUTH} web={{ ...web, repos: ['example-org/api'], refusal }} current="example-org/api" />)
    expect(await screen.findByText(/no longer have read access/)).toBeTruthy()
    expect(window.location.hash).toBe('')
  })

  it('shows the list the web sign-in already fetched without asking again, then refreshes on the slow poll', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] })
    try {
      const calls = stubList({ repositories: [ROW('example-org/api', { agents: 1 }), ROW('example-org/new', { agents: 1 })], unchecked: 0, complete: true })
      const list = { rows: [{ repository: 'example-org/api', provider: 'github', unfinished: { total: 0, moving: 0, stuck: 0 }, needsYou: 1, agents: 1, lastActivityAt: null, complete: true }], unchecked: 0, complete: true }
      const web = { repos: ['example-org/api'], pick: vi.fn(), refusal: null, list: { value: list, fetchedAt: Date.now() } }
      render(<RepositoriesPage auth={AUTH} web={web} current={null} />)
      // Rows at once, no skeleton and no second request on mount.
      expect(screen.getByRole('button', { name: 'Open what needs you in example-org/api' })).toBeTruthy()
      expect(document.querySelector('.answer-text')?.textContent).toBe('api needs you.')
      expect(calls).toEqual([])
      await act(async () => { await vi.advanceTimersByTimeAsync(119_000) })
      expect(calls).toEqual([])
      await act(async () => { await vi.advanceTimersByTimeAsync(1_000) })
      expect(calls).toEqual(['/v1/hub/session/repositories'])
      expect(screen.getByRole('button', { name: 'Open example-org/new' })).toBeTruthy()
    } finally {
      vi.useRealTimers()
    }
  })

  it('asks at once when the handed-over list is older than the poll', async () => {
    const calls = stubList({ repositories: [ROW('example-org/new', { agents: 1 })], unchecked: 0, complete: true })
    const list = { rows: [], unchecked: 0, complete: true }
    const web = { repos: [], pick: vi.fn(), refusal: null, list: { value: list, fetchedAt: Date.now() - 121_000 } }
    render(<RepositoriesPage auth={AUTH} web={web} current={null} />)
    await screen.findByRole('button', { name: 'Open example-org/new' })
    expect(calls).toEqual(['/v1/hub/session/repositories'])
  })

  it('has no recap prompt: no window toggle, no agent hand-off, no "What your agent receives" panel', async () => {
    stubList({ repositories: [ROW('example-org/api', { agents: 1 })], unchecked: 0, complete: true })
    render(<RepositoriesPage auth={AUTH} web={{ repos: [], pick: vi.fn(), refusal: null }} current={null} />)
    await screen.findByRole('list', { name: 'Moving on their own' })
    expect(screen.queryByText('Catch up with your agent')).toBeNull()
    expect(screen.queryByRole('group', { name: 'What your agent receives' })).toBeNull()
    expect(screen.queryByRole('button', { name: '24 hours' })).toBeNull()
    expect(document.querySelector('.rp-sec')).toBeNull()
  })

  describe('Done lately', () => {
    const REPO = 'example-org/api'
    const wid = (c: string) => `wrk_${c.repeat(32)}`
    const outcome = (c: string, seq: number, secsAgo: number, kind: 'completion' | 'decision' = 'completion', extra: Record<string, unknown> = {}) =>
      ({ seq, at: now() - secsAgo, kind, item: ITEM(wid(c).slice(0, 36), { id: wid(c), title: `Shipped ${c}`, owner_login: 'sam', lead_agent: 'copilot', current: { summary: `Summary ${c}` }, ...extra }) })
    const ov = (recent: unknown[], extra: Record<string, unknown> = {}) => ({ current: [], overlaps: [], recent, recent_since: now() - 86_400, until: now(), recent_complete: true, history_retention_gap: false, ...extra }) as unknown as WorkOverview
    const brief = (over: Record<string, unknown> = {}) => ({
      id: 'b1', digest: 'b'.repeat(64), status: 'ready', generated_at: now() - 600, generated_by: 'claude-code',
      window: { event_from: 1, event_through: 2, merge_from: 0, merge_through: 0, subscriber: 'human', complete: true },
      sources: [{ id: 's1', label: 'Shipped a', work_id: wid('1'), commit: null, pull_number: null }], warnings: [],
      output: { headline: 'Invoice rounding shipped.', headline_references: ['s1'], facts: [{ text: 'Done.', references: ['s1'] }], implications: [] }, ...over,
    })
    function stubHub(narrative: unknown | null) {
      const calls: string[] = []
      vi.stubGlobal('fetch', async (url: string, init?: RequestInit) => {
        calls.push(`${url}|${(init?.headers as Record<string, string> | undefined)?.Authorization ?? ''}`)
        if (url === '/v1/hub/session/repositories') return new Response(JSON.stringify({ repositories: [ROW(REPO, { agents: 1 })], unchecked: 0, complete: true }), { status: 200 })
        if (url.startsWith('/v1/hub/standup/narrative') && narrative !== null) return new Response(JSON.stringify(narrative), { status: 200 })
        return new Response('{}', { status: 404 })
      })
      return calls
    }
    const mount = (overview: WorkOverview | null, token = 'read-token') =>
      render(<RepositoriesPage auth={AUTH} web={{ repos: [], pick: vi.fn(), refusal: null }} current={REPO} overview={overview} token={token} />)
    const section = async () => (await screen.findByRole('heading', { name: 'Done in api lately' })).closest('section') as HTMLElement

    it('lists reported outcomes newest first, at most six, with who, the checkpoint summary and a link to all finished work', async () => {
      stubHub(null)
      const recent = [outcome('1', 1, 7000), outcome('2', 7, 60), outcome('3', 3, 3000, 'decision'), outcome('4', 4, 2000), outcome('5', 5, 1500), outcome('6', 6, 900), outcome('7', 2, 5000)]
      mount(ov(recent))
      const sec = await section()
      expect(sec.textContent).toContain('Reported in the last day, newest first.')
      const rows = within(within(sec).getByRole('list')).getAllByRole('listitem')
      expect(rows.map((r) => r.querySelector('.rp-done-t')?.textContent)).toEqual(['Shipped 2', 'Shipped 6', 'Shipped 5', 'Shipped 4', 'Shipped 3', 'Shipped 7'])
      expect(rows[0]!.querySelector('.rp-done-t')?.getAttribute('href')).toBe(`#work/${wid('2')}`)
      expect(rows[0]!.querySelector('.rp-done-who')?.textContent).toBe('Finished · sam’s Copilot')
      expect(rows[0]!.querySelector('.rp-done-say')?.textContent).toBe('Summary 2')
      expect(rows[0]!.querySelector('.rp-done-at')?.textContent).toMatch(/min.* ago|now/)
      expect(rows[4]!.querySelector('.rp-done-who')?.textContent).toBe('Decision · sam’s Copilot')
      expect(within(sec).getByRole('link', { name: /See all finished work in api/ }).getAttribute('href')).toBe('#work/finished')
      expect(sec.textContent).not.toContain('This list is partial')
    })

    it('says plainly when nothing was reported finished, and leaves out the briefing when none is ready', async () => {
      const calls = stubHub(brief({ status: 'pending', generated_at: null, generated_by: null, output: null, sources: [] }))
      mount(ov([]))
      const sec = await section()
      expect(sec.textContent).toContain('Nothing was reported finished in api in the last day.')
      expect(within(sec).queryByRole('list')).toBeNull()
      await waitFor(() => expect(calls.some((c) => c.includes('/standup/narrative'))).toBe(true))
      await act(async () => { await new Promise((r) => setTimeout(r, 20)) })
      expect(sec.querySelector('.rp-brief')).toBeNull()
    })

    it('says the list is partial when the overview says so', async () => {
      stubHub(null)
      mount(ov([outcome('1', 1, 60)], { recent_complete: false }))
      expect((await section()).textContent).toContain('This list is partial. The overview limit was reached; more completions exist.')
    })

    it('names the retention gap when history was compacted', async () => {
      stubHub(null)
      mount(ov([outcome('1', 1, 60)], { recent_complete: false, history_retention_gap: true }))
      expect((await section()).textContent).toContain('Some outcomes in this window were compacted from retained history.')
    })

    it('states a window longer than a day in the overview’s own terms', async () => {
      stubHub(null)
      mount(ov([], { recent_since: now() - 7 * 86_400 }))
      expect((await section()).textContent).toContain('Nothing was reported finished in api in the last 7 days.')
    })

    it('shows the ready briefing headline above the list, with who wrote it and the way to read it in full', async () => {
      const calls = stubHub(brief())
      mount(ov([outcome('1', 1, 60)]))
      const sec = await section()
      await waitFor(() => expect(sec.querySelector('.rp-brief')).toBeTruthy())
      const b = sec.querySelector('.rp-brief')!
      expect(b.querySelector('.rp-brief-h')?.textContent).toBe('Invoice rounding shipped.')
      expect(b.querySelector('.rp-brief-by')?.textContent).toMatch(/^Written by Claude Code · .+ · Read the full briefing$/)
      expect(within(b as HTMLElement).getByRole('link', { name: 'Read the full briefing' }).getAttribute('href')).toBe('#activity')
      expect(b.compareDocumentPosition(sec.querySelector('.rp-done')!) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
      expect(calls.filter((c) => c.startsWith('/v1/hub/standup/narrative'))).toEqual(['/v1/hub/standup/narrative?subscriber=human|Bearer read-token'])
    })

    it('asks for no briefing without a token, and shows nothing while the overview is unknown', async () => {
      const calls = stubHub(brief())
      const { rerender } = mount(ov([outcome('1', 1, 60)]), '')
      await section()
      expect(calls.some((c) => c.includes('/standup/narrative'))).toBe(false)
      rerender(<RepositoriesPage auth={AUTH} web={{ repos: [], pick: vi.fn(), refusal: null }} current={REPO} overview={null} token="read-token" />)
      expect(screen.queryByRole('heading', { name: 'Done in api lately' })).toBeNull()
    })

    it('is left out with no open repository', async () => {
      stubHub(null)
      render(<RepositoriesPage auth={AUTH} web={{ repos: [], pick: vi.fn(), refusal: null }} current={null} overview={ov([outcome('1', 1, 60)])} token="read-token" />)
      await screen.findByRole('list', { name: 'Moving on their own' })
      expect(screen.queryByRole('heading', { name: /Done in/ })).toBeNull()
    })
  })

  it('shows who is working in the open repository from the overview, hides stale work, and points elsewhere', async () => {
    stubList({ repositories: [ROW('example-org/api', { agents: 2, unfinished: { total: 3, moving: 3, stuck: 0 } }), ROW('example-org/ops', { agents: 3 }), ROW('example-org/docs', { agents: 0, unfinished: { total: 1, moving: 1, stuck: 0 } })], unchecked: 0, complete: true })
    const overview = OVERVIEW([
      ITEM('a', { owner_login: 'maria', title: 'Refactor billing' }),
      ITEM('b', { owner_login: 'dev', title: 'Docs for webhooks', state: 'blocked' }),
      ITEM('c', { owner_login: 'sam', title: 'Settings layout', state: 'handover' }),
      ITEM('d', { owner_login: 'old-timer', title: 'Went quiet days ago', claim_status: 'stale' }),
      ITEM('e', { owner_login: 'finished-fiona', title: 'Already done', state: 'finished' }),
    ])
    render(<RepositoriesPage auth={AUTH} web={{ repos: [], pick: vi.fn(), refusal: null }} current="example-org/api" overview={overview} />)
    const rail = await screen.findByRole('complementary', { name: 'Working now' })
    expect(within(rail).getByRole('heading', { name: 'Working now in api' })).toBeTruthy()
    const people = within(rail).getAllByRole('listitem').filter((li) => li.classList.contains('rp-person'))
    // Worst first: blocked, then handover, then active with the time since its last check-in.
    expect(people.map((p) => p.querySelector('b')?.textContent)).toEqual(['dev', 'sam', 'maria'])
    expect(people[0]!.querySelector('.rp-pt')?.textContent).toBe('blocked')
    expect(people[0]!.classList.contains('is-bad')).toBe(true)
    expect(people[1]!.querySelector('.rp-pt')?.textContent).toBe('handover')
    expect(people[1]!.classList.contains('is-warn')).toBe(true)
    expect(people[2]!.querySelector('.rp-pt')?.textContent).toMatch(/min.* ago/)
    expect(people[2]!.querySelector('small')?.textContent).toBe('Refactor billing')
    expect(people[2]!.querySelector('.rp-av')?.textContent).toBe('M')
    expect(rail.textContent).not.toContain('old-timer')
    expect(rail.textContent).not.toContain('finished-fiona')
    // Other repositories with an agent connected, and why their people are not named.
    expect(within(within(rail).getByRole('list', { name: /Other repositories/ })).getAllByRole('listitem').map((l) => l.textContent)).toEqual(['example-org/ops3 agents connected'])
    expect(rail.textContent).toContain('Names show for the repository you have open. Open another to see who is in it.')
    const catchUp = within(rail).getByRole('link', { name: /Catch up on api/ })
    expect(catchUp.getAttribute('href')).toBe('#activity')
  })

  it('says nobody is working yet when there is no open repository, or only stale work', async () => {
    stubList({ repositories: [ROW('example-org/api', { agents: 1 })], unchecked: 0, complete: true })
    const { rerender } = render(<RepositoriesPage auth={AUTH} web={{ repos: [], pick: vi.fn(), refusal: null }} current={null} overview={OVERVIEW([ITEM('a')])} />)
    const rail = await screen.findByRole('complementary', { name: 'Working now' })
    expect(within(rail).getByRole('heading', { name: 'Working now in this repository' })).toBeTruthy()
    expect(rail.textContent).toContain('Nobody yet. People and their agents appear here the moment they connect.')
    expect(within(rail).queryByRole('link')).toBeNull()
    rerender(<RepositoriesPage auth={AUTH} web={{ repos: [], pick: vi.fn(), refusal: null }} current="example-org/api" overview={OVERVIEW([ITEM('a', { claim_status: 'stale' })])} />)
    expect(rail.textContent).toContain('Nobody yet.')
    expect(rail.querySelectorAll('.rp-person')).toHaveLength(0)
    // No other repository has an agent, so there is no note about names.
    expect(rail.textContent).not.toContain('Names show for the repository')
  })

  it('asks nothing without a web sign-in', () => {
    const calls = stubList({})
    render(<RepositoriesPage auth={{ kind: 'ready', value: null }} current={null} />)
    expect(screen.getByText('Sign in on the web to see every repository')).toBeTruthy()
    expect(calls).toEqual([])
  })
})
