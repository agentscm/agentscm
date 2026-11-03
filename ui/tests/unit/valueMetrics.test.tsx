import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { ValueMetricsSection } from '../../src/ValueMetrics.tsx'
import { decodeValueMetrics, fetchValueMetrics } from '../../src/valueMetrics.ts'

const SESSION = { login: 'example-reader', provider: 'github', admin: false, csrf: 'synthetic-session' }
const ROWS = ['example-org/api', 'example-org/docs'].map(repository => ({ repository, provider: 'github', unfinished: { total: 0, moving: 0, stuck: 0 }, needsYou: 0, agents: 0, lastActivityAt: null, complete: true }))
const evidence = (id: string) => ({ id, kind: 'issue', state: 'resolved', repository: 'example-org/api', work_id: 'wrk_example', source: 'abc123', recipe: 'test-v1/run-1', policy: 'policy-v1', observed_at: 1793000000, detail: `Current evidence resolved ${id}` })
function projection(extra: Record<string, unknown> = {}) {
  return {
    version: 1, method: 'agentscm.observed.v1', assessment_id: 'assessment-example', as_of: 1793000010, since: 1792400000, until: 1793000000,
    scope: [{ provider: 'github', host: 'github.com', repository: 'example-org/api', repository_id: 1 }],
    context: { availability: 'partial', returned: 2, acknowledged: 1, applied: null }, issues: { availability: 'partial', unresolved: 0, resolved: 1, dismissed: null, false_trigger: null },
    followups: { availability: 'partial', current: 1, pending: 1, failed: null, stale: 0, cancelled: null, unknown: 0 },
    resources: { input_tokens: '90071992547409931234', output_tokens: null, costs: [{ provider: 'example-provider', model: 'example-model', kind: 'reported_charge', currency: 'USD', basis: 'reported', amount_micros: '1234567', requests: 2 }, { provider: 'example-provider', model: 'example-model', kind: 'list_price_estimate', currency: 'EUR', basis: 'reference', amount_micros: '4567890', requests: 1 }], missing_cost_requests: 1, unallocated_usage_records: 0, unallocated_overhead: null },
    coverage: { state: 'partial', eligible_works: null, observed_works: 1, complete: false, gaps: ['disconnected_collector', 'usage_coverage_unknown'], omitted: 2 },
    evidence: [evidence('episode-1')], next_after: null, merge_observations: 0, ...extra,
  }
}
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })
afterEach(() => vi.unstubAllGlobals())

describe('contribution projection boundaries', () => {
  it('rejects unknown versions and preserves unknown counts, large integers and accounting bases', () => {
    expect(() => decodeValueMetrics(projection({ version: 2 }))).toThrow('Unsupported')
    const decoded = decodeValueMetrics(projection({ issues: { unresolved: -1, resolved: 1, dismissed: Number.MAX_SAFE_INTEGER + 1 } }))
    expect(decoded.issues).toEqual({ availability: 'unknown', unresolved: null, resolved: 1, dismissed: null, false_trigger: null })
    expect(decoded.resources.input_tokens).toBe('90071992547409931234')
    expect(decoded.resources.output_tokens).toBeNull()
    expect(decoded.resources.costs.map(c => [c.currency, c.basis, c.amount_micros])).toEqual([['USD', 'reported', '1234567'], ['EUR', 'reference', '4567890']])
    expect(decoded.coverage.eligible_works).toBeNull()
    expect(decoded.context.applied).toBeNull()
  })

  it('uses one session aggregate with an encoded server filter and enforces a body bound', async () => {
    const fetcher = vi.fn().mockResolvedValue(response(projection()))
    vi.stubGlobal('fetch', fetcher)
    await fetchValueMetrics({ since: 100, until: 200, repository: 'example-org/api', after: 'opaque+cursor' }, new AbortController().signal)
    const [url, init] = fetcher.mock.calls[0]!
    const params = new URL(url, 'https://example.invalid').searchParams
    expect(params.get('repository')).toBe('example-org/api')
    expect(params.get('after')).toBe('opaque+cursor')
    expect(params.get('limit')).toBe('20')
    expect(init).toMatchObject({ credentials: 'same-origin', cache: 'no-store' })
    expect(fetcher).toHaveBeenCalledTimes(1)
    fetcher.mockResolvedValueOnce(new Response(' '.repeat(1024 * 1024 + 1)))
    await expect(fetchValueMetrics({ since: 100, until: 200 }, new AbortController().signal)).rejects.toThrow('too large')
  })
})

