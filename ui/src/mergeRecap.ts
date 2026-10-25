import { validPublicationId } from './publications.ts'

const COMMIT_ID = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/i
const MAX_MERGES = 20
const MAX_SCANNED = 21
const MAX_LABEL = 255
const MAX_TITLE = 500

export interface MergeRecapItem {
  id: string
  title: string | null
  branch: string
  base: string
  head_commit: string
  merge_commit: string
  pull_number: number
  updated_at: number
}

export interface MergeRecap {
  since: number
  until: number
  time_basis: 'last_recorded_update'
  scope: 'tracked_publications'
  merges: MergeRecapItem[]
  complete: boolean
  scanned_count: number
}

const fail = (field: string): never => {
  throw new Error(`The hub returned an invalid merge recap (${field})`)
}

const record = (value: unknown, field: string): Record<string, unknown> =>
  value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown>
    : fail(field)

const integer = (value: unknown, field: string, max = Number.MAX_SAFE_INTEGER): number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 && value <= max ? value : fail(field)

const array = (value: unknown, field: string, max: number): unknown[] => {
  if (!Array.isArray(value) || value.length > max) fail(field)
  return value as unknown[]
}

const boolean = (value: unknown, field: string): boolean =>
  typeof value === 'boolean' ? value : fail(field)

const text = (value: unknown, field: string, max: number): string => {
  if (typeof value !== 'string' || value.length === 0 || new TextEncoder().encode(value).length > max) fail(field)
  const valueText = value as string
  // Provider-authored labels are display data. Reject control characters instead of silently changing identity fields.
  if ([...valueText].some((character) => {
    const code = character.charCodeAt(0)
    return code < 32 || (code >= 127 && code <= 159)
  })) fail(field)
  return valueText
}

function item(value: unknown, since: number, until: number): MergeRecapItem {
  const row = record(value, 'merge_recap.merges.item')
  const id = text(row['id'], 'merge_recap.merges.id', 64)
  if (!validPublicationId(id)) fail('merge_recap.merges.id')
  const title = row['title'] === null ? null : text(row['title'], 'merge_recap.merges.title', MAX_TITLE)
  const head = text(row['head_commit'], 'merge_recap.merges.head_commit', 64)
  const merge = text(row['merge_commit'], 'merge_recap.merges.merge_commit', 64)
  if (!COMMIT_ID.test(head)) fail('merge_recap.merges.head_commit')
  if (!COMMIT_ID.test(merge)) fail('merge_recap.merges.merge_commit')
  const updatedAt = integer(row['updated_at'], 'merge_recap.merges.updated_at', 8_640_000_000_000)
  if (updatedAt < since || updatedAt > until) fail('merge_recap.merges.updated_at')
  const pullNumber = integer(row['pull_number'], 'merge_recap.merges.pull_number')
  if (pullNumber === 0) fail('merge_recap.merges.pull_number')
  return {
    id,
    title,
    branch: text(row['branch'], 'merge_recap.merges.branch', MAX_LABEL),
    base: text(row['base'], 'merge_recap.merges.base', MAX_LABEL),
    head_commit: head,
    merge_commit: merge,
    pull_number: pullNumber,
    updated_at: updatedAt,
  }
}

/** Missing or null means an older Hub or an unavailable optional stored read. It never means an empty scan. */
export function decodeMergeRecap(value: unknown, overviewSince: number, overviewUntil: number): MergeRecap | null {
  if (value === undefined || value === null) return null
  const recap = record(value, 'merge_recap')
  const since = integer(recap['since'], 'merge_recap.since', 8_640_000_000_000)
  const until = integer(recap['until'], 'merge_recap.until', 8_640_000_000_000)
  if (since !== overviewSince || until !== overviewUntil || since > until) fail('merge_recap.window')
  if (recap['time_basis'] !== 'last_recorded_update') fail('merge_recap.time_basis')
  if (recap['scope'] !== 'tracked_publications') fail('merge_recap.scope')
  const rawMerges = array(recap['merges'], 'merge_recap.merges', MAX_MERGES)
  const merges = rawMerges.map((entry) => item(entry, since, until))
  if (new Set(merges.map((entry) => entry.id)).size !== merges.length) fail('merge_recap.merges.id')
  const complete = boolean(recap['complete'], 'merge_recap.complete')
  const scannedCount = integer(recap['scanned_count'], 'merge_recap.scanned_count', MAX_SCANNED)
  if (scannedCount !== (complete ? merges.length : MAX_SCANNED)) fail('merge_recap.scanned_count')
  return {
    since,
    until,
    time_basis: 'last_recorded_update',
    scope: 'tracked_publications',
    merges,
    complete,
    scanned_count: scannedCount,
  }
}

/** An optional auxiliary read must not make the independently useful Work overview unavailable. */
export function decodeOptionalMergeRecap(value: unknown, overviewSince: number, overviewUntil: number): MergeRecap | null {
  try {
    return decodeMergeRecap(value, overviewSince, overviewUntil)
  } catch {
    return null
  }
}

export const mergeRecapHref = (id: string): string | null =>
  validPublicationId(id) ? `#landed/${encodeURIComponent(id)}` : null

export const shortCommit = (commit: string): string => commit.slice(0, 12)
