import { fireEvent, render, screen, waitFor } from '@testing-library/react'
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
  it('lists the Hub’s rows in its order, counts the unchecked ones without names, and drops names that aren’t repositories', async () => {
    stubList({ repositories: [ROW('example-org/api', { needs_you: 2, unfinished: { total: 3, moving: 1, stuck: 2 }, agents: 4, last_activity_at: 1_790_000_000 }), ROW('example-org/docs', { complete: false, agents: 1 }), ROW('not a repo')], unchecked: 1, complete: true })
    render(<RepositoriesPage auth={AUTH} current={null} />)
    await screen.findByText('api')
    const rows = screen.getAllByRole('listitem')
    expect(rows.map((r) => r.querySelector('.repo-row-name')?.textContent)).toEqual(['example-org/api', 'example-org/docs'])
    expect(rows[0]!.textContent).toContain('2 need you')
    expect(rows[0]!.textContent).toContain('3 unfinished · 1 moving · 2 stuck · 4 agents connected')
    // An incomplete row's counts are lower bounds, and a row with nothing to decide says so.
    expect(rows[1]!.textContent).toContain('Nothing needs you')
    expect(rows[1]!.textContent).toContain('at least 1 agent connected')
    expect(screen.getByText('1 repository could not be checked')).toBeTruthy()
    expect(document.querySelector('.answer-text')?.textContent).toBe('2 repositories you can read. 1 needs you.')
  })

  it('opens a row through the web sign-in and goes to its Dashboard once its token is in', async () => {
    stubList({ repositories: [ROW('example-org/api'), ROW('example-org/docs')], unchecked: 0, complete: true })
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
    stubList({ repositories: [ROW('example-org/api'), ROW('example-org/docs')], unchecked: 0, complete: true })
    const web = { repos: ['example-org/api', 'example-org/docs'], pick: vi.fn(), refusal: null }
    const { rerender } = render(<RepositoriesPage auth={AUTH} web={web} current="example-org/api" />)
    fireEvent.click(await screen.findByRole('button', { name: 'Open example-org/docs' }))
    const refusal = { code: 'insufficient_role', message: 'You no longer have read access to example-org/docs.', fix: null }
    rerender(<RepositoriesPage auth={AUTH} web={{ ...web, repos: ['example-org/api'], refusal }} current="example-org/api" />)
    expect(await screen.findByText(/no longer have read access/)).toBeTruthy()
    expect(window.location.hash).toBe('')
  })

  it('asks nothing without a web sign-in', () => {
    const calls = stubList({})
    render(<RepositoriesPage auth={{ kind: 'ready', value: null }} current={null} />)
    expect(screen.getByText('Sign in on the web to see every repository')).toBeTruthy()
    expect(calls).toEqual([])
  })
})
