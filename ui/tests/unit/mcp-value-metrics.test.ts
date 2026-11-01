import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { fireEvent, screen, waitFor } from '@testing-library/dom'
import { describe, expect, it, vi } from 'vitest'

const html = readFileSync(resolve(process.cwd(), '../src/hub/mcp_views/value-metrics.html'), 'utf8')
const script = new DOMParser().parseFromString(html, 'text/html').querySelector('script')!.textContent!
const value = {
  version: 1, method: 'agentscm.observed.v1', assessment_id: 'assessment-example', since: 100, until: 200,
  context: { availability: 'partial', returned: 2, acknowledged: 0, applied: null },
  issues: { availability: 'partial', unresolved: 1, resolved: 0, dismissed: null, false_trigger: null },
  followups: { availability: 'unknown', current: 0, pending: 0, failed: null, stale: 0, cancelled: null, unknown: 0 },
  resources: { input_tokens: '90071992547409931234', output_tokens: null, missing_cost_requests: 1, unallocated_overhead: null, costs: [], groups: [] },
  coverage: { state: 'partial', complete: false, observed_works: 1, eligible_works: null, omitted: 1, gaps: ['Unsupported source; coverage unknown'] },
  evidence: [{ id: 'episode-example', kind: 'issue', state: 'unresolved', detail: '<img src=x onerror=alert(1)>', work_id: 'wrk_example', source: 'abc123', recipe: 'check-v1', policy: 'policy-v1', basis: 'retained_refusal', assurance: 'self_reported', availability: 'available', applicability: 'current_source' }], next_after: null,
}
function setup() {
  document.body.innerHTML = '<main id="app"></main>'
  const send = vi.fn()
  const fakeWindow = { parent: { postMessage: send }, addEventListener: vi.fn() }
  const load = new Function('window', 'document', 'ResizeObserver', `${script};return {take,fail,parse,money,quantity};`) as (win: typeof fakeWindow, doc: Document, observer: undefined) => { take: (result: unknown, silent?: boolean) => void; fail: () => void; parse: (result: unknown) => unknown; money: (value: string | null, currency: string) => string; quantity: (value: string | null) => string }
  return { ...load(fakeWindow, document, undefined), send, fakeWindow }
}

