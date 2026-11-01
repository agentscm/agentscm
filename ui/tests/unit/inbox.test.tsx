import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { BrowserSession } from '../../src/devices.ts'
import type { Contested, LiveAgent } from '../../src/derive.ts'
import { InboxPage } from '../../src/pages/Inbox.tsx'
import type { WorkItem, WorkOverview } from '../../src/standup.ts'
import type { Poll } from '../../src/usePoll.ts'

const now = Math.floor(Date.now() / 1000)
const wid = (c: string) => `wrk_${c.repeat(32)}`
const item = (c: string, title: string, login: string, agent: string, over: Partial<WorkItem> = {}): WorkItem =>
  ({
    id: wid(c), title, state: 'active', claim_status: 'active', created_at: now - 1800, updated_at: now - 60, last_checkin_at: now - 60, blocker: null, scope: ['src/billing.rs'], next_action: null, current: null, branch: null,
    lead_agent: agent, lead_token: `tok_${agent}`, lead_session: 's', owner_login: login, participants: [{ login, agent, token: `tok_${agent}`, session: 's', role: 'lead', owner: `github:github.com:${login}` }], ...over,
  }) as unknown as WorkItem
const billing = item('a', 'Refactor the billing module', 'maria', 'claude-code')
const rounding = item('b', 'Fix invoice rounding', 'dev', 'codex')
const stale = item('c', 'Write the webhook retry documentation', 'maria', 'codex', { claim_status: 'stale', scope: ['docs/webhooks.md'] })
const overviewOf = (over: Partial<WorkOverview> = {}) =>
  ({
    current: [billing, rounding, stale],
    recent: [{ seq: 8, at: now - 2400, kind: 'decision', item: billing }],
    overlaps: [{ left: billing, right: rounding, reasons: [{ left_path: 'src/billing.rs', right_path: 'src/billing.rs' }], reasons_complete: true }],
    current_complete: true, recent_complete: true, overlaps_complete: true, ...over,
  }) as unknown as WorkOverview
const ready = (value: WorkOverview = overviewOf()) => ({ kind: 'ready', value, fetchedAt: 0, refreshError: null, retry: () => {} }) as Poll<WorkOverview>

const agent = (login: string, token: string, name: string) => ({ token, agent: name, session: `${name}-session`, owner: `github:github.com:${login}`, login })
const MARIA = agent('maria', 'tok_maria', 'claude-code')
const DEV = agent('dev', 'tok_dev', 'codex')
const SAM = agent('sam', 'tok_sam', 'copilot')
const request = (n: number, requester: ReturnType<typeof agent>, lead: ReturnType<typeof agent>, work: string, title: string, over: Record<string, unknown> = {}) => ({
  id: `crq_${n.toString(16).padStart(32, '0')}`, requester,
  proposal: { work_id: null, source: null, request_key: `req_${n.toString(16).padStart(32, '0')}`, title, scope: ['src/billing.rs'] },
  targets: [{ work_id: work, lead, generation: 1, reasons: [{ your_path: 'src/billing.rs', their_path: 'src/billing.rs' }], reasons_complete: true }],
  status: 'pending', acknowledgements: [], acknowledgements_complete: true, responses: [], responses_complete: true,
  created_at: now - 900, updated_at: now - 600, created_seq: n * 10, updated_seq: n * 10 + 2, ...over,
})
const ack = (target: string | null, who: ReturnType<typeof agent>, through: number, seq: number) => ({ target_work_id: target, agent: who, generation: 1, through_seq: through, acknowledged_at: now - 500, seq })
const forMe = request(1, SAM, MARIA, wid('a'), 'Round invoice totals per currency', { acknowledgements: [ack(wid('a'), MARIA, 10, 12)] })
const iAsked = request(2, MARIA, DEV, wid('b'), 'Move tax out of invoice totals', { acknowledgements: [ack(wid('b'), DEV, 20, 22)] })
const betweenOthers = request(3, SAM, DEV, wid('b'), 'Sam asks dev about ledgers')
const closed = request(4, DEV, MARIA, wid('a'), 'Settings page layout', { status: 'resolved', acknowledgements: [ack(wid('a'), MARIA, 40, 42)], updated_seq: 44 })
const page = (requests: unknown[]) => ({ from: 0, through: 200, next_after: (requests.at(-1) as { updated_seq?: number } | undefined)?.updated_seq ?? 0, requests, complete: true, retention_gap: false })

