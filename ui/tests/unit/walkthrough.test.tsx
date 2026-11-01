import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { walkthroughFixture, walkthroughFixtureResponse, WALKTHROUGH_HEAD, WALKTHROUGH_ID, WALKTHROUGH_OLD_ID, WALKTHROUGH_REPOSITORY } from '../../dev/walkthrough.ts'
import { ExplorerPage } from '../../src/pages/Explorer.tsx'
import { Walkthrough } from '../../src/pages/Walkthrough.tsx'
import { decodeWalkthrough, decodeWalkthroughSource, fetchWalkthrough, fetchWalkthroughSource } from '../../src/walkthrough.ts'
const json = (v: unknown, status = 200) => new Response(JSON.stringify(v), { status, headers: { 'content-type': 'application/json' } })
const calls: URL[] = []
function stub(override?: (url: URL) => Response | Promise<Response> | undefined) {
  calls.length = 0
  vi.stubGlobal('fetch', async (input: RequestInfo | URL, init?: RequestInit) => {
    expect(init?.method ?? 'GET').toBe('GET')
    const url = new URL(String(input), 'http://localhost'); calls.push(url)
    return override?.(url) ?? json(walkthroughFixtureResponse(url))
  })
}
afterEach(() => { vi.unstubAllGlobals(); localStorage.clear(); sessionStorage.clear() })
describe('walkthrough', () => {
  it('renders authored content safely, exact deleted-side lines, history, gaps and compact copy', async () => {
    stub(); const user = userEvent.setup()
    render(<Walkthrough repository={WALKTHROUGH_REPOSITORY} currentHead={WALKTHROUGH_HEAD} />)
    await user.selectOptions(await screen.findByLabelText('Immutable version history'), WALKTHROUGH_ID)
    expect(await screen.findByText('<script>alert("quoted data")</script>')).toBeTruthy()
    expect(document.querySelector('script')).toBeNull()
    expect(screen.getAllByText('tests/request.rs')).toHaveLength(2)
    expect(screen.getByText('Evidence unavailable')).toBeTruthy()
    await user.click(screen.getByRole('button', { name: /Before · src\/fallback.rs/ }))
    expect((await screen.findByLabelText('Exact source for src/fallback.rs')).textContent).toContain('permissive_fallback')
    expect(screen.getByText('3', { selector: '.explorer-code-line > span' })).toBeTruthy()
    await user.click(await screen.findByRole('button', { name: 'Copy walkthrough context' }))
    expect(JSON.parse(await navigator.clipboard.readText()).walkthrough_id).toBe(WALKTHROUGH_ID)
    await user.click(screen.getByRole('button', { name: WALKTHROUGH_OLD_ID.slice(0, 12) }))
    expect(await screen.findByText('Earlier request-validation explanation')).toBeTruthy()
    expect(screen.getByText('First version')).toBeTruthy()
  })
  it('shows useful empty-case CLI instructions without attempting a write', async () => {
    stub(url => url.pathname.endsWith('walkthroughs') ? json({ version: 1, repository: WALKTHROUGH_REPOSITORY, walkthroughs: [], complete: true, omitted: 0 }) : undefined)
    render(<Walkthrough repository={WALKTHROUGH_REPOSITORY} currentHead={WALKTHROUGH_HEAD} />)
    expect(await screen.findByText('Create a change walkthrough')).toBeTruthy()
    expect(screen.getByText(/agentscm walkthrough create/).textContent).toContain('--base FULL_BASE_COMMIT')
    expect(calls).toHaveLength(1)
  })

  it('preserves both historical comparison commits in Explain this change creation guidance', async () => {
    const base = 'b'.repeat(40), historical = 'f'.repeat(40), tree = 'c'.repeat(40)
    stub(url => {
      if (url.pathname.endsWith('/history')) return json({ version: 1, repository_id: WALKTHROUGH_REPOSITORY, commit: WALKTHROUGH_HEAD, tree, path: null, path_interpretation: 'exact', entries: [{ oid: historical, parents: [base], author_name: 'Synthetic author', author_time: 100, subject: 'Historical change' }], next: null, complete: true, limit_state: 'complete', shallow: false })
      if (url.pathname.endsWith('/diff')) return json({ version: 1, repository_id: WALKTHROUGH_REPOSITORY, left: base, left_tree: tree, right: historical, right_tree: tree, path: null, changes: [], patch: { state: 'text', content: 'historical patch', truncated: false }, next: null, complete: true, limit_state: 'complete' })
      if (url.pathname.endsWith('/walkthroughs')) return json({ version: 1, repository: WALKTHROUGH_REPOSITORY, walkthroughs: [], complete: true, omitted: 0 })
      return undefined
    })
    const user = userEvent.setup()
    render(<ExplorerPage />)
    await user.click(await screen.findByRole('tab', { name: 'History' }))
    await user.click(await screen.findByRole('button', { name: /View change/ }))
    await user.click(await screen.findByRole('button', { name: 'Explain this change' }))
    const command = (await screen.findByText(/agentscm walkthrough create/)).textContent!
    expect(command).toContain(`--base ${base} --head ${historical}`)
    expect(command).not.toContain(`--head ${WALKTHROUGH_HEAD}`)
    const notice = screen.getByText('Historical comparison selected').parentElement!
    expect(notice.textContent).toContain(WALKTHROUGH_HEAD)
    expect(notice.textContent).toContain('Version applicability is compared separately')
  })

  it('retains exact comparison creation guidance beside nonmatching and matching version history', async () => {
    const base = 'b'.repeat(40), comparedHead = 'f'.repeat(40)
    stub(url => {
      if (!url.pathname.endsWith('/walkthroughs')) return undefined
      const listing = walkthroughFixtureResponse(url) as { walkthroughs: { id: string; change_id: string; base: string; head: string }[] }
      listing.walkthroughs[0] = { ...listing.walkthroughs[0]!, change_id: 'matching-change', base, head: comparedHead }
      return json(listing)
    })
    render(<Walkthrough repository={WALKTHROUGH_REPOSITORY} currentHead={WALKTHROUGH_HEAD} creationPair={{ base, head: comparedHead }} />)
    const versions = await screen.findByLabelText('Immutable version history')
    const guidance = screen.getByText('Create a walkthrough for this comparison').closest('details')!
    expect(guidance.open).toBe(false)
    expect(guidance.textContent).toContain(`--base ${base} --head ${comparedHead}`)
    expect(guidance.textContent).not.toContain(`--head ${WALKTHROUGH_HEAD}`)
    expect(versions.querySelectorAll('option')).toHaveLength(3)
    expect(screen.getByRole('option', { name: /matching-change.*matches comparison/ })).toBeTruthy()
    expect(screen.getByRole('option', { name: /request-validation.*historical/ })).toBeTruthy()
  })
  it('shows different-source applicability and rejects foreign repository identity', async () => {
    stub(); const user = userEvent.setup()
    render(<Walkthrough repository={WALKTHROUGH_REPOSITORY} currentHead={'f'.repeat(40)} />)
    await user.selectOptions(await screen.findByLabelText('Immutable version history'), WALKTHROUGH_ID)
    expect(await screen.findByText('Different selected source')).toBeTruthy()
    stub(url => url.pathname.endsWith('/walkthrough') ? json({ ...walkthroughFixture(), record: { ...walkthroughFixture().record, repository: 'other/repository' } }) : undefined)
    await expect(fetchWalkthrough(WALKTHROUGH_REPOSITORY, WALKTHROUGH_ID, WALKTHROUGH_HEAD, new AbortController().signal)).rejects.toThrow()
  })
  it('fences obsolete detail responses after a version switch', async () => {
    let finish: ((value: Response) => void) | undefined
    stub(url => url.pathname.endsWith('/walkthrough') && url.searchParams.get('id') === WALKTHROUGH_ID ? new Promise<Response>(resolve => { finish = resolve }) : undefined)
    const user = userEvent.setup()
    render(<Walkthrough repository={WALKTHROUGH_REPOSITORY} currentHead={WALKTHROUGH_HEAD} />)
    const versions = await screen.findByLabelText('Immutable version history')
    await user.selectOptions(versions, WALKTHROUGH_ID)
    await waitFor(() => expect(finish).toBeTypeOf('function'))
    await user.selectOptions(versions, WALKTHROUGH_OLD_ID)
    expect(await screen.findByText('Earlier request-validation explanation')).toBeTruthy()
    await act(async () => { finish!(json(walkthroughFixture())) })
    expect(screen.queryByRole('heading', { name: 'Reject invalid requests before execution' })).toBeNull()
  })

  it('integrates with explorer tab navigation and shows source read failures', async () => {
    stub(url => url.pathname.endsWith('walkthrough-source') ? json({ error: 'source_unavailable' }, 404) : undefined)
    const user = userEvent.setup()
    render(<ExplorerPage />)
    const tab = await screen.findByRole('tab', { name: 'Walkthrough' })
    await user.click(tab)
    expect(tab.getAttribute('aria-selected')).toBe('true')
    await user.selectOptions(await screen.findByLabelText('Immutable version history'), WALKTHROUGH_ID)
    await user.click(await screen.findByRole('button', { name: /Before · src\/fallback.rs/ }))
    expect(await screen.findByText('Source unavailable')).toBeTruthy()
    expect(calls.some(url => url.pathname.endsWith('walkthrough-source'))).toBe(true)
    expect(calls.some(url => url.pathname.endsWith('conversation'))).toBe(false)
  })

  it.each([WALKTHROUGH_HEAD, 'f'.repeat(40)])('separates execution success and evidence head from selected source %s', async (currentHead) => {
    const action = { entry: 'action', relation: 'exact_source_binding', path_relation: 'not_requested', work_id: 'work-example', generation: 1, session_id: 'session-example', action_id: 'action-example', operation_id: 'operation-example', action_kind: 'execution', recipe_provenance: 'observed_native_input', recipe_digest: '1'.repeat(64), setup_digest: '2'.repeat(64), started_at: 100, source: { kind: 'git', repository: WALKTHROUGH_REPOSITORY, worktree_id: 'checkout-example', head: WALKTHROUGH_HEAD, tree: 'c'.repeat(40), base: 'b'.repeat(40), dirty: false, untracked_count: 0, omissions: [], captured_snapshot: null, snapshot_coverage: 'complete' }, applicability: 'matching_recorded_clean_source', start_retention: 'pending', result: { operation_id: 'finish-example', outcome: 'succeeded', result: 'unknown', assurance: 'caller_asserted', coverage: 'partial', drift: 'none_observed', finished_at: 101, retention: 'pending' } }
    stub(url => url.pathname.endsWith('/walkthrough') ? json({ ...walkthroughFixture(WALKTHROUGH_ID, currentHead), evidence: { state: 'available', page: { version: 1, repository: WALKTHROUGH_REPOSITORY, commit: WALKTHROUGH_HEAD, path: null, requested_work_id: null, entries: [action], projection_coverage: { retained_envelopes: 1, compacted_event_batches_unattributed: 0, unavailable_bodies: 0, unverified_bodies: 0, capture_gaps: 0, complete: true }, next_cursor: null, complete: true } } }) : undefined)
    const user = userEvent.setup()
    render(<Walkthrough repository={WALKTHROUGH_REPOSITORY} currentHead={currentHead} />)
    await user.selectOptions(await screen.findByLabelText('Immutable version history'), WALKTHROUGH_ID)
    expect(await screen.findByText('execution · outcome succeeded · verification unknown')).toBeTruthy()
    expect(screen.getByText('partial')).toBeTruthy()
    expect(screen.getByText('caller_asserted')).toBeTruthy()
    expect(screen.queryByText('passed')).toBeNull()
    expect(screen.getByText(/Evidence applicability is assessed against/).textContent).toContain(WALKTHROUGH_HEAD)
    if (currentHead !== WALKTHROUGH_HEAD) {
      expect(screen.getByText('Different selected source')).toBeTruthy()
      expect(screen.getByText(/Evidence applicability is assessed against/).textContent).toContain(`It has not been evaluated against the currently selected commit ${currentHead}`)
    }
  })

  it('enforces shared draft/source limits and accepts the full renamed-path manifest bound', () => {
    const fixture = walkthroughFixture()
    const withDraft = (draft: unknown) => ({ ...fixture, record: { ...fixture.record, draft } })
    expect(() => decodeWalkthrough(withDraft({ ...fixture.record.draft, steps: Array.from({ length: 33 }, (_, i) => ({ id: `s${i}`, title: 'step', body: '', anchors: [] })) }))).toThrow()
    expect(() => decodeWalkthrough(withDraft({ ...fixture.record.draft, relationships: Array.from({ length: 65 }, () => ({ from: 'validation', to: 'fallback', label: 'declared' })) }))).toThrow()
    expect(() => decodeWalkthrough(withDraft({ ...fixture.record.draft, steps: [0, 1].map(i => ({ id: `s${i}`, title: 'step', body: '', anchors: Array.from({ length: 65 }, (_, j) => ({ id: `a${i}-${j}`, side: 'head', path: 'source.rs', start_line: 1, end_line: 1 })) })) }))).toThrow()
    const source = { version: 1, walkthrough_id: WALKTHROUGH_ID, anchor: { id: 'a', side: 'head', path: 'source.rs', start_line: 1, end_line: 200 }, commit: WALKTHROUGH_HEAD, oid: WALKTHROUGH_HEAD, content: 'x', interpretation: 'exact_source_lines' }
    expect(decodeWalkthroughSource(source).anchor.end_line).toBe(200)
    expect(() => decodeWalkthroughSource({ ...source, anchor: { ...source.anchor, end_line: 201 } })).toThrow()
    expect(() => decodeWalkthroughSource({ ...source, content: 'x'.repeat(65537) })).toThrow()
    expect(decodeWalkthrough({ ...fixture, record: { ...fixture.record, unassigned_paths: Array.from({ length: 2000 }, (_, i) => `source-${i}.rs`) } }).record.unassigned_paths).toHaveLength(2000)
  })
  it('rejects malformed graph, anchor ranges, source substitutions and unsupported versions', async () => {
    const fixture = walkthroughFixture()
    expect(() => decodeWalkthrough({ ...fixture, version: 2 })).toThrow()
    expect(() => decodeWalkthrough({ ...fixture, record: { ...fixture.record, draft: { ...fixture.record.draft, relationships: [{ from: 'absent', to: 'fallback', label: 'x' }] } } })).toThrow()
    expect(() => decodeWalkthroughSource({ version: 1, walkthrough_id: WALKTHROUGH_ID, anchor: { id: 'x', side: 'base', path: '../secret', start_line: 2, end_line: 1 }, commit: WALKTHROUGH_HEAD, oid: WALKTHROUGH_HEAD, content: 'secret', interpretation: 'exact_source_lines' })).toThrow()
    const detail = decodeWalkthrough(fixture), anchor = detail.record.draft.steps[0]!.anchors[0]!
    stub(url => url.pathname.endsWith('walkthrough-source') ? json({ ...(walkthroughFixtureResponse(url) as object), commit: 'f'.repeat(40) }) : undefined)
    await expect(fetchWalkthroughSource(detail, anchor, new AbortController().signal)).rejects.toThrow()
  })
})
