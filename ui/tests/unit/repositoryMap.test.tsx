import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { decodeRepositoryMap, fetchRepositoryMap, mapChanges, relationChanges, type MapSnapshot, type RepositoryMap as MapData } from '../../src/repositoryMap.ts'
import { RepositoryMap } from '../../src/pages/RepositoryMap.tsx'
import { ExplorerPage } from '../../src/pages/Explorer.tsx'
import { walkthroughFixtureResponse, WALKTHROUGH_HEAD, WALKTHROUGH_REPOSITORY } from '../../dev/walkthrough.ts'

const head = WALKTHROUGH_HEAD, base = 'b'.repeat(40), tree = 'c'.repeat(40)
const repository = WALKTHROUGH_REPOSITORY
function snapshot(commit = head): MapSnapshot {
  return { commit, tree, files: [{ path: 'ui/main.ts', oid: 'd'.repeat(40), kind: 'file', inspection: 'inspected' }, { path: 'server/api.ts', oid: 'e'.repeat(40), kind: 'file', inspection: 'inspected' }], relations: [{ from: 0, to: 1, line: 2, kind: 'import_candidate' }], unresolved: [], coverage: { discovered_files: 2, tree_complete: true, omitted_files: 0, inspected_files: 2, omitted_relations: 0, omitted_unresolved: 0 } }
}
function fixture(): MapData { return { version: 1, repository_id: repository, extractor: 'static-reference-candidates-v1', scope: '', head: snapshot(), base: snapshot(base) } }
const response = (v: unknown) => new Response(JSON.stringify(v), { headers: { 'content-type': 'application/json' } })
afterEach(() => { vi.unstubAllGlobals(); localStorage.clear(); sessionStorage.clear() })

