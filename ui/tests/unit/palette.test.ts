import { describe, expect, it } from 'vitest'
import { SECTIONS } from '../../src/App.tsx'
import { filterHits, paletteHits } from '../../src/TopNav.tsx'
import type { Derived } from '../../src/derive.ts'
import type { WorkOverview } from '../../src/standup.ts'

const W1 = `wrk_${'1'.repeat(32)}`
const W2 = `wrk_${'2'.repeat(32)}`
const d = {
  agents: [
    { name: 'claude-ivy', owner: 'maya', task: 'Rebuild shell', holdings: [{ spec: 'ui/src/App.tsx' }, { spec: 'src/hub/mcp.rs' }] },
    { name: 'codex-orion', owner: 'dev', task: null, holdings: [{ spec: 'src/hub/mcp.rs' }] },
  ],
} as unknown as Derived
const overview = {
  current: [{ id: W1, title: 'Bound coordination history', lead_agent: 'codex-orion', state: 'blocked' }],
  recent: [{ kind: 'completion', item: { id: W2, title: 'Seed the dev hub', lead_agent: 'claude-juno' } }],
} as unknown as WorkOverview

describe('paletteHits', () => {
  const hits = paletteHits(SECTIONS, d, overview)
  it('lists every page first', () => {
    expect(hits.slice(0, SECTIONS.length).map((h) => h.href)).toEqual(SECTIONS.map((s) => `#${s.key}`))
  })
  it('de-duplicates held files across agents', () => {
    expect(hits.filter((h) => h.group === 'Held files').map((h) => h.label)).toEqual(['ui/src/App.tsx', 'src/hub/mcp.rs'])
  })
  it('agents without a task show only the owner', () => {
    expect(hits.find((h) => h.label === 'codex-orion' && h.group === 'Agents')!.hint).toBe('dev')
  })
  it('work and recent outcomes link to the work detail route', () => {
    expect(hits.find((h) => h.group === 'Work')!.href).toBe(`#work/${W1}`)
    expect(hits.find((h) => h.group === 'Recently finished')).toMatchObject({ href: `#work/${W2}`, hint: 'claude-juno · reported completion' })
  })
  it('works before data loads', () => {
    expect(paletteHits(SECTIONS, null, null)).toHaveLength(SECTIONS.length + 1) // every page, plus History › Changes
  })
})

describe('filterHits', () => {
  const hits = paletteHits(SECTIONS, d, overview)
  it('empty or blank query shows only pages', () => {
    expect(filterHits(hits, '').every((h) => h.group === 'Pages')).toBe(true)
    expect(filterHits(hits, '   ')).toHaveLength(SECTIONS.length + 1)
  })
  it('matches label and hint case-insensitively', () => {
    expect(filterHits(hits, 'COORD').map((h) => h.label)).toContain('Bound coordination history')
    expect(filterHits(hits, 'maya').map((h) => h.label)).toEqual(['claude-ivy'])
  })
  it('returns nothing for no match', () => {
    expect(filterHits(hits, 'zzzz-no-such-thing')).toEqual([])
  })
  it('caps results at 40', () => {
    const many = Array.from({ length: 60 }, (_, i) => ({ group: 'Agents', icon: 'robot', label: `agent-${i}`, hint: 'x', href: '#now' }))
    expect(filterHits(many, 'agent')).toHaveLength(40)
  })
})
