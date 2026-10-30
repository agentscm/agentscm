import { render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { InboxPage } from '../../src/pages/Inbox.tsx'
import type { WorkItem, WorkOverview } from '../../src/standup.ts'
import type { Poll } from '../../src/usePoll.ts'

// The mock hub's billing overlap (dev/mock.ts `overview()`), with no directed requests.
const now = Math.floor(Date.now() / 1000)
const item = (c: string, title: string, login: string, agent: string, over: Partial<WorkItem> = {}): WorkItem =>
  ({
    id: `wrk_${c.repeat(32)}`, title, state: 'active', claim_status: 'active', updated_at: now - 60, last_checkin_at: now - 60, blocker: null, scope: ['src/billing.rs'], next_action: null, current: null, branch: null,
    lead_agent: agent, lead_token: `tok_${agent}`, lead_session: 's', owner_login: login, participants: [{ login, agent, token: `tok_${agent}`, session: 's', role: 'lead', owner: `github:github.com:${login}` }], ...over,
  }) as unknown as WorkItem
const billing = item('a', 'Refactor the billing module', 'maria', 'claude-code')
const rounding = item('b', 'Fix invoice rounding', 'dev', 'codex')
const stale = item('c', 'Write the webhook retry documentation', 'maria', 'codex', { claim_status: 'stale' })
const overview = {
  current: [billing, rounding, stale],
  recent: [{ seq: 8, at: now - 2400, kind: 'decision', item: billing }],
  overlaps: [{ left: billing, right: rounding, reasons: [{ left_path: 'src/billing.rs', right_path: 'src/billing.rs' }], reasons_complete: true }],
  current_complete: true, recent_complete: true, overlaps_complete: true,
} as unknown as WorkOverview
const ready = { kind: 'ready', value: overview } as Poll<WorkOverview>

beforeEach(() => {
  const none = { from: 0, through: 40, next_after: 0, requests: [], complete: true, retention_gap: false }
  vi.stubGlobal('fetch', vi.fn().mockImplementation(async () => new Response(JSON.stringify(none), { status: 200 })))
})
afterEach(() => vi.unstubAllGlobals())

describe('Inbox', () => {
  it('names both leads on an overlap and offers a copy button for each agent', async () => {
    render(<InboxPage token="t" overview={ready} />)
    const overlaps = within(document.getElementById('box-overlaps')!)
    expect(overlaps.getByText('Talk to maria and dev: their agents both plan to change src/billing.rs. Nothing is blocked yet.')).toBeTruthy()
    expect(overlaps.getByRole('button', { name: 'Copy prompt for maria’s claude-code' })).toBeTruthy()
    expect(overlaps.getByRole('button', { name: 'Copy prompt for dev’s codex' })).toBeTruthy()
    expect(overlaps.getByText('maria’s claude-code')).toBeTruthy()
    expect(overlaps.getAllByText('Plans to change:').length).toBe(2)
    expect(document.getElementById('box-overlaps')!.textContent).toContain('Two tasks plan the same files')
    expect(document.getElementById('box-requests')!.textContent).toContain('Agents ask each other to split, join or hand over. They answer themselves; each item says what to tell them if it stalls.')
  })

  it('puts overlaps first when no request is open, and keeps the request box as one line', async () => {
    render(<InboxPage token="t" overview={ready} />)
    await screen.findByText('No directed requests')
    const requests = document.getElementById('box-requests')!
    const overlapsBox = document.getElementById('box-overlaps')!
    expect(overlapsBox.compareDocumentPosition(requests) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
    expect(requests.textContent).not.toMatch(/Refreshes every 15 seconds/)
  })

  it('collapses the secondary boxes with their counts in the summary', () => {
    render(<InboxPage token="t" overview={ready} />)
    for (const label of ['Quiet agents and tasks with no stated files · 1', 'Recent decisions · 1']) {
      const details = screen.getByText(label).closest('details')!
      expect(details.open).toBe(false)
    }
  })

  it('offers Open in only for the viewer’s own agent on an overlap', () => {
    const mine = { me: 'maria', repo: 'acme/widgets', originUrl: 'https://github.com/acme/widgets' }
    const { unmount } = render(<InboxPage token="t" overview={ready} handoff={mine} />)
    expect(screen.getByRole('link', { name: /Open in Claude Code/ }).getAttribute('href')).toMatch(/^claude-cli:\/\/open\?repo=acme\/widgets&q=/)
    expect(screen.queryByRole('link', { name: /Open in Codex/ })).toBeNull()
    unmount()
    render(<InboxPage token="t" overview={ready} handoff={{ ...mine, me: 'someone-else' }} />)
    expect(screen.queryByRole('link', { name: /Open in/ })).toBeNull()
  })
})