describe('MCP contribution app', () => {
  it('renders the shared unknown/partial values with evidence as inert text', () => {
    const api = setup()
    api.take({ structuredContent: value })
    expect(screen.getByText('Prepared for return')).toBeTruthy()
    expect(screen.getByText('Acknowledged')).toBeTruthy()
    expect(screen.getByText('Applied')).toBeTruthy()
    expect(document.body.textContent).toContain('Unknown')
    expect(document.body.textContent).toContain('90,071,992,547,409,931,234')
    expect(document.body.textContent).toContain('Unsupported source; coverage unknown')
    expect(document.body.textContent).toContain('<img src=x onerror=alert(1)>')
    expect(document.body.textContent).toContain('Self-reported')
    expect(document.querySelector('img')).toBeNull()
    expect(screen.getByRole('button', { name: 'Refresh' })).toBeTruthy()
    expect(api.send.mock.calls.flat().filter(m => typeof m === 'object' && m.method === 'tools/call')).toHaveLength(0)
  })

  it('rejects incompatible results and removes stale evidence on a failed read', () => {
    const api = setup()
    expect(api.parse({ structuredContent: { ...value, version: 2 } })).toBeNull()
    expect(api.parse({ structuredContent: { ...value, method: 'unknown' } })).toBeNull()
    expect(api.parse({ structuredContent: [] })).toBeNull()
    api.take({ structuredContent: value })
    api.fail()
    expect(document.body.textContent).not.toContain('episode-example')
    expect(document.body.textContent).not.toContain('90,071,992,547,409,931,234')
    expect(screen.getByRole('button', { name: /Retry|Refresh/ })).toBeTruthy()
  })

  it('requests data only after a refresh gesture and preserves an explicit Work filter', async () => {
    const api = setup()
    api.take({ structuredContent: value })
    const [, handler] = api.fakeWindow.addEventListener.mock.calls.find(([name]) => name === 'message')!
    handler({ source: api.fakeWindow.parent, data: { jsonrpc: '2.0', method: 'ui/notifications/tool-input', params: { arguments: { work_id: 'wrk_example' } } } })
    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }))
    await waitFor(() => expect(api.send.mock.calls.flat().some(m => typeof m === 'object' && m.method === 'tools/call')).toBe(true))
    const call = api.send.mock.calls.flat().find(m => typeof m === 'object' && m.method === 'tools/call')
    expect(call.params).toEqual({ name: 'agentscm_value_metrics', arguments: { work_id: 'wrk_example' } })
  })
  it('ignores an older success after the newer request was refused', async () => {
    const api = setup()
    api.take({ structuredContent: value })
    const [, handler] = api.fakeWindow.addEventListener.mock.calls.find(([name]) => name === 'message')!
    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }))
    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }))
    const requests = api.send.mock.calls.flat().filter(m => typeof m === 'object' && m.method === 'tools/call')
    expect(requests).toHaveLength(2)
    handler({ source: api.fakeWindow.parent, data: { jsonrpc: '2.0', id: requests[1].id, result: { isError: true } } })
    await waitFor(() => expect(document.body.textContent).toContain('Contribution data unavailable'))
    handler({ source: api.fakeWindow.parent, data: { jsonrpc: '2.0', id: requests[0].id, result: { structuredContent: value } } })
    await Promise.resolve()
    expect(document.body.textContent).not.toContain('episode-example')
    expect(document.body.textContent).not.toContain('90,071,992,547,409,931,234')
  })

  it('ignores an older failure after a newer successful request', async () => {
    const api = setup()
    api.take({ structuredContent: value })
    const [, handler] = api.fakeWindow.addEventListener.mock.calls.find(([name]) => name === 'message')!
    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }))
    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }))
    const requests = api.send.mock.calls.flat().filter(m => typeof m === 'object' && m.method === 'tools/call')
    handler({ source: api.fakeWindow.parent, data: { jsonrpc: '2.0', id: requests[1].id, result: { structuredContent: { ...value, assessment_id: 'current-assessment' } } } })
    await waitFor(() => expect(document.body.textContent).toContain('current-assessment'))
    handler({ source: api.fakeWindow.parent, data: { jsonrpc: '2.0', id: requests[0].id, error: { code: -32000 } } })
    await Promise.resolve()
    expect(document.body.textContent).toContain('current-assessment')
    expect(document.body.textContent).not.toContain('Contribution data unavailable')
  })

  it('clears the previous Work scope and rejects its outstanding or host-notified result', async () => {
    const api = setup()
    const [, handler] = api.fakeWindow.addEventListener.mock.calls.find(([name]) => name === 'message')!
    const input = (work_id: string) => handler({ source: api.fakeWindow.parent, data: { jsonrpc: '2.0', method: 'ui/notifications/tool-input', params: { arguments: { work_id } } } })
    const result = (work_id: string, id: string) => ({ structuredContent: { ...value, assessment_id: id, evidence: [{ ...value.evidence[0], id, work_id }] }, _meta: { 'agentscm/value-metrics-scope': { work_id, since: null, until: null } } })
    input('wrk_old')
    handler({ source: api.fakeWindow.parent, data: { jsonrpc: '2.0', method: 'ui/notifications/tool-result', params: result('wrk_old', 'old-evidence') } })
    expect(document.body.textContent).toContain('old-evidence')
    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }))
    const oldRequest = api.send.mock.calls.flat().filter(m => typeof m === 'object' && m.method === 'tools/call').at(-1)
    input('wrk_current')
    expect(document.body.textContent).not.toContain('old-evidence')
    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }))
    const currentRequest = api.send.mock.calls.flat().filter(m => typeof m === 'object' && m.method === 'tools/call').at(-1)
    handler({ source: api.fakeWindow.parent, data: { jsonrpc: '2.0', id: currentRequest.id, result: result('wrk_current', 'current-evidence') } })
    await waitFor(() => expect(document.body.textContent).toContain('current-evidence'))
    handler({ source: api.fakeWindow.parent, data: { jsonrpc: '2.0', id: oldRequest.id, result: result('wrk_old', 'old-evidence') } })
    await Promise.resolve()
    handler({ source: api.fakeWindow.parent, data: { jsonrpc: '2.0', method: 'ui/notifications/tool-result', params: result('wrk_old', 'old-evidence') } })
    expect(document.body.textContent).not.toContain('old-evidence')
    expect(document.body.textContent).toContain('current-evidence')
  })

  it('keeps the refusal fence when old same-scope host notifications follow another tool input', async () => {
    const api = setup()
    const [, handler] = api.fakeWindow.addEventListener.mock.calls.find(([name]) => name === 'message')!
    const input = { jsonrpc: '2.0', method: 'ui/notifications/tool-input', params: { arguments: {} } }
    const original = { jsonrpc: '2.0', method: 'ui/notifications/tool-result', params: { structuredContent: value } }
    handler({ source: api.fakeWindow.parent, data: input })
    handler({ source: api.fakeWindow.parent, data: original })
    expect(document.body.textContent).toContain('episode-example')
    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }))
    const request = api.send.mock.calls.flat().filter(m => typeof m === 'object' && m.method === 'tools/call').at(-1)
    handler({ source: api.fakeWindow.parent, data: { jsonrpc: '2.0', id: request.id, result: { isError: true } } })
    await waitFor(() => expect(document.body.textContent).toContain('Contribution data unavailable'))
    handler({ source: api.fakeWindow.parent, data: original })
    expect(document.body.textContent).not.toContain('episode-example')
    handler({ source: api.fakeWindow.parent, data: input })
    handler({ source: api.fakeWindow.parent, data: original })
    expect(document.body.textContent).not.toContain('episode-example')
    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }))
    const freshRequest = api.send.mock.calls.flat().filter(m => typeof m === 'object' && m.method === 'tools/call').at(-1)
    handler({ source: api.fakeWindow.parent, data: { jsonrpc: '2.0', id: freshRequest.id, result: { structuredContent: { ...value, assessment_id: 'authoritative-fresh' } } } })
    await waitFor(() => expect(document.body.textContent).toContain('authoritative-fresh'))
  })

  it('formats exact currency micros without rounding small charges or combining bases', () => {
    const api = setup()
    expect(api.money('1', 'USD')).toBe('USD 0.000001')
    expect(api.money('42000', 'USD')).toBe('USD 0.042')
    expect(api.money('1000000', 'EUR')).toBe('EUR 1.00')
    expect(api.money('90071992547409931234', 'USD')).toBe('USD 90,071,992,547,409.931234')
    expect(api.money(null, 'USD')).toBe('Unknown')
    api.take({ structuredContent: { ...value, resources: { ...value.resources, costs: [
      { provider: 'provider', model: 'model', kind: 'reported_charge', currency: 'USD', amount_micros: '1', basis: 'reported', requests: 1 },
      { provider: 'provider', model: 'model', kind: 'list_price_estimate', currency: 'EUR', amount_micros: '2000000', basis: 'reference', requests: 1 },
    ] } } })
    expect(document.querySelector('.cost-list b')?.textContent).toBe('Reported charge')
    expect(screen.getByText('Reference-price equivalent')).toBeTruthy()
    expect(screen.getByText('USD 0.000001')).toBeTruthy()
    expect(screen.getByText('EUR 2.00')).toBeTruthy()
    expect(document.body.textContent).not.toContain('micros ·')
  })

  it('keeps technical identities out of evidence summary and provides native disclosures', () => {
    const api = setup()
    api.take({ structuredContent: value })
    const evidence = document.querySelector('.evidence-row')!
    const summary = evidence.querySelector('summary')!
    expect(summary.textContent).toContain('Check episode')
    expect(summary.textContent).toContain('Unresolved')
    expect(summary.textContent).not.toContain('episode-example')
    expect(summary.textContent).not.toContain('wrk_example')
    summary.focus()
    expect(document.activeElement).toBe(summary)
    const projection = [...document.querySelectorAll('summary')].find(s => s.textContent === 'Projection details')!
    expect(projection.parentElement!.tagName).toBe('DETAILS')
    expect(projection.parentElement!.hasAttribute('open')).toBe(false)
    expect(document.querySelectorAll('.coverage')).toHaveLength(1)
  })

  it('labels only unsupported null outcome categories as Not captured', () => {
    const api = setup()
    api.take({ structuredContent: value })
    for (const label of ['Dismissed', 'False triggers', 'Failed', 'Cancelled']) {
      expect(screen.getByText(label).nextElementSibling?.textContent).toBe('Not captured')
    }
    expect(screen.getByText('Applied').nextElementSibling?.textContent).toBe('Unknown')
    expect(screen.getByText('Output tokens').nextElementSibling?.textContent).toBe('Unknown')
    expect(screen.getByText('Resolved').nextElementSibling?.textContent).toBe('0')
    expect(screen.getByText('Stale').nextElementSibling?.textContent).toBe('0')
    api.take({ structuredContent: { ...value, issues: { ...value.issues, dismissed: 0 } } })
    expect(screen.getByText('Dismissed').nextElementSibling?.textContent).toBe('0')
  })

})
