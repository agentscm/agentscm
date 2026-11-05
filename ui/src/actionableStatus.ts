import { ApiError } from './api.ts'
import { withTimeout } from './timeout.ts'

export type StatusState = 'active' | 'awaiting_party' | 'blocked' | 'reassessment_needed' | 'retired'
export interface StatusRecipient { kind: 'person' | 'lead_agent' | 'unknown' | 'none'; owner: string | null; login: string | null; agent: string | null }
export interface StatusWork {
  work_id: string; title: string; generation: number; updated_seq: number; state: StatusState; recipient: StatusRecipient; needs_you: boolean; action: string; reported_next_action: string | null; freshness: 'active' | 'stale' | 'unknown'; limits: string[]
  reconciliation: { current: boolean; declaration_seq: number; current_evidence_digest: string | null; current_source_coverage: { source: string; state: string; checked_at_seconds: number; unread_pages: number; deferred: number }[]; stale_reasons: string[]; digest: string } | null
  operational: { reservation: { status: 'held' | 'none' }; outcome: { value: string; provenance: string }; report: { at: number; kind: string } | null }
}
export interface StatusAction { id: string; kind: string; work_ids: string[]; recipients: StatusRecipient[]; needs_you: boolean; action: string; updated_seq: number; semantic_version?: string | null; limits: string[] }
export interface ActionableStatus {
  schema_version: 1; repository: string; repository_id: number | null; provider: string; host: string; viewer_owner: string; work: StatusWork[]; actions: StatusAction[]
  counts: { active: number; awaiting_party: number; blocked: number; reassessment_needed: number; retired: number; held_reservations: number; needs_you: number }
  observed_at: number; complete: boolean; atomic_snapshot: boolean; limits: string[]; digest: string
}
const record = (v: unknown): Record<string, unknown> => { if (!v || typeof v !== 'object' || Array.isArray(v)) throw new Error('Invalid shared status'); return v as Record<string, unknown> }
const text = (v: unknown, max = 1024): string => { if (typeof v !== 'string' || new TextEncoder().encode(v).length > max || /[\p{Cc}\p{Cf}]/u.test(v)) throw new Error('Invalid shared status text'); return v }
const number = (v: unknown): number => { if (typeof v !== 'number' || !Number.isSafeInteger(v) || v < 0) throw new Error('Invalid shared status number'); return v }
const bool = (v: unknown): boolean => { if (typeof v !== 'boolean') throw new Error('Invalid shared status flag'); return v }
const optional = (v: unknown) => v === null ? null : text(v)
const list = <T>(v: unknown, max: number, decode: (x: unknown) => T): T[] => { if (!Array.isArray(v) || v.length > max) throw new Error('Invalid shared status list'); return v.map(decode) }
const choice = <T extends string>(v: unknown, choices: readonly T[]): T => { if (!choices.includes(v as T)) throw new Error('Invalid shared status choice'); return v as T }
const recipient = (v: unknown): StatusRecipient => { const r = record(v); return { kind: choice(r.kind, ['person', 'lead_agent', 'unknown', 'none']), owner: optional(r.owner), login: optional(r.login), agent: optional(r.agent) } }
const strings = (v: unknown) => list(v, 256, (x) => text(x))
export function decodeStatus(v: unknown): ActionableStatus {
  const r = record(v)
  if (r.schema_version !== 1) throw new Error('Unsupported shared status version')
  const work = list(r.work, 100, (v): StatusWork => {
    const w = record(v), op = record(w.operational), reservation = record(op.reservation), outcome = record(op.outcome), report = op.report === null ? null : record(op.report)
    const reconciliation = w.reconciliation === null ? null : record(w.reconciliation)
    return { reconciliation: reconciliation ? { current: bool(reconciliation.current), declaration_seq: number(reconciliation.declaration_seq), current_evidence_digest: optional(reconciliation.current_evidence_digest), current_source_coverage: list(reconciliation.current_source_coverage, 32, (v) => { const p = record(v); return { source: text(p.source), state: text(p.state), checked_at_seconds: number(p.checked_at_seconds), unread_pages: number(p.unread_pages), deferred: number(p.deferred) } }), stale_reasons: strings(reconciliation.stale_reasons), digest: text(reconciliation.digest) } : null, work_id: text(w.work_id), title: text(w.title, 500), generation: number(w.generation), updated_seq: number(w.updated_seq), state: choice(w.state, ['active', 'awaiting_party', 'blocked', 'reassessment_needed', 'retired']), recipient: recipient(w.recipient), needs_you: bool(w.needs_you), action: text(w.action), reported_next_action: optional(w.reported_next_action), freshness: choice(w.freshness, ['active', 'stale', 'unknown']), limits: strings(w.limits), operational: { reservation: { status: choice(reservation.status, ['held', 'none']) }, outcome: { value: text(outcome.value), provenance: text(outcome.provenance) }, report: report ? { at: number(report.at), kind: text(report.kind) } : null } }
  })
  const actions = list(r.actions, 1000, (v): StatusAction => { const a = record(v); return { id: text(a.id, 2048), kind: text(a.kind), work_ids: strings(a.work_ids), recipients: list(a.recipients, 100, recipient), needs_you: bool(a.needs_you), action: text(a.action, 4096), updated_seq: number(a.updated_seq), semantic_version: a.semantic_version === undefined ? null : optional(a.semantic_version), limits: strings(a.limits) } })
  const c = record(r.counts)
  const counts = { active: number(c.active), awaiting_party: number(c.awaiting_party), blocked: number(c.blocked), reassessment_needed: number(c.reassessment_needed), retired: number(c.retired), held_reservations: number(c.held_reservations), needs_you: number(c.needs_you) }
  if (counts.needs_you !== work.filter((w) => w.needs_you).length + actions.filter((a) => a.needs_you).length || new Set(work.map((w) => w.work_id)).size !== work.length || new Set(actions.map((a) => a.id)).size !== actions.length) throw new Error('Inconsistent shared status counts')
  return { schema_version: 1, repository: text(r.repository), repository_id: r.repository_id === null ? null : number(r.repository_id), provider: text(r.provider), host: text(r.host), viewer_owner: text(r.viewer_owner), work, actions, counts, observed_at: number(r.observed_at), complete: bool(r.complete), atomic_snapshot: bool(r.atomic_snapshot), limits: strings(r.limits), digest: text(r.digest, 128) }
}
export async function fetchStatus(token: string, signal: AbortSignal): Promise<ActionableStatus> {
  const response = await fetch('/v1/hub/standup/status', { signal: withTimeout(signal), cache: 'no-store', credentials: 'omit', headers: { Authorization: `Bearer ${token}` } })
  if (!response.ok) throw new ApiError(response.status, 'Shared status could not load. Retry or open the Work page.')
  if (!response.body) throw new Error('Shared status has no response body')
  const reader = response.body.getReader(), chunks: Uint8Array[] = []
  let length = 0
  try { for (;;) { const { done, value } = await reader.read(); if (done) break; length += value.length; if (length > 4 * 1024 * 1024) throw new Error('Shared status exceeded its response limit'); chunks.push(value) } } catch (error) { await reader.cancel().catch(() => undefined); throw error } finally { reader.releaseLock() }
  const bytes = new Uint8Array(length); let at = 0; for (const chunk of chunks) { bytes.set(chunk, at); at += chunk.length }
  return decodeStatus(JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes)))
}
export const statusLabel = (state: StatusState) => ({ active: 'Active', awaiting_party: 'Awaiting a named party', blocked: 'Blocked', reassessment_needed: 'Reassessment needed', retired: 'Retired' })[state]
export const recipientLabel = (r: StatusRecipient): string => r.login ? r.kind === 'person' ? r.login : `${r.login}’s agent` : r.kind === 'none' ? 'No action assigned' : 'Recipient unknown'

export const statusProvenance = (w: StatusWork): string => [
  `Claim ${w.freshness}.`,
  w.operational.report ? `Agent report ${w.operational.report.kind} at ${new Date(w.operational.report.at * 1000).toLocaleString()}.` : 'No retained agent report.',
  ...(w.reconciliation?.current_source_coverage.map((p) => `${p.source}: ${p.state}; checked ${new Date(p.checked_at_seconds * 1000).toLocaleString()}.`) ?? ['Provider coverage unavailable.']),
  ...(w.reconciliation && !w.reconciliation.current ? ['No current reconciliation synthesis; retained reports are history.'] : []),
].join(' ')
