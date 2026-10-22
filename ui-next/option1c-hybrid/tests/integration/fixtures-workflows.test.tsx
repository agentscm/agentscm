// Filled workflow states the live dev hub cannot produce (no native workflow binding), from fixtures.
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { saveToken } from '../../../../ui/src/storage.ts'
import App from '../../src/App.tsx'
import { WorkflowsPage } from '../../src/pages/Workflows.tsx'
import { fromMock, json, stubHub } from '../fixtures/mockFetch.ts'

const LOAD = { timeout: 5_000 }
const page = () => render(<WorkflowsPage token="fixture-token" repository="acme/widgets" />)
const at = (hash: string) => act(() => {
  window.location.hash = hash
  window.dispatchEvent(new HashChangeEvent('hashchange'))
})
const isRun = (url: URL, id: string) => url.pathname === `/v1/hub/skill-workflows/${id}`

afterEach(() => {
  vi.unstubAllGlobals()
  localStorage.clear()
  sessionStorage.clear()
})

describe('workflow run list', () => {
  it('pages with the server cursor, newer and older', async () => {
    const sent = stubHub()
    page()
    await screen.findByText('claude-review-failed', {}, LOAD)
    expect(screen.getAllByRole('link', { name: /retained-run|claude-review/ })).toHaveLength(16)
    fireEvent.click(screen.getByRole('button', { name: /Older/ }))
    await screen.findByText('retained-run-029', {}, LOAD)
    expect(screen.queryByText('claude-review-failed')).toBeNull()
    expect(sent.at(-1)!.url).toBe('/v1/hub/skill-workflows?before=4985')
    fireEvent.click(screen.getByRole('button', { name: /Older/ }))
    await screen.findByText('retained-run-013', {}, LOAD)
    // Last page: 45 runs at 16 per page leaves 13, and no further cursor.
    expect(screen.getAllByRole('link', { name: /retained-run/ })).toHaveLength(13)
    expect((screen.getByRole('button', { name: /Older/ }) as HTMLButtonElement).disabled).toBe(true)
    fireEvent.click(screen.getByRole('button', { name: /Newer/ }))
    await screen.findByText('retained-run-029', {}, LOAD)
    expect(sent.every((r) => r.method === 'GET')).toBe(true)
  })

  it('filters the current page by text and status', async () => {
    stubHub()
    page()
    await screen.findByText('claude-review-failed', {}, LOAD)
    fireEvent.change(screen.getByRole('combobox', { name: 'Filter by status' }), { target: { value: 'failed' } })
    const rows = screen.getAllByRole('link', { name: /retained-run|claude-review/ })
    expect(rows.length).toBeGreaterThan(0)
    for (const r of rows) expect(within(r).getByText('Failed')).toBeTruthy()
    fireEvent.change(screen.getByRole('textbox', { name: 'Filter workflow runs on this page' }), { target: { value: 'no-such-run' } })
    expect(screen.getByText('No run on this page matches')).toBeTruthy()
  })

  it('a run ID lookup only accepts valid IDs', async () => {
    stubHub()
    page()
    await screen.findByText('claude-review-failed', {}, LOAD)
    const box = screen.getByRole('textbox', { name: 'Run ID' })
    const go = box.closest('form')!.querySelector('button')!
    fireEvent.change(box, { target: { value: '../../etc' } })
    expect(go.disabled).toBe(true)
    fireEvent.change(box, { target: { value: 'claude-review-stale' } })
    expect(go.disabled).toBe(false)
    fireEvent.submit(box.closest('form')!)
    expect(window.location.hash).toBe('#workflows/runs/claude-review-stale')
  })
})

