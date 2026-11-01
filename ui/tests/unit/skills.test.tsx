import { act, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { SECTIONS, sectionFromHash } from '../../src/App.tsx'
import { SkillsPage } from '../../src/pages/Skills.tsx'
import {
  addCommand, adoptedSummary, adoptionLabel, stepCounts, assuranceView, decodeRun, enforcement, gateSteps, isSelfReported, removeCommand, runMatches, runStateLabel, skillsRoute, type Gate, type RunView,
} from '../../src/skills.ts'
import { crumbsFor } from '../../src/TopNav.tsx'
import { fromMock, stubHub, type Override } from '../fixtures/mockFetch.ts'

afterEach(() => {
  vi.unstubAllGlobals()
  window.location.hash = ''
})
const gate = (id: string, state: Gate['state'], assurance: string | null): Gate => ({ id, state, by: 'author', enforce: [], min_assurance: 'self_reported', assurance, evidence: [] })

describe('skills routes', () => {
  it.each([
    ['#skills', 'library'],
    ['#skills/runs', 'runs'],
    ['#skills/runs/run_41', 'run'],
    ['#skills/runs/%E0%A4%A', 'runs'],
    ['#skills/runs/bad id', 'runs'],
    ['#skills/runs/a/b', 'runs'],
    ['#skills/other', 'library'],
  ])('%s → %s', (hash, kind) => expect(skillsRoute(hash).kind).toBe(kind))
  it('belongs to the Skills nav item and has crumbs', () => {
    const s = SECTIONS.find((x) => x.key === 'skills')!
    expect(s.nav).toBe('skills')
    expect(sectionFromHash('#skills/runs/run_41')).toBe('skills')
    expect(crumbsFor('#skills/runs/run_41', s, null).map((c) => c.label)).toEqual(['Skills', 'Runs', 'Run run_41'])
    expect(crumbsFor('#skills', s, null)).toHaveLength(1)
  })
})

describe('labels', () => {
  it('names the run state in plain words', () => {
    expect(runStateLabel({ state: 'open', current_gate: 'review' })).toBe('At review')
    expect(runStateLabel({ state: 'open', current_gate: 'ci' })).toBe('Waiting on ci')
    expect(runStateLabel({ state: 'completed', current_gate: null })).toBe('Completed')
  })
  it('states where adoption came from', () => {
    expect(adoptionLabel('default_branch', 'main')).toBe('Adoption: read from main on GitHub or GitLab')
    expect(adoptionLabel('reported_by_checkout', null)).toBe('Adoption: reported by checkout')
    expect(adoptionLabel('none', null)).toBe('Not adopted')
  })
  it('only enforcing values count as enforced', () => {
    const lib = { catalog: [{ name: 'a', sha256: 'x', gates: [{ id: 'tests', enforce: ['stop'] }, { id: 'ci', enforce: ['forge_status'] }, { id: 'merge', enforce: ['advise'] }] }], adoption: { pins: [{ name: 'a', sha256: 'x', status: 'adopted' }, { name: 'b', sha256: 'y', status: 'unknown_version' }] } }
    expect(stepCounts(lib as never)).toEqual({ steps: 3, canStop: 1 })
    expect(enforcement([{ enforce: ['stop'] }, { enforce: ['forge_status', 'advise'] }, { enforce: ['signal'] }])).toEqual({ enforced: ['Stop hook'], planned: ['merge check'] })
    expect(enforcement([{ enforce: ['advise'] }, { enforce: ['signal'] }])).toEqual({ enforced: [], planned: [] })
    // native is planned, not enforced, for skill gates.
    expect(enforcement([{ enforce: ['native'] }])).toEqual({ enforced: [], planned: ['native approval and landing'] })
  })
  it('sums open runs for adopted skills only, from the counts the hub sends', () => {
    const lib = { adoption: { pins: [{ name: 'a', status: 'adopted' }, { name: 'b', status: 'adopted' }, { name: 'c', status: 'unknown_version' }] }, counts: { a: { open: 2, at_stop_gate: 1, completed: 3 }, c: { open: 9, at_stop_gate: 9, completed: 0 } } }
    expect(adoptedSummary(lib as never)).toEqual({ names: ['a', 'b'], open: 2, atStop: 1 })
  })
  it('builds the exact CLI commands', () => {
    const defs = [{ name: 'merge_method', description: '', type: 'string', default: 'any', enum: [] }]
    expect(addCommand('pr-gate')).toBe('agentscm skill add pr-gate')
    expect(addCommand('pr-gate', { merge_method: 'squash', ci_label: 'ci extended' }, defs)).toBe("agentscm skill add pr-gate --param merge_method=squash --param 'ci_label=ci extended'")
    expect(addCommand('pr-gate', { merge_method: 'any' }, defs)).toBe('agentscm skill add pr-gate')
    expect(removeCommand('pr-gate')).toBe('agentscm skill remove pr-gate')
  })
})

describe('self-reported rule', () => {
  it('labels self-reported assurance as a warning with text, never as a receipt', () => {
    expect(isSelfReported('self_reported')).toBe(true)
    expect(isSelfReported('receipt')).toBe(false)
    expect(assuranceView('self_reported')).toMatchObject({ label: 'Self-reported', tone: 'warn' })
    expect(assuranceView('receipt')).toMatchObject({ label: 'Receipt', tone: 'ok' })
    expect(assuranceView('mystery')!.label).toMatch(/^Unrecognised/)
    expect(assuranceView(null)).toBeNull()
  })
  it('puts the label in the track and the search text', () => {
    const steps = gateSteps([gate('branch', 'done', 'observed'), gate('tests', 'done', 'self_reported'), gate('review', 'current', null), gate('fix', 'not_reached', null)])
    expect(steps.map((s) => [s.word, s.tone])).toEqual([['Done', 'ok'], ['Self-reported', 'warn'], ['Current', 'brand'], ['Not reached', 'muted']])
    expect(steps[1]!.detail).toMatch(/did not check/)
  })
  it('matches runs by pr, branch, sha and state', () => {
    const run = { branch: 'feat/x', pull_request: 41, head_sha: 'a1b2c3d9', next_action: 'Needs review', state: 'open', current_gate: 'review' } as RunView
    for (const q of ['#41', 'feat/x', 'a1b2c3d', 'at review', '']) expect(runMatches(run, q)).toBe(true)
    expect(runMatches(run, 'zzz')).toBe(false)
  })
  it('rejects an unknown gate state instead of guessing', () => {
    expect(() => decodeRun({ run_id: 'r', repo: 'a/b', branch: 'x', pull_request: null, head_sha: 'a', state: 'open', skill: { name: 's', workflow_version: 1, sha256: 'z' }, current_gate: null, next_action: '', blocks_finish: false, gates: [{ id: 'g', state: 'weird', by: 'a', enforce: [], min_assurance: 'x', assurance: null, evidence: [] }], updated_at: 1 })).toThrow()
  })
})

describe('Skills page against the mock hub', () => {
  const at = (hash: string) => act(() => { window.location.hash = hash })
  it('library shows adoption source, applicability, commands and attention', async () => {
    const sent = stubHub()
    at('#skills')
    render(<SkillsPage repo="acme/widgets" token="read" />)
    expect(await screen.findByText('Adoption: read from main on GitHub or GitLab')).toBeTruthy()
    for (const t of ['When to use', 'Prerequisites', 'Not for']) expect(screen.getAllByText(t).length).toBeGreaterThan(0)
    expect(screen.getAllByText('How it works').length).toBe(2)
    expect(screen.getAllByText('Full procedure').length).toBe(2)
    expect(document.body.textContent).toContain('same agent token and session')
    // Adopted pr-gate uses the pin's walkthrough (merge method squash); numbers come from the track, not the text.
    expect(screen.getAllByText('The author runs the tests on the exact head commit.').length).toBe(1)
    expect(screen.getAllByText('Can stop the agent').length).toBe(3)
    expect(screen.getAllByRole('heading', { name: 'pr-gate v1' }).length).toBe(1)
    expect(document.body.textContent).not.toContain('name: pr-gate')
    expect(screen.getAllByText(/Needs: the merge \(squash\)/).length).toBe(1)
    expect(screen.queryByText(/^1\. /)).toBeNull()
    expect(screen.getByText('Merge check: planned')).toBeTruthy()
    expect(screen.getByText('Can stop the agent (opt-in)')).toBeTruthy()
    expect(document.body.textContent).not.toMatch(/Enforced/i)
    // The headline answers from the counts the hub already sends; the caveat moves to the fact line.
    const answer = document.querySelector('.answer-text')!.textContent!.replace(/\s+/g, ' ').trim()
    expect(answer).toMatch(/^Agents here are asked to follow 1 skill, pr-gate\. \d+ runs? (is|are) open(; \d+ (is|are) waiting at a stop gate)?\.$/)
    expect(document.body.textContent).toMatch(/\d+ of \d+ steps can stop an agent, where the opt-in Stop hook is installed; agents report their own evidence/)
    expect(screen.queryByText(/Adoption takes effect/)).toBeNull()
    // Available skills stay collapsed to head, summary and command; How it works waits behind a disclosure.
    const avail = screen.getByText('agentscm skill add post-merge-check').closest('li')!
    const how = within(avail).getByText('How it works').closest('details')!
    expect(how.open).toBe(false)
    expect(how.contains(within(avail).getByText('Full procedure'))).toBe(true)
    expect(how.contains(within(avail).getAllByText('When to use')[0]!)).toBe(true)
    // Needs attention leads, without a box around its notice.
    const notice = screen.getByText('legacy-review is pinned but not in force')
    expect(notice.closest('.hub-box, .box')).toBeNull()
    expect(notice.compareDocumentPosition(screen.getByText('Adopted')) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
    expect(screen.getByText('agentscm skill add post-merge-check')).toBeTruthy()
    expect(screen.getByText('agentscm skill remove pr-gate')).toBeTruthy()
    expect(screen.getByText('Unknown version')).toBeTruthy()
    expect(sent.every((s) => s.method === 'GET')).toBe(true)
  })
  it('runs list and detail show Self-reported', async () => {
    stubHub()
    at('#skills/runs')
    const { unmount } = render(<SkillsPage repo="acme/widgets" token="read" />)
    expect(await screen.findByText('At review')).toBeTruthy()
    expect(screen.getByText('Waiting on ci')).toBeTruthy()
    expect(screen.getAllByText('Self-reported').length).toBeGreaterThan(0)
    unmount()
    at('#skills/runs/run_41')
    render(<SkillsPage repo="acme/widgets" token="read" />)
    const table = await screen.findByRole('table')
    await waitFor(() => expect(within(table).getAllByText('Self-reported').length).toBeGreaterThan(0))
    expect(screen.getByText('Yes')).toBeTruthy()
  })
  // The mock hub's overview, with the billing task on run_41's branch.
  const onBranch: Override = (url, init) =>
    url.pathname === '/v1/hub/standup/overview'
      ? fromMock(url, init, (o) => ({ ...o, current: (o['current'] as { title: string }[]).map((t) => (t.title === 'Refactor the billing module' ? { ...t, branch: 'feat/sync-retry-budget' } : t)) }))
      : undefined
  it('a run whose branch matches a task names the task and person first, and sums up its proof', async () => {
    stubHub(onBranch)
    at('#skills/runs')
    const { unmount } = render(<SkillsPage repo="acme/widgets" token="read" />)
    const row = (await screen.findByText('Refactor the billing module · maria’s claude-code')).closest('a')!
    expect(row.textContent!.indexOf('Refactor the billing module')).toBeLessThan(row.textContent!.indexOf('feat/sync-retry-budget'))
    unmount()
    at('#skills/runs/run_41')
    render(<SkillsPage repo="acme/widgets" token="read" />)
    await screen.findByRole('table')
    // run_41 has an observed gate and a self-reported one: the summary is the weaker.
    expect(screen.getByText(/Proof so far/).textContent).toBe('Proof so far: Agent says')
    expect((await screen.findByRole('link', { name: 'Refactor the billing module' })).getAttribute('href')).toMatch(/^#work\/wrk_/)
  })
})
