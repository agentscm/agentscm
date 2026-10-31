import { fireEvent, render, screen, within } from '@testing-library/react'
import { describe, expect, it, vi, afterEach } from 'vitest'
import { contextFixture } from '../../dev/prContext.ts'
import { DashboardPage } from '../../src/pages/Dashboard.tsx'
import type { HubData } from '../../src/useHub.ts'
import type { Work } from '../../src/types.ts'
import { ContextFacts, PrContextPanel } from '../../src/PrContextPanel.tsx'
import { decodePrContext, fetchPrContext } from '../../src/prContext.ts'
import { clearPollCache } from '../../src/usePoll.ts'

afterEach(() => { vi.restoreAllMocks(); clearPollCache() })
describe('PR context boundaries', () => {
  it('forwards the Dashboard token and observed repository/head through the Now context mount', async () => {
    const head = 'a'.repeat(40)
    const work: Work = {
      intents: [], intents_truncated: false, connected: true,
      pull_requests: [{ number: 42, title: 'Fixture PR', state: 'open', draft: false,
        head_ref: 'feature/context-42', head_sha: head, base_ref: 'main', author: { login: 'sam' }, files: [] }],
    }
    const data: HubData = {
      whoami: { token: { id: 'viewer', name: 'status-page', repository: 'acme/widgets', read_only: true },
        owner: { provider: 'github', host: 'github.com', login: 'fixture-viewer' } },
      work, activity: null, health: null, fresh: new Set(), lastOk: Date.now(), failing: false,
    }
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response(JSON.stringify(contextFixture(42))))
    render(<DashboardPage section="now" question="Where is work?" token="dashboard-context-token"
      repo="display/label" data={data} d={null} overview={{ kind: 'loading' }} />)
    await screen.findByRole('link', { name: 'Inspect workflow record' })
    expect(fetcher).toHaveBeenCalledTimes(1)
    const [url, init] = fetcher.mock.calls[0]!
    expect(url).toBe(`/v1/hub/repos/acme%2Fwidgets/pr-context?number=42&expected_head=${head}`)
    expect(new Headers(init?.headers).get('authorization')).toBe('Bearer dashboard-context-token')
    expect(init?.credentials).toBe('omit')
    expect(init?.cache).toBe('no-store')
  })

  it('rejects cross-repository, cross-PR and malformed assurance responses', () => {
    const source = contextFixture(42)
    expect(() => decodePrContext(source, 'acme/other', 42)).toThrow(/repository mismatch/)
    expect(() => decodePrContext(source, 'acme/widgets', 43)).toThrow(/identity mismatch/)
    expect(() => decodePrContext({ ...source, workflows: [{ ...source.workflows![0], basis: 'verified' }] }, 'acme/widgets', 42)).toThrow(/basis/)
    expect(() => decodePrContext({ ...source, head_relation: 'clean' }, 'acme/widgets', 42)).toThrow()
  })
  it('accepts legacy identity omission but validates a supplied stable repository ID', () => {
    const legacy = { ...contextFixture(42), repository_id: undefined }
    expect(decodePrContext(legacy, 'acme/widgets', 42).repository_id).toBeUndefined()
    expect(decodePrContext({ ...legacy, repository_id: 91 }, 'acme/widgets', 42).repository_id).toBe(91)
    for (const repository_id of [0, -1, '91', Number.MAX_SAFE_INTEGER + 1]) {
      expect(() => decodePrContext({ ...legacy, repository_id }, 'acme/widgets', 42)).toThrow()
    }
  })
  it('validates coverage for missing-PR fixtures and rejects an incomplete contract', () => {
    expect(decodePrContext(contextFixture(0), 'acme/widgets', 42).availability).toBe('not_observed')
    expect(() => decodePrContext({ version: 1, availability: 'not_observed' }, 'acme/widgets', 42)).toThrow('Invalid PR context')
  })
  it('uses millisecond observation age for fresh, old and future clocks', () => {
    const data = contextFixture(42)
    data.pull_request!.observed_at_ms = Date.now()
    const view = render(<ContextFacts data={data} refresh={() => {}} />)
    expect(screen.getByText('Retained observation')).toBeTruthy()
    data.pull_request!.observed_at_ms -= 16 * 60 * 1000
    view.rerender(<ContextFacts data={data} refresh={() => {}} />)
    expect(screen.getByText('Older or uncertain observation')).toBeTruthy()
    data.pull_request!.observed_at_ms = Date.now() + 120_000
    view.rerender(<ContextFacts data={data} refresh={() => {}} />)
    expect(screen.getByText('Older or uncertain observation')).toBeTruthy()
  })
  it('labels changed head and earlier evidence without a current blocker claim', () => {
    render(<ContextFacts data={contextFixture(43)} refresh={() => {}} />)
    expect(screen.getByText('The observed head changed')).toBeTruthy()
    expect(screen.getByText('Some workflow evidence belongs to another head')).toBeTruthy()
    expect(screen.queryByText('Reported blocker')).toBeNull()
    expect(screen.getByRole('link', { name: 'Inspect workflow record' }).getAttribute('href')).toBe('#skills/runs/context_43')
    expect(screen.getByText(/not GitHub approval/)).toBeTruthy()
  })
  it('links overlapping Work and catch-up without treating a scope match as a PR link', () => {
    render(<ContextFacts data={contextFixture(42)} refresh={() => {}} />)
    expect(screen.getByText(/Shared paths do not prove a conflict or link a task/)).toBeTruthy()
    expect(screen.getByRole('link', { name: 'Fix invoice rounding' }).getAttribute('href')).toBe(`#work/wrk_${'b'.repeat(32)}`)
    expect(screen.getByRole('link', { name: 'Catch up since your last review' }).getAttribute('href')).toBe('#activity')
  })
  it('shows authors and known coverage gaps without opening details', () => {
    const data = contextFixture(42)
    data.coverage!.last_sync_partial = true
    render(<ContextFacts data={data} refresh={() => {}} />)
    expect(screen.getAllByText('by sam')).toHaveLength(2)
    expect(screen.getByText('Reported on this head')).toBeTruthy()
    expect(screen.getByText('Some context coverage is incomplete').closest('details')).toBeNull()
  })
  it('rejects unsafe URLs and keeps missing checks visible', () => {
    const fixture = contextFixture(45)
    fixture.pull_request!.url = 'javascript:alert(1)'
    render(<ContextFacts data={fixture} refresh={() => {}} />)
    expect(screen.queryByRole('link', { name: 'Open PR on forge' })).toBeNull()
    expect(screen.getByText('Reported blocker')).toBeTruthy()
    expect(screen.getByText('Provider observations are not a merge decision')).toBeTruthy()
  })
  it('keeps unavailable access separate from an empty result and retries', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response('{}', { status: 403 }))
    render(<PrContextPanel repo="acme/widgets" token="synthetic" pulls={[{ number: 44, title: 'Restricted', state: 'open', draft: false, head_ref: 'feature', base_ref: 'main', author: { login: 'sam' }, files: [] }]} />)
    expect(await screen.findByText('PR context is unavailable')).toBeTruthy()
    expect(screen.queryByText('No overlap in the retained, bounded records. This is not an all-clear.')).toBeNull()
    fireEvent.click(screen.getByRole('button', { name: 'Try again' }))
    await screen.findByText('PR context is unavailable')
    expect(fetcher).toHaveBeenCalledTimes(2)
  })
  it('hides a previous result after access is revoked on refresh', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(new Response(JSON.stringify(contextFixture(42)))).mockResolvedValueOnce(new Response('{}', { status: 403 }))
    render(<PrContextPanel repo="acme/widgets" token="revoked-fixture" pulls={[{ number: 42, title: 'Invoice', state: 'open', draft: false, head_ref: 'feature', base_ref: 'main', author: { login: 'sam' }, files: [] }]} />)
    await screen.findByRole('link', { name: 'Inspect workflow record' })
    fireEvent.click(screen.getByRole('button', { name: 'Refresh context' }))
    await screen.findByText('PR context is unavailable')
    expect(screen.queryByRole('link', { name: 'Inspect workflow record' })).toBeNull()
  })
  it('retains mirror gaps when the selected PR was not observed', async () => {
    const coverage = { ...contextFixture(42).coverage!, last_sync_partial: true }
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response(JSON.stringify({ version: 1, availability: 'not_observed', coverage })))
    render(<PrContextPanel repo="acme/widgets" token="missing-fixture" pulls={[{ number: 42, title: 'Invoice', state: 'open', draft: false, head_ref: 'feature', base_ref: 'main', author: { login: 'sam' }, files: [] }]} />)
    await screen.findByText('This PR is not in the retained mirror')
    expect(screen.getByText(/this PR may be missing because of that gap/)).toBeTruthy()
  })
  it('rejects invalid Work IDs before they reach the rendering helper', () => {
    const data = contextFixture(42)
    data.work_overlaps![0]!.work_id = 'invalid'
    expect(() => decodePrContext(data, 'acme/widgets', 42)).toThrow('Invalid Work ID')
  })
  it('bounds streamed payloads and omits cookies', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response('x'.repeat(49 * 1024)))
    await expect(fetchPrContext('acme/widgets', 42, undefined, 'synthetic', new AbortController().signal)).rejects.toThrow('too large')
    expect(fetcher.mock.calls[0]?.[1]?.credentials).toBe('omit')
  })
})

