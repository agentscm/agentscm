// Owner recovery from the Hub: what a signed-in person may do about one Work item, what each
// remedy would change, and the person transition that applies it. The same service and rules an
// agent's agentscm_work_recovery uses. Every call needs the web sign-in (HttpOnly cookie, plus
// its CSRF token for a change).
//
// API for other packages:
//   RecoveryRemedy, RecoveryWork, RemedyPreview, BatchItem
//   fetchRecovery(session, repository, workId, signal)   the person's view of one Work item
//   previewRemedy(session, repository, workId, remedy)    what one remedy would change and its fences
//   applyTransition(session, repository, preview, operationId, reason)   one person transition; resolves to its batch outcome
//   PERSON_REMEDIES                                       the remedies a person can apply from the Hub
import { ApiError } from './api.ts'
import type { BrowserSession } from './devices.ts'
import { refusalFrom } from './refusal.ts'
import { withTimeout } from './timeout.ts'

export type RemedyKind = 'release' | 'retain' | 'complete' | 'abandon' | 'supersede' | 'reopen' | 'resume' | 'handover'
/** Transitions a person applies through the Hub; resume, reopen and handover need an agent session. */
export const PERSON_REMEDIES: RemedyKind[] = ['release', 'complete', 'abandon', 'retain']

export interface RecoveryRemedy {
  remedy: RemedyKind
  permitted: boolean
  refusal: { code: string; message: string } | null
}
export interface RecoveryWork {
  work_id: string
  bucket: 'active' | 'needs_you' | 'resumable' | 'finished' | 'other'
  standing: 'lead' | 'owner' | 'admin' | 'other'
  why: string
  next_action: string | null
  blocker: string | null
  requests: string[]
  dependents: string[]
  remedies: RecoveryRemedy[]
}
export interface RemedyPreview {
  work_id: string
  remedy: RemedyKind
  permitted: boolean
  refusal: { code: string; message: string } | null
  generation: number
  expected_updated_seq: number
  summary: string
  freed_paths: string[]
  kept_paths: string[]
  requests: string[]
  dependents: string[]
  unfinished: boolean
}
export interface BatchItem {
  outcome: 'applied' | 'replayed' | 'conflict' | 'refused' | 'unavailable'
  code: string | null
  message: string | null
  retry: string
}

const KINDS = new Set<string>(['release', 'retain', 'complete', 'abandon', 'supersede', 'reopen', 'resume', 'handover'])
const BUCKETS = new Set<string>(['active', 'needs_you', 'resumable', 'finished', 'other'])
const STANDINGS = new Set<string>(['lead', 'owner', 'admin', 'other'])
const OUTCOMES = new Set<string>(['applied', 'replayed', 'conflict', 'refused', 'unavailable'])

function rec(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new ApiError(502, 'The Hub sent an unreadable recovery answer')
  return value as Record<string, unknown>
}
function text(value: unknown, max = 2000): string {
  if (typeof value !== 'string') throw new ApiError(502, 'The Hub sent an unreadable recovery answer')
  return value.slice(0, max)
}
const optional = (value: unknown): string | null => (typeof value === 'string' ? value.slice(0, 2000) : null)
const strings = (value: unknown): string[] => (Array.isArray(value) ? value.filter((v): v is string => typeof v === 'string').slice(0, 256) : [])
function choice<T extends string>(value: unknown, allowed: Set<string>): T {
  if (typeof value !== 'string' || !allowed.has(value)) throw new ApiError(502, 'The Hub sent an unreadable recovery answer')
  return value as T
}
function refusal(value: unknown): { code: string; message: string } | null {
  if (value === null || value === undefined) return null
  const r = rec(value)
  return { code: text(r['code'], 64), message: text(r['message']) }
}

async function call(path: string, session: BrowserSession, body: unknown, signal?: AbortSignal): Promise<unknown> {
  const res = await fetch(path, {
    method: 'POST',
    signal: withTimeout(signal),
    cache: 'no-store',
    credentials: 'same-origin',
    headers: { 'x-csrf-token': session.csrf, 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })
  if (!res.ok) {
    let parsed: unknown = null
    try {
      parsed = await res.json()
    } catch {
      /* the status still says enough */
    }
    throw new ApiError(res.status, `HTTP ${res.status}`, refusalFrom(parsed))
  }
  return res.json()
}

export function decodeRecoveryWork(value: unknown): RecoveryWork {
  const w = rec(value)
  const follow = rec(w['follow_up'])
  return {
    work_id: text(w['work_id'], 64),
    bucket: choice(w['bucket'], BUCKETS),
    standing: choice(w['standing'], STANDINGS),
    why: text(w['why']),
    next_action: optional(follow['next_action']),
    blocker: optional(follow['blocker']),
    requests: strings(w['requests']),
    dependents: strings(w['dependents']),
    remedies: (Array.isArray(w['remedies']) ? w['remedies'] : []).map((r) => {
      const o = rec(r)
      return { remedy: choice<RemedyKind>(o['remedy'], KINDS), permitted: o['permitted'] === true, refusal: refusal(o['refusal']) }
    }),
  }
}

export async function fetchRecovery(session: BrowserSession, repository: string, workId: string, signal?: AbortSignal): Promise<RecoveryWork | null> {
  const page = rec(await call('/v1/hub/session/work/recovery', session, { repository, work_id: workId }, signal))
  const work = Array.isArray(page['work']) ? page['work'] : []
  return work.length ? decodeRecoveryWork(work[0]) : null
}

export function decodePreview(value: unknown): RemedyPreview {
  const p = rec(value)
  const e = rec(p['effects'])
  const follow = rec(e['follow_up'])
  const generation = p['generation']
  const seq = p['expected_updated_seq']
  if (typeof generation !== 'number' || typeof seq !== 'number') throw new ApiError(502, 'The Hub sent an unreadable recovery answer')
  return {
    work_id: text(p['work_id'], 64),
    remedy: choice(p['remedy'], KINDS),
    permitted: p['permitted'] === true,
    refusal: refusal(p['refusal']),
    generation,
    expected_updated_seq: seq,
    summary: text(e['summary']),
    freed_paths: strings(e['freed_paths']),
    kept_paths: strings(e['kept_paths']),
    requests: strings(e['requests']),
    dependents: strings(e['dependents']),
    unfinished: follow['unfinished'] === true,
  }
}

export async function previewRemedy(session: BrowserSession, repository: string, workId: string, remedy: RemedyKind, signal?: AbortSignal): Promise<RemedyPreview> {
  return decodePreview(await call('/v1/hub/session/work/recovery/preview', session, { repository, work_id: workId, remedy }, signal))
}

/** Applies the previewed transition with the preview's fences; a moved fence refuses it with nothing changed. */
export async function applyTransition(session: BrowserSession, repository: string, preview: RemedyPreview, operationId: string, reason: string): Promise<BatchItem> {
  const result = rec(
    await call('/v1/hub/session/work/transitions', session, {
      repository,
      items: [{ work_id: preview.work_id, operation_id: operationId, generation: preview.generation, expected_updated_seq: preview.expected_updated_seq, transition: preview.remedy, reason }],
    }),
  )
  const items = Array.isArray(result['items']) ? result['items'] : []
  const item = rec(items[0])
  return { outcome: choice(item['outcome'], OUTCOMES), code: optional(item['code']), message: optional(item['message']), retry: text(item['retry']) }
}
