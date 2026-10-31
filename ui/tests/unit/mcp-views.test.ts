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
