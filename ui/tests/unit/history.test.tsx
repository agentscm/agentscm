// History › Finished and Changes against the synthetic hub (dev/mock.ts): row order, the headline, empty states and the finished rows.
import { render, screen, within } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { newestFirst, WorkPage } from '../../src/pages/Work.tsx'
import { fetchOverview, type BriefingEvent, type WorkOverview } from '../../src/standup.ts'
import type { Poll } from '../../src/usePoll.ts'
import { fromMock, stubHub, type Override } from '../fixtures/mockFetch.ts'

afterEach(() => {
  vi.unstubAllGlobals()
  window.location.hash = ''
})
const LOAD = { timeout: 5_000 }
const now = () => Math.floor(Date.now() / 1000)

async function open(hash: string, ...overrides: Override[]) {
  window.location.hash = hash
  stubHub(...overrides)
  const value = await fetchOverview('t', new AbortController().signal)
  const overview = { kind: 'ready', value, fetchedAt: Date.now(), refreshError: null, retry: () => {} } as Poll<WorkOverview>
  return render(<WorkPage token="t" overview={overview} />)
}

/** Rewrites the mock briefing's events. */
const briefing = (edit: (events: Record<string, unknown>[]) => Record<string, unknown>[]): Override => (url, init) =>
  url.pathname === '/v1/hub/standup/briefing' ? fromMock(url, init, (b) => ({ ...b, events: edit(b['events'] as Record<string, unknown>[]) })) : undefined

describe('newestFirst', () => {
  it('orders a page by time, not by sequence, and breaks ties by sequence', () => {
    const e = (seq: number, at: number) => ({ seq, at }) as BriefingEvent
    expect(newestFirst([e(10, 100), e(11, 300), e(12, 200), e(13, 300)]).map((x) => x.seq)).toEqual([13, 11, 12, 10])
  })
})

describe('History › Changes', () => {
  it('lists the page newest first and answers with the count and the latest time', async () => {
    // The Hub sends a page in sequence order, oldest first; the newest here is a minute old.
    await open('#work/history', briefing((events) => events.map((ev, i) => ({ ...ev, at: now() - 60 - (events.length - 1 - i) * 600 }))))
    await screen.findByText('Recent changes', {}, LOAD)
    const rows = [...document.querySelectorAll('.change-row')]
    expect(rows.length).toBeGreaterThan(1)
    const times = rows.map((r) => Date.parse(r.querySelector('.change-when time')!.getAttribute('datetime')!))
    expect(times).toEqual([...times].sort((a, b) => b - a))
    const answer = document.querySelector('.answer-text')!.textContent
    expect(answer).toBe(`${rows.length} changes on this page, the latest 1 min. ago.`)
    expect(document.querySelector('.answer-fact')!.textContent).not.toContain('Reading changes nothing')
  })

  it('names the agent as the person’s agent and shows the view switch as a lens', async () => {
    await open('#work/history')
    await screen.findByRole('tab', { name: /Recent changes/ }, LOAD)
    expect(screen.getByRole('tab', { name: /Recent changes/ }).getAttribute('aria-selected')).toBe('true')
    const who = [...document.querySelectorAll('.change-who > span[title]')].map((w) => w.textContent!)
    expect(who.length).toBeGreaterThan(0)
    for (const w of who) expect(w).toMatch(/^\S+’s \S+/)
  })

  it('says what makes a change appear when the page is empty, without tool names', async () => {
    await open('#work/history', briefing(() => []))
    expect(await screen.findByText('No changes on this page', {}, LOAD)).toBeTruthy()
    expect(document.querySelector('.answer-text')!.textContent).toBe('No changes on this page.')
    const text = document.querySelector('.empty-text')!.textContent!
    expect(text).toBe('Changes appear here when an agent starts, joins or checks in on a task.')
    expect(document.body.textContent).not.toMatch(/agentscm_/)
  })
})

describe('History › Finished', () => {
  const twoDone: Override = (url, init) =>
    url.pathname === '/v1/hub/standup/overview'
      ? fromMock(url, init, (ov) => {
          // The mock's finished task has no checkpoint; lend it one so the report line shows.
          const report = ((ov['current'] as Record<string, unknown>[])[0]!['current'] as Record<string, unknown>)
          const recent = (ov['recent'] as Record<string, unknown>[]).map((o) => (o['kind'] === 'completion' ? { ...o, item: { ...(o['item'] as Record<string, unknown>), current: report } } : o))
          const done = recent.find((o) => o['kind'] === 'completion')!
          const older = { ...done, seq: 3, at: now() - 7200, item: { ...(done['item'] as Record<string, unknown>), id: `wrk_${'9'.repeat(32)}`, title: 'Older finished task' } }
          return { ...ov, recent: [...recent, older] }
        })
      : undefined

  it('shows one row per finished task, newest first, with who, time, the last report and a muted Agent says', async () => {
    await open('#work', twoDone)
    const list = await screen.findByRole('list', { name: 'Reported finished' }, LOAD)
    const rows = within(list).getAllByRole('listitem')
    expect(rows).toHaveLength(2)
    expect(rows[1]!.querySelector('a')!.textContent).toBe('Older finished task')
    for (const row of rows) {
      expect(row.querySelector('.rp-done-at time')).toBeTruthy()
      expect(row.querySelector('.rp-done-who')!.textContent).toMatch(/’s .*Agent says/)
      expect(row.querySelector('.pill.tone-muted')!.textContent).toBe('Agent says')
      expect(row.querySelector('.pill.tone-warn')).toBeNull()
    }
    expect(rows[0]!.querySelector('.rp-done-say')!.textContent).toBe('Tax module extracted; tests green')
    expect(document.querySelector('.finished-grid')).toBeNull()
  })

  it('says what makes a finished task appear when there is none, without tool names', async () => {
    await open('#work', (url, init) => (url.pathname === '/v1/hub/standup/overview' ? fromMock(url, init, (ov) => ({ ...ov, recent: [] })) : undefined))
    expect(await screen.findByText('Nothing reported finished in this window', {}, LOAD)).toBeTruthy()
    expect(document.querySelector('.empty-text')!.textContent).toBe('When an agent reports a task finished, it shows here.')
    expect(document.body.textContent).not.toMatch(/agentscm_/)
  })
})
