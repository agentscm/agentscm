// The Hub owns this versioned projection and all accounting. The browser only formats its fields.
import { ApiError } from './api.ts'
import { clean } from './publications.ts'
import { withTimeout } from './timeout.ts'

export interface ValueEvidence {
  id: string; kind: string; state: string; repository: string | null; work_id: string | null
  source: string | null; recipe: string | null; policy: string | null; basis: string; assurance: string; availability: string; applicability: string; observed_at: number | null; detail: string
}
export interface ValueMetrics {
  version: 1; method: 'agentscm.observed.v1'; assessment_id: string; as_of: number; since: number; until: number
  scope: { provider: string; host: string; repository: string; repository_id: number | null }[]
  context: { availability: string; returned: number | null; acknowledged: number | null; applied: number | null }
  issues: { availability: string; unresolved: number | null; resolved: number | null; dismissed: number | null; false_trigger: number | null }
  followups: { availability: string; current: number | null; pending: number | null; failed: number | null; stale: number | null; cancelled: number | null; unknown: number | null }
  resources: {
    input_tokens: string | null; output_tokens: string | null
    groups: { provider: string; model: string; category: string; usage_basis: string; input_tokens: string | null; output_tokens: string | null; cache_read_input_tokens: string | null; cache_creation_input_tokens: string | null; requests: number | null }[]
    costs: { provider: string; model: string; kind: string; currency: string; basis: string; amount_micros: string; requests: number | null }[]
    missing_cost_requests: number | null; unallocated_usage_records: number | null; unallocated_overhead: string | null
  }
  coverage: { state: string; eligible_works: number | null; observed_works: number | null; complete: boolean; gaps: string[]; omitted: number | null }
  evidence: ValueEvidence[]; next_after: string | null; merge_observations: number | null
}
export interface ValueQuery { since: number; until: number; repository?: string; after?: string }
const rec = (v: unknown): Record<string, unknown> => v && typeof v === 'object' && !Array.isArray(v) ? v as Record<string, unknown> : {}
const arr = (v: unknown): unknown[] => Array.isArray(v) ? v : []
const txt = (v: unknown, max = 500) => clean(v, max)
const opt = (v: unknown) => typeof v === 'string' && v !== '' ? txt(v) : null
const count = (v: unknown) => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0 ? v : null
const decimal = (v: unknown) => typeof v === 'string' && /^\d{1,100}$/.test(v) ? v : null
const REPO = /^[A-Za-z0-9._-]+\/[A-Za-z0-9._/-]+$/

