import { validWorkId } from './standup.ts'
import { validRunId } from './skills.ts'
import { ApiError } from './api.ts'
import { withTimeout } from './timeout.ts'

export interface PrContext {
  version: 1
  availability: 'available' | 'not_connected' | 'not_observed'
  repository?: string
  repository_id?: number
  pull_request?: { number: number; title: string; author: string; head_ref: string; head_sha: string | null; base_ref: string; base_sha: string | null; draft: boolean; url: string | null; observed_at_ms: number; basis: 'provider_observed' }
  provider_evidence?: ProviderEvidence
  expected_head?: string | null
  head_relation?: 'same' | 'changed' | 'unknown'
  pull_request_overlaps?: { number: number; title: string; author: string; head_sha: string | null; draft: boolean; observed_at_ms: number; paths: string[]; paths_omitted: number; files_partial: boolean }[]
  pull_request_overlaps_omitted?: number
  work_overlaps?: { work_id: string; title: string; agent: string; owner: string; updated_at: number; claim_status: string; blocker: string | null; next_action: string | null; paths: string[]; paths_partial: boolean }[]
  work_overlaps_omitted?: number
  workflows?: { run_id: string; head_sha: string | null; applicability: 'matches_observed_head' | 'earlier_or_other_head' | 'unknown'; current_gate: string | null; next_action: string; blocks_finish: boolean; updated_at: number; basis: 'self_reported' }[]
  workflows_omitted?: number
  coverage?: { files_partial: boolean; work_complete: boolean; work_observed_at: number; pull_requests_complete: false; workflows_complete: false; last_sync_partial: boolean | null; retained_pr_scan_saturated: boolean }
}

const SHA = /^([a-fA-F0-9]{40}|[a-fA-F0-9]{64})$/
export const validHead = (head: string | null | undefined): head is string => !!head && SHA.test(head)
const object = (v: unknown): Record<string, unknown> => {
  if (!v || typeof v !== 'object' || Array.isArray(v)) throw new Error('Invalid PR context')
  return v as Record<string, unknown>
}
const string = (v: unknown): string => { if (typeof v !== 'string') throw new Error('Invalid PR context text'); return v }
const number = (v: unknown): number => { if (typeof v !== 'number' || !Number.isSafeInteger(v) || v < 0) throw new Error('Invalid PR context number'); return v }
const boolean = (v: unknown): boolean => { if (typeof v !== 'boolean') throw new Error('Invalid PR context flag'); return v }
const nullable = (v: unknown): string | null => v === null ? null : string(v)
const head = (v: unknown): string | null => { const h = nullable(v); if (h !== null && !validHead(h)) throw new Error('Invalid PR head'); return h }
const list = (v: unknown): unknown[] => { if (!Array.isArray(v) || v.length > 8) throw new Error('Invalid PR context list'); return v }
const paths = (v: unknown) => list(v).map(string)

function decodeCoverage(value: unknown): NonNullable<PrContext['coverage']> {
  const c = object(value)
  if (c.pull_requests_complete !== false || c.workflows_complete !== false) throw new Error('Unsupported context coverage')
  return { files_partial: boolean(c.files_partial), work_complete: boolean(c.work_complete), work_observed_at: number(c.work_observed_at), pull_requests_complete: false, workflows_complete: false, last_sync_partial: c.last_sync_partial === null ? null : boolean(c.last_sync_partial), retained_pr_scan_saturated: boolean(c.retained_pr_scan_saturated) }
}

