import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const read = (name: string) => readFileSync(resolve(process.cwd(), `../src/hub/mcp_views/${name}`), 'utf8')
const now = Math.floor(Date.now() / 1000)

type Msg = { jsonrpc: string; id?: number; method?: string; params?: { name?: string; arguments?: Record<string, unknown> } }
type Data = Record<string, unknown>

// Loads a view's script into jsdom with a fake host: a recording parent window, a controllable
// document.hidden / visibilitychange, an IntersectionObserver the test can fire, and fake timers.
function mount(file: string, tool: string) {
  const parsed = new DOMParser().parseFromString(read(file), 'text/html')
  const script = parsed.querySelector('script')!
  const code = script.textContent!
  script.remove()
  document.body.innerHTML = parsed.body.innerHTML
  const sent: Msg[] = []
  const answered = new Set<number>()
  const listeners: Record<string, () => void> = {}
  let hidden = false
  let message: (e: unknown) => void = () => {}
  let intersect: (entries: { isIntersecting: boolean }[]) => void = () => {}
  const doc = new Proxy(document, {
    get(target, prop) {
      if (prop === 'hidden') return hidden
      if (prop === 'addEventListener') return (name: string, fn: () => void) => void (listeners[name] = fn)
      const value = Reflect.get(target, prop, target)
      return typeof value === 'function' ? value.bind(target) : value
    },
  })
  const parent = { postMessage: (m: Msg) => void sent.push(m) }
  const win = { parent, crypto: window.crypto, addEventListener: (_: string, fn: typeof message) => void (message = fn) }
  class Observer {
    constructor(fn: typeof intersect) {
      intersect = fn
    }
    observe() {}
  }
  new Function('window', 'document', 'ResizeObserver', 'IntersectionObserver', code)(win, doc, undefined, Observer)

  const host = {
    sent,
    root: document.getElementById('app')!,
    calls: (name = tool) => sent.filter((m) => m.method === 'tools/call' && m.params?.name === name),
    inFlight: () => host.calls().filter((m) => !answered.has(m.id!)),
    reply: async (id: number, result: unknown) => {
      answered.add(id)
      message({ source: parent, data: { jsonrpc: '2.0', id, result } })
      await vi.advanceTimersByTimeAsync(0)
    },
    answer: (data: Data) => host.reply(host.inFlight()[0]!.id!, { structuredContent: data }),
    reject: async () => {
      const id = host.inFlight()[0]!.id!
      answered.add(id)
      message({ source: parent, data: { jsonrpc: '2.0', id, error: { code: -1, message: 'x' } } })
      await vi.advanceTimersByTimeAsync(0)
    },
    push: (data: Data) => message({ source: parent, data: { jsonrpc: '2.0', method: 'ui/notifications/tool-result', params: { structuredContent: data } } }),
    hide: async (value: boolean) => {
      hidden = value
      listeners.visibilitychange?.()
      await vi.advanceTimersByTimeAsync(0)
    },
    onscreen: async (value: boolean) => {
      intersect([{ isIntersecting: value }])
      await vi.advanceTimersByTimeAsync(0)
    },
    wait: (ms: number) => vi.advanceTimersByTimeAsync(ms),
    fold: (label: string) =>
      Array.from(document.querySelectorAll('details')).find((d) => d.firstElementChild?.textContent?.includes(label)) as HTMLDetailsElement,
    refreshButton: () => document.querySelector<HTMLButtonElement>('button[aria-label="Refresh"]')!,
  }
  return host
}

const item = (over: Data) => ({ id: 'wrk_a', title: 'Task', state: 'active', claim_status: 'active', last_checkin_at: now - 60, lead_agent: 'claude-code', owner_login: 'maria', repository: 'acme/widgets', ...over })
const overview = (current: Data[]) => ({ current, recent: [], overlaps: [], current_complete: true, overlaps_complete: true, recent_complete: true, history_retention_gap: false })
const request = (id: string, status: string) => ({
  id, status, updated_at: now - 120, proposal: { title: id }, requester: { agent: 'a' },
  targets: [{ lead: { agent: 'b' }, reasons: [{ your_path: 'p1' }], reasons_complete: true }],
  acknowledgements: [{ agent: { agent: 'b' }, target_work_id: 'w', acknowledged_at: now - 60 }], acknowledgements_complete: true,
  responses: [{ responder: { agent: 'b' }, kind: 'join', note: 'On it', created_at: now - 30 }], responses_complete: true,
})
const inbox = (requests: Data[], over: Data = {}) => ({ requests, complete: true, ...over })