/** Unknown and malformed measurements stay unknown, rather than becoming reassuring zeros. */
export function decodeValueMetrics(raw: unknown): ValueMetrics {
  const j = rec(raw)
  if (j['version'] !== 1 || j['method'] !== 'agentscm.observed.v1') throw new Error('Unsupported contribution projection')
  const asOf = count(j['as_of']), since = count(j['since']), until = count(j['until'])
  if (asOf === null || since === null || until === null) throw new Error('The Hub returned an unreadable contribution window')
  if (arr(j['scope']).length > 200 || arr(j['evidence']).length > 50) throw new Error('The contribution projection exceeds its bounds')
  const c = rec(j['context']), i = rec(j['issues']), f = rec(j['followups']), r = rec(j['resources']), cv = rec(j['coverage'])
  if (arr(r['costs']).length > 200 || arr(r['groups']).length > 200 || arr(cv['gaps']).length > 100) throw new Error('The contribution projection exceeds its bounds')
  return {
    version: 1, method: 'agentscm.observed.v1', assessment_id: txt(j['assessment_id']), as_of: asOf, since, until,
    scope: arr(j['scope']).slice(0, 200).map(rec).filter(s => typeof s['repository'] === 'string' && REPO.test(s['repository'])).map(s => ({ provider: txt(s['provider']), host: txt(s['host']), repository: txt(s['repository']), repository_id: count(s['repository_id']) })),
    context: { availability: txt(c['availability']) || 'unknown', returned: count(c['returned']), acknowledged: count(c['acknowledged']), applied: count(c['applied']) },
    issues: { availability: txt(i['availability']) || 'unknown', unresolved: count(i['unresolved']), resolved: count(i['resolved']), dismissed: count(i['dismissed']), false_trigger: count(i['false_trigger']) },
    followups: { availability: txt(f['availability']) || 'unknown', current: count(f['current']), pending: count(f['pending']), failed: count(f['failed']), stale: count(f['stale']), cancelled: count(f['cancelled']), unknown: count(f['unknown']) },
    resources: {
      input_tokens: decimal(r['input_tokens']), output_tokens: decimal(r['output_tokens']),
      groups: arr(r['groups']).slice(0, 200).map(rec).map(g => ({ provider: txt(g['provider']), model: txt(g['model']), category: txt(g['category']), usage_basis: txt(g['usage_basis']), input_tokens: decimal(g['input_tokens']), output_tokens: decimal(g['output_tokens']), cache_read_input_tokens: decimal(g['cache_read_input_tokens']), cache_creation_input_tokens: decimal(g['cache_creation_input_tokens']), requests: count(g['requests']) })),
      costs: arr(r['costs']).slice(0, 200).map(rec).filter(c => decimal(c['amount_micros']) !== null).map(c => ({ provider: txt(c['provider']), model: txt(c['model']), kind: txt(c['kind']), currency: txt(c['currency']), basis: txt(c['basis']), amount_micros: decimal(c['amount_micros'])!, requests: count(c['requests']) })),
      missing_cost_requests: count(r['missing_cost_requests']), unallocated_usage_records: count(r['unallocated_usage_records']), unallocated_overhead: opt(r['unallocated_overhead']),
    },
    coverage: { state: cv['state'] === 'partial' ? 'partial' : 'unknown', eligible_works: count(cv['eligible_works']), observed_works: count(cv['observed_works']), complete: cv['complete'] === true, gaps: arr(cv['gaps']).slice(0, 100).map(g => txt(g)).filter(Boolean), omitted: count(cv['omitted']) },
    evidence: arr(j['evidence']).slice(0, 100).map(rec).map(e => ({ id: txt(e['id']), kind: txt(e['kind']), state: txt(e['state']), repository: typeof e['repository'] === 'string' && REPO.test(e['repository']) ? e['repository'] : null, work_id: opt(e['work_id']), source: opt(e['source']), recipe: opt(e['recipe']), policy: opt(e['policy']), basis: txt(e['basis']) || 'unknown', assurance: txt(e['assurance']) || 'unknown', availability: txt(e['availability']) || 'unknown', applicability: txt(e['applicability']) || 'unknown', observed_at: count(e['observed_at']), detail: txt(e['detail'], 2000) })),
    next_after: opt(j['next_after']), merge_observations: count(j['merge_observations']),
  }
}

/** One permission-scoped aggregate; never mint tokens or fetch each repository's history. */
export async function fetchValueMetrics(query: ValueQuery, signal: AbortSignal): Promise<ValueMetrics> {
  const params = new URLSearchParams({ since: String(query.since), until: String(query.until), limit: '20' })
  if (query.repository) params.set('repository', query.repository)
  if (query.after) params.set('after', query.after)
  const res = await fetch(`/v1/hub/session/value-metrics?${params}`, { signal: withTimeout(signal), cache: 'no-store', credentials: 'same-origin' })
  if (!res.ok) throw new ApiError(res.status, `HTTP ${res.status}`)
  // Bounded independently of Content-Length; a missing header cannot bypass the limit.
  if (!res.body) throw new Error('The Hub returned no contribution data')
  const reader = res.body.getReader()
  const chunks: Uint8Array[] = []
  let bytes = 0
  try {
    for (;;) {
      const next = await reader.read()
      if (next.done) break
      bytes += next.value.byteLength
      if (bytes > 1024 * 1024) { await reader.cancel(); throw new Error('The contribution projection is too large') }
      chunks.push(next.value)
    }
  } finally { reader.releaseLock() }
  const all = new Uint8Array(bytes)
  let offset = 0
  for (const chunk of chunks) { all.set(chunk, offset); offset += chunk.byteLength }
  return decodeValueMetrics(JSON.parse(new TextDecoder().decode(all)))
}
