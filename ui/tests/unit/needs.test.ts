import { describe, expect, it } from 'vitest'
import type { Contested, Derived } from '../../src/derive.ts'
import { areaOf, areas, needsYou } from '../../src/needs.ts'
import type { WorkItem, WorkOverview } from '../../src/standup.ts'

const item = (id: string, over: Partial<WorkItem>): WorkItem =>
  ({ id, title: id, state: 'active', claim_status: 'active', updated_at: 100, last_checkin_at: 100, blocker: null, scope: [], lead_agent: 'codex', owner_login: 'dev', ...over }) as WorkItem
const overview = (current: WorkItem[], overlaps: WorkOverview['overlaps'] = []) => ({ current, recent: [], overlaps }) as unknown as WorkOverview
const derived = (over: Partial<Derived>) => ({ agents: [], groups: [], filesHeld: 0, contested: [], prOverlaps: [], otherOpenPrs: 0, intentById: new Map(), ...over }) as Derived

describe('needsYou', () => {
  it('lists only decisions, worst first: a blocker before a contested file before a quiet claim before a handover', () => {
    const ov = overview([
      item('handover', { state: 'handover' }),
      item('quiet', { claim_status: 'stale' }),
      item('fine', {}),
      item('blocked', { state: 'blocked' }),
    ])
    const d = derived({ contested: [{ path: 'src/a.rs', holders: [] }] })
    expect(needsYou(ov, d).map((n) => n.key)).toEqual(['blocked:blocked', 'contested:src/a.rs', 'quiet:quiet', 'handover:handover'])
  })
  it('a quiet lead counts once as quiet, even when the task is also offered for handover', () => {
    expect(needsYou(overview([item('h', { state: 'handover', claim_status: 'unknown' })]), null).map((n) => n.kind)).toEqual(['quiet'])
  })
  it('nothing loaded is nothing to decide, not an error', () => {
    expect(needsYou(null, null)).toEqual([])
  })
})

describe('who can act', () => {
  const agent = (owner: string, name = 'codex') => ({ id: `${owner}:${name}`, owner, name })
  const ask = (over: Partial<NonNullable<Contested['handoff']>> = {}) => ({ request: 'r', at: 1, from: { owner: 'dev', name: 'cursor' }, to: { owner: 'maria', name: 'claude-code' }, note: null, ...over })
  const party = (c: Omit<Contested, 'path'>) => needsYou(null, derived({ contested: [{ path: 'src/a.rs', ...c }] }))[0]!.who
  it('blockers, quiet claims, handovers, overlaps and pull requests are a person’s to act on', () => {
    const ov = overview([item('b', { state: 'blocked' }), item('q', { claim_status: 'stale' }), item('h', { state: 'handover' })], [{ left: item('l', {}), right: item('r', {}), reasons: [] }] as unknown as WorkOverview['overlaps'])
    const d = derived({ prOverlaps: [{ pr: { number: 7 }, with: [] }] as unknown as Derived['prOverlaps'] })
    expect(needsYou(ov, d).map((n) => n.who)).toEqual(['you', 'you', 'you', 'you', 'you'])
  })
  it('a paused lead gave its reason, so the agents have it in hand', () => {
    expect(needsYou(overview([item('p', { state: 'paused', claim_status: 'stale' })]), null).map((n) => [n.kind, n.who])).toEqual([['quiet', 'agents']])
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
