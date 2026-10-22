import { describe, expect, it } from 'vitest'
import { SECTIONS, sectionFromHash } from '../../src/App.tsx'
import { crumbsFor } from '../../src/TopNav.tsx'
import { workTabFromHash, workTabHref } from '../../src/pages/Work.tsx'
import { workspaceRoute, workspaceRunHref } from '../../src/pages/Workspace.tsx'
import type { WorkOverview } from '../../../../ui/src/standup.ts'

const ID = `wrk_${'a'.repeat(32)}`
const section = (key: string) => SECTIONS.find((s) => s.key === key)!

describe('sectionFromHash', () => {
  it.each([
    ['', 'now'],
    ['#', 'now'],
    ['#now', 'now'],
    ['#work', 'work'],
    [`#work/${ID}`, 'work'],
    ['#work/coordination', 'work'],
    ['#activity', 'activity'],
    ['#workflows/runs/run_1/steps/s1', 'workflows'],
    ['#settings', 'settings'],
    ['#health', 'health'],
    // The current status page's links keep working.
    ['#coordination', 'now'],
    [`#coordination/work/${ID}`, 'work'],
    // Unknown or hostile hashes fall back to the first page.
    ['#nope', 'now'],
    ['#__proto__', 'now'],
    ['#token=abc', 'now'],
  ])('%s → %s', (hash, want) => {
    expect(sectionFromHash(hash)).toBe(want)
  })
})

describe('work tabs in the hash', () => {
  it('parses known tabs and defaults to working', () => {
    expect(workTabFromHash('#work')).toBe('working')
    expect(workTabFromHash('#work/finished')).toBe('finished')
    expect(workTabFromHash('#work/coordination')).toBe('coordination')
    expect(workTabFromHash('#work/history')).toBe('history')
    expect(workTabFromHash(`#work/${ID}`)).toBe('working')
    expect(workTabFromHash('#work/bogus')).toBe('working')
  })
  it('round-trips through workTabHref', () => {
    for (const t of ['working', 'finished', 'coordination', 'history'] as const) expect(workTabFromHash(workTabHref(t))).toBe(t)
  })
})

describe('crumbsFor', () => {
  const overview = {
    current: [{ id: ID, title: 'Rebuild the hub shell' }],
    recent: [{ item: { id: `wrk_${'b'.repeat(32)}`, title: 'Finished thing' } }],
  } as unknown as WorkOverview

  it('work tab crumbs link to the tab', () => {
    expect(crumbsFor('#work/coordination', section('work'), null)).toEqual([
      { label: 'Work', href: '#work', icon: 'kanban' },
      { label: 'Needs coordination', href: '#work/coordination' },
    ])
  })
  it('work detail shows the item title from the overview', () => {
    expect(crumbsFor(`#work/${ID}`, section('work'), overview).at(-1)).toEqual({ label: 'Rebuild the hub shell' })
    expect(crumbsFor(`#work/wrk_${'b'.repeat(32)}`, section('work'), overview).at(-1)).toEqual({ label: 'Finished thing' })
  })
  it('work detail falls back to a short id when the title is not loaded', () => {
    expect(crumbsFor(`#work/${ID}`, section('work'), null).at(-1)!.label).toBe(`Work item ${ID.slice(0, 12)}…`)
  })
  it('workflow run and step crumbs', () => {
    const c = crumbsFor('#workflows/runs/run_0123456789abcdef/steps/lint', section('workflows'), null)
    expect(c.map((x) => x.label)).toEqual(['Workflows', 'Run run_0123456789…', 'Step lint'])
    expect(c[1]!.href).toBe('#workflows/runs/run_0123456789abcdef')
  })
  it('other sections are a single crumb', () => {
    expect(crumbsFor('#health', section('health'), null)).toEqual([{ label: 'Hub health', href: '#health', icon: 'heart-pulse' }])
  })
})

describe('workspace routes and crumbs', () => {
  const R = encodeURIComponent('acme/widgets')
  it.each([
    ['#workspace', { tab: 'repositories' }],
    ['#workspace/members', { tab: 'members' }],
    ['#workspace/settings', { tab: 'settings' }],
    ['#workspace/bogus', { tab: 'repositories' }],
    [`#workspace/workflow/${R}/run-1`, { tab: 'workflow', repositoryId: 'acme/widgets', runId: 'run-1' }],
    [`#workspace/workflow/${R}/run-1/steps/lint%2Fdocs`, { tab: 'workflow', repositoryId: 'acme/widgets', runId: 'run-1', stepId: 'lint/docs' }],
    [`#workspace/workflow/${R}/run-1/steps/%E0%A4%A`, { tab: 'workflow', repositoryId: 'acme/widgets', runId: 'run-1' }],
    ['#workspace/workflow/%E0%A4%A/run-1', { tab: 'workflow', invalid: true }],
    ['#workspace/workflow', { tab: 'workflow', invalid: true }],
  ])('%s', (hash, want) => {
    expect(workspaceRoute(hash)).toEqual(want)
  })
  it('crumbs name the tab, run and step, each linked', () => {
    const ws = section('workspace')
    expect(crumbsFor('#workspace/members', ws, null).at(-1)).toEqual({ label: 'Members', href: '#workspace/members' })
    expect(crumbsFor(`#workspace/workflow/${R}/synthetic-workspace-run/steps/lint`, ws, null)).toEqual([
      { label: 'Workspace', href: '#workspace', icon: 'people' },
      { label: 'Repositories & runs', href: '#workspace' },
      { label: 'Run synthetic-work…', href: workspaceRunHref('acme/widgets', 'synthetic-workspace-run'), mono: true },
      { label: 'Step lint', mono: true },
    ])
    expect(crumbsFor('#workspace/workflow/%E0%A4%A/x', ws, null).map((c) => c.label)).toEqual(['Workspace', 'Repositories & runs'])
  })
})
