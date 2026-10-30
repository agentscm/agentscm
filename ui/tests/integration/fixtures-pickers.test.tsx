// Typeahead pickers and shared pagers rendered against the synthetic mock hub (dev/mock.ts).
import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import App from '../../src/App.tsx'
import { ChangeRecordPage } from '../../src/pages/ChangeRecord.tsx'
import { LandedPage } from '../../src/pages/Landed.tsx'
import { saveToken } from '../../src/storage.ts'
import { WorkflowsPage } from '../../src/pages/Workflows.tsx'
import { json, stubHub } from '../fixtures/mockFetch.ts'

const LOAD = { timeout: 5_000 }
const REPO = 'acme/widgets'
const at = (hash: string) => act(() => {
  window.location.hash = hash
  window.dispatchEvent(new HashChangeEvent('hashchange'))
})

afterEach(() => {
  vi.unstubAllGlobals()
  localStorage.clear()
  sessionStorage.clear()
})

describe('Landed picker', () => {
  it('searches publications on the server with q and a small limit, then opens the pick', async () => {
    const sent = stubHub()
    at('#landed')
    render(<LandedPage repo={REPO} token="fixture-token" />)
    const picker = await screen.findByRole('combobox', { name: 'Find a change' }, LOAD)
    expect(document.querySelector('[data-tour="landed-picker"]')).toBeTruthy()
    fireEvent.focus(picker)
    await screen.findAllByRole('option', {}, LOAD)
    fireEvent.change(picker, { target: { value: 'retry-backoff-111' } })
    // The previous options stay visible while the search runs, so wait for the request itself.
    await waitFor(() => expect(sent.some((r) => r.url.includes('/publications?') && r.url.includes('q=retry-backoff-111') && r.url.includes('limit=10'))).toBe(true), LOAD)
    const option = await screen.findByRole('option', { name: /retry-backoff-111/ }, LOAD)
    // Human label first (branch, PR number, title), the ID only as secondary text.
    expect(option.textContent).toContain('PR #211')
    expect(option.textContent).toContain('Synthetic retry backoff change 111')
    fireEvent.click(option)
    await waitFor(() => expect(window.location.hash).toMatch(/^#landed\/pub_[0-9a-f]{32}$/))
  })

  it('pages the list through the hash and Newer returns to the first page', async () => {
    const sent = stubHub()
    at('#landed')
    render(<LandedPage repo={REPO} token="fixture-token" />)
    await screen.findByText(/recent changes merged/, {}, LOAD)
    expect(document.querySelector('[data-tour="pager"]')).toBeTruthy()
    fireEvent.click(screen.getByRole('button', { name: /Older changes/ }))
    await waitFor(() => expect(window.location.hash).toMatch(/^#landed\?before=pub_/))
    await waitFor(() => expect(sent.some((r) => r.url.includes('before=pub_'))).toBe(true), LOAD)
    fireEvent.click(await screen.findByRole('button', { name: /Newer changes/ }, LOAD))
    await waitFor(() => expect(window.location.hash).toBe('#landed'))
  })
})

describe('Change record picker', () => {
  it('searches work items by text and opens the chosen record', async () => {
    const sent = stubHub()
    render(<ChangeRecordPage repo={REPO} token="fixture-token" />)
    const picker = screen.getByRole('combobox', { name: 'Find a work item' })
    expect(document.querySelector('[data-tour="change-record-picker"]')).toBeTruthy()
    fireEvent.focus(picker)
    fireEvent.change(picker, { target: { value: 'ledger export task 12' } })
    const option = await screen.findByRole('option', { name: /ledger export task 12/ }, LOAD)
    expect(sent.some((r) => r.url.startsWith('/v1/hub/standup/work?') && r.url.includes('q=ledger+export+task+12'))).toBe(true)
    fireEvent.click(option)
    expect(window.location.hash).toMatch(/^#change-record\/wrk_[0-9a-f]{32}$/)
  })
})

describe('Activity load older', () => {
  it('appends older events using the oldest sequence and ends with a note', async () => {
    const sent = stubHub()
    saveToken('fixture-token', false)
    at('#activity')
    render(<App />)
    const button = await screen.findByRole('button', { name: /Load older/ }, LOAD)
    expect(document.querySelector('[data-tour="activity-load-older"]')).toBeTruthy()
    const before = screen.getAllByRole('listitem').length
    for (let i = 0; i < 6 && screen.queryByText('That’s everything.') === null; i++) {
      fireEvent.click(screen.getByRole('button', { name: /Load older/ }))
      await waitFor(() => expect(sent.filter((r) => /activity\?limit=50&before=\d+/.test(r.url))).toHaveLength(i + 1), LOAD)
      await waitFor(() => expect((screen.queryByRole('button', { name: /Load older/ }) as HTMLButtonElement | null)?.disabled ?? false).toBe(false), LOAD)
    }
    await screen.findByText('That’s everything.', {}, LOAD)
    expect(screen.getAllByRole('listitem').length).toBeGreaterThan(before + 100)
    expect(button).toBeTruthy()
  })
})

describe('Searching older pages from a picker', () => {
  it('Workflows: an empty scan with a cursor offers "Search older runs", which requests before=<cursor>', async () => {
    const run = { run_id: 'old-match', revision: 'a'.repeat(64), state: 'succeeded', name: 'Old gate', sequence: '10' }
    const sent = stubHub((url) => {
      if (url.pathname !== '/v1/hub/skill-workflows' || !url.searchParams.has('q')) return undefined
      return url.searchParams.get('before') === '77'
        ? json(200, { version: 1, availability: 'available', runs: [run], next_cursor: null })
        : json(200, { version: 1, availability: 'available', runs: [], next_cursor: '77' })
    })
    render(<WorkflowsPage token="fixture-token" repository={REPO} />)
    const box = await screen.findByRole('combobox', { name: 'Find a run' }, LOAD)
    fireEvent.focus(box)
    fireEvent.change(box, { target: { value: 'old gate' } })
    await screen.findByText('No matches in the newest runs searched', {}, LOAD)
    fireEvent.click(screen.getByRole('option', { name: 'Search older runs' }))
    await screen.findByRole('option', { name: /Old gate/ }, LOAD)
    expect(sent.some((r) => r.url.includes('before=77') && r.url.includes('q=old+gate'))).toBe(true)
  })

  it('Change records: "Show more results" follows next_cursor', async () => {
    const sent = stubHub()
    render(<ChangeRecordPage repo={REPO} token="fixture-token" />)
    const picker = screen.getByRole('combobox', { name: 'Find a work item' })
    fireEvent.focus(picker)
    fireEvent.change(picker, { target: { value: 'Synthetic' } })
    await screen.findAllByRole('option', {}, LOAD)
    const first = screen.getAllByRole('option').length
    fireEvent.click(await screen.findByRole('option', { name: 'Show more results' }, LOAD))
    await waitFor(() => expect(screen.getAllByRole('option').length).toBeGreaterThan(first), LOAD)
    expect(sent.some((r) => r.url.startsWith('/v1/hub/standup/work?') && r.url.includes('cursor=wrk_'))).toBe(true)
  })

  it('Landed: a capped scan with a cursor says so and offers "Show more results"', async () => {
    const sent = stubHub((url) => {
      if (!url.pathname.endsWith('/publications') || !url.searchParams.has('q')) return undefined
      return url.searchParams.has('before')
        ? json(200, { publications: [], next_cursor: null, complete: true })
        : json(200, { publications: [], next_cursor: `pub_${'a'.repeat(32)}`, complete: false, searched_to: 5 })
    })
    at('#landed')
    render(<LandedPage repo={REPO} token="fixture-token" />)
    const picker = await screen.findByRole('combobox', { name: 'Find a change' }, LOAD)
    fireEvent.focus(picker)
    fireEvent.change(picker, { target: { value: 'nothing here' } })
    await screen.findByText('No matches in the results searched so far', {}, LOAD)
    fireEvent.keyDown(picker, { key: 'ArrowDown' })
    fireEvent.keyDown(picker, { key: 'Enter' })
    await waitFor(() => expect(sent.some((r) => r.url.includes('before=pub_') && r.url.includes('q=nothing+here'))).toBe(true), LOAD)
  })
})
