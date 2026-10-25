import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { screen } from '@testing-library/dom'
import { describe, expect, it } from 'vitest'

const html = readFileSync(resolve(process.cwd(), '../src/hub/mcp_views/work-overview.html'), 'utf8')

describe('MCP work catch-up', () => {
  it('renders a text-only fixed catch-up delivery and withholds acknowledgement while partial', () => {
    const script = new DOMParser().parseFromString(html, 'text/html').querySelector('script')?.textContent
    expect(script).toBeTruthy()
    expect(() => new Function(script!)).not.toThrow()
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
