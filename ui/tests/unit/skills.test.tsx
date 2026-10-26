import { act, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { SECTIONS, sectionFromHash } from '../../src/App.tsx'
import { SkillsPage } from '../../src/pages/Skills.tsx'
import {
  addCommand, adoptionLabel, assuranceView, decodeRun, enforcement, gateSteps, isSelfReported, removeCommand, runMatches, runStateLabel, skillsRoute, type Gate, type RunView,
} from '../../src/skills.ts'
import { crumbsFor } from '../../src/TopNav.tsx'
import { stubHub } from '../fixtures/mockFetch.ts'

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
    expect(adoptionLabel('default_branch', 'main')).toBe('Adoption: read from main on the forge')
    expect(adoptionLabel('reported_by_checkout', null)).toBe('Adoption: reported by checkout')
    expect(adoptionLabel('none', null)).toBe('Not adopted')
  })
  it('only enforcing values count as enforced', () => {
    expect(enforcement([{ enforce: ['stop'] }, { enforce: ['forge_status', 'advise'] }, { enforce: ['signal'] }])).toEqual({ enforced: ['Stop hook'], planned: ['merge check'] })
    expect(enforcement([{ enforce: ['advise'] }, { enforce: ['signal'] }])).toEqual({ enforced: [], planned: [] })
    // native is planned, not enforced, for skill gates.
    expect(enforcement([{ enforce: ['native'] }])).toEqual({ enforced: [], planned: ['native approval and landing'] })
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
    expect(await screen.findByText('Adoption: read from main on the forge')).toBeTruthy()
    for (const t of ['When to use', 'Prerequisites', 'Not for']) expect(screen.getAllByText(t).length).toBeGreaterThan(0)
    expect(screen.getAllByText('How it works').length).toBe(2)
    expect(screen.getAllByText('Full procedure').length).toBe(2)
    expect(document.body.textContent).toContain('same agent token and session')
    // Adopted pr-gate uses the pin's walkthrough (merge method squash); numbers come from the track, not the text.
    expect(screen.getAllByText('The author runs the tests on the exact head commit.').length).toBe(1)
    expect(screen.getAllByText('Blocks finish').length).toBe(3)
    expect(screen.getAllByRole('heading', { name: 'pr-gate v1' }).length).toBe(1)
    expect(document.body.textContent).not.toContain('name: pr-gate')
    expect(screen.getAllByText(/Needs: the merge \(squash\)/).length).toBe(1)
    expect(screen.queryByText(/^1\. /)).toBeNull()
    expect(screen.getByText(/Planned: merge check/)).toBeTruthy()
    expect(screen.getByText(/Enforced: Stop hook/)).toBeTruthy()
    expect(document.body.textContent).not.toMatch(/Enforced:[^.]*forge status/i)
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
})
