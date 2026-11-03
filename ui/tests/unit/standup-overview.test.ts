import { describe, expect, it } from 'vitest'
import { decodeOverview } from '../../src/standup'

const base = {
  recent_since: 0,
  until: 10,
  current: [],
  recent: [],
  overlaps: [],
  current_complete: true,
  recent_complete: true,
  overlaps_complete: true,
  history_retention_gap: false,
}

describe('decodeOverview settled_count', () => {
  it('reads the count of Work with a merged pull request', () => {
    expect(decodeOverview({ ...base, settled_count: 3 }).settled_count).toBe(3)
  })

  it('treats a missing count as zero', () => {
    expect(decodeOverview(base).settled_count).toBe(0)
  })

  it('rejects a count that is not a non-negative integer', () => {
    for (const bad of [-1, 1.5, '2', null]) expect(() => decodeOverview({ ...base, settled_count: bad })).toThrow()
  })
})
