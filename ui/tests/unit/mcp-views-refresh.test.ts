import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import type { ActionableStatus } from '../../src/actionableStatus.ts'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

// Load shared evidence only at test runtime; the UI Docker build copies only ui/.
const statusFixture = JSON.parse(readFileSync(resolve(process.cwd(), '../src/hub/fixtures/actionable-status.json'), 'utf8')) as ActionableStatus

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
    result: (result: Data) => message({ source: parent, data: { jsonrpc: '2.0', method: 'ui/notifications/tool-result', params: result } }),
    push: (data: Data) => host.result({ structuredContent: data }),
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
const overview = (current: Data[]) => ({ actionable_status: { ...statusFixture, counts: { ...statusFixture.counts, active: current.filter((w) => w.state === 'active').length, needs_you: current.filter((w) => w.state === 'blocked').length }, actions: [], work: current.map((w) => ({ ...statusFixture.work[0], title: w.title, state: w.state, needs_you: w.state === 'blocked' })) } })
const views = [
  {
    name: 'work overview', file: 'work-overview.html', tool: 'agentscm_work_overview', fold: 'All work', args: { detail: 'full' },
    before: overview([item({})]),
    after: overview([item({}), item({ id: 'wrk_b', title: 'Stuck', state: 'blocked', blocker: { summary: 'needs api' } })]),
    said: '1 action needs you',
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
    expect(host.calls()[0]!.params!.arguments).toEqual(v.args)
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

  it("overview asks for every field when the agent's reply is compact", async () => {
    const v = views[0]!
    const host = mount(v.file, v.tool)
    host.push({ detail: 'compact', actionable_status: { detail: 'compact', counts: statusFixture.counts } })
    expect(host.calls()).toHaveLength(1)
    expect(host.calls()[0]!.params!.arguments).toEqual({ detail: 'full' })
    await host.answer(v.after)
    expect(host.root.textContent).toContain(v.said)
  })

  it.each(views)('$name has no conversation-pushing call in its source', (v) => {
    expect(read(v.file)).not.toMatch(/ui\/message|ui\/update-model-context/)
  })

  it('distinguishes loading, a rejected first result and empty success', async () => {
    const host = mount('work-overview.html', 'agentscm_work_overview')
    expect(host.root.textContent).toContain('Loading work overview…')
    const text = 'Rejected <img src=x onerror=alert(1)> argument'
    host.result({ isError: true, content: [{ type: 'text', text }] })
    expect(host.root.textContent).toContain(text)
    expect(host.root.textContent).toContain('Unavailable')
    expect(host.root.querySelector('img')).toBeNull()
    expect(host.root.textContent).not.toMatch(/Loading work overview|No data returned|No current work/)
    host.refreshButton().click()
    await host.answer({ actionable_status: { ...statusFixture, complete: true, work: [], actions: [], counts: { active: 0, needs_you: 0, awaiting_party: 0, blocked: 0, reassessment_needed: 0, held_reservations: 0 } } })
    expect(host.root.textContent).toContain('No current work in this overview.')
    expect(host.root.textContent).not.toContain(text)
  })

  it('shows a first RPC error and retries using the data-only tool', async () => {
    const host = mount('work-overview.html', 'agentscm_work_overview')
    host.refreshButton().click()
    await host.reject()
    expect(host.root.textContent).toContain('Unavailable')
    expect(host.root.textContent).toContain('x')
    expect(host.root.textContent).not.toContain('Showing data from')
    await host.wait(60_000)
    expect(host.calls()).toHaveLength(2)
    expect(host.calls()[1]!.params).toEqual({ name: 'agentscm_work_overview', arguments: { detail: 'full' } })
    await host.answer(views[0]!.before)
    expect(host.root.textContent).not.toContain('Unavailable')
  })

  it('preserves the last success and error text after a failed notification, then recovers', () => {
    const host = mount('work-overview.html', 'agentscm_work_overview')
    host.push(views[0]!.before)
    host.result({ isError: true, content: [{ type: 'text', text: 'Read refused' }] })
    expect(host.root.textContent).toMatch(/Showing data from \d\d:\d\d:\d\d/)
    expect(host.root.textContent).toContain('Read refused')
    expect(host.root.textContent).toContain('Task')
    host.push(views[0]!.after)
    expect(host.root.textContent).not.toContain('Read refused')
    expect(host.root.textContent).not.toContain('Could not refresh')
  })

  it('keeps the last rendered data and timestamp when nested refresh data is malformed', async () => {
    vi.setSystemTime(new Date(2026, 0, 1, 12, 0, 0))
    const host = mount('work-overview.html', 'agentscm_work_overview')
    host.push(views[0]!.before)
    const summary = host.root.querySelector('.big')!.textContent
    await host.wait(5_000)
    host.refreshButton().click()
    await host.answer({ actionable_status: { ...statusFixture, counts: { ...statusFixture.counts, active: 99 }, actions: [null] } })
    expect(host.root.querySelector('.big')!.textContent).toBe(summary)
    expect(host.root.textContent).toContain('Showing data from 12:00:00')
    expect(host.root.textContent).toContain('Task')
    host.refreshButton().click()
    await host.answer(views[0]!.after)
    expect(host.root.textContent).toContain('Stuck')
    expect(host.root.textContent).not.toContain('Could not refresh')
  })

  it('treats malformed results as errors, never empty work', () => {
    const host = mount('work-overview.html', 'agentscm_work_overview')
    host.result({ content: [{ type: 'text', text: 'not JSON' }] })
    expect(host.root.textContent).toContain('incomplete work overview')
    host.result({ structuredContent: { actionable_status: { work: [null], counts: {} } } })
    expect(host.root.textContent).toContain('incomplete work overview')
    expect(host.root.textContent).not.toContain('No current work')
  })
})
