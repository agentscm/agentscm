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
//   fetchInventory(session, repository, after, signal)    an administrator's page of every owner's unfinished Work, with findings
import { ApiError } from './api.ts'
import type { BrowserSession } from './devices.ts'
import { refusalFrom } from './refusal.ts'
import { withTimeout } from './timeout.ts'

export type RemedyKind = 'release' | 'retain' | 'complete' | 'abandon' | 'supersede' | 'reopen' | 'prune_participants' | 'resume' | 'handover'
/** Transitions a person applies through the Hub; resume, reopen and handover need an agent session. */
export const PERSON_REMEDIES: RemedyKind[] = ['release', 'complete', 'abandon', 'retain', 'prune_participants']

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
  /** The sessions a prune would remove; for other remedies, the sessions that lose the Work. */
  fenced: { login: string; agent: string; role: string }[]
  /** Exactly the participants the prune apply names; empty for every other remedy. */
  participants: { token: string; session: string }[]
}
export interface InventoryFinding {
  /** Unknown kinds keep their raw text. */
  kind: string
  evidence: string
  complete: boolean
  pull_request: number | null
}
export interface InventoryWork {
  work_id: string
  title: string
  owner_login: string
  findings: InventoryFinding[]
}
export interface Inventory {
  work: InventoryWork[]
  next_after: string | null
  /** False for an older Hub that sends no findings: an empty list there is not "nothing noticed". */
  findings_reported: boolean
  /** False when the Hub says this page does not cover everything. */
  complete: boolean
}
export interface BatchItem {
  outcome: 'applied' | 'replayed' | 'conflict' | 'refused' | 'unavailable'
  code: string | null
  message: string | null
  retry: string
}

const KINDS = new Set<string>(['release', 'retain', 'complete', 'abandon', 'supersede', 'reopen', 'prune_participants', 'resume', 'handover'])
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

function findingList(value: unknown): unknown[] {
  if (value === undefined) return []
  if (!Array.isArray(value)) throw new ApiError(502, 'The Hub sent an unreadable recovery answer')
  return value
}

export function decodeInventory(value: unknown): Inventory {
  const page = rec(value)
  const work = page['work']
  if (!Array.isArray(work)) throw new ApiError(502, 'The Hub sent an unreadable recovery answer')
  const after = page['next_after']
  return {
    findings_reported: page['findings_reported'] === true,
    complete: page['complete'] === true,
    work: work.map((w) => {
      const o = rec(w)
      return {
        work_id: text(o['work_id'], 64),
        title: text(o['title'], 300),
        owner_login: text(o['owner_login'], 200),
        findings: findingList(o['findings']).map((f) => {
          const r = rec(f)
          const pr = r['pull_request']
          return { kind: text(r['kind'], 64), evidence: text(r['evidence']), complete: r['complete'] === true, pull_request: typeof pr === 'number' && Number.isInteger(pr) && pr > 0 ? pr : null }
        }),
      }
    }),
    next_after: typeof after === 'string' && after ? after.slice(0, 200) : null,
  }
}

/** A Hub administrator's page of every owner's unfinished Work in a repository; the Hub refuses anyone else. */
export async function fetchInventory(session: BrowserSession, repository: string, after: string | null, signal?: AbortSignal): Promise<Inventory> {
  return decodeInventory(await call('/v1/hub/session/work/recovery', session, { repository, all_owners: true, ...(after ? { after } : {}) }, signal))
}

function decodeFenced(value: unknown): RemedyPreview['fenced'] {
  return (Array.isArray(value) ? value : []).slice(0, 256).map((f) => {
    const o = rec(f)
    return { login: text(o['login'], 200), agent: text(o['agent'], 200), role: text(o['role'], 64) }
  })
}

/** The prune apply's participants, exactly as the Hub named them; anything malformed is an error, never dropped. */
function decodeParticipants(value: unknown): RemedyPreview['participants'] {
  if (value === undefined) return []
  if (!Array.isArray(value)) throw new ApiError(502, 'The Hub sent an unreadable recovery answer')
  return value.map((v) => {
    const o = rec(v)
    return { token: text(o['token'], 200), session: text(o['session'], 200) }
  })
}

export function decodePreview(value: unknown): RemedyPreview {
  const p = rec(value)
  const e = rec(p['effects'])
  const follow = rec(e['follow_up'])
  const generation = p['generation']
  const seq = p['expected_updated_seq']
  if (typeof generation !== 'number' || typeof seq !== 'number') throw new ApiError(502, 'The Hub sent an unreadable recovery answer')
  const remedy = choice<RemedyKind>(p['remedy'], KINDS)
  const apply = p['apply'] === null || p['apply'] === undefined ? null : rec(p['apply'])
  const participants = apply ? decodeParticipants(rec(apply['arguments'])['participants']) : []
  if (remedy === 'prune_participants' && p['permitted'] === true && participants.length === 0) throw new ApiError(502, 'The Hub sent an unreadable recovery answer')
  return {
    work_id: text(p['work_id'], 64),
    remedy,
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
    fenced: decodeFenced(e['fenced']),
    participants,
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
      items: [{ work_id: preview.work_id, operation_id: operationId, generation: preview.generation, expected_updated_seq: preview.expected_updated_seq, transition: preview.remedy, reason, ...(preview.participants.length ? { participants: preview.participants } : {}) }],
    }),
  )
  const items = Array.isArray(result['items']) ? result['items'] : []
  const item = rec(items[0])
  return { outcome: choice(item['outcome'], OUTCOMES), code: optional(item['code']), message: optional(item['message']), retry: text(item['retry']) }
}