const handoffRow = (id: string, state: string, mine: 'holder' | 'requester', over: Record<string, unknown> = {}) => ({
  id, path: 'src/webhooks/retry.rs', state, note: 'I need the retry policy for the webhook docs.', reply: null, created_at: now - 420, decided_at: null, holder_told: true, requester_told: false, asked_via_hub: false, answered_via_hub: false,
  requester: mine === 'requester' ? { agent: 'claude-code', login: 'maria', mine: true, token: 'tok_maria', session: 'ses-1' } : { agent: 'codex', login: 'dev', mine: false },
  holder: mine === 'holder' ? { agent: 'claude-code', login: 'maria', mine: true, token: 'tok_maria', session: 'ses-1' } : { agent: 'codex', login: 'dev', mine: false },
  ...over,
})

const me = { me: 'maria', repo: 'acme/widgets', originUrl: 'https://github.com/acme/widgets' }
const session: BrowserSession = { login: 'maria', provider: 'github', admin: false, csrf: 'csrf-token-0123456789abcdef' }

interface Call { url: string; method: string; body: Record<string, unknown> | null; csrf: string | null }
let calls: Call[]
/** A Hub that serves these requests and handoffs and answers POSTs with `post`. */
function hub(requests: unknown[], handoffs: unknown[] = [], post: (call: Call) => Response = () => new Response('{}', { status: 500 })) {
  calls = []
  vi.stubGlobal('fetch', vi.fn(async (input: string, init?: RequestInit) => {
    const url = String(input)
    const method = init?.method ?? 'GET'
    const headers = (init?.headers ?? {}) as Record<string, string>
    const call: Call = { url, method, body: typeof init?.body === 'string' ? (JSON.parse(init.body) as Record<string, unknown>) : null, csrf: headers['x-csrf-token'] ?? null }
    calls.push(call)
    if (method === 'POST') return post(call)
    if (url.startsWith('/v1/hub/session/handoffs')) return new Response(JSON.stringify({ handoffs }), { status: 200 })
    return new Response(JSON.stringify(page(requests)), { status: 200 })
  }))
}
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

beforeEach(() => hub([]))
afterEach(() => {
  vi.unstubAllGlobals()
  window.localStorage.clear()
})

const list = () => screen.getByRole('navigation', { name: 'Conversations' })
const timeline = () => screen.getByRole('list', { name: 'What has happened' })

