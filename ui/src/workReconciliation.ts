import { ApiError } from './api.ts'
import { withTimeout } from './timeout.ts'
import type { WorkItem } from './standup.ts'

export interface ConditionDisposition {
  id: string
  state: 'satisfied' | 'not_satisfied' | 'unknown'
  obstructing: boolean
  limits: string[]
}
export interface ReconciliationSummary {
  current: {
    conditions: ConditionDisposition[]
    links: ConditionDisposition[]
    reassessments: { id: string; reason: string }[]
    reservation_disposition: 'held' | 'released' | 'explicitly_retained' | 'held_owner_decision_required'
    next_action: string
  } | null
  stale_reasons: string[]
}
const record = (v: unknown): Record<string, unknown> => {
  if (!v || typeof v !== 'object' || Array.isArray(v)) throw new Error('Invalid reconciliation response')
  return v as Record<string, unknown>
}
const text = (v: unknown, max = 500): string => {
  if (typeof v !== 'string' || v.length > max || /[\p{Cc}\p{Cf}]/u.test(v)) throw new Error('Invalid reconciliation text')
  return v
}
const list = <T>(v: unknown, max: number, read: (v: unknown) => T): T[] => {
  if (!Array.isArray(v) || v.length > max) throw new Error('Invalid reconciliation list')
  return v.map(read)
}
const number = (v: unknown): number => {
  if (typeof v !== 'number' || !Number.isSafeInteger(v) || v < 1) throw new Error('Invalid reconciliation version')
  return v
}
const condition = (v: unknown): ConditionDisposition => {
  const row = record(v)
  if (!['satisfied', 'not_satisfied', 'unknown'].includes(String(row.state)) || typeof row.obstructing !== 'boolean') throw new Error('Invalid prerequisite state')
  if ((row.state === 'satisfied') === row.obstructing) throw new Error('Inconsistent prerequisite obstruction')
  return { id: text(row.id, 255), state: row.state as ConditionDisposition['state'], obstructing: row.obstructing, limits: list(row.limits, 32, (v) => text(v, 255)) }
}

/** Retained historical success is deliberately never promoted to a current result. */
export function decodeReconciliation(v: unknown, item: Pick<WorkItem, 'id' | 'generation' | 'updated_seq'>): ReconciliationSummary {
  const row = record(v)
  if (row.schema_version !== 2 || row.work_id !== item.id) throw new Error('Reconciliation identity mismatch')
  const generation = number(row.generation), seq = number(row.updated_seq), declaration = number(row.declaration_seq)
  const stale_reasons = list(row.stale_reasons, 16, (v) => text(v, 255))
  if (generation !== item.generation || seq !== item.updated_seq) stale_reasons.push('work_detail_changed')
  if (row.current === null) return { current: null, stale_reasons }
  if (stale_reasons.length) return { current: null, stale_reasons }
  const current = record(row.current)
  if (current.work_id !== item.id || number(current.generation) !== generation || number(current.work_seq) !== seq || number(current.declaration_seq) !== declaration) throw new Error('Reconciliation version mismatch')
  const disposition = current.reservation_disposition
  if (!['held', 'released', 'explicitly_retained', 'held_owner_decision_required'].includes(String(disposition))) throw new Error('Invalid reservation disposition')
  return {
    stale_reasons,
    current: {
      conditions: list(current.conditions, 100, condition),
      links: list(current.links, 33, condition),
      reassessments: list(current.reassessments, 100, (v) => { const r = record(v); return { id: text(r.id, 255), reason: text(r.reason) } }),
      reservation_disposition: disposition as NonNullable<ReconciliationSummary['current']>['reservation_disposition'],
      next_action: text(current.next_action),
    },
  }
}
export async function fetchReconciliation(item: Pick<WorkItem, 'id' | 'generation' | 'updated_seq'>, token: string, signal: AbortSignal): Promise<ReconciliationSummary> {
  const response = await fetch(`/v1/hub/standup/work/${encodeURIComponent(item.id)}/reconciliation-v2`, { signal: withTimeout(signal), cache: 'no-store', credentials: 'omit', headers: { Authorization: `Bearer ${token}` } })
  if (!response.ok) throw new ApiError(response.status, `Prerequisite evidence could not load (HTTP ${response.status}).`)
  if (!response.body) throw new Error('Prerequisite evidence has no response body')
  const reader = response.body.getReader()
  const chunks: Uint8Array[] = []
  let length = 0
  try {
    for (;;) {
      const { done, value } = await reader.read()
      if (done) break
      length += value.byteLength
      // A full view can contain both the retained and current 128 KiB snapshots.
      if (length > 512 * 1024) { await reader.cancel(); throw new Error('Prerequisite evidence exceeds the response limit') }
      chunks.push(value)
    }
  } finally { reader.releaseLock() }
  const bytes = new Uint8Array(length)
  let offset = 0
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length }
  return decodeReconciliation(JSON.parse(new TextDecoder().decode(bytes)) as unknown, item)
}