describe('recorded contributions', () => {
  it('shows outcomes, honest null/partial states and inspectable evidence without adding monetary amounts', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(projection())))
    const open = vi.fn()
    render(<ValueMetricsSection session={SESSION} repositories={ROWS} onOpen={open} />)
    await screen.findByRole('region', { name: 'Issues surfaced and resolved' })
    expect(screen.getByRole('heading', { name: 'Coverage: partial' })).toBeTruthy()
    expect(screen.getByRole('heading', { name: 'What AgentSCM recorded' })).toBeTruthy()
    expect(screen.getAllByRole('radio').map(r => r.getAttribute('value'))).toEqual(['7', '30', '90'])
    expect(screen.getByText('1 observed Works · Unknown eligible Works · 2 omitted records')).toBeTruthy()
    expect(screen.getByText(/Zero observations do not establish zero problems/)).toBeTruthy()
    expect(screen.getAllByText(/90,071,992,547,409,931,234/).length).toBeGreaterThan(0)
    expect(screen.getByText(/AgentSCM overhead: Unknown \/ unallocated/)).toBeTruthy()
    expect(screen.getByText(/1,234,567 USD micros · reported/)).toBeTruthy()
    expect(screen.getByText(/4,567,890 EUR micros · reference/)).toBeTruthy()
    fireEvent.click(screen.getByText('Inspect recorded evidence'))
    fireEvent.click(screen.getByText('Current evidence resolved episode-1'))
    expect(screen.getByText('abc123')).toBeTruthy()
    expect(screen.getByText('test-v1/run-1')).toBeTruthy()
    fireEvent.click(screen.getByRole('button', { name: 'Open example-org/api' }))
    expect(open).toHaveBeenCalledWith('example-org/api')
  })

  it('distinguishes unsupported outcomes from observed zero and generic unknown', async () => {
    const fetcher = vi.fn().mockResolvedValueOnce(response(projection())).mockResolvedValueOnce(response(projection({
      issues: { availability: 'partial', unresolved: 0, resolved: 1, dismissed: 0, false_trigger: 0 },
      followups: { availability: 'partial', current: 1, pending: 1, failed: 0, stale: 0, cancelled: 0, unknown: 0 },
    })))
    vi.stubGlobal('fetch', fetcher)
    render(<ValueMetricsSection session={SESSION} repositories={ROWS} />)
    await screen.findByRole('region', { name: 'Issues surfaced and resolved' })
    const valueFor = (label: string) => screen.getByText(label, { selector: 'dt' }).nextElementSibling?.textContent
    for (const label of ['Dismissed', 'False triggers', 'Failed', 'Cancelled']) expect(valueFor(label)).toBe('Not captured')
    expect(valueFor('Unresolved')).toBe('0')
    expect(valueFor('Applied')).toBe('Unknown')
    expect(valueFor('Output tokens')).toBe('Unknown')
    fireEvent.click(screen.getByRole('button', { name: 'Refresh contributions' }))
    await waitFor(() => expect(valueFor('Dismissed')).toBe('0'))
    for (const label of ['False triggers', 'Failed', 'Cancelled']) expect(valueFor(label)).toBe('0')
    expect(valueFor('Applied')).toBe('Unknown')
  })

  it('keeps non-additive reports with unknown scope or request identity visible when additive missing-cost is zero', async () => {
    // The server excludes these reports from additive totals; zero missing additive costs is not full cost coverage.
    const raw = projection({ resources: { input_tokens: null, output_tokens: null, costs: [], groups: [], missing_cost_requests: 0, unallocated_usage_records: 2, unallocated_overhead: null } })
    expect(decodeValueMetrics(raw).resources.unallocated_usage_records).toBe(2)
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(raw)))
    render(<ValueMetricsSection session={SESSION} repositories={ROWS} />)
    await screen.findByRole('region', { name: 'Recorded resources' })
    expect(screen.getByText(/0 additive requests have no cost recorded. Unallocated usage records: 2/)).toBeTruthy()
    expect(screen.getByText('No cost amounts recorded. Cash cost is unknown.')).toBeTruthy()
    expect(screen.queryByText('0 requests have no cost recorded.')).toBeNull()
  })

  it('changes the server period/scope and hides the previous projection immediately', async () => {
    const fetcher = vi.fn().mockResolvedValue(response(projection()))
    vi.stubGlobal('fetch', fetcher)
    render(<ValueMetricsSection session={SESSION} repositories={ROWS} />)
    await screen.findByRole('region', { name: 'Issues surfaced and resolved' })
    fetcher.mockImplementationOnce(() => new Promise(() => {}))
    fireEvent.click(screen.getByRole('radio', { name: 'Last 30 days' }))
    expect(screen.queryByRole('region', { name: 'Issues surfaced and resolved' })).toBeNull()
    await waitFor(() => expect(fetcher).toHaveBeenCalledTimes(2))
    const params = new URL(fetcher.mock.calls[1]![0], 'https://example.invalid').searchParams
    expect(Number(params.get('until')) - Number(params.get('since'))).toBe(30 * 86400)
    fetcher.mockResolvedValueOnce(response(projection()))
    fireEvent.change(screen.getByLabelText('Repository scope'), { target: { value: 'example-org/docs' } })
    await screen.findByRole('region', { name: 'Issues surfaced and resolved' })
    expect(new URL(fetcher.mock.calls[2]![0], 'https://example.invalid').searchParams.get('repository')).toBe('example-org/docs')
  })

  it('does not retain measurements when a refreshed read is refused or the session changes', async () => {
    const fetcher = vi.fn().mockResolvedValue(response(projection()))
    vi.stubGlobal('fetch', fetcher)
    const view = render(<ValueMetricsSection session={SESSION} repositories={ROWS} />)
    await screen.findByRole('region', { name: 'Issues surfaced and resolved' })
    fetcher.mockResolvedValueOnce(response({}, 403))
    fireEvent.click(screen.getByRole('button', { name: 'Refresh contributions' }))
    await screen.findByText('Your access could not be confirmed. Sign in again or refresh your repository list.')
    expect(screen.queryByRole('region', { name: 'Issues surfaced and resolved' })).toBeNull()
    fetcher.mockImplementationOnce(() => new Promise(() => {}))
    view.rerender(<ValueMetricsSection session={{ ...SESSION, login: 'another-reader', csrf: 'another-session' }} repositories={ROWS} />)
    expect(screen.queryByText(/access could not be confirmed/)).toBeNull()
    expect(screen.queryByRole('region', { name: 'Issues surfaced and resolved' })).toBeNull()
  })

  it('pages evidence with the same scope/window and keeps server totals unchanged', async () => {
    const fetcher = vi.fn().mockResolvedValueOnce(response(projection({ next_after: 'page-2' }))).mockResolvedValueOnce(response(projection({ evidence: [evidence('episode-2')] })))
    vi.stubGlobal('fetch', fetcher)
    render(<ValueMetricsSection session={SESSION} repositories={ROWS} />)
    fireEvent.click(await screen.findByText('Inspect recorded evidence'))
    fireEvent.click(await screen.findByRole('button', { name: 'Load more evidence' }))
    await screen.findByText('Current evidence resolved episode-2')
    expect(screen.getByText('Current evidence resolved episode-1')).toBeTruthy()
    expect(screen.getByRole('region', { name: 'Issues surfaced and resolved' })).toBeTruthy()
    const first = new URL(fetcher.mock.calls[0]![0], 'https://example.invalid').searchParams
    const second = new URL(fetcher.mock.calls[1]![0], 'https://example.invalid').searchParams
    expect(first.get('since')).toBeTruthy()
    // Follow the Hub's returned fixed bounds, including a server clock correction.
    expect(second.get('since')).toBe('1792400000')
    expect(second.get('until')).toBe('1793000000')
    expect(second.get('after')).toBe('page-2')
    expect(screen.queryByRole('button', { name: 'Load more evidence' })).toBeNull()
  })

  it('says empty evidence is unobserved and late responses cannot restore a revoked scope', async () => {
    let resolveOld: (value: Response) => void = () => {}
    const fetcher = vi.fn().mockImplementationOnce(() => new Promise<Response>(resolve => { resolveOld = resolve })).mockResolvedValueOnce(response(projection({ evidence: [] })))
    vi.stubGlobal('fetch', fetcher)
    const view = render(<ValueMetricsSection session={SESSION} repositories={ROWS} />)
    await waitFor(() => expect(fetcher).toHaveBeenCalledTimes(1))
    view.rerender(<ValueMetricsSection session={SESSION} repositories={[]} />)
    await screen.findByText('No evidence returned for this scope and period. Contributions remain unobserved.')
    await act(async () => resolveOld(response(projection())))
    expect(screen.queryByText('Current evidence resolved episode-1')).toBeNull()
  })
})
