import { describe, expect, it } from 'vitest'
import { derive, type Contested, type Derived } from '../../src/derive.ts'
import { areaOf, areas, inboxBadge, needsYou } from '../../src/needs.ts'
import type { CoordinationRequest, WorkItem, WorkOverview } from '../../src/standup.ts'
import type { Activity, Work } from '../../src/types.ts'
import wire from '../fixtures/needs-you.json'

const item = (id: string, over: Partial<WorkItem>): WorkItem =>
  ({ id, title: id, state: 'active', claim_status: 'active', updated_at: 100, last_checkin_at: 100, blocker: null, scope: [], lead_agent: 'codex', owner_login: 'dev', ...over }) as WorkItem
const overview = (current: WorkItem[], overlaps: WorkOverview['overlaps'] = []) => ({ current, recent: [], overlaps }) as unknown as WorkOverview
const derived = (over: Partial<Derived>) => ({ agents: [], groups: [], filesHeld: 0, contested: [], prOverlaps: [], otherOpenPrs: 0, intentById: new Map(), ...over }) as Derived

describe('needsYou', () => {
  const pair = (l: Partial<WorkItem>, r: Partial<WorkItem>) => ({ left: item('l', l), right: item('r', r), reasons: [] }) as unknown as WorkOverview['overlaps'][number]
  it('lists only decisions, worst first: a blocker before a contested file before an overlap', () => {
    const ov = overview([item('fine', {}), item('blocked', { state: 'blocked' })], [pair({}, {})])
    const d = derived({ contested: [{ path: 'src/a.rs', holders: [] }] })
    expect(needsYou(ov, d).map((n) => n.key)).toEqual(['blocked:blocked', 'contested:src/a.rs', 'overlap:l:r'])
  })
  it('paused, handover and stale-claim tasks are not decisions, but a blocked one stays even when its claim is stale', () => {
    const ov = overview([
      item('handover', { state: 'handover', claim_status: 'stale' }),
      item('paused', { state: 'paused' }),
      item('stale', { claim_status: 'stale' }),
      item('unknown', { claim_status: 'unknown' }),
      item('stuck', { state: 'blocked', claim_status: 'stale' }),
    ])
    expect(needsYou(ov, null).map((n) => n.key)).toEqual(['blocked:stuck'])
  })
  it('an overlap is live only when both sides hold an active claim and are active or blocked', () => {
    const keys = (o: ReturnType<typeof pair>) => needsYou(overview([], [o]), null).map((n) => n.kind)
    expect(keys(pair({}, {}))).toEqual(['overlap'])
    expect(keys(pair({ state: 'blocked' }, {}))).toEqual(['overlap'])
    expect(keys(pair({ claim_status: 'stale' }, {}))).toEqual([])
    expect(keys(pair({}, { claim_status: 'unknown' }))).toEqual([])
    expect(keys(pair({ state: 'paused' }, {}))).toEqual([])
    expect(keys(pair({}, { state: 'handover' }))).toEqual([])
  })
  it('nothing loaded is nothing to decide, not an error', () => {
    expect(needsYou(null, null)).toEqual([])
  })
})

describe('a request to an idle task', () => {
  const lead = (login: string) => ({ token: `t-${login}`, agent: 'claude-code', session: 's', owner: login, login })
  const request = (over: Partial<CoordinationRequest> = {}) => ({
    id: 'crq_1', requester: lead('maria'), status: 'pending', responses: [], acknowledgements: [], created_at: 50,
    targets: [{ work_id: 'mine', lead: lead('dev'), generation: 1, reasons: [{ your_path: 'ui/src/pages/Catchup.tsx', their_path: 'ui/src/pages/Catchup.tsx' }], reasons_complete: true }],
    ...over,
  }) as unknown as CoordinationRequest
  const mine = (over: Partial<WorkItem>) => overview([item('mine', { owner_login: 'dev', ...over })])
  it('needs the owner when their task is paused or quiet, even though a quiet task alone is not listed', () => {
    for (const over of [{ state: 'paused', claim_status: 'stale' }, { claim_status: 'stale' }, { state: 'paused' }] as Partial<WorkItem>[]) {
      expect(needsYou(mine(over), null, { requests: [request()], me: 'dev' }).map((n) => [n.kind, n.who, n.path])).toEqual([['asked', 'you', 'ui/src/pages/Catchup.tsx']])
    }
  })
  it('leaves it to the agents while the lead is running, answered, closed, or someone else’s', () => {
    const reply = { target_work_id: 'mine', target_generation: 1, seq: 9 } as unknown as CoordinationRequest['responses'][number]
    expect(needsYou(mine({}), null, { requests: [request()], me: 'dev' })).toEqual([])
    expect(needsYou(mine({ claim_status: 'stale' }), null, { requests: [request({ responses: [reply] })], me: 'dev' }).map((n) => n.kind)).toEqual([])
    expect(needsYou(mine({ claim_status: 'stale' }), null, { requests: [request({ status: 'withdrawn' })], me: 'dev' }).map((n) => n.kind)).toEqual([])
    expect(needsYou(mine({ claim_status: 'stale' }), null, { requests: [request()], me: 'maria' }).map((n) => n.kind)).toEqual([])
  })
  it('the Inbox badge counts live overlaps and every unanswered request to the viewer’s agents, running or not', () => {
    const reply = { target_work_id: 'mine', target_generation: 1, seq: 9 } as unknown as CoordinationRequest['responses'][number]
    const ov = overview([item('mine', { owner_login: 'dev' })], [{ left: item('l', {}), right: item('r', {}), reasons: [] }, { left: item('l', {}), right: item('s', { claim_status: 'stale' }), reasons: [] }] as unknown as WorkOverview['overlaps'])
    expect(inboxBadge(ov, { requests: [request(), request({ id: 'crq_2', responses: [reply] }), request({ id: 'crq_3', status: 'resolved' })], me: 'dev' })).toEqual({ n: 2, tip: '1 overlap to talk about · 1 request waiting on you' })
    expect(inboxBadge(ov, { requests: [request()], me: 'maria' })).toEqual({ n: 1, tip: '1 overlap to talk about' })
    expect(inboxBadge(null, undefined)).toEqual({ n: 0, tip: '' })
  })
})

