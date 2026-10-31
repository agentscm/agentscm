// The task page against the synthetic hub (dev/mock.ts): the lifecycle strip, the Change record button and agent reports.
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { WorkDetail } from '../../src/pages/WorkDetail.tsx'
import { fetchOverview, type WorkOverview } from '../../src/standup.ts'
import type { Poll } from '../../src/usePoll.ts'
import { json, stubHub, type Override } from '../fixtures/mockFetch.ts'

afterEach(() => vi.unstubAllGlobals())
const LOAD = { timeout: 5_000 }
const wid = (c: string) => `wrk_${c.repeat(32)}`

async function open(c: string, ...overrides: Override[]) {
  stubHub(...overrides)
  const value = await fetchOverview('t', new AbortController().signal)
  const overview = { kind: 'ready', value, fetchedAt: Date.now(), refreshError: null, retry: () => {} } as Poll<WorkOverview>
  render(<WorkDetail id={wid(c)} token="t" overview={overview} />)
  const strip = await screen.findByRole('list', { name: 'Where this task is' }, LOAD)
  /** One step by its label; waits until its read has settled. */
  const step = async (label: string) => {
    let li!: HTMLElement
    await waitFor(() => {
      li = within(strip).getAllByRole('listitem').find((x) => x.querySelector('.track-label')?.textContent === label)!
      expect(li.textContent).not.toContain('Checking…')
    }, LOAD)
    return { li, word: li.querySelector('.track-word')!.textContent, href: li.querySelector('a')?.getAttribute('href') ?? null, text: li.textContent }
  }
  return { strip, step }
}