describe('Inbox', () => {
  it('groups conversations by who has to move, and says how many need the person', async () => {
    hub([forMe, iAsked, betweenOthers, closed], [handoffRow('hof_1', 'pending', 'holder'), handoffRow('hof_2', 'pending', 'requester', { path: 'src/ledger.rs' }), handoffRow('hof_3', 'granted', 'holder', { path: 'src/tax.rs', decided_at: now - 100, requester_told: true })])
    render(<InboxPage token="t" overview={ready()} handoff={me} session={session} />)
    await screen.findByText('Round invoice totals per currency')
    await userEvent.click(screen.getByRole('switch', { name: /Include resolved/ }))
    await screen.findByText('Settings page layout')
    const heads = within(list()).getAllByRole('heading', { level: 2 }).map((h) => h.textContent)
    expect(heads).toEqual(['Needs you', 'Waiting on others', 'Between others', 'Earlier'])
    const group = (name: string) => within(within(list()).getByRole('heading', { name }).closest('.in-group') as HTMLElement).getAllByRole('button').map((b) => b.querySelector('.in-t')!.textContent)
    // An overlap on the person's own Work, a request addressed to their agent and a file asked of their agent need them.
    expect(group('Needs you').sort()).toEqual(['Both plan to change billing.rs', 'Hand over retry.rs?', 'Round invoice totals per currency'].sort())
    expect(group('Waiting on others').sort()).toEqual(['Hand over ledger.rs?', 'Move tax out of invoice totals'].sort())
    expect(group('Between others')).toEqual(['Sam asks dev about ledgers'])
    expect(group('Earlier')).toEqual(expect.arrayContaining(['Settings page layout', 'Hand over tax.rs?', 'Refactor the billing module']))
    expect(document.querySelector('.answer-text')!.textContent).toBe('3 conversations need you.')
    // Everything the old page said is still said: the signal note, and that nothing here merges or approves work.
    expect(document.body.textContent).toContain('Nothing here merges or approves work.')
    expect(document.body.textContent).toContain('An overlap is a reason to talk, not proof the code conflicts.')
  })

  it('draws the selected conversation as a timeline in the order things happened', async () => {
    hub([{ ...forMe, status: 'responded', updated_seq: 15, responses: [{ id: `crs_${'1'.repeat(32)}`, operation_id: 'op-1', target_work_id: wid('a'), responder: MARIA, target_generation: 1, kind: 'propose_split', note: 'I take tax, you take rounding', via: 'hub', created_at: now - 100, seq: 15 }] }])
    render(<InboxPage token="t" overview={ready(overviewOf({ overlaps: [] }))} handoff={me} session={session} />)
    await screen.findAllByText('Round invoice totals per currency')
    const rows = within(timeline()).getAllByRole('listitem').map((r) => r.textContent ?? '')
    const order = ['sam’s Copilot asked to coordinate', 'AgentSCM noticed the overlap', 'Your Claude Code received it', 'You, as your Claude Code proposed a scope split · from the Hub', 'sam’s Copilot sees it at its next edit']
    expect(order.map((t) => rows.findIndex((r) => r.includes(t)))).toEqual([0, 1, 2, 3, 4])
    // The person's answer is shown as the person's, with the agent they answered for; it is never the agent's own line.
    expect(rows[3]).toContain('I take tax, you take rounding')
    expect(rows.some((r) => r.startsWith('Your Claude Code proposed'))).toBe(false)
    expect(screen.getByRole('heading', { level: 2, name: /Sam’s Copilot|sam’s Copilot/i }).textContent).toContain('wants to start “Round invoice totals per currency”')
  })

  it('keeps the typed note when the choice changes, and sends it with the CSRF token', async () => {
    hub([forMe], [], (call) =>
      json({ ...forMe, status: 'responded', updated_seq: 15, responses: [{ id: `crs_${'2'.repeat(32)}`, operation_id: String(call.body!['operation_id']), target_work_id: wid('a'), responder: MARIA, target_generation: 1, kind: call.body!['kind'], note: call.body!['note'], via: 'hub', created_at: now, seq: 15 }] }),
    )
    render(<InboxPage token="t" overview={ready(overviewOf({ overlaps: [] }))} handoff={me} session={session} />)
    await screen.findAllByText('Round invoice totals per currency')
    expect(screen.getByText('Answer as your Claude Code')).toBeTruthy()
    const note = screen.getByRole('textbox', { name: 'Note' }) as HTMLInputElement
    await userEvent.type(note, 'Take tax, I keep rounding')
    for (const name of ['Join their work', 'Hand mine over', 'Decline']) {
      await userEvent.click(screen.getByRole('button', { name }))
      expect(note.value).toBe('Take tax, I keep rounding')
      expect(screen.getByRole('button', { name }).getAttribute('aria-pressed')).toBe('true')
    }
    expect(screen.getByRole('button', { name: 'Propose a split' }).getAttribute('aria-pressed')).toBe('false')
    await userEvent.click(screen.getByRole('button', { name: 'Join their work' }))
    await userEvent.click(screen.getByRole('button', { name: 'Send' }))

    // One POST: the agent that leads the Work, the exact request version, a replay key, and the CSRF token of the page.
    await waitFor(() => expect(calls.filter((c) => c.method === 'POST')).toHaveLength(1))
    const sent = calls.find((c) => c.method === 'POST')!
    expect(sent.url).toBe('/v1/hub/session/coordination/reply')
    expect(sent.csrf).toBe(session.csrf)
    expect(sent.body).toMatchObject({ token: 'tok_maria', request_id: forMe.id, target_work_id: wid('a'), session: 'claude-code-session', expected_updated_seq: 12, kind: 'join', note: 'Take tax, I keep rounding' })
    expect(String(sent.body!['operation_id'])).toMatch(/^web-/)

    // At once the timeline shows it, marked as from the Hub, with the marker for the other agent seeing it at its next edit.
    const rows = within(timeline()).getAllByRole('listitem').map((r) => r.textContent ?? '')
    expect(rows.some((r) => r.includes('You, as your Claude Code') && r.includes('offered to join · from the Hub') && r.includes('Take tax, I keep rounding'))).toBe(true)
    expect(rows.at(-1)).toContain('sam’s Copilot')
    expect(rows.at(-1)).toContain('sees it at its next edit')
    expect(screen.getByRole('status', { name: '' }).textContent).toMatch(/Sent as your Claude Code: Join their work\. sam’s agent sees it at its next edit\./)
    expect(screen.queryByRole('button', { name: 'Send' })).toBeNull()
  })

  it('shows why a send failed and keeps the answer so it can be sent again', async () => {
    hub([forMe], [], () => json({ error: 'not_your_agent', message: 'That agent is not yours, or it is no longer connected' }, 403))
    render(<InboxPage token="t" overview={ready(overviewOf({ overlaps: [] }))} handoff={me} session={session} />)
    await screen.findAllByText('Round invoice totals per currency')
    await userEvent.type(screen.getByRole('textbox', { name: 'Note' }), 'hello')
    await userEvent.click(screen.getByRole('button', { name: 'Send' }))
    expect((await screen.findByRole('alert')).textContent).toContain('That agent is not yours, or it is no longer connected')
    expect((screen.getByRole('textbox', { name: 'Note' }) as HTMLInputElement).value).toBe('hello')
    expect(screen.getByRole('button', { name: 'Send' })).toBeTruthy()
    // A key is reused for the same answer, so a retry cannot answer twice.
    const first = calls.find((c) => c.method === 'POST')!.body!['operation_id']
    await userEvent.click(screen.getByRole('button', { name: 'Send' }))
    await waitFor(() => expect(calls.filter((c) => c.method === 'POST')).toHaveLength(2))
    expect(calls.filter((c) => c.method === 'POST')[1]!.body!['operation_id']).toBe(first)
  })

  it('turns a busy or signed-out Hub into a sentence', async () => {
    hub([forMe], [], () => json({ error: 'busy' }, 503))
    render(<InboxPage token="t" overview={ready(overviewOf({ overlaps: [] }))} handoff={me} session={session} />)
    await screen.findAllByText('Round invoice totals per currency')
    await userEvent.click(screen.getByRole('button', { name: 'Send' }))
    expect((await screen.findByRole('alert')).textContent).toContain('The Hub is busy right now. Nothing was sent')
  })

  it('answers a file handoff for the person’s own agent: hand it over or keep it', async () => {
    hub([], [handoffRow('hof_1', 'pending', 'holder')], () => json({ outcome: 'granted', requester: { agent: 'codex', login: 'dev' } }))
    render(<InboxPage token="t" overview={ready(overviewOf({ overlaps: [] }))} handoff={me} session={session} />)
    await screen.findAllByText('Hand over retry.rs?')
    expect(screen.getByRole('button', { name: 'Hand it over' }).getAttribute('aria-pressed')).toBe('true')
    expect(screen.getByRole('button', { name: 'Keep it' })).toBeTruthy()
    await userEvent.click(screen.getByRole('button', { name: 'Send' }))
    await waitFor(() => expect(calls.some((c) => c.method === 'POST')).toBe(true))
    const sent = calls.find((c) => c.method === 'POST')!
    expect(sent.url).toBe('/v1/hub/session/handoff/answer')
    expect(sent.body).toEqual({ token: 'tok_maria', session: 'ses-1', path: 'src/webhooks/retry.rs', accept: true })
    const rows = await waitFor(() => {
      const r = within(timeline()).getAllByRole('listitem').map((x) => x.textContent ?? '')
      expect(r.some((x) => x.includes('handed it over · from the Hub'))).toBe(true)
      return r
    })
    expect(rows.at(-1)).toContain('dev’s Codex')
    expect(rows.at(-1)).toContain('sees it at its next edit')
  })

  it('offers "Ask … to hand it over" only where their agent holds a file and the person’s agent wants it', async () => {
    const agents = [{ id: 'ses_1', owner: 'maria', token: 'tok_maria', name: 'claude-code', holdings: [{ spec: 'src/billing.rs', contested: true }], intent: { id: 'int_1', via: ['hook:claude'] } }] as unknown as LiveAgent[]
    const contested = [{ path: 'src/billing.rs', holders: [{ id: 'maria\u0000tok_maria', owner: 'maria', name: 'claude-code' }, { id: 'dev\u0000tok_dev', owner: 'dev', name: 'codex' }] }] as unknown as Contested[]
    hub([], [], () => json({ outcome: 'requested', holder: { agent: 'codex', login: 'dev' } }))
    const { unmount } = render(<InboxPage token="t" overview={ready()} handoff={me} session={session} live={{ agents, contested }} />)
    await screen.findAllByText('Both plan to change billing.rs')
    await userEvent.click(screen.getByRole('button', { name: 'Ask dev’s Codex to hand it over' }))
    await waitFor(() => expect(calls.some((c) => c.method === 'POST')).toBe(true))
    expect(calls.find((c) => c.method === 'POST')!.body).toEqual({ token: 'tok_maria', path: 'src/billing.rs' })
    const rows = within(timeline()).getAllByRole('listitem').map((r) => r.textContent ?? '')
    expect(rows.some((r) => r.includes('You, as your Claude Code') && r.includes('asked dev’s Codex to hand it over · from the Hub'))).toBe(true)
    expect(rows.at(-1)).toContain('sees it at its next edit')
    unmount()

    // Not when their agent does not hold it, when the person is not signed in on the web, or without the live picture.
    hub([])
    for (const props of [{ live: { agents, contested: [] }, session }, { live: { agents, contested }, session: null }, { session }]) {
      const view = render(<InboxPage token="t" overview={ready()} handoff={me} {...props} />)
      await screen.findAllByText('Both plan to change billing.rs')
      expect(screen.queryByRole('button', { name: /to hand it over/ })).toBeNull()
      view.unmount()
    }
  })

  it('does not offer to answer without a web sign-in, and still offers a prompt to copy', async () => {
    hub([forMe])
    render(<InboxPage token="t" overview={ready(overviewOf({ overlaps: [] }))} handoff={me} />)
    await screen.findAllByText('Round invoice totals per currency')
    expect(screen.queryByRole('button', { name: 'Send' })).toBeNull()
    expect(screen.getByText(/Sign in on the web to answer for your Claude Code from here/)).toBeTruthy()
    expect(screen.getByText('Copy a prompt instead')).toBeTruthy()
    expect(screen.getByRole('button', { name: 'Copy prompt for maria’s claude-code' })).toBeTruthy()
  })

  it('offers Open in only for the viewer’s own agent on an overlap', async () => {
    const { unmount } = render(<InboxPage token="t" overview={ready()} handoff={me} />)
    await screen.findAllByText('Both plan to change billing.rs')
    expect(screen.getByRole('link', { name: /Open in Claude Code/ }).getAttribute('href')).toMatch(/^claude-cli:\/\/open\?repo=acme\/widgets&q=/)
    expect(screen.queryByRole('link', { name: /Open in Codex/ })).toBeNull()
    unmount()
    render(<InboxPage token="t" overview={ready()} handoff={{ ...me, me: 'someone-else' }} />)
    await screen.findAllByText('Both plan to change billing.rs')
    expect(screen.queryByRole('link', { name: /Open in/ })).toBeNull()
  })

  it('keeps the partial, retention and quiet-agent notes', async () => {
    hub([{ ...forMe, acknowledgements_complete: false }])
    render(<InboxPage token="t" overview={ready(overviewOf({ overlaps: [], overlaps_complete: false }))} handoff={me} session={session} />)
    await screen.findAllByText('Round invoice totals per currency')
    expect(screen.getByText('Overlap list is incomplete')).toBeTruthy()
    expect(screen.getAllByText(/Some receipt or reply history is omitted/).length).toBeGreaterThan(0)
    const quiet = screen.getByText('Quiet agents and tasks with no stated files · 1').closest('details')!
    expect(quiet.open).toBe(false)
  })
})