describe('repository map', () => {
  it('rejects invalid source paths, indices, coverage and foreign repository responses', async () => {
    const data = fixture()
    expect(decodeRepositoryMap(data)).toEqual(data)
    for (const mutate of [
      (m: MapData) => { m.head.files[0]!.path = '../outside' },
      (m: MapData) => { m.head.relations[0]!.to = 10 },
      (m: MapData) => { m.head.coverage.omitted_files = 1 },
      (m: MapData) => { m.head.files[0]!.inspection = 'unsupported' },
      (m: MapData) => { m.scope = 'elsewhere' },
    ]) { const m = structuredClone(data); mutate(m); expect(() => decodeRepositoryMap(m)).toThrow() }
    vi.stubGlobal('fetch', async () => response({ ...data, repository_id: 'other/repository' }))
    await expect(fetchRepositoryMap(repository, head, base, '', new AbortController().signal)).rejects.toThrow(/another source/)
  })

  it('keeps body-only changes visible and does not call incomplete reference coverage a removal', () => {
    const before = snapshot(base), after = snapshot()
    after.files[1]!.oid = 'f'.repeat(40)
    expect(mapChanges(before, after)).toEqual([{ path: 'server/api.ts', state: 'Changed' }])
    expect(relationChanges(before, after)).toEqual([])
    after.relations = []
    expect(relationChanges(before, after)[0]!.state).toBe('No longer observed')
    after.files[0]!.inspection = 'not_inspected'
    expect(relationChanges(before, after)[0]!.state).toBe('Comparison incomplete')
    after.files.pop(); after.coverage.omitted_files = 1
    expect(mapChanges(before, after)[0]!.state).toBe('Outside the other snapshot’s coverage')
  })

  it('opens exact before-side source, drills into components and hands off to explanations', async () => {
    const calls: URL[] = [], explain = vi.fn(), scope = vi.fn()
    vi.stubGlobal('fetch', async (input: RequestInfo | URL) => {
      const url = new URL(String(input), 'http://localhost'); calls.push(url)
      if (url.pathname.endsWith('/map')) return response(fixture())
      return response({ version: 1, repository_id: repository, commit: url.searchParams.get('commit'), tree, path: url.searchParams.get('path'), kind: 'file', oid: 'd'.repeat(40), size: 13, state: 'text', content: 'source at base' })
    })
    const user = userEvent.setup()
    render(<RepositoryMap repository={repository} commit={head} base={base} scope="" onScope={scope} onExplain={explain} />)
    await user.click(await screen.findByRole('button', { name: /Before ·/ }))
    await user.click(screen.getByRole('button', { name: /^ui 1$/ }))
    await user.click(screen.getByRole('button', { name: 'ui/main.ts' }))
    expect(await screen.findByText('source at base')).toBeTruthy()
    expect(calls.at(-1)!.searchParams.get('commit')).toBe(base)
    await user.click(screen.getByRole('button', { name: 'Explore this component' }))
    expect(scope).toHaveBeenCalledWith('ui')
    await user.click(screen.getByRole('button', { name: /Explanations & evidence/ }))
    expect(explain).toHaveBeenCalledOnce()
  })

  it('explains empty filters and missing components without calling unknown coverage changed', async () => {
    const data = fixture()
    data.base!.files = [data.base!.files[1]!]; data.base!.relations = []
    data.base!.coverage.omitted_files = 1; data.base!.coverage.inspected_files = 1
    vi.stubGlobal('fetch', async () => response(data))
    const user = userEvent.setup()
    render(<RepositoryMap repository={repository} commit={head} base={base} scope="" onScope={vi.fn()} onExplain={vi.fn()} />)
    expect(await screen.findByText('Comparison incomplete', { exact: true })).toBeTruthy()
    expect(screen.queryByText('Source changed')).toBeNull()
    await user.type(screen.getByRole('textbox', { name: 'Filter map files' }), 'missing-file')
    expect(screen.getByText(/No files match this filter/)).toBeTruthy()
    await user.click(screen.getByRole('button', { name: 'Show all returned files' }))
    const node = screen.getByRole('button', { name: 'Select component ui' })
    node.focus(); await user.keyboard('{Enter}')
    await user.click(screen.getByRole('button', { name: /Before ·/ }))
    expect(screen.getByText('This component has no returned files in this revision.')).toBeTruthy()
    await user.click(screen.getByRole('button', { name: 'Show all returned files' }))
    expect(screen.getByRole('button', { name: 'server/api.ts' })).toBeTruthy()
  })

  it('recovers a missing historical parent by keeping the chosen head', async () => {
    const historical = 'f'.repeat(40), calls: URL[] = []
    vi.stubGlobal('fetch', async (input: RequestInfo | URL) => {
      const url = new URL(String(input), 'http://localhost'); calls.push(url)
      if (url.pathname.endsWith('/history')) return response({ version: 1, repository_id: repository, commit: head, tree, path: null, path_interpretation: 'exact', entries: [{ oid: historical, parents: [base], author_name: 'Synthetic author', author_time: 100, subject: 'Shallow boundary' }], next: null, complete: true, limit_state: 'complete', shallow: true })
      if (url.pathname.endsWith('/map')) {
        if (url.searchParams.has('base')) return new Response('{}', { status: 404 })
        const data = fixture(); data.head.commit = historical; data.base = null
        return response(data)
      }
      return response(walkthroughFixtureResponse(url))
    })
    const user = userEvent.setup()
    render(<ExplorerPage />)
    await user.click(await screen.findByRole('tab', { name: 'History' }))
    await user.click(await screen.findByRole('button', { name: 'View map' }))
    await user.click(await screen.findByRole('button', { name: 'Map this commit only' }))
    expect(await screen.findByText('How this source connects')).toBeTruthy()
    expect(calls.at(-1)!.searchParams.get('commit')).toBe(historical)
    expect(calls.at(-1)!.searchParams.has('base')).toBe(false)
  })

  it('discards old maps when the selected source changes during a read', async () => {
    let finish: ((response: Response) => void) | undefined
    vi.stubGlobal('fetch', async (input: RequestInfo | URL) => {
      const url = new URL(String(input), 'http://localhost')
      if (url.searchParams.get('commit') === head) return new Promise<Response>(resolve => { finish = resolve })
      const data = fixture(); data.head.commit = 'f'.repeat(40)
      return response(data)
    })
    const props = { repository, base, scope: '', onScope: vi.fn(), onExplain: vi.fn() }
    const { rerender } = render(<RepositoryMap {...props} commit={head} />)
    await waitFor(() => expect(finish).toBeTypeOf('function'))
    rerender(<RepositoryMap {...props} commit={'f'.repeat(40)} />)
    expect(await screen.findByText('ffffffffffff')).toBeTruthy()
    await act(async () => { finish!(response(fixture())) })
    expect(screen.getByText('ffffffffffff')).toBeTruthy()
    expect(screen.queryByText('aaaaaaaaaaaa')).toBeNull()
  })

  it.each([true, false])('preserves historical map selection through explanations (parent: %s)', async (hasParent) => {
    const historical = 'f'.repeat(40), calls: URL[] = []
    vi.stubGlobal('fetch', async (input: RequestInfo | URL) => {
      const url = new URL(String(input), 'http://localhost'); calls.push(url)
      if (url.pathname.endsWith('/history')) return response({ version: 1, repository_id: repository, commit: head, tree, path: null, path_interpretation: 'exact', entries: [{ oid: historical, parents: hasParent ? [base] : [], author_name: 'Synthetic author', author_time: 100, subject: 'Earlier API change' }, { oid: '9'.repeat(40), parents: ['8'.repeat(40)], author_name: 'Synthetic author', author_time: 90, subject: 'Another change' }], next: null, complete: true, limit_state: 'complete', shallow: false })
      if (url.pathname.endsWith('/diff')) return response({ version: 1, repository_id: repository, left: '8'.repeat(40), left_tree: tree, right: '9'.repeat(40), right_tree: tree, path: null, changes: [], patch: { state: 'text', content: '', truncated: false }, next: null, complete: true, limit_state: 'complete' })
      if (url.pathname.endsWith('/map')) { const m = fixture(); m.head.commit = historical; m.base = hasParent ? m.base : null; return response(m) }
      return response(walkthroughFixtureResponse(url))
    })
    const user = userEvent.setup()
    render(<ExplorerPage />)
    await user.click(await screen.findByRole('tab', { name: 'History' }))
    await user.click((await screen.findAllByRole('button', { name: 'View map' }))[0]!)
    expect(await screen.findByText(hasParent ? 'How the connections changed' : 'How this source connects')).toBeTruthy()
    const request = calls.find(c => c.pathname.endsWith('/map'))!
    expect(request.searchParams.get('commit')).toBe(historical)
    expect(request.searchParams.get('base')).toBe(hasParent ? base : null)
    await user.click(screen.getByRole('button', { name: /Explanations & evidence/ }))
    const command = (await screen.findByText(/agentscm walkthrough create/)).textContent!
    expect(command).toContain(`--head ${historical}`)
    if (!hasParent) expect(screen.getByText(/This map has no comparison base/)).toBeTruthy()
    await user.click(screen.getByRole('tab', { name: 'Map' }))
    expect(await screen.findByText(hasParent ? 'How the connections changed' : 'How this source connects')).toBeTruthy()
    const requests = calls.filter(c => c.pathname.endsWith('/map'))
    expect(requests.at(-1)!.searchParams.get('commit')).toBe(historical)
    expect(requests.at(-1)!.searchParams.get('base')).toBe(hasParent ? base : null)
    await user.click(screen.getByRole('tab', { name: 'History' }))
    await user.click((await screen.findAllByRole('button', { name: /View change/ }))[1]!)
    await user.click(screen.getByRole('tab', { name: 'Walkthrough' }))
    const nextCommand = (await screen.findByText(/agentscm walkthrough create/)).textContent!
    expect(nextCommand).toContain(`--base ${'8'.repeat(40)} --head ${'9'.repeat(40)}`)
  })
})
