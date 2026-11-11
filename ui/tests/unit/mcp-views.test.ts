import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ActionableStatus } from '../../src/actionableStatus.ts'

const html = readFileSync(resolve(process.cwd(), '../src/hub/mcp_views/work-overview.html'), 'utf8')
const fixture = JSON.parse(readFileSync(resolve(process.cwd(), '../src/hub/fixtures/actionable-status.json'), 'utf8')) as ActionableStatus
function load() {
  const parsed = new DOMParser().parseFromString(html, 'text/html')
  const script = parsed.querySelector('script')!.textContent!
  document.body.innerHTML = '<div id="app"></div><div id="live"></div>'
  const sent: { method: string }[] = []
  const win = { parent: { postMessage: (m: { method: string }) => sent.push(m) }, addEventListener: () => {} }
  const render = new Function('window', 'document', 'ResizeObserver', 'IntersectionObserver', `${script}; return render;`)(win, document, undefined, undefined) as (root: HTMLElement, data: unknown, at: Date) => void
  return { root: document.getElementById('app')!, render, sent }
}

beforeEach(() => vi.useFakeTimers())
afterEach(() => { vi.useRealTimers(); document.body.innerHTML = '' })

describe('MCP overview view', () => {
  it('stays self-contained, icon-only and under the size cap', () => {
    expect(Buffer.byteLength(html)).toBeLessThan(60_000)
    expect(html).not.toMatch(/https?:\/\/(?!www\.w3\.org\/2000\/svg)/)
    expect(html).not.toMatch(/[⊘≠⇄▶‖⏳↩✓⚠▼⏱]/)
    expect(html).not.toMatch(/text-transform:\s*uppercase|<img|innerHTML|@import|<link/)
    for (const text of ['#17142e', 'prefers-reduced-motion', 'focus-visible']) expect(html).toContain(text)
  })

  it('shows shared actionable status, escapes untrusted text and folds full evidence', () => {
    const { root, render } = load()
    const evil = '<img src=x onerror=alert(1)>'
    const data = { ...fixture, work: fixture.work.map((w, i) => i === 0 ? { ...w, title: evil, action: '<b>needs api</b>', needs_you: true } : w) }
    render(root, { actionable_status: data }, new Date())
    expect(root.querySelector('.big')!.textContent).toBe(`${fixture.counts.active} active task. ${fixture.counts.needs_you} actions need you.`)
    expect(root.querySelectorAll('.needs li').length).toBeGreaterThan(0)
    expect(root.textContent).toContain(evil)
    expect(root.textContent).toContain('<b>needs api</b>')
    expect(root.querySelector('img,b')).toBeNull()
    expect(root.querySelector('button[aria-label="Refresh"]')).toBeTruthy()
    expect(root.querySelectorAll('details.f')).toHaveLength(1)
    expect(root.querySelector('details.f')!.textContent).toContain('All work and evidence')
    expect(root.querySelectorAll('.w li')).toHaveLength(fixture.work.length)
    expect(root.textContent).toContain('Provider check times are shown separately')
    expect(root.textContent).toContain(fixture.digest)
  })

  it('keeps incomplete status distinct from clear coverage', () => {
    const { root, render } = load()
    render(root, { actionable_status: { ...fixture, work: [], actions: [], complete: false } }, new Date())
    expect(root.textContent).toContain('Coverage incomplete; missing signals remain unknown.')
    expect(root.textContent).not.toContain('No current work in this overview.')
  })

  it('clips long titles and reports missing shared status', () => {
    const { root, render } = load()
    render(root, { actionable_status: { ...fixture, work: [{ ...fixture.work[0], title: 'x'.repeat(600) }] } }, new Date())
    expect(root.textContent).not.toContain('x'.repeat(301))
    root.textContent = ''
    render(root, {}, new Date())
    expect(root.textContent).toContain('Shared status unavailable')
    expect(root.querySelector('.big')).toBeNull()
  })

  it('starts without a tool call and only references the data tool for refresh', () => {
    const { sent } = load()
    expect(sent.some((m) => m.method === 'ui/initialize')).toBe(true)
    expect(sent.filter((m) => m.method === 'tools/call')).toEqual([])
    const names = new Set((html.match(/agentscm_[a-z_]+/g) ?? []).filter((n) => n !== 'agentscm_view_settings'))
    expect([...names]).toEqual(['agentscm_work_overview'])
  })
})
