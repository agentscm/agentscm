import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { screen } from '@testing-library/dom'
import { describe, expect, it } from 'vitest'

const html = readFileSync(resolve(process.cwd(), '../src/hub/mcp_views/work-overview.html'), 'utf8')

describe('MCP work overview merge recap', () => {
  it('has valid script syntax and bounds untrusted rows to twenty text-only entries', () => {
    const script = new DOMParser().parseFromString(html, 'text/html').querySelector('script')?.textContent
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

  it('renders a text-only fixed catch-up delivery and withholds acknowledgement while partial', () => {
    const script = new DOMParser().parseFromString(html, 'text/html').querySelector('script')?.textContent
    document.body.innerHTML = '<div id="app"></div><div id="find"></div>'
    const load = new Function('window', 'document', 'ResizeObserver', `${script}; return {CATCH_CLIENT,CATCH,catchupValue,renderCatchup,appendCatch};`) as (
      win: Window,
      doc: Document,
      observer: undefined,
    ) => { CATCH_CLIENT: string; CATCH: { page: unknown; busy: boolean; err: string | null; note: null }; catchupValue: (value: unknown) => unknown; renderCatchup: (root: HTMLElement) => void; appendCatch: (first: unknown, next: unknown) => unknown }
    const { CATCH_CLIENT, CATCH, catchupValue, renderCatchup, appendCatch } = load(window, document, undefined)
    const value = {
      subscriber: 'human', delivery_client: CATCH_CLIENT, delivery: `cup_${'a'.repeat(32)}`,
      event_from: 2, event_through: 3, event_after: 2, next_event_after: 2,
      merge_from: 0, merge_through: 1, merge_after: 0, next_merge_after: 1,
      work_events: [], coordination_events: [],
      merges: [{
        id: 'merge-1', seq: 1, source: 'forge_pull', source_id: 'github:129', occurred_at: null, observed_at: 150,
        title: '<img src=x onerror=alert(1)>', branch: 'feat/catchup', base: 'main', head_commit: '1'.repeat(40), merge_commit: '2'.repeat(40), pull_number: 129,
        paths: ['ui/src/catchup.ts'], paths_complete: false,
      }],
      retention: { work_gap: true, coordination_gap: false, merge_gap: false, event_available_from: 2, merge_available_from: 0 },
      source_coverage: [
        { source: 'tracked_publication', state: 'observed_only', checked_at: 150, unread_pages: 0, deferred: 0 },
        { source: 'forge_pull', state: 'incomplete', checked_at: 150, unread_pages: 2, deferred: 1 },
      ],
      complete: false,
    }
    CATCH.page = catchupValue(value)
    const root = document.getElementById('app')!
    renderCatchup(root)

    expect(root.textContent).toContain('Since you last checked')
    expect(root.textContent).toContain('<img src=x onerror=alert(1)>')
    expect(root.textContent).toContain('Earlier records are no longer retained')
    expect(root.textContent).toContain('2 source pages remain unread')
    expect(CATCH_CLIENT).toMatch(/^mcp_/)
    expect(html).toContain("catchCall({subscriber:'human',delivery_client:CATCH_CLIENT},false)")
    expect(document.querySelector('img')).toBeNull()
    expect(screen.getByRole('button', { name: 'Load more' })).toBeTruthy()
    expect(screen.queryByRole('button', { name: 'Mark reviewed' })).toBeNull()
    expect(catchupValue({ ...value, complete: true })).toBeNull()
    const sanitizedTitle = 'Family 👨👩👧👦'
    const sanitized = catchupValue({ ...value, merges: [{ ...value.merges[0], title: sanitizedTitle }] }) as { merges: Array<{ title: string }> }
    expect(sanitized.merges[0]?.title).toBe(sanitizedTitle)
    CATCH.page = sanitized
    renderCatchup(root)
    expect(root.textContent).toContain(sanitizedTitle)
    expect(catchupValue({ ...value, merges: [{ ...value.merges[0], title: 'Family 👨‍👩‍👧‍👦' }] })).toBeNull()
    expect(catchupValue({ ...value, merges: [{ ...value.merges[0], title: 'Family \u202ereversed' }] })).toBeNull()
    const refreshedCoverage = value.source_coverage.map((source) => ({ ...source, state: 'unavailable', checked_at: 200 }))
    const next = catchupValue({ ...value, next_event_after: 3, merge_after: 1, merges: [], source_coverage: refreshedCoverage, complete: true })
    expect(appendCatch(CATCH.page, next)).toMatchObject({ complete: true, source_coverage: refreshedCoverage })

  })
})