export function decodePrContext(value: unknown, repo: string, pr: number): PrContext {
  const o = object(value)
  if (o.version !== 1) throw new Error('Unsupported PR context version')
  const repositoryId = o.repository_id === undefined ? {} : { repository_id: positive(o.repository_id) }
  if (o.availability === 'not_connected') return { version: 1, availability: o.availability, ...repositoryId }
  if (o.availability === 'not_observed') return { version: 1, availability: o.availability, ...repositoryId, coverage: decodeCoverage(o.coverage) }
  if (o.availability !== 'available' || o.repository !== repo) throw new Error('PR context repository mismatch')
  const p = object(o.pull_request)
  if (p.number !== pr || p.basis !== 'provider_observed') throw new Error('PR context identity mismatch')
  if (!['same', 'changed', 'unknown'].includes(string(o.head_relation))) throw new Error('Invalid head relation')
  const currentHead = head(p.head_sha), expectedHead = head(o.expected_head)
  const relation = currentHead && expectedHead ? currentHead.toLowerCase() === expectedHead.toLowerCase() ? 'same' : 'changed' : 'unknown'
  if (o.head_relation !== relation) throw new Error('PR context head relation mismatch')
  return {
    version: 1, availability: 'available', repository: repo, ...repositoryId,
    pull_request: { number: pr, title: string(p.title), author: string(p.author), head_ref: string(p.head_ref), head_sha: head(p.head_sha), base_ref: string(p.base_ref), base_sha: head(p.base_sha), draft: boolean(p.draft), url: nullable(p.url), observed_at_ms: number(p.observed_at_ms), basis: 'provider_observed' },
    provider_evidence: decodeProviderEvidence(o.provider_evidence, pr, head(p.head_sha), head(p.base_sha)),
    expected_head: head(o.expected_head), head_relation: o.head_relation as PrContext['head_relation'],
    pull_request_overlaps: list(o.pull_request_overlaps).map((v) => { const r = object(v); return { number: number(r.number), title: string(r.title), author: string(r.author), head_sha: head(r.head_sha), draft: boolean(r.draft), observed_at_ms: number(r.observed_at_ms), paths: paths(r.paths), paths_omitted: number(r.paths_omitted), files_partial: boolean(r.files_partial) } }),
    pull_request_overlaps_omitted: number(o.pull_request_overlaps_omitted),
    work_overlaps: list(o.work_overlaps).map((v) => { const r = object(v); if (!validWorkId(string(r.work_id))) throw new Error('Invalid Work ID'); return { work_id: string(r.work_id), title: string(r.title), agent: string(r.agent), owner: string(r.owner), updated_at: number(r.updated_at), claim_status: string(r.claim_status), blocker: nullable(r.blocker), next_action: nullable(r.next_action), paths: paths(r.paths), paths_partial: boolean(r.paths_partial) } }),
    work_overlaps_omitted: number(o.work_overlaps_omitted),
    workflows: list(o.workflows).map((v) => {
      const r = object(v)
      if (!validRunId(string(r.run_id))) throw new Error('Invalid workflow ID')
      if (r.basis !== 'self_reported' || !['matches_observed_head', 'earlier_or_other_head', 'unknown'].includes(string(r.applicability))) throw new Error('Invalid workflow basis')
      return { run_id: string(r.run_id), head_sha: head(r.head_sha), applicability: r.applicability as 'matches_observed_head' | 'earlier_or_other_head' | 'unknown', current_gate: nullable(r.current_gate), next_action: string(r.next_action), blocks_finish: boolean(r.blocks_finish), updated_at: number(r.updated_at), basis: 'self_reported' }
    }),
    workflows_omitted: number(o.workflows_omitted),
    coverage: decodeCoverage(o.coverage),
  }
}

export async function fetchContextJson(path: string, token: string, signal: AbortSignal): Promise<unknown> {
  const response = await fetch(path, { signal: withTimeout(signal), cache: 'no-store', credentials: 'omit', headers: { Authorization: `Bearer ${token}` } })
  if (!response.ok) throw new ApiError(response.status, response.status === 403 ? 'Your repository access could not be confirmed. Check your sign-in and try again.' : `PR context could not load (HTTP ${response.status}).`)
  if (!response.body) throw new Error('PR context has no body')
  const reader = response.body.getReader()
  const chunks: Uint8Array[] = []
  let length = 0
  try {
    for (;;) {
      const { done, value } = await reader.read()
      if (done) break
      length += value.byteLength
      if (length > 48 * 1024) { await reader.cancel(); throw new Error('PR context is too large') }
      chunks.push(value)
    }
  } finally { reader.releaseLock() }
  const bytes = new Uint8Array(length)
  let offset = 0
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length }
  return JSON.parse(new TextDecoder().decode(bytes)) as unknown
}

