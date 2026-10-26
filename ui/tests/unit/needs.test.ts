import { describe, expect, it } from 'vitest'
import type { Derived } from '../../src/derive.ts'
import { areaOf, areas, crossings, needsYou } from '../../src/needs.ts'
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
    expect(crossings(null, null)).toBe(0)
  })
})

describe('areas', () => {
  it('a folder spec and the files under it share one area; top-level files share the root', () => {
    expect(['src/**', 'src/billing.rs', './src/tax.rs'].map(areaOf)).toEqual(['src', 'src', 'src'])
    expect(['README.md', '**'].map(areaOf)).toEqual(['', ''])
  })
  it('joins holders and planned tasks per folder, with crossings first', () => {
    const d = derived({
      agents: [{ id: 's1', name: 'codex', owner: 'dev', holdings: [{ spec: 'docs/a.md', contested: false }] } as Derived['agents'][number]],
      contested: [{ path: 'web/app.ts', holders: [] }],
    })
    const got = areas(overview([item('w', { scope: ['web/**', 'docs/b.md'] })]), d)
    expect(got.map((a) => [a.name, a.holders.length, a.planned.length, a.crossings])).toEqual([
      ['web', 0, 1, 1],
      ['docs', 1, 1, 0],
    ])
  })
})
