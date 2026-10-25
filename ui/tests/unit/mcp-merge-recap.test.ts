import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

const html = readFileSync(resolve(process.cwd(), '../src/hub/mcp_views/work-overview.html'), 'utf8')

describe('MCP work overview merge recap', () => {
  it('has valid script syntax and bounds untrusted rows to twenty text-only entries', () => {
    const script = html.match(/<script>([\s\S]*)<\/script>/)?.[1]
    expect(script).toBeTruthy()
    expect(() => new Function(script!)).not.toThrow()

    document.body.innerHTML = '<div id="app"></div><div id="find"></div>'
    const load = new Function('window', 'document', 'ResizeObserver', `${script}; return {renderRecap,recapValue};`) as (
      win: Window,
      doc: Document,
      observer: undefined,
    ) => { renderRecap: (root: HTMLElement, value: unknown) => void; recapValue: (value: unknown) => unknown }
    const { renderRecap, recapValue } = load(window, document, undefined)
    const rows = Array.from({ length: 20 }, (_, n) => ({
      id: `pub_${n.toString(16).padStart(32, '0')}`,
      title: n === 0 ? '<img src=x onerror=alert(1)>' : `Merged change ${n}`,
      branch: n === 0 ? '<img src=x onerror=branch()>' : `feat/${n}`,
      base: 'main',
      head_commit: '1'.repeat(40),
      merge_commit: n.toString(16).padStart(40, '0'),
      pull_number: n + 1,
      updated_at: 150,
    }))
    const value = { since: 100, until: 200, time_basis: 'last_recorded_update', scope: 'tracked_publications', merges: rows, complete: false, scanned_count: 21 }
    renderRecap(document.getElementById('app')!, value)

    expect(document.querySelectorAll('.recap > .merge')).toHaveLength(3)
    expect(document.querySelectorAll('.recap details .merge')).toHaveLength(17)
    expect(document.querySelectorAll('.source')).toHaveLength(20)
    expect(document.querySelectorAll('.source code')).toHaveLength(80)
    expect(document.querySelector('img')).toBeNull()
    expect(document.body.textContent).toContain('<img src=x onerror=alert(1)>')
    expect(document.body.textContent).toContain('<img src=x onerror=branch()>')
    expect(document.body.textContent).toContain('1'.repeat(40))
    expect(document.body.textContent).toContain('Partial stored result')
    expect(html).toContain('.source code{overflow-wrap:anywhere;word-break:break-all}')
    expect(html).toContain('@media (max-width:420px)')
    expect(recapValue({ ...value, merges: [rows[0], rows[0]], complete: true, scanned_count: 2 })).toBeNull()
    expect(recapValue({ ...value, since: -1 })).toBeNull()
    expect(recapValue({ ...value, scanned_count: 22 })).toBeNull()
  })
})