export async function fetchPrContext(repo: string, pr: number, expectedHead: string | undefined, token: string, signal: AbortSignal): Promise<PrContext> {
  const query = new URLSearchParams({ number: String(pr) })
  if (validHead(expectedHead)) query.set('expected_head', expectedHead)
  const context = decodePrContext(await fetchContextJson(`/v1/hub/repos/${encodeURIComponent(repo)}/pr-context?${query}`, token, signal), repo, pr)
  if (validHead(expectedHead) && context.availability === 'available' && context.expected_head?.toLowerCase() !== expectedHead.toLowerCase()) throw new Error('PR context comparison mismatch')
  return context
}

export interface ProviderItem {
  provider_id: number
  commit_sha: string
  head_relation: 'same' | 'different'
  state: string
  name: string | null
  reviewer: string | null
  conclusion: string | null
  source_url: string | null
  provider_time: string | null
  provider_time_kind: 'submitted' | 'completed' | 'started' | 'updated' | null
  attempt: null
  tested_merge_sha: null
}
export interface ProviderCategory {
  availability: string
  view: 'latest_by_name' | 'latest_observed' | 'provider_list' | 'unavailable'
  complete: boolean
  items: ProviderItem[]
  omitted: number
  malformed: number
  pages_read: number
  unread_pages: number | null
}
export interface ProviderSnapshot {
  repository_id: number
  number: number
  head_sha: string
  base_sha: string
  observed_at: number
  permissions: { availability: string; pull_requests: string | null; checks: string | null; statuses: string | null }
  reviews: ProviderCategory
  check_runs: ProviderCategory
  commit_statuses: ProviderCategory
  retention_truncated: boolean
}
export type ProviderEvidence = { availability: 'observed'; snapshot: ProviderSnapshot } | { availability: 'unavailable' | 'not_collected' | 'identity_unavailable' | 'different_or_unknown_revision' }

