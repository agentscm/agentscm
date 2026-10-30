import { describe, expect, it } from 'vitest'
import type { ChangeRecordSummaryV1 } from '../../src/changeRecord.ts'
import { lifecycle, type LifecycleInput } from '../../src/lifecycle.ts'
import type { Publication } from '../../src/publications.ts'
import type { RunView } from '../../src/skills.ts'
import type { WorkItem, WorkOverview } from '../../src/standup.ts'
import { relative } from '../../src/ui.tsx'

const NOW = 1_800_000_000_000
const W = `wrk_${'a'.repeat(32)}`
const item = (over: Partial<WorkItem> = {}): WorkItem =>
  ({
    id: W, title: 'Fix login', state: 'active', claim_status: 'active', created_at: NOW / 1000 - 7200, blocker: null, next_action: null, current: null, scope: ['src/a.rs'], branch: 'fix-login',
    lead_agent: 'codex', lead_token: 't', lead_session: 's', owner_login: 'maria',
    participants: [{ login: 'maria', agent: 'codex', token: 't', session: 's', role: 'lead' }], ...over,
  }) as unknown as WorkItem
const overview = (over: Partial<WorkOverview> = {}) => ({ current: [], recent: [], overlaps: [], overlaps_complete: true, ...over }) as unknown as WorkOverview
const input = (over: Partial<LifecycleInput> = {}): LifecycleInput => ({
  item: item(), overview: overview(), requests: [], skillRuns: [], changeRecord: 'none', publications: [], pulls: [], now: NOW, ...over,
})
const step = (i: LifecycleInput, key: string) => lifecycle(i).find((s) => s.key === key)!