describe('who can act', () => {
  const agent = (owner: string, name = 'codex') => ({ id: `${owner}:${name}`, owner, name })
  const ask = (over: Partial<NonNullable<Contested['handoff']>> = {}) => ({ request: 'r', at: 1, from: { owner: 'dev', name: 'cursor' }, to: { owner: 'maria', name: 'claude-code' }, note: null, ...over })
  const party = (c: Omit<Contested, 'path'>) => needsYou(null, derived({ contested: [{ path: 'src/a.rs', ...c }] }))[0]!.who
  it('blockers, overlaps and pull requests are a person’s to act on', () => {
    const ov = overview([item('b', { state: 'blocked' })], [{ left: item('l', {}), right: item('r', {}), reasons: [] }] as unknown as WorkOverview['overlaps'])
    const d = derived({ prOverlaps: [{ pr: { number: 7 }, with: [] }] as unknown as Derived['prOverlaps'] })
    expect(needsYou(ov, d).map((n) => n.who)).toEqual(['you', 'you', 'you'])
  })
  it('two holders, or an edit let through after a refusal, need a person', () => {
    expect(party({ holders: [agent('dev'), agent('maria')] })).toBe('you')
    expect(party({ holders: [agent('maria')], refused: { at: 1, owner: 'dev', name: 'cursor' }, repeated: { at: 2, owner: 'dev', name: 'cursor' } })).toBe('you')
    expect(party({ holders: [agent('dev'), agent('maria')], handoff: ask({ answer: { kind: 'declined', at: 2 } }) })).toBe('you')
  })
  it('a pending, answered or lapsed handoff, or a refusal alone, is the agents’ to settle', () => {
    expect(party({ holders: [agent('dev'), agent('maria')], handoff: ask() })).toBe('agents')
    expect(party({ holders: [agent('maria')], handoff: ask({ answer: { kind: 'granted', at: 2 } }) })).toBe('agents')
    expect(party({ holders: [agent('maria')], handoff: ask({ answer: { kind: 'declined', at: 2, reason: 'busy' } }) })).toBe('agents')
    expect(party({ holders: [], handoff: ask({ lapsed: true }) })).toBe('agents')
    expect(party({ holders: [agent('maria')], refused: { at: 1, owner: 'dev', name: 'cursor' } })).toBe('agents')
  })
})

describe('areas', () => {
  it('a folder spec and the files under it share one area; top-level files share the root', () => {
    expect(['src/**', 'src/billing.rs', './src/tax.rs'].map(areaOf)).toEqual(['src', 'src', 'src'])
    expect(['README.md', '**'].map(areaOf)).toEqual(['', ''])
  })
  it('joins holders and planned tasks per folder, with contested files and overlaps first', () => {
    const d = derived({
      agents: [{ id: 's1', name: 'codex', owner: 'dev', holdings: [{ spec: 'docs/a.md', contested: false }] } as Derived['agents'][number]],
      contested: [{ path: 'web/app.ts', holders: [] }],
    })
    const got = areas(overview([item('w', { scope: ['web/**', 'docs/b.md'] })]), d)
    expect(got.map((a) => [a.name, a.holders.length, a.planned.length, a.contested, a.overlaps])).toEqual([
      ['web', 0, 1, 1, 0],
      ['docs', 1, 1, 0, 0],
    ])
  })
})

describe('the menu-bar companion', () => {
  // The tray (`src/tray/companion_hub.rs`) reads the same fixture and asserts the same keys.
  it('counts what needsYou lists for a person, for one shared fixture', () => {
    const d = derive(wire.work as unknown as Work, wire.activity as unknown as Activity, wire.now)
    const asked = { requests: wire.coordination.requests as unknown as CoordinationRequest[], me: wire.me }
    const keys = needsYou(wire.overview as unknown as WorkOverview, d, asked)
      .filter((n) => n.who === 'you')
      .map((n) => n.key)
    expect(keys.sort()).toEqual([...wire.expected].sort())
  })
})