export function safeContextUrl(raw: string | null): string | null {
  if (!raw || raw.length > 2048 || /[\p{Cc}\p{Cf}]/u.test(raw)) return null
  try { const url = new URL(raw); return url.protocol === 'https:' && !url.username && !url.password && !url.search ? url.href : null } catch { return null }
}
const positive = (v: unknown) => { const n = number(v); if (!n) throw new Error('Invalid provider identity'); return n }
const label = (v: unknown): string => { const s = string(v); if (!s || s.length > 512 || /[\p{Cc}\p{Cf}]/u.test(s)) throw new Error('Invalid provider label'); return s }
const providerTime = (v: unknown): string | null => {
  if (v === null || v === undefined) return null
  const value = string(v)
  if (value.length > 40 || !/^\d{4}-\d{2}-\d{2}T/.test(value) || !Number.isFinite(Date.parse(value))) throw new Error('Invalid provider time')
  return value
}
function decodeProviderCategory(value: unknown, kind: 'reviews' | 'check_runs' | 'commit_statuses', observedHead: string): ProviderCategory {
  const c = object(value)
  const availability = label(c.availability)
  const view = c.view === undefined ? 'unavailable' : label(c.view)
  if (!['latest_by_name', 'latest_observed', 'provider_list', 'unavailable'].includes(view) || (['available', 'incomplete'].includes(availability) && view !== (kind === 'check_runs' ? 'latest_by_name' : kind === 'reviews' ? 'latest_observed' : 'provider_list'))) throw new Error('Invalid provider view')
  const complete = boolean(c.complete)
  const omitted = number(c.omitted), malformed = number(c.malformed), pages_read = number(c.pages_read)
  const unread_pages = c.unread_pages === null ? null : number(c.unread_pages)
  if (pages_read > (kind === 'reviews' ? 2 : 1)) throw new Error('Invalid provider page bound')
  const items = list(c.items).map((value): ProviderItem => {
    const row = object(value), commit = head(row.commit_sha)
    if (!commit || row.attempt !== null || row.tested_merge_sha !== null) throw new Error('Invalid provider identity')
    const same = commit.toLowerCase() === observedHead.toLowerCase()
    if (row.head_relation !== (same ? 'same' : 'different') || (kind !== 'reviews' && !same)) throw new Error('Provider commit mismatch')
    const state = label(row.state)
    const states = kind === 'reviews' ? ['APPROVED', 'CHANGES_REQUESTED', 'COMMENTED', 'DISMISSED', 'PENDING'] : kind === 'check_runs' ? ['queued', 'in_progress', 'completed', 'waiting', 'requested', 'pending'] : ['pending', 'success', 'failure', 'error']
    if (!states.includes(state)) throw new Error('Invalid provider state')
    const conclusion = row.conclusion === undefined || row.conclusion === null ? null : label(row.conclusion)
    if (conclusion && !['success', 'failure', 'neutral', 'cancelled', 'skipped', 'timed_out', 'action_required', 'stale', 'startup_failure'].includes(conclusion)) throw new Error('Invalid provider conclusion')
    if (kind === 'check_runs' && state === 'completed' && !conclusion) throw new Error('Missing provider conclusion')
    const timeKind = kind === 'reviews' ? 'submitted' : kind === 'commit_statuses' ? 'updated' : row.provider_completed_at ? 'completed' : row.provider_started_at ? 'started' : null
    const time = timeKind === 'submitted' ? row.provider_submitted_at : timeKind === 'updated' ? row.provider_updated_at : timeKind === 'completed' ? row.provider_completed_at : timeKind === 'started' ? row.provider_started_at : null
    const timeValue = providerTime(time)
    if ((kind !== 'check_runs' || state === 'completed') && timeValue === null) throw new Error('Missing provider time')
    return { provider_id: positive(row.provider_id), commit_sha: commit, head_relation: same ? 'same' : 'different', state,
      name: row.name === undefined ? null : label(row.name), reviewer: row.reviewer === undefined || row.reviewer === null ? null : label(row.reviewer), conclusion, source_url: safeContextUrl(nullable(row.source_url)),
      provider_time: timeValue, provider_time_kind: timeKind, attempt: null, tested_merge_sha: null }
  })
  if (new Set(items.map((item) => item.provider_id)).size !== items.length) throw new Error('Duplicate provider identity')
  if (complete && (pages_read < 1 || availability !== 'available' || omitted > 0 || malformed > 0 || unread_pages !== 0)) throw new Error('Invalid provider coverage')
  if (!['available', 'incomplete'].includes(availability) && items.length) throw new Error('Unavailable provider items')
  return { availability, view: view as ProviderCategory['view'], complete, items, omitted, malformed, pages_read, unread_pages }
}
function decodeProviderEvidence(value: unknown, pr: number, observedHead: string | null, observedBase: string | null): ProviderEvidence {
  if (value === undefined) return { availability: 'not_collected' }
  const row = object(value)
  if (row.availability !== 'observed') {
    if (!['unavailable', 'not_collected', 'identity_unavailable', 'different_or_unknown_revision'].includes(string(row.availability))) throw new Error('Invalid provider availability')
    return { availability: row.availability as Exclude<ProviderEvidence['availability'], 'observed'> }
  }
  if (row.requester_access !== 'hub_token_scope' || row.live_requester_access !== 'not_revalidated') throw new Error('Invalid provider access basis')
  const s = object(row.snapshot), h = head(s.head_sha), b = head(s.base_sha)
  if (s.version !== 1 || s.provider !== 'github' || s.number !== pr || !h || !b || h.toLowerCase() !== observedHead?.toLowerCase() || b.toLowerCase() !== observedBase?.toLowerCase() || s.atomic_snapshot !== false || s.attempt_history !== false) throw new Error('Provider snapshot identity mismatch')
  if (new TextEncoder().encode(JSON.stringify(s)).byteLength > 16 * 1024) throw new Error('Provider snapshot too large')
  const permissions = object(s.permissions)
  return { availability: 'observed', snapshot: { repository_id: positive(s.repository_id), number: positive(s.number), head_sha: h, base_sha: b, observed_at: number(s.observed_at),
    permissions: { availability: label(permissions.availability), pull_requests: permissions.pull_requests === undefined ? null : label(permissions.pull_requests), checks: permissions.checks === undefined ? null : label(permissions.checks), statuses: permissions.statuses === undefined ? null : label(permissions.statuses) },
    reviews: decodeProviderCategory(s.reviews, 'reviews', h), check_runs: decodeProviderCategory(s.check_runs, 'check_runs', h), commit_statuses: decodeProviderCategory(s.commit_statuses, 'commit_statuses', h), retention_truncated: boolean(s.retention_truncated) } }
}
