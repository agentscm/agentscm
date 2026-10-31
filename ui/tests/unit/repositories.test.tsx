import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { DeviceAuth } from '../../src/devices.ts'
import { RepositoriesPage } from '../../src/pages/Repositories.tsx'

const AUTH = { kind: 'ready', value: { kind: 'browser', session: { login: 'example-user', provider: 'github', admin: false, csrf: 'csrf-token-0123456789' } } } as const satisfies { kind: 'ready'; value: DeviceAuth }
const ROW = (repository: string, extra: Record<string, unknown> = {}) => ({ repository, repository_id: 1, provider: 'github', unfinished: { total: 0, moving: 0, stuck: 0 }, needs_you: 0, agents: 0, last_activity_at: null, complete: true, ...extra })

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

  it('leads with the active repositories, worst first, and folds the quiet ones into one line', async () => {
    stubList({ repositories: [ROW('example-org/api', { needs_you: 2, unfinished: { total: 3, moving: 1, stuck: 2 }, agents: 4, last_activity_at: now() - 60 }), ROW('example-org/docs', { complete: false, agents: 1 }), ROW('example-org/recent', { last_activity_at: now() - 86_400 }), ROW('example-org/old', { last_activity_at: now() - 30 * 86_400 }), ROW('example-org/empty'), ROW('not a repo')], unchecked: 1, complete: true })
    render(<RepositoriesPage auth={AUTH} web={{ repos: [], pick: vi.fn(), refusal: null }} current={null} />)
    const active = await screen.findByRole('list', { name: /Repositories with activity/ })
    const rows = within(active).getAllByRole('listitem')
    expect(rows.map((r) => r.querySelector('.repo-row-name')?.textContent)).toEqual(['example-org/api', 'example-org/docs', 'example-org/recent'])
    expect(rows[0]!.textContent).toContain('2 need you')
    expect(rows[0]!.textContent).toContain('4 agents connected · 3 unfinished · 1 moving · 2 stuck · active')
    // An incomplete row's counts are lower bounds; nothing to decide shows no badge and no list of negatives.
    expect(rows[1]!.querySelector('.repo-row-status')?.textContent).toBe('')
    expect(rows[1]!.textContent).toContain('at least 1 agent connected')
    expect(rows[1]!.textContent).not.toContain('no activity')
    expect(screen.queryByText('Nothing needs you', { selector: '.pill' })).toBeNull()
    expect(document.querySelector('.answer-text')?.textContent).toBe('3 repositories have activity. 1 needs you.')
    expect(document.querySelector('.answer-fact')?.textContent).toBe('The other 2 are quiet.')
    expect(screen.getByRole('button', { name: /2 quiet repositories/ }).getAttribute('aria-expanded')).toBe('false')
    expect(screen.getByText('1 repository could not be checked')).toBeTruthy()
  })

  it('opens the quiet line into a list the filter narrows, and opens a repository from it', async () => {
    stubList({ repositories: [ROW('example-org/api', { agents: 1 }), ROW('example-org/docs'), ROW('example-org/design'), ROW('other-org/notes')], unchecked: 0, complete: true })
    const pick = vi.fn()
    render(<RepositoriesPage auth={AUTH} web={{ repos: [], pick, refusal: null }} current="example-org/api" />)
    const toggle = await screen.findByRole('button', { name: /3 quiet repositories/ })
    expect(screen.getByText('Nothing needs you')).toBeTruthy()
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
    expect(screen.getByRole('button', { name: 'Open example-org/api' })).toBeTruthy()
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
      expect(screen.getByRole('button', { name: 'Open example-org/api' })).toBeTruthy()
      expect(document.querySelector('.answer-text')?.textContent).toBe('1 repository has activity. 1 needs you.')
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

  it('asks nothing without a web sign-in', () => {
    const calls = stubList({})
    render(<RepositoriesPage auth={{ kind: 'ready', value: null }} current={null} />)
    expect(screen.getByText('Sign in on the web to see every repository')).toBeTruthy()
    expect(calls).toEqual([])
  })
})
