import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'

const read = (name: string) => readFileSync(resolve(process.cwd(), `../src/hub/mcp_views/${name}`), 'utf8')
const files = { overview: read('work-overview.html'), inbox: read('coordination-inbox.html') }

type View = { render: (root: HTMLElement, data: unknown, at: Date) => void }
function load(html: string): View {
  const script = new DOMParser().parseFromString(html, 'text/html').querySelector('script')!.textContent!
  document.body.innerHTML = '<div id="app"></div>'
  const make = new Function('window', 'document', 'ResizeObserver', `${script}; return {render};`) as (w: Window, d: Document, o: undefined) => View
  return make(window, document, undefined)
}
const now = Math.floor(Date.now() / 1000)
const item = (over: Record<string, unknown>) => ({
  id: 'wrk_a', title: 'Task', state: 'active', claim_status: 'active', last_checkin_at: now - 60, lead_agent: 'claude-code', owner_login: 'maria', repository: 'acme/widgets', ...over,
})

describe('MCP views Twilight restyle', () => {
  it.each(Object.entries(files))('%s stays self-contained, icon-only and under the size cap', (_name, html) => {
    expect(Buffer.byteLength(html)).toBeLessThan(60_000)
    expect(html).not.toMatch(/https?:\/\/(?!www\.w3\.org\/2000\/svg)/)
    expect(html).not.toMatch(/[⊘≠⇄▶‖⏳↩✓⚠▼⏱]/)
    expect(html).not.toMatch(/text-transform:\s*uppercase|<img|innerHTML|@import|<link/)
    expect(html).toContain('#17142e')
    expect(html).toContain('prefers-reduced-motion')
    expect(html).toContain('focus-visible')
  })

  it('overview leads with one sentence and only the rows that need a person, as text', () => {
    const { render } = load(files.overview)
    const root = document.getElementById('app')!
    render(root, {
      current: [
        item({ title: '<img src=x onerror=alert(1)>', state: 'blocked', blocker: { summary: '<b>needs api</b>' } }),
        item({ id: 'wrk_b', title: 'Fine', state: 'active' }),
      ],
      recent: [], overlaps: [], current_complete: true, overlaps_complete: true, recent_complete: true, history_retention_gap: false,
    }, new Date())
    expect(root.querySelector('.big')!.textContent).toBe('1 agent working. 1 thing needs a person.')
    expect(root.querySelectorAll('.needs li')).toHaveLength(1)
    expect(root.textContent).toContain('<img src=x onerror=alert(1)>')
    expect(root.textContent).toContain('<b>needs api</b>')
    expect(root.querySelector('img,b')).toBeNull()
    expect(root.querySelector('button[aria-label="Refresh"]')).toBeTruthy()
    expect(root.querySelector('details.f')!.textContent).toContain('All work')
    expect(root.querySelector('input')).toBeNull()
  })

  it('overview names no tool but its own', () => {
    const names = new Set((files.overview.match(/agentscm_[a-z_]+/g) ?? []).filter((n) => n !== 'agentscm_view_settings'))
    expect([...names]).toEqual(['agentscm_work_overview'])
  })

  it('overview folds only All work and points to the Hub for catch-up', () => {
    const { render } = load(files.overview)
    const root = document.getElementById('app')!
    render(root, {
      current: [item({})], recent: [], overlaps: [], current_complete: true, overlaps_complete: true, recent_complete: true, history_retention_gap: false,
    }, new Date())
    const folds = root.querySelectorAll('details.f')
    expect(folds).toHaveLength(1)
    expect(folds[0]!.textContent).toContain('All work')
    for (const text of ['merge observation', 'Mark reviewed', 'Briefing', 'Load more']) expect(root.textContent).not.toContain(text)
    expect(root.textContent).toContain('Hub › Catch up')
  })

  it('overview says how many unfinished items are not listed because their branch merged', () => {
    const { render } = load(files.overview)
    const root = document.getElementById('app')!
    const data = { current: [item({})], recent: [], overlaps: [], current_complete: true, overlaps_complete: true, recent_complete: true, history_retention_gap: false }
    render(root, { ...data, settled_count: 2 }, new Date())
    expect(root.querySelector('details.f')!.textContent).toContain('1 task · 0 finished recently · 2 with a merged PR')
    root.textContent = ''
    render(root, data, new Date())
    expect(root.textContent).not.toContain('merged PR')
  })

  it('starting the overview posts no tool call', () => {
    const script = new DOMParser().parseFromString(files.overview, 'text/html').querySelector('script')!.textContent!
    const posted: { method?: string }[] = []
    const stub = { parent: { postMessage: (m: { method?: string }) => posted.push(m) }, addEventListener: () => {} }
    document.body.innerHTML = '<div id="app"></div>'
    new Function('window', 'document', 'ResizeObserver', script)(stub, document, undefined)
    expect(posted.length).toBeGreaterThan(0)
    expect(posted.filter((m) => m.method === 'tools/call')).toEqual([])
  })

  it.each([{ overlaps_complete: false, current_complete: true }, { overlaps_complete: true, current_complete: false }])(
    'overview does not claim nothing needs a person on partial data %o',
    (flags) => {
      const { render } = load(files.overview)
      const root = document.getElementById('app')!
      render(root, { current: [], recent: [], overlaps: [], recent_complete: true, history_retention_gap: false, ...flags }, new Date())
      expect(root.querySelector('.big')!.textContent).not.toContain('Nothing needs a person.')
    },
  )

  it('inbox shows the track, withdrawn state, two path chips and folded history', () => {
    const { render } = load(files.inbox)
    const root = document.getElementById('app')!
    const request = (id: string, status: string, over = {}) => ({
      id, status, updated_at: now - 120, proposal: { title: `<i>${id}</i>` }, requester: { agent: 'a' },
      targets: [{ lead: { agent: 'b' }, reasons: [{ your_path: 'p1' }, { your_path: 'p2' }, { your_path: 'p3' }], reasons_complete: true }],
      acknowledgements: [{ agent: { agent: 'b' }, target_work_id: 'w', acknowledged_at: now - 60 }], acknowledgements_complete: true,
      responses: [{ responder: { agent: 'b' }, kind: 'join', note: 'On it', created_at: now - 30 }], responses_complete: true, ...over,
    })
    render(root, { requests: [request('crq_1', 'responded'), request('crq_2', 'withdrawn')], complete: true }, new Date())
    const [first, second] = Array.from(root.querySelectorAll('article.req'))
    expect(first!.querySelectorAll('.track li.on')).toHaveLength(3)
    expect(first!.querySelector('.track li[aria-current=step]')!.textContent).toBe('Replied')
    expect(first!.querySelector('blockquote')!.textContent).toContain('On it')
    expect(Array.from(first!.querySelectorAll('.chips span')).map((s) => s.textContent)).toEqual(['p1', 'p2', '1 more'])
    expect(first!.querySelector('details.f')!.textContent).toContain('Receipts (1) and replies (1)')
    expect(second!.querySelector('.track')).toBeNull()
    expect(second!.textContent).toContain('Withdrawn')
    expect(root.textContent).toContain('<i>crq_1</i>')
    expect(root.querySelector('i')).toBeNull()
    expect(root.textContent).toContain('Read-only')
  })
})