function providerFixture() {
  const fixture = contextFixture(42)
  fixture.pull_request!.base_sha = 'f'.repeat(40)
  const category = (items: Record<string, unknown>[] = []) => ({ view: 'provider_list', availability: 'available', complete: true, items, omitted: 0, malformed: 0, pages_read: 1, unread_pages: 0 })
  const row = { provider_id: 12, commit_sha: 'a'.repeat(40), head_relation: 'same', state: 'completed', conclusion: 'success', name: 'Unit checks', source_url: 'https://github.com/acme/widgets/actions/runs/12', provider_completed_at: '2026-10-30T10:00:00Z', attempt: null, tested_merge_sha: null }
  return { ...fixture, provider_evidence: { availability: 'observed', requester_access: 'hub_token_scope', live_requester_access: 'not_revalidated', snapshot: {
    version: 1, provider: 'github', repository_id: 91, number: 42, head_sha: 'a'.repeat(40), base_sha: 'f'.repeat(40), observed_at: Date.now(), atomic_snapshot: false, attempt_history: false, retention_truncated: false,
    permissions: { availability: 'observed', pull_requests: 'available', checks: 'available', statuses: 'permission_missing' },
    reviews: { ...category([{ provider_id: 13, attempt: null, tested_merge_sha: null, state: 'APPROVED', reviewer: 'sam', commit_sha: 'b'.repeat(40), head_relation: 'different', provider_submitted_at: '2026-10-30T09:00:00Z', source_url: 'https://github.com/acme/widgets/pull/42#pullrequestreview-13' }]), view: 'latest_observed' },
    check_runs: { ...category([row]), view: 'latest_by_name' }, commit_statuses: { ...category(), availability: 'permission_missing', complete: false, pages_read: 0, unread_pages: null },
  } } }
}