const views = [
  {
    name: 'work overview', file: 'work-overview.html', tool: 'agentscm_work_overview', fold: 'All work',
    before: overview([item({})]),
    after: overview([item({}), item({ id: 'wrk_b', title: 'Stuck', state: 'blocked', blocker: { summary: 'needs api' } })]),
    said: '1 thing needs a person',
  },
  {
    name: 'coordination inbox', file: 'coordination-inbox.html', tool: 'agentscm_coordination_inbox', fold: 'Receipts',
    before: inbox([request('crq_1', 'pending')]),
    after: inbox([request('crq_1', 'responded')]),
    said: '1 replied',
  },
]

describe('MCP views auto-refresh', () => {
  beforeEach(() => vi.useFakeTimers())
  afterEach(() => {
    vi.useRealTimers()
    document.body.innerHTML = ''
  })

  it.each(views)('$name re-calls its own tool every 30 s, only while on screen', async (v) => {
    const host = mount(v.file, v.tool)
    host.push(v.before)
    await host.wait(29_999)
    expect(host.calls()).toHaveLength(0)
    await host.wait(1)
    expect(host.calls()).toHaveLength(1)
    expect(host.calls()[0]!.params!.arguments).toEqual({})
    await host.answer(v.before)

    await host.hide(true)
    await host.wait(10 * 60_000)
    expect(host.calls()).toHaveLength(1)
    await host.hide(false)
    expect(host.calls()).toHaveLength(2)
    await host.answer(v.before)

    await host.onscreen(false)
    await host.wait(10 * 60_000)
    expect(host.calls()).toHaveLength(2)
    await host.onscreen(true)
    expect(host.calls()).toHaveLength(3)
  })

  it.each(views)('$name does not restart the timer early when it comes back before 30 s', async (v) => {
    const host = mount(v.file, v.tool)
    host.push(v.before)
    await host.wait(10_000)
    await host.hide(true)
    await host.wait(5_000)
    await host.hide(false)
    await host.wait(14_999)
    expect(host.calls()).toHaveLength(0)
    await host.wait(1)
    expect(host.calls()).toHaveLength(1)
  })

  it.each(views)('$name never has two calls in flight', async (v) => {
    const host = mount(v.file, v.tool)
    host.push(v.before)
    await host.wait(30_000)
    expect(host.inFlight()).toHaveLength(1)

    await host.hide(true)
    await host.hide(false)
    await host.onscreen(true)
    await host.wait(10 * 60_000)
    expect(host.calls()).toHaveLength(1)

    host.refreshButton().click()
    host.refreshButton().click()
    expect(host.calls()).toHaveLength(1)

    await host.answer(v.before)
    expect(host.calls()).toHaveLength(2)
    expect(host.inFlight()).toHaveLength(1)
    await host.answer(v.before)
    expect(host.calls()).toHaveLength(2)
    await host.wait(30_000)
    expect(host.calls()).toHaveLength(3)
  })

  it.each(views)('$name backs off after errors, keeps the last data and recovers', async (v) => {
    const host = mount(v.file, v.tool)
    host.push(v.before)
    const shown = host.root.textContent
    await host.wait(30_000)
    await host.reject()
    expect(host.root.textContent).toContain('Could not refresh. Showing data from')
    expect(host.root.textContent).toContain(shown!.slice(0, 30))

    await host.wait(59_999)
    expect(host.calls()).toHaveLength(1)
    await host.wait(1)
    expect(host.calls()).toHaveLength(2)
    await host.reject()
    await host.wait(119_999)
    expect(host.calls()).toHaveLength(2)
    await host.wait(1)
    expect(host.calls()).toHaveLength(3)

    await host.answer(v.before)
    expect(host.root.textContent).not.toContain('Could not refresh')
    await host.wait(30_000)
    expect(host.calls()).toHaveLength(4)
  })

  it.each(views)('$name keeps open folds, focus and scroll across a refresh and announces the change politely', async (v) => {
    const host = mount(v.file, v.tool)
    host.push(v.before)
    const open = host.fold(v.fold)
    open.open = true
    open.dispatchEvent(new Event('toggle'))
    host.refreshButton().focus()
    document.documentElement.scrollTop = 120

    await host.wait(30_000)
    await host.answer(v.after)
    expect(host.fold(v.fold)).not.toBe(open)
    expect(host.fold(v.fold).open).toBe(true)
    expect(document.activeElement).toBe(host.refreshButton())
    expect(document.documentElement.scrollTop).toBe(120)

    const live = document.getElementById('live')!
    expect(live.getAttribute('role')).toBe('status')
    expect(live.getAttribute('aria-live')).toBe('polite')
    expect(live.textContent).toContain(v.said)
    live.textContent = ''
    await host.wait(30_000)
    await host.answer(v.after)
    expect(live.textContent).toBe('')
    expect(host.sent.filter((m) => m.method === 'ui/message' || m.method === 'ui/update-model-context')).toEqual([])
  })

  it.each(views)('$name has no conversation-pushing call in its source', (v) => {
    expect(read(v.file)).not.toMatch(/ui\/message|ui\/update-model-context/)
  })

  it('inbox refreshes the page the person is on, and Refresh returns to the first page', async () => {
    const v = views[1]!
    const host = mount(v.file, v.tool)
    host.push(inbox([request('crq_1', 'pending')], { complete: false, next_after: 5, through: 9 }))
    Array.from(document.querySelectorAll('button')).find((b) => b.textContent === 'Next page')!.click()
    expect(host.calls()[0]!.params!.arguments).toEqual({ after: 5, through: 9 })
    await host.answer(inbox([request('crq_2', 'pending')]))
    expect(host.root.textContent).toContain('Showing a later page')

    await host.wait(30_000)
    expect(host.calls()[1]!.params!.arguments).toEqual({ after: 5, through: 9 })
    await host.answer(inbox([request('crq_2', 'responded')]))
    expect(host.root.textContent).toContain('Showing a later page')

    host.refreshButton().click()
    expect(host.calls()[2]!.params!.arguments).toEqual({})
  })

  it('overview keeps loaded catch-up pages and their open source details across a refresh', async () => {
    const v = views[0]!
    const host = mount(v.file, v.tool)
    host.push(v.before)
    const catchup = () => host.calls('agentscm_work_catchup')
    const client = catchup()[0]!.params!.arguments!.delivery_client as string
    const page = (over: Data) => ({
      subscriber: 'human', delivery_client: client, delivery: `cup_${'a'.repeat(32)}`,
      event_from: 2, event_through: 3, event_after: 2, next_event_after: 2,
      merge_from: 0, merge_through: 1, merge_after: 0, next_merge_after: 1,
      work_events: [], coordination_events: [], merges: [],
      retention: { work_gap: false, coordination_gap: false, merge_gap: false, event_available_from: 2, merge_available_from: 0 },
      source_coverage: [], complete: false, ...over,
    })
    const merge = {
      id: 'merge-1', seq: 1, source: 'forge_pull', source_id: 'github:129', occurred_at: null, observed_at: 150, title: 'First merge',
      branch: 'feat/a', base: 'main', head_commit: '1'.repeat(40), merge_commit: '2'.repeat(40), pull_number: 129, paths: ['ui/a.ts'], paths_complete: true,
    }
    await host.reply(catchup()[0]!.id!, { structuredContent: page({ merges: [merge] }) })
    expect(host.root.textContent).toContain('First merge')

    Array.from(document.querySelectorAll('button')).find((b) => b.textContent === 'Load more')!.click()
    expect(catchup()).toHaveLength(2)
    const next = page({
      event_after: 2, next_event_after: 3, merge_after: 1, next_merge_after: 1, complete: true,
      work_events: [{ seq: 3, at: 150, kind: 'checkpoint', work_id: 'wrk_x', item: { title: 'Second page work', lead_agent: 'codex', current: { summary: 'did it' } } }],
    })
    await host.reply(catchup()[1]!.id!, { structuredContent: next })
    expect(host.root.textContent).toContain('Second page work')

    const details = Array.from(document.querySelectorAll<HTMLDetailsElement>('details.sd')).find((d) => d.textContent?.includes('merge-1'))!
    details.open = true
    details.dispatchEvent(new Event('toggle'))

    await host.wait(30_000)
    await host.answer(v.after)
    expect(catchup()).toHaveLength(2)
    expect(host.root.textContent).toContain('First merge')
    expect(host.root.textContent).toContain('Second page work')
    expect(Array.from(document.querySelectorAll<HTMLDetailsElement>('details.sd')).find((d) => d.textContent?.includes('merge-1'))!.open).toBe(true)
  })
})
