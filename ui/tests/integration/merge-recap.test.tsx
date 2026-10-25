import { render, screen } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import type { MergeRecap } from '../../src/mergeRecap.ts'
import type { WorkOverview } from '../../src/standup.ts'
import { RecentMergeActivity, WorkPage } from '../../src/pages/Work.tsx'

const PUB = `pub_${'a'.repeat(32)}`
const recap = (over: Partial<MergeRecap> = {}): MergeRecap => ({
  since: 100,
  until: 200,
  time_basis: 'last_recorded_update',
  scope: 'tracked_publications',
  merges: [],
  complete: true,
  scanned_count: 0,
  ...over,
})

const overview = (merge_recap: MergeRecap | null): WorkOverview => ({
  recent_since: 100,
  until: 200,
  current: [],
  recent: [],
  overlaps: [],
  current_complete: true,
  recent_complete: true,
  overlaps_complete: true,
  history_retention_gap: false,
  merge_recap,
})

describe('recent merge activity UI', () => {
  it('distinguishes an unavailable legacy recap from an empty complete scan', () => {
    const { rerender } = render(<RecentMergeActivity recap={null} />)
    expect(screen.getByText('Recent merge activity unavailable')).toBeTruthy()
    expect(screen.getByText(/No merge coverage is implied/)).toBeTruthy()

    rerender(<RecentMergeActivity recap={recap()} />)
    expect(screen.getByText('No merged publications were found in this stored update window.')).toBeTruthy()
    expect(screen.getByText(/0 matching publication records inspected; all matching merge rows are shown/)).toBeTruthy()

    rerender(<RecentMergeActivity recap={recap({ complete: false, scanned_count: 21 })} />)
    expect(screen.getByText('This partial result includes no merge rows; more matching merges are omitted.')).toBeTruthy()
  })

  it('shows a partial stored read with escaped titles and a safe landed link', () => {
    const { container } = render(<RecentMergeActivity recap={recap({
      complete: false,
      scanned_count: 21,
      merges: [{
        id: PUB,
        title: '<img src=x onerror=alert(1)>',
        branch: '<img src=x onerror=branch()>',
        base: 'main',
        head_commit: '1'.repeat(40),
        merge_commit: '2'.repeat(40),
        pull_number: 128,
        updated_at: 150,
      }],
    })} />)

    const link = screen.getByRole('link', { name: '<img src=x onerror=alert(1)>' })
    expect(link.getAttribute('href')).toBe(`#landed/${PUB}`)
    expect(document.querySelector('img')).toBeNull()
    expect(screen.getByText(/Partial stored result/)).toBeTruthy()
    expect(screen.getByText(/Only changes published through AgentSCM/)).toBeTruthy()
    expect(screen.getByText(/21 matching publication records inspected; more matching merges are omitted/)).toBeTruthy()
    expect(screen.getByText(PUB)).toBeTruthy()
    expect(screen.getByText('Branch → base')).toBeTruthy()
    expect(screen.getByText('<img src=x onerror=branch()>')).toBeTruthy()
    expect(screen.getByText('main')).toBeTruthy()
    expect(screen.getByText('1'.repeat(40))).toBeTruthy()
    expect(screen.getByText('2'.repeat(40))).toBeTruthy()
    expect(container.querySelectorAll('details .tech code.text-break')).toHaveLength(5)
    expect(container.scrollWidth).toBeLessThanOrEqual(container.clientWidth)
  })

  it('keeps the last Work snapshot visible and labels a failed refresh while catch-up loads independently', () => {
    window.location.hash = '#work'
    render(<WorkPage token="read-only" overview={{
      kind: 'ready',
      value: overview(recap()),
      fetchedAt: 1_000_000,
      refreshError: 'connection refused',
      retry: vi.fn(),
    }} />)
    expect(screen.getByText('Could not refresh')).toBeTruthy()
    expect(screen.getByText('Since you last checked')).toBeTruthy()
    expect(screen.getByText('No unfinished work is assigned right now.')).toBeTruthy()
    expect(screen.getByText(/connection refused/)).toBeTruthy()
  })
})