describe('workflow run detail', () => {
  it('failed review: step graph with failed and blocked steps, and the next action', async () => {
    stubHub()
    at('#workflows/runs/claude-review-failed')
    page()
    await screen.findByText('Resolve 1 review finding', {}, LOAD)
    const node = (id: string) => screen.getByRole('link', { name: new RegExp(`^${id}`) })
    expect(node('claude-review').className).toContain('tone-bad')
    expect(node('evidence-bundle').className).toContain('tone-bad')
    expect(node('landing-readiness').className).toContain('tone-bad')
    expect(node('unit-tests').className).toContain('tone-ok')
    expect(within(node('optional-docs')).getByText('optional')).toBeTruthy()
    // Levels follow `needs`: the first column is the only root.
    const levels = [...document.querySelectorAll('.dag-level')].map((l) => [...l.querySelectorAll('.dag-node span:not(.muted)')].map((s) => s.textContent))
    expect(levels[0]).toEqual(['source-snapshot'])
    expect(levels.at(-1)).toEqual(['landing-readiness'])
    expect(screen.getByText('Failed')).toBeTruthy()
  })

  it('step deep link shows findings with severities, unresolved items, covered paths and evidence', async () => {
    stubHub()
    at('#workflows/runs/claude-review-failed/steps/claude-review')
    page()
    const step = (await screen.findByText('Step: claude-review', {}, LOAD)).closest('section')!
    const findings = within(step).getAllByRole('listitem').filter((li) => li.classList.contains('finding'))
    expect(findings.map((f) => [f.className.match(/tone-\w+/)![0], within(f).getByText(/^(error|warning|note)$/).textContent])).toEqual([
      ['tone-bad', 'error'],
      ['tone-muted', 'note'],
    ])
    expect(within(step).getByText('src/billing.rs:118-121')).toBeTruthy()
    expect(within(step).getByText('tests/billing_test.rs:44')).toBeTruthy()
    expect(within(step).getByText(/unsupported-currency behavior is not covered/)).toBeTruthy()
    expect(within(step).getByText('Covered paths (4)')).toBeTruthy()

    at('#workflows/runs/claude-review-failed/steps/source-snapshot')
    const snap = (await screen.findByText('Step: source-snapshot', {}, LOAD)).closest('section')!
    expect(within(snap).getByText('Evidence (2 files)')).toBeTruthy()
    expect(within(snap).getByText('1,842 B')).toBeTruthy()
  })

  it('stale source applicability asks for a rerun', async () => {
    stubHub()
    at('#workflows/runs/claude-review-stale')
    page()
    expect(await screen.findByText('Rerun on the current source', {}, LOAD)).toBeTruthy()
    expect(screen.getByText('the current policy epoch no longer matches this run')).toBeTruthy()
  })

  it.each([
    ['stale', 'Refresh the landing binding', 'tone-warn'],
    ['unavailable', 'Inspect the landing binding', 'tone-bad'],
  ])('a %s landing binding is called out', async (state, title, tone) => {
    stubHub((url, init) =>
      isRun(url, 'claude-review-passed')
        ? fromMock(url, init, (b) => {
            const p = b['projection'] as Record<string, Record<string, unknown>>
            p['present_landing_binding'] = { ...p['present_landing_binding'], state, reason: `binding is ${state}` }
            return b
          })
        : undefined,
    )
    at('#workflows/runs/claude-review-passed')
    page()
    const notice = (await screen.findByText(title, {}, LOAD)).closest('.notice, [role="status"], [role="alert"], div')!
    expect(screen.getByText(`binding is ${state}`)).toBeTruthy()
    expect(notice.outerHTML + notice.parentElement!.outerHTML).toContain(tone)
    fireEvent.click(screen.getByText('Technical details'))
    expect(screen.getByText(`${state} · main`)).toBeTruthy()
  })

  it('a passing run with current bindings says landing still revalidates', async () => {
    stubHub()
    at('#workflows/runs/claude-review-passed')
    page()
    expect(await screen.findByText('Checks and bindings are current', {}, LOAD)).toBeTruthy()
  })
})

describe('bad workflow responses never render as data', () => {
  it.each([
    ['malformed (wrong shape)', () => json(200, { version: 1, availability: 'available', projection: { run: 'nope' } }), /./],
    ['not JSON', () => new Response('<html>proxy error</html>', { status: 200 }), /./],
    ['oversized (declared)', () => json(200, {}, { 'content-length': String(5 * 1024 * 1024) }), /too large/],
    ['not found', () => json(404, { error: 'not found' }), /No retained run with this ID/],
    ['busy', () => json(503, { error: 'unavailable' }), /temporarily unavailable or busy/],
  ])('%s → an error notice with retry', async (_name, reply, text) => {
    stubHub((url) => (isRun(url, 'claude-review-failed') ? reply() : undefined))
    at('#workflows/runs/claude-review-failed')
    page()
    const title = await screen.findByText('Could not load this run', {}, LOAD)
    const box = title.closest('div')!.parentElement!
    expect(box.textContent).toMatch(text)
    expect(screen.queryByRole('link', { name: /^claude-review/ })).toBeNull()
    expect(screen.getByRole('button', { name: 'Retry' })).toBeTruthy()
  })

  it('oversized body without a content-length is cut off while streaming', async () => {
    const big = '"' + 'x'.repeat(4 * 1024 * 1024 + 10) + '"'
    stubHub((url) => (isRun(url, 'claude-review-failed') ? new Response(big, { status: 200 }) : undefined))
    at('#workflows/runs/claude-review-failed')
    page()
    await screen.findByText('Could not load this run', {}, LOAD)
    expect(screen.getByText(/too large/)).toBeTruthy()
  })

  it('a run list that is unavailable says why', async () => {
    stubHub((url) => (url.pathname === '/v1/hub/skill-workflows' ? json(200, { version: 1, availability: 'unavailable', reason: 'native_workflow_binding_replaced' }) : undefined))
    page()
    expect(await screen.findByText('The configured native repository identity has changed.', {}, LOAD)).toBeTruthy()
  })
})

describe('workflow breadcrumbs in the full app', () => {
  it('Workflows › Run … › Step …, each crumb a working link', async () => {
    stubHub()
    saveToken('fixture-token', false)
    at('#workflows/runs/claude-review-failed/steps/claude-review')
    render(<App />)
    const nav = await screen.findByRole('navigation', { name: 'Breadcrumb' }, LOAD)
    await waitFor(() => expect(within(nav).getAllByRole('listitem').map((li) => li.textContent)).toEqual(['acme/widgets', 'Workflows', 'Run claude-review-…', 'Step claude-review']), LOAD)
    expect(within(nav).getByRole('link', { name: 'Run claude-review-…' }).getAttribute('href')).toBe('#workflows/runs/claude-review-failed')
    expect(within(nav).getByRole('link', { name: 'Workflows' }).getAttribute('href')).toBe('#workflows')
    await screen.findByText('Step: claude-review', {}, LOAD)
    fireEvent.click(within(nav).getByRole('link', { name: 'Run claude-review-…' }))
    at('#workflows/runs/claude-review-failed')
    await waitFor(() => expect(within(nav).getAllByRole('listitem')).toHaveLength(3))
    expect(screen.queryByText('Step: claude-review')).toBeNull()
  })
})
