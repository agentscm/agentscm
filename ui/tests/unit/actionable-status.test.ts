import { resolve } from 'node:path'
import { readFileSync } from 'node:fs'
import { describe, expect, it } from 'vitest'
import { render, screen } from '@testing-library/react'
import { createElement } from 'react'
import type { ActionableStatus } from '../../src/actionableStatus.ts'
import { inboxBadge, needsYou, needHideKey, setupBadge } from '../../src/needs.ts'
import { decodeStatus, recipientLabel, statusLabel, statusProvenance } from '../../src/actionableStatus.ts'
import { DashboardPage, dashboardStatus, SharedAnswer, SharedNeeds } from '../../src/pages/Dashboard.tsx'
import type { WorkOverview } from '../../src/standup.ts'
import type { Derived } from '../../src/derive.ts'
import type { HubData } from '../../src/useHub.ts'

// Load shared evidence only at test runtime; the UI Docker build copies only ui/.
const fixture = JSON.parse(readFileSync(resolve(process.cwd(), '../src/hub/fixtures/actionable-status.json'), 'utf8')) as ActionableStatus

describe('shared actionable status', () => {
  it('uses frozen counts and named recipients without reinterpreting reports', () => {
    const status = decodeStatus(fixture)
    expect(status.counts.needs_you).toBe(3)
    expect(status.work.filter((w) => w.needs_you)).toHaveLength(2)
    expect(status.work[1]!.needs_you).toBe(false)
    expect(needsYou(null, null, undefined, status)).toHaveLength(3)
    expect(needsYou(null, null).filter((n) => n.who === 'you')).toHaveLength(0)
    expect(recipientLabel(status.work[2]!.recipient)).toBe('viewer')
    expect(statusLabel(status.work[2]!.state)).toBe('Awaiting a named party')
    expect(dashboardStatus(null, null, null, null, new Set(), undefined, status).needs).toHaveLength(3)
  })
  it('uses semantic action versions for browser dismissals with legacy fixture compatibility', () => {
    const status = decodeStatus(fixture)
    const action = status.actions.find((a) => a.needs_you)!
    const key = () => needHideKey(needsYou(null, null, undefined, status).find((n) => n.key === action.id)!)
    expect(key()).toBe(`${action.id}@${action.updated_seq}`)
    action.semantic_version = 'first-semantic-version'
    const before = key()
    status.observed_at += 60
    status.digest = 'new-provider-read'
    expect(key()).toBe(before)
    action.semantic_version = 'changed-head-files-or-recipient'
    expect(key()).not.toBe(before)
    const wire = { ...fixture, actions: fixture.actions.map((a) => ({ ...a, semantic_version: 'typed-version' })) }
    expect(decodeStatus(wire).actions[0]!.semantic_version).toBe('typed-version')
    expect(() => decodeStatus({ ...wire, actions: [{ ...wire.actions[0], semantic_version: 12 }] })).toThrow()
  })
  it('retains provider coverage and check time separately from status read time', () => {
    const value = structuredClone(fixture) as unknown as { work: Record<string, unknown>[] }
    value.work[0]!.reconciliation = { current: false, declaration_seq: 4, current_evidence_digest: 'synthetic-evidence', current_source_coverage: [{ source: 'forge_pull', state: 'unavailable', checked_at_seconds: 123, unread_pages: 1, deferred: 2 }], stale_reasons: ['provider_unavailable'], digest: 'synthetic-reconciliation' }
    const status = decodeStatus(value)
    expect(status.work[0]!.reconciliation!.current_source_coverage[0]!.checked_at_seconds).toBe(123)
    expect(statusProvenance(status.work[0]!)).toContain('forge_pull: unavailable; checked')
    expect(statusProvenance(status.work[0]!)).toContain('No current reconciliation synthesis')
  })
  it('rejects malformed flags and inconsistent counts', () => {
    expect(() => decodeStatus({ ...fixture, counts: { ...fixture.counts, needs_you: 4 } })).toThrow()
    expect(() => decodeStatus({ ...fixture, complete: 'yes' })).toThrow()
    expect(() => decodeStatus({ ...fixture, work: Array(101).fill(fixture.work[0]) })).toThrow()
  })
  it('does not claim clear when shared status is missing or partial', () => {
    expect(dashboardStatus(null, null, null, null, new Set(), undefined, null).home?.text).toBe('Shared status unavailable')
    const empty = decodeStatus({ ...fixture, work: [], actions: [], counts: { ...fixture.counts, needs_you: 0 } })
    expect(dashboardStatus(null, null, null, null, new Set(), undefined, empty).home?.text).toContain('partial')
  })
  it('keeps unavailable shared status out of personal badges despite legacy blockers and overlaps', () => {
    const item = (id: string) => ({ id, title: id, state: 'blocked', claim_status: 'active', owner_login: 'other', updated_at: 100, last_checkin_at: 100, scope: [] })
    const ov = { current: [item('blocked')], recent: [], overlaps: [{ left: item('left'), right: item('right'), reasons: [] }] } as unknown as WorkOverview
    expect(needsYou(ov, null, undefined, null)).toHaveLength(2)
    const unavailable = dashboardStatus(ov, null, null, null, new Set(), undefined, null)
    expect(unavailable.home?.text).toBe('Shared status unavailable')
    expect(unavailable.needs).toEqual([])
  })
  it('shows deterministic status and handover recipient without generated prose', () => {
    const overview = { current: [], recent: [], overlaps: [], current_complete: true, recent_complete: true, overlaps_complete: true } as unknown as WorkOverview
    const d = { agents: [], groups: [], contested: [], prOverlaps: [] } as unknown as Derived
    render(createElement(DashboardPage, { section: 'home', question: 'Status', token: 'synthetic', repo: 'acme/widgets', data: { whoami: null, failing: false } as unknown as HubData, d, overview: { kind: 'ready', value: overview, fetchedAt: 0, refreshError: null, retry: () => undefined }, status: { kind: 'ready', value: decodeStatus(fixture), fetchedAt: 0, refreshError: null, retry: () => undefined } }))
    expect(screen.getAllByText('Review handover')).toHaveLength(2)
    expect(screen.getByText('Awaiting a named party · viewer')).toBeTruthy()
    expect(screen.queryByText(/waiting for another agent/i)).toBeNull()
    expect(screen.getByText(/Coverage is incomplete/)).toBeTruthy()
  })
  it('sends failed checks and stopped gates to their pages and counts them in the badge', () => {
    const status = decodeStatus(fixture)
    const base = status.actions[0]!
    status.actions = [
      { ...base, id: 'check:rep_synthetic:5:agentscm/local-validation', kind: 'failed_check', action: 'Required check agentscm/local-validation reports failure on pull request #5; fix or rerun it before merging.' },
      { ...base, id: 'gate:run_synthetic:tests', kind: 'skill_gate', action: 'pr-gate is at its tests gate on branch feature/g and the agent that recorded it has stopped.' },
    ]
    status.counts.needs_you = status.work.filter((w) => w.needs_you).length + 2
    expect(dashboardStatus(null, null, null, null, new Set(), undefined, status).needs).toHaveLength(status.counts.needs_you)
    expect(inboxBadge(null, undefined, status).n).toBe(0)
    render(createElement(SharedNeeds, { status: { kind: 'ready', value: status, fetchedAt: 0, refreshError: null, retry: () => undefined } }))
    expect(screen.getByRole('link', { name: 'Open changes' }).getAttribute('href')).toBe('#landed')
    expect(screen.getByRole('link', { name: 'Open Skills' }).getAttribute('href')).toBe('#skills')
  })
  it('badges the viewer’s own agent setup problems and lists only those with Work in Needs you', () => {
    const status = decodeStatus(fixture)
    const base = status.actions[0]!
    const viewer = { kind: 'person' as const, owner: status.viewer_owner, login: 'viewer', agent: null }
    const other = { ...viewer, owner: 'usr_synthetic_other', login: 'other' }
    status.actions = [
      { ...base, id: 'setup:agent_trust_required:aaaa', kind: 'agent_setup', recipients: [viewer], needs_you: true, work_ids: ['wrk_11111111111111111111111111111111'], action: 'Your agent laptop needs its AgentSCM hooks and MCP server approved.' },
      { ...base, id: 'setup:edit_hook_unverified:bbbb', kind: 'agent_setup', recipients: [viewer], needs_you: false, work_ids: [], action: 'Your agent laptop reached the Hub over MCP, but no edit hook has.' },
      { ...base, id: 'setup:agent_token_expired:repository', kind: 'agent_setup', recipients: [other], needs_you: false, work_ids: [], action: 'Their token expired.' },
    ]
    status.counts.needs_you = status.work.filter((w) => w.needs_you).length + 1
    expect(setupBadge(status)).toEqual({ n: 2, tip: '2 of your agents have setup problems' })
    expect(setupBadge(null).n).toBe(0)
    expect(dashboardStatus(null, null, null, null, new Set(), undefined, status).needs.filter((n) => n.key.startsWith('setup:'))).toHaveLength(1)
    expect(inboxBadge(null, undefined, status).n).toBe(0)
    render(createElement(SharedNeeds, { status: { kind: 'ready', value: status, fetchedAt: 0, refreshError: null, retry: () => undefined } }))
    expect(screen.getByRole('link', { name: 'Open Agents' }).getAttribute('href')).toBe('#setup')
  })
  it('labels provider failure and retained refresh results separately from clear status', () => {
    const status = decodeStatus(fixture)
    status.work[0]!.limits = ['provider_coverage_unavailable']
    const poll = { kind: 'ready' as const, value: status, fetchedAt: 0, refreshError: 'Synthetic refresh failure', retry: () => undefined }
    render(createElement('div', null, createElement(SharedAnswer, { question: 'Status', status: poll }), createElement(SharedNeeds, { status: poll })))
    expect(document.querySelector('.answer-text')!.textContent).toContain('Last known:')
    expect(screen.getByText('Provider evidence is unavailable')).toBeTruthy()
    expect(screen.queryByText('All clear')).toBeNull()
  })

})