describe('provider evidence boundaries', () => {
  it('renders bounded provider facts without claiming aggregate readiness', () => {
    const data = decodePrContext(providerFixture(), 'acme/widgets', 42)
    render(<ContextFacts data={data} refresh={() => {}} />)
    expect(screen.getByText('Unit checks')).toBeTruthy()
    expect(within(screen.getByRole('region', { name: 'Reviews' })).getByText(/by sam/)).toBeTruthy()
    expect(screen.getAllByText(/Attempt unknown · tested merge unknown/)).toHaveLength(2)
    expect(screen.getByText(/different from observed head/)).toBeTruthy()
    expect(screen.getByText(/Incomplete or unavailable: permission missing/)).toBeTruthy()
    expect(screen.getByText(/live requester access on GitHub was not revalidated/)).toBeTruthy()
    expect(screen.getAllByRole('link', { name: 'Inspect provider record' })).toHaveLength(2)
  })
  it('accepts two observed review pages while retaining explicit partial coverage', () => {
    const fixture = providerFixture()
    Object.assign(fixture.provider_evidence.snapshot.reviews, { pages_read: 2, unread_pages: 1, complete: false, availability: 'incomplete' })
    const data = decodePrContext(fixture, 'acme/widgets', 42)
    if (data.provider_evidence?.availability !== 'observed') throw new Error('Missing fixture snapshot')
    expect(data.provider_evidence.snapshot.reviews.view).toBe('latest_observed')
    expect(data.provider_evidence.snapshot.reviews.complete).toBe(false)
    fixture.provider_evidence.snapshot.reviews.pages_read = 3
    expect(() => decodePrContext(fixture, 'acme/widgets', 42)).toThrow(/page bound/)
    fixture.provider_evidence.snapshot.reviews.pages_read = 2
    fixture.provider_evidence.snapshot.check_runs.pages_read = 2
    expect(() => decodePrContext(fixture, 'acme/widgets', 42)).toThrow(/page bound/)
  })
  it('rejects mismatched snapshot head, PR, repository ID and malformed item commit', () => {
    for (const mutate of [
      (f: ReturnType<typeof providerFixture>) => { f.provider_evidence.snapshot.head_sha = 'e'.repeat(40) },
      (f: ReturnType<typeof providerFixture>) => { f.provider_evidence.snapshot.number = 43 },
      (f: ReturnType<typeof providerFixture>) => { f.provider_evidence.snapshot.repository_id = 0 },
      (f: ReturnType<typeof providerFixture>) => { f.provider_evidence.snapshot.check_runs.items[0]!.commit_sha = 'b'.repeat(40) },
    ]) {
      const fixture = providerFixture(); mutate(fixture)
      expect(() => decodePrContext(fixture, 'acme/widgets', 42)).toThrow()
    }
  })
  it('rejects unsupported assurance and coverage claims', () => {
    const fixture = providerFixture()
    fixture.provider_evidence.snapshot.check_runs.omitted = 1
    expect(() => decodePrContext(fixture, 'acme/widgets', 42)).toThrow(/coverage/)
    fixture.provider_evidence.snapshot.check_runs.omitted = 0
    fixture.provider_evidence.snapshot.check_runs.items[0]!.attempt = 2
    expect(() => decodePrContext(fixture, 'acme/widgets', 42)).toThrow(/identity/)
  })
  it('removes unsafe provider links and rejects oversized category lists', () => {
    const fixture = providerFixture()
    fixture.provider_evidence.snapshot.check_runs.items[0]!.source_url = 'https://user:secret@example.invalid/path'
    const data = decodePrContext(fixture, 'acme/widgets', 42)
    if (data.provider_evidence?.availability !== 'observed') throw new Error('Missing fixture snapshot')
    expect(data.provider_evidence.snapshot.check_runs.items[0]!.source_url).toBeNull()
    fixture.provider_evidence.snapshot.check_runs.items = Array.from({ length: 9 }, (_, index) => ({ ...fixture.provider_evidence.snapshot.check_runs.items[0], provider_id: index + 1 }))
    expect(() => decodePrContext(fixture, 'acme/widgets', 42)).toThrow(/list/)
  })
})
