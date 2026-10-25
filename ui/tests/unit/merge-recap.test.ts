import { describe, expect, it } from 'vitest'
import { decodeMergeRecap, mergeRecapHref } from '../../src/mergeRecap.ts'
import { decodeOverview } from '../../src/standup.ts'

const id = (digit = 'a') => `pub_${digit.repeat(32)}`
const commit = (digit: string) => digit.repeat(40)
const row = (over: Record<string, unknown> = {}) => ({
  id: id(),
  title: 'Keep the stored view truthful',
  branch: 'feat/recap',
  base: 'main',
  head_commit: commit('1'),
  merge_commit: commit('2'),
  pull_number: 128,
  updated_at: 150,
  ...over,
})
const recap = (over: Record<string, unknown> = {}) => ({
  since: 100,
  until: 200,
  time_basis: 'last_recorded_update',
  scope: 'tracked_publications',
  merges: [row()],
  complete: true,
  scanned_count: 1,
  ...over,
})
const overview = (over: Record<string, unknown> = {}) => ({
  recent_since: 100,
  until: 200,
  current: [],
  recent: [],
  overlaps: [],
  current_complete: true,
  recent_complete: true,
  overlaps_complete: true,
  history_retention_gap: false,
  ...over,
})

describe('merge recap decoder', () => {
  it('keeps legacy missing and explicit null recaps unavailable', () => {
    expect(decodeOverview(overview()).merge_recap).toBeNull()
    expect(decodeOverview(overview({ merge_recap: null })).merge_recap).toBeNull()
    expect(decodeOverview(overview({ merge_recap: recap({ scope: 'all_forge_merges' }) })).merge_recap).toBeNull()
  })

  it('accepts the exact bounded stored-publication contract', () => {
    expect(decodeMergeRecap(recap(), 100, 200)).toMatchObject({
      time_basis: 'last_recorded_update',
      scope: 'tracked_publications',
      complete: true,
      scanned_count: 1,
      merges: [{ id: id(), pull_number: 128, merge_commit: commit('2') }],
    })
  })

  it.each([
    ['wrong window', recap({ until: 201 })],
    ['negative timestamp', recap({ since: -1 })],
    ['invented scope', recap({ scope: 'all_forge_merges' })],
    ['bad commit', recap({ merges: [row({ merge_commit: 'javascript:alert(1)' })] })],
    ['unsafe publication id', recap({ merges: [row({ id: '../../settings' })] })],
    ['too many rows', recap({ merges: Array.from({ length: 21 }, (_, n) => row({ id: id((n % 10).toString()) })) })],
    ['impossible scan count', recap({ scanned_count: 0 })],
    ['old broad scan count', recap({ scanned_count: 200 })],
    ['lookahead above contract', recap({ scanned_count: 22 })],
    ['partial without lookahead row', recap({ complete: false, scanned_count: 20 })],
  ])('rejects malformed payloads: %s', (_name, value) => {
    expect(() => decodeMergeRecap(value, 100, 200)).toThrow(/invalid merge recap/)
  })

  it('only makes validated publication ids into internal links', () => {
    expect(mergeRecapHref(id())).toBe(`#landed/${id()}`)
    expect(mergeRecapHref('pub_a/../../settings')).toBeNull()
    expect(mergeRecapHref('javascript:alert(1)')).toBeNull()
  })
})