describe('lifecycle', () => {
  it('always returns the six steps in order', () => {
    for (const i of [input(), input({ overview: null, requests: null, skillRuns: null, changeRecord: null, publications: null, pulls: null }), input({ requests: 'failed', skillRuns: 'failed', changeRecord: 'failed', publications: 'failed', pulls: 'failed' })])
      expect(lifecycle(i).map((s) => s.label)).toEqual(['Claimed', 'Working', 'Coordination', 'Checks', 'Merge', 'Finished'])
  })

  it('says Checking… while a read is loading', () => {
    const steps = lifecycle(input({ overview: null, requests: null, skillRuns: null, changeRecord: null, publications: null, pulls: null }))
    expect(steps.slice(2, 5).map((s) => [s.word, s.tone, s.proof])).toEqual(Array(3).fill(['Checking…', 'muted', 'unknown']))
  })

  it('says Couldn’t load, never a pass, when a read fails', () => {
    const steps = lifecycle(input({ requests: 'failed', changeRecord: 'failed', publications: 'failed' }))
    expect(steps.slice(2, 5).map((s) => [s.word, s.tone, s.proof])).toEqual(Array(3).fill(['Couldn’t load', 'muted', 'unknown']))
  })

  it('claims with the lead’s name and age, and calls an unchecked-in task quiet', () => {
    const claimed = step(input(), 'claimed')
    expect(claimed.word).toBe(relative(NOW / 1000 - 7200, NOW))
    expect(claimed.detail).toMatch(/^Claimed by maria’s /)
    const working = step(input({ item: item({ claim_status: 'stale' }) }), 'working')
    expect([working.word, working.detail, working.href]).toEqual(['Quiet', 'No progress report yet', '#agent-reports'])
  })

  it('puts an overlap ahead of everything else in Coordination, and says when scope is missing', () => {
    const other = item({ id: `wrk_${'b'.repeat(32)}`, title: 'Refactor auth' })
    const ov = overview({ overlaps: [{ left: other, right: item(), reasons: [], reasons_complete: true }] })
    expect(step(input({ overview: ov }), 'coordination')).toMatchObject({ word: 'Overlaps “Refactor auth”', tone: 'warn', proof: 'observed', href: '#inbox' })
    expect(step(input({ item: item({ scope: [] }) }), 'coordination')).toMatchObject({ word: 'Scope not declared', proof: 'unknown' })
    expect(step(input(), 'coordination')).toMatchObject({ word: 'No overlap', tone: 'ok', proof: 'observed' })
    expect(step(input({ overview: overview({ overlaps_complete: false }) }), 'coordination')).toMatchObject({ word: 'Unknown', proof: 'unknown' })
  })

  it('reads checks from the change record first, with its proof and decision', () => {
    const record = {
      work_id: W,
      latest_action: { kind: 'verification', result: 'passed', assurance: 'caller_asserted' },
      decision: { status: 'proposed', author_kind: 'agent' },
    } as unknown as ChangeRecordSummaryV1
    const s = step(input({ changeRecord: record }), 'checks')
    expect(s).toMatchObject({ word: 'Passed', proof: 'reported', href: `#change-record/${W}`, detail: 'Decision: proposed by an agent, not human approval' })
    expect(s.tone).not.toBe('ok')
    const exec = { ...record, latest_action: { kind: 'execution', result: 'unknown', assurance: 'observed_trusted_local' } } as unknown as ChangeRecordSummaryV1
    expect(step(input({ changeRecord: exec }), 'checks')).toMatchObject({ word: 'Ran, not verified', proof: 'collector', proofTip: expect.stringMatching(/hub did not see it/) })
  })

  it('draws a passed check green only for independent verification, and says how each step gets there', () => {
    const rec = (assurance: string) => ({ work_id: W, latest_action: { kind: 'verification', result: 'passed', assurance }, decision: { status: 'missing', author_kind: 'none' } }) as unknown as ChangeRecordSummaryV1
    expect(['caller_asserted', 'observed_trusted_local', 'protected_execution', 'independent_verification'].map((a) => step(input({ changeRecord: rec(a) }), 'checks').tone)).toEqual(['warn', 'brand', 'brand', 'ok'])
    const how = lifecycle(input()).map((s) => s.how ?? '')
    expect(how.every((h) => h.length > 0)).toBe(true)
    expect(how[0]).toMatch(/agentscm_start/)
    expect(how[4]).toMatch(/GitHub or GitLab/)
  })

  it('falls back to a skill run on the branch, and is Agent says when any gate is self-reported', () => {
    const run = { run_id: 'run_1', branch: 'fix-login', state: 'open', current_gate: 'review', updated_at: 1, gates: [
      { assurance: 'receipt', evidence: [{ assurance: 'receipt' }] },
      { assurance: 'self_reported', evidence: [{ assurance: 'self_reported' }] },
    ] } as unknown as RunView
    expect(step(input({ skillRuns: [run] }), 'checks')).toMatchObject({ word: 'At review', proof: 'reported', href: '#skills/runs/run_1' })
    expect(step(input(), 'checks')).toMatchObject({ word: 'None recorded', proof: 'unknown', detail: 'No check run or change record names fix-login.' })
  })

  it('cannot match checks for a branchless task', () => {
    expect(step(input({ item: item({ branch: null }) }), 'checks').detail).toBe('No branch reported, so checks can’t be matched.')
  })

  it('never shows a finished task as merged without a publication', () => {
    const steps = lifecycle(input({ item: item({ state: 'finished', claim_status: 'stale' }) }))
    const merge = steps.find((s) => s.key === 'merge')!
    expect(merge.tone).not.toBe('ok')
    expect(merge.word).toBe('Not seen')
    // Agent-only evidence is never green, and Working doesn't repeat Finished's word.
    expect(steps.find((s) => s.key === 'finished')).toMatchObject({ word: 'Reported', tone: 'warn', proof: 'reported' })
    expect(steps.find((s) => s.key === 'working')).toMatchObject({ word: 'Ended', tone: 'muted' })
  })

  it('backs Claimed with the hub’s own record, and never draws agent-only evidence green', () => {
    expect(step(input(), 'claimed')).toMatchObject({ tone: 'ok', proof: 'observed' })
    expect(step(input({ item: item({ created_at: 0 }) }), 'claimed')).toMatchObject({ tone: 'warn', proof: 'unknown' })
    expect(step(input({ item: item({ state: 'finished' }) }), 'finished')).toMatchObject({ tone: 'warn', proof: 'reported' })
  })

  it('says Not yet with no badge for an open task, and Not seen with no badge once the hub looked', () => {
    expect(step(input(), 'finished')).toMatchObject({ word: 'Not yet', proof: null })
    expect(step(input(), 'merge')).toMatchObject({ word: 'Not seen', proof: null })
    expect(step(input({ item: item({ branch: null }) }), 'merge')).toMatchObject({ word: 'Not seen', proof: 'unknown' })
  })

  it('reads Merge from a publication, then from an open pull request', () => {
    const pub = { id: `pub_${'c'.repeat(32)}`, branch: 'fix-login', branchState: 'created', pr: { state: 'merged', number: 12 }, external: null } as unknown as Publication
    expect(step(input({ publications: [pub] }), 'merge')).toMatchObject({ word: 'Merged · PR #12', proof: 'observed', href: `#landed/${pub.id}`, detail: 'CI after merge: Not tracked' })
    const pr = { number: 3, state: 'open', head_ref: 'fix-login', html_url: 'javascript:alert(1)' }
    expect(step(input({ pulls: [pr as never] }), 'merge')).toMatchObject({ word: 'PR #3 open', proof: 'observed', href: undefined })
  })
})