describe('Inbox with nothing to settle', () => {
  it('shows the idle scene and says in words that nothing is happening', async () => {
    hub([])
    render(<InboxPage token="t" overview={ready(overviewOf({ overlaps: [], recent: [] }))} handoff={me} session={session} />)
    const scene = await screen.findByRole('status')
    expect(scene.className).toContain('in-idle')
    expect(within(scene).getByRole('heading', { level: 2, name: 'Waiting for a conflict. None yet.' })).toBeTruthy()
    // The art is decoration only: hidden from assistive technology and out of the tab order.
    const art = scene.querySelector('svg')!
    expect(art.getAttribute('aria-hidden')).toBe('true')
    expect(art.getAttribute('focusable')).toBe('false')
    expect(art.querySelectorAll('.in-cur')).toHaveLength(2)
    expect(document.querySelector('.answer-text')!.textContent).toBe('No two tasks plan to touch the same files, and no request is waiting.')
  })

  it('keeps the same two panes, with earlier history in the list', async () => {
    hub([closed])
    render(<InboxPage token="t" overview={ready(overviewOf({ overlaps: [] }))} handoff={me} session={session} />)
    await screen.findByRole('status')
    await userEvent.click(screen.getByRole('switch', { name: /Include resolved/ }))
    await screen.findByText('Settings page layout')
    expect(within(list()).getByRole('heading', { name: 'Earlier' })).toBeTruthy()
    expect(screen.getByRole('heading', { level: 2, name: 'Waiting for a conflict. None yet.' })).toBeTruthy()
    // Choosing a past conversation replaces the scene with it.
    await userEvent.click(within(list()).getByRole('button', { name: /Settings page layout/ }))
    expect(screen.queryByRole('heading', { name: 'Waiting for a conflict. None yet.' })).toBeNull()
    expect(within(timeline()).getByText('closed the request')).toBeTruthy()
  })

  it('does not call a partial snapshot an all-clear', async () => {
    hub([])
    render(<InboxPage token="t" overview={ready(overviewOf({ overlaps: [], recent: [], overlaps_complete: false }))} handoff={me} session={session} />)
    await screen.findByText('Overlap list is incomplete')
    expect(document.querySelector('.in-idle')!.textContent).toContain('No overlap in this partial snapshot. An empty list is not an all-clear.')
  })

  it('is fully still under reduced motion, and a phone gets one column', () => {
    const css = readFileSync(join(process.cwd(), 'src/twilight.css'), 'utf8')
    // Every animation of the scene lives in a no-preference block; the reduce block switches any left off.
    const reduce = css.slice(css.indexOf('@media (prefers-reduced-motion: reduce) {\n  .in-idle *'))
    expect(reduce.slice(0, 200)).toMatch(/\.in-idle \*[^}]*animation: none !important/)
    const motion = css.slice(css.indexOf('@media (prefers-reduced-motion: no-preference) {\n  .in-idle .in-cur-a'), css.indexOf('@media (prefers-reduced-motion: reduce) {\n  .in-idle *'))
    expect(motion).toContain('animation: in-walk-a')
    expect(motion).toContain('animation: in-walk-b')
    const outside = css.replace(motion, '')
    expect(outside).not.toMatch(/\.in-idle[^{]*\{[^}]*animation: in-/)
    expect(css).toMatch(/@container inbox \(max-width: 760px\) \{[^@]*\.in-wrap \{ grid-template-columns: minmax\(0, 1fr\);/)
  })
})

describe('Inbox lists', () => {
  it('selects another conversation from the list', async () => {
    hub([forMe, iAsked])
    render(<InboxPage token="t" overview={ready(overviewOf({ overlaps: [] }))} handoff={me} session={session} />)
    await screen.findAllByText('Round invoice totals per currency')
    expect(screen.getByRole('heading', { level: 2, name: /wants to start/ })).toBeTruthy()
    await userEvent.click(within(list()).getByRole('button', { name: /Move tax out of invoice totals/ }))
    expect(screen.getByRole('heading', { level: 2, name: /Your Claude Code asked dev’s Codex to coordinate on/ })).toBeTruthy()
    // What it is waiting for is the last thing on the timeline.
    const rows = within(timeline()).getAllByRole('listitem').map((r) => r.textContent ?? '')
    expect(rows.at(-1)).toContain('dev’s Codex')
    expect(rows.at(-1)).toContain('has not replied yet')
    fireEvent.change(screen.getByRole('textbox', { name: 'Search agents, tasks, or paths' }), { target: { value: 'nothing matches this' } })
    expect(within(list()).queryByRole('button', { name: /Move tax/ })).toBeNull()
  })
})