describe('WorkDetail', () => {
  it('mounts the explicit PR relationship and reloads the detail when its Work sequence changed', async () => {
    let detailReads = 0
    const id = wid('b')
    const { strip } = await open('b', (url) => {
      if (url.pathname === `/v1/hub/standup/work/${id}`) detailReads++
      if (url.pathname === `/v1/hub/standup/work/${id}/pr-link`) return json(200, {
        work_id: id, repository: 'acme/widgets', repository_id: 100, generation: 1,
        updated_seq: Number(url.searchParams.get('expected_updated_seq')) + 1,
        snapshot_status: 'work_changed', head_status: 'not_compared', link: null,
      })
      return undefined
    })
    expect(within(strip).getAllByRole('listitem')).toHaveLength(6)
    expect(await screen.findByRole('heading', { name: 'Linked pull request' }, LOAD)).toBeTruthy()
    fireEvent.click(await screen.findByRole('button', { name: 'Reload Work detail' }, LOAD))
    await waitFor(() => expect(detailReads).toBe(2), LOAD)
  })

  it('draws six steps with evidence links for an active task', async () => {
    const { strip, step } = await open('a')
    expect(within(strip).getAllByRole('listitem')).toHaveLength(6)
    const checks = await step('Checks')
    expect(checks.word).toBe('At review')
    expect(checks.text).toContain('Agent says')
    expect(checks.href).toBe('#skills/runs/run_41')
    const coordination = await step('Coordination')
    expect(coordination.word).toBe('Overlaps “Fix invoice rounding”')
    expect(coordination.href).toBe('#inbox')
    expect((await step('Merge')).word).toBe('Not seen')
    // The mock has no change record (404), so there is no button.
    expect(screen.queryByRole('link', { name: /Change record/ })).toBeNull()
    expect(screen.getByText(/Led by maria’s claude-code/).textContent).toBe('Led by maria’s claude-code.')
    // The person comes first; the branch, pull request and head follow in the one fact line, once the reads that name them settle.
    await waitFor(() => expect(document.querySelector('.answer-fact')!.textContent).toMatch(/^On feat\/sync-retry-budget.*head [0-9a-f]{7}\. Last check-in /), LOAD)
    expect(screen.getByText('Technical details').closest('details')!.textContent).toContain('feat/sync-retry-budget')
    // The overlap the Dashboard lists for this task, with a copy button per lead's agent.
    expect(screen.getByRole('heading', { name: 'Needs you' })).toBeTruthy()
    expect(screen.getByText('Talk to maria and dev: their agents both plan to change src/billing.rs. Nothing is blocked yet.')).toBeTruthy()
    expect(screen.getByRole('button', { name: /Copy prompt for maria’s claude-code/ })).toBeTruthy()
    expect(screen.getByRole('button', { name: /Copy prompt for dev’s codex/ })).toBeTruthy()
    // People: one role label, the person first. Reports: person and age, kind only for a blocker, no repeated Next.
    expect(screen.getByText('Files this task plans to change')).toBeTruthy()
    expect(screen.queryByText('current lead')).toBeNull()
    const people = screen.getByRole('heading', { name: /People/ }).closest('section')!
    expect(people.textContent).toContain('maria’s claude-code')
    expect(people.querySelector('li')!.textContent).toBe('CCmaria’s claude-code ·lead')
    const report = document.querySelector('.report')!
    expect(report.textContent).toContain('Latest report')
    expect(report.textContent).not.toMatch(/checkpoint|progress|Next/)
    expect(report.textContent).toMatch(/Reported by maria’s claude-code · /)
    // Claimed is the hub's own record; Finished and Merge have nothing to prove on an open task and draw no badge.
    expect((await step('Claimed')).text).toContain('Hub saw it')
    const finished = await step('Finished')
    expect([finished.word, finished.text]).toEqual(['Not yet', 'FinishedNot yet'])
    expect((await step('Merge')).text).not.toContain('Not known')
    const badges = (label: string) => within(strip).getAllByRole('listitem').find((x) => x.querySelector('.track-label')?.textContent === label)!.querySelectorAll('.track-proof > *').length
    expect(['Claimed', 'Working', 'Coordination', 'Checks'].map(badges)).toEqual([1, 1, 1, 1])
    expect(['Merge', 'Finished'].map(badges)).toEqual([0, 0])
  })

  it('links a merged task to its published change and labels Finished as agent-reported', async () => {
    const { step } = await open('f')
    const merge = await step('Merge')
    expect(merge.word).toBe('Merged · PR #220')
    expect(merge.href).toMatch(/^#landed\/pub_[0-9a-f]{32}$/)
    const finished = await step('Finished')
    expect(finished.word).toBe('Reported')
    expect(finished.text).toContain('Agent says')
  })

  it('shows the open blocker and no empty-checkpoint message for a blocked task', async () => {
    await open('c')
    expect(await screen.findByText('Open blocker', {}, LOAD)).toBeTruthy()
    expect(screen.queryByText('No checkpoint yet')).toBeNull()
    expect(screen.getByText('Answer the blocker where maria’s codex is running, or reassign the task.')).toBeTruthy()
  })

  it('shows the Change record button when a record exists, and a record with no action leaves Checks to the skill run', async () => {
    const cov = 'unknown'
    const record = {
      version: 1, availability: 'available',
      summary: {
        schema_version: 1, work_id: wid('a'), generation: 1, as_of: 1_798_000_000, intent: { title: 'Refactor the billing module', scope: [], scope_omitted: 0 }, subject: null,
        sessions: { count: 0, coverage: cov, gaps: 0 }, latest_action: null,
        decision: { status: 'missing', reference: null, summary: null, source_refs: [], source_refs_omitted: 0, author_kind: null, author_agent: null, at: null, generation: null },
        next_action: null, completeness: { events: cov, source: cov, setup: cov, recipe: cov, result: cov, usage: cov }, unknowns: [],
      },
    }
    const { step } = await open('a', (url) => (url.pathname.includes('/change-records/') ? json(200, record) : undefined))
    expect((await screen.findByRole('link', { name: /Change record/ }, LOAD)).getAttribute('href')).toBe(`#change-record/${wid('a')}`)
    expect((await step('Checks')).word).toBe('At review')
  })

  it('keeps the work ID inside Technical details and names the source in plain words', async () => {
    const { step } = await open('a')
    await step('Merge')
    const tech = screen.getByText('Technical details').closest('details')!
    const outside = document.body.textContent!.replace(tech.textContent!, '')
    expect(outside).not.toContain('wrk_')
    expect(screen.getByText('No issue linked')).toBeTruthy()
    expect(tech.textContent).toContain('Plain Git work request req-a')
  })

  it('offers Open in only for the viewer’s own agent in Needs you', async () => {
    stubHub()
    const value = await fetchOverview('t', new AbortController().signal)
    const overview = { kind: 'ready', value, fetchedAt: Date.now(), refreshError: null, retry: () => {} } as Poll<WorkOverview>
    const mine = { me: 'maria', repo: 'acme/widgets', originUrl: 'https://github.com/acme/widgets' }
    const { unmount } = render(<WorkDetail id={wid('a')} token="t" overview={overview} handoff={mine} />)
    expect((await screen.findByRole('link', { name: /Open in Claude Code/ }, LOAD)).getAttribute('href')).toMatch(/^claude-cli:\/\/open\?repo=acme\/widgets&q=/)
    expect(screen.queryByRole('link', { name: /Open in Codex/ })).toBeNull()
    unmount()
    render(<WorkDetail id={wid('a')} token="t" overview={overview} handoff={{ ...mine, me: 'someone-else' }} />)
    await screen.findByRole('button', { name: /Copy prompt for maria’s claude-code/ }, LOAD)
    expect(screen.queryByRole('link', { name: /Open in/ })).toBeNull()
  })
})
