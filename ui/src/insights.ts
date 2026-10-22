import { ApiError } from './api.ts'

const MAX_RESPONSE_BYTES = 4 * 1024 * 1024
const MAX_ITEMS = 20
const MAX_GROUPS = 500

export type UsageBasis = 'reported' | 'provider_result'
export type CostKind = 'reported_charge' | 'list_price_estimate'
export type WorkState = 'active' | 'blocked' | 'paused' | 'finished' | 'handover'
export type ClaimStatus = 'active' | 'stale' | 'unknown'

export interface InsightTiming {
  elapsed_seconds: number | null
  active_seconds: number | null
  blocked_seconds: number | null
  paused_seconds: number | null
  handover_seconds: number | null
  finished_seconds: number | null
  unknown_seconds: number | null
}

export interface InsightSignals { takeovers: number | null; handovers: number | null; reopened: number | null }

export interface TokenGroup {
  agent: string
  provider: string
  model: string
  usage_basis: UsageBasis
  observations: number
  input_tokens: number | null
  output_tokens: number | null
  input_known_observations: number
  output_known_observations: number
}

export interface CostGroup {
  kind: CostKind
  currency: string
  basis: string
  usage_basis: UsageBasis
  agent: string
  provider: string
  model: string
  amount_micros: number
  observations: number
}

export interface UsageSummary {
  observations: number
  coverage: 'unknown' | 'partial'
  input_tokens: number | null
  output_tokens: number | null
  input_known_observations: number
  output_known_observations: number
  missing_cost_observations: number
  reported_failed_attempts: number
  groups_complete: boolean
  cost_groups: CostGroup[]
  token_groups: TokenGroup[]
}

export interface RecordedCheck {
  head_sha: string
  state: string
  phase: string
  binding_digest: string | null
  observed_at: number
}

export interface PublicationEvidence {
  publication_id: string
  commit: string
  branch_state: string
  pr_number: number | null
  merge_commit: string | null
  observed_at: number
  checks: RecordedCheck[]
  association: string
}

export interface WorkInsight {
  version: number
  as_of: number
  work_id: string
  title: string
  state: WorkState
  claim_status: ClaimStatus
  created_at: number
  updated_at: number
  reported_finished: boolean
  history_complete: boolean
  history_retention_gap: boolean
  scanned_events: number
  timing: InsightTiming
  signals: InsightSignals
  usage: UsageSummary
  publications: PublicationEvidence[]
  publications_complete: boolean
  commits_truncated: boolean
  evidence_basis: string
  approval: string
}

export interface InsightPage {
  version: number
  as_of: number
  since: number
  until: number
  through: number
  next_after: string | null
  complete: boolean
  items: WorkInsight[]
}

type JsonObject = Record<string, unknown>
const object = (value: unknown, label: string): JsonObject => {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error(`Invalid ${label}`)
  return value as JsonObject
}
const string = (value: unknown, label: string): string => {
  if (typeof value !== 'string' || value.length > 1024) throw new Error(`Invalid ${label}`)
  return value
}
const integer = (value: unknown, label: string): number => {
  if (!Number.isSafeInteger(value) || (value as number) < 0) throw new Error(`Invalid ${label}`)
  return value as number
}
const nullableInteger = (value: unknown, label: string): number | null => value === null ? null : integer(value, label)
const nullableString = (value: unknown, label: string): string | null => value === null ? null : string(value, label)
const boolean = (value: unknown, label: string): boolean => {
  if (typeof value !== 'boolean') throw new Error(`Invalid ${label}`)
  return value
}
const array = (value: unknown, label: string, maximum: number): unknown[] => {
  if (!Array.isArray(value) || value.length > maximum) throw new Error(`Invalid ${label}`)
  return value
}
const oneOf = <T extends string>(value: unknown, values: readonly T[], label: string): T => {
  if (typeof value !== 'string' || !values.includes(value as T)) throw new Error(`Invalid ${label}`)
  return value as T
}

const usageBasis = (value: unknown) => oneOf(value, ['reported', 'provider_result'] as const, 'usage basis')
const identity = (value: unknown, label: string) => {
  const parsed = string(value, label)
  if (parsed.length === 0 || parsed.length > 255) throw new Error(`Invalid ${label}`)
  return parsed
}
const version = (value: unknown): number => {
  if (value !== 1) throw new Error('Unsupported work insight version')
  return 1
}
const workId = (value: unknown): string => {
  const parsed = identity(value, 'work id')
  if (!/^wrk_[0-9a-f]{32}$/.test(parsed)) throw new Error('Invalid work id')
  return parsed
}

function decodeTokenGroup(value: unknown): TokenGroup {
  const v = object(value, 'token group')
  return {
    agent: identity(v['agent'], 'agent'), provider: identity(v['provider'], 'provider'), model: identity(v['model'], 'model'),
    usage_basis: usageBasis(v['usage_basis']), observations: integer(v['observations'], 'observations'),
    input_tokens: nullableInteger(v['input_tokens'], 'input tokens'), output_tokens: nullableInteger(v['output_tokens'], 'output tokens'),
    input_known_observations: integer(v['input_known_observations'], 'known input observations'),
    output_known_observations: integer(v['output_known_observations'], 'known output observations'),
  }
}

function decodeCostGroup(value: unknown): CostGroup {
  const v = object(value, 'cost group')
  return {
    kind: oneOf(v['kind'], ['reported_charge', 'list_price_estimate'] as const, 'cost kind'),
    currency: identity(v['currency'], 'currency'), basis: identity(v['basis'], 'cost basis'), usage_basis: usageBasis(v['usage_basis']),
    agent: identity(v['agent'], 'agent'), provider: identity(v['provider'], 'provider'), model: identity(v['model'], 'model'),
    amount_micros: integer(v['amount_micros'], 'cost'), observations: integer(v['observations'], 'observations'),
  }
}

function decodeUsage(value: unknown): UsageSummary {
  const v = object(value, 'usage')
  return {
    observations: integer(v['observations'], 'usage observations'), coverage: oneOf(v['coverage'], ['unknown', 'partial'] as const, 'coverage'),
    input_tokens: nullableInteger(v['input_tokens'], 'input tokens'), output_tokens: nullableInteger(v['output_tokens'], 'output tokens'),
    input_known_observations: integer(v['input_known_observations'], 'known input observations'),
    output_known_observations: integer(v['output_known_observations'], 'known output observations'),
    missing_cost_observations: integer(v['missing_cost_observations'], 'missing cost observations'),
    reported_failed_attempts: integer(v['reported_failed_attempts'], 'reported failed attempts'),
    groups_complete: boolean(v['groups_complete'], 'usage group completeness'),
    cost_groups: array(v['cost_groups'], 'cost groups', MAX_GROUPS).map(decodeCostGroup),
    token_groups: array(v['token_groups'], 'token groups', MAX_GROUPS).map(decodeTokenGroup),
  }
}

function decodeCheck(value: unknown): RecordedCheck {
  const v = object(value, 'recorded check')
  return { head_sha: string(v['head_sha'], 'check head'), state: identity(v['state'], 'check state'), phase: identity(v['phase'], 'check phase'), binding_digest: nullableString(v['binding_digest'], 'binding digest'), observed_at: integer(v['observed_at'], 'check time') }
}

function decodePublication(value: unknown): PublicationEvidence {
  const v = object(value, 'publication')
  return {
    publication_id: identity(v['publication_id'], 'publication id'), commit: identity(v['commit'], 'commit'), branch_state: identity(v['branch_state'], 'branch state'),
    pr_number: nullableInteger(v['pr_number'], 'pull request number'), merge_commit: nullableString(v['merge_commit'], 'merge commit'), observed_at: integer(v['observed_at'], 'publication time'),
    checks: array(v['checks'], 'recorded checks', 500).map(decodeCheck), association: identity(v['association'], 'association'),
  }
}

export function decodeWorkInsight(value: unknown): WorkInsight {
  const v = object(value, 'work insight')
  const timing = object(v['timing'], 'timing')
  const signals = object(v['signals'], 'signals')
  return {
    version: version(v['version']), as_of: integer(v['as_of'], 'as of'), work_id: workId(v['work_id']), title: string(v['title'], 'title'),
    state: oneOf(v['state'], ['active', 'blocked', 'paused', 'finished', 'handover'] as const, 'work state'),
    claim_status: oneOf(v['claim_status'], ['active', 'stale', 'unknown'] as const, 'claim status'),
    created_at: integer(v['created_at'], 'created time'), updated_at: integer(v['updated_at'], 'updated time'), reported_finished: boolean(v['reported_finished'], 'reported finished'),
    history_complete: boolean(v['history_complete'], 'history completeness'), history_retention_gap: boolean(v['history_retention_gap'], 'history retention gap'), scanned_events: integer(v['scanned_events'], 'scanned events'),
    timing: {
      elapsed_seconds: nullableInteger(timing['elapsed_seconds'], 'elapsed seconds'), active_seconds: nullableInteger(timing['active_seconds'], 'active seconds'),
      blocked_seconds: nullableInteger(timing['blocked_seconds'], 'blocked seconds'), paused_seconds: nullableInteger(timing['paused_seconds'], 'paused seconds'),
      handover_seconds: nullableInteger(timing['handover_seconds'], 'handover seconds'), finished_seconds: nullableInteger(timing['finished_seconds'], 'finished seconds'),
      unknown_seconds: nullableInteger(timing['unknown_seconds'], 'unknown seconds'),
    },
    signals: { takeovers: nullableInteger(signals['takeovers'], 'takeovers'), handovers: nullableInteger(signals['handovers'], 'handovers'), reopened: nullableInteger(signals['reopened'], 'reopened') },
    usage: decodeUsage(v['usage']), publications: array(v['publications'], 'publications', 16).map(decodePublication),
    publications_complete: boolean(v['publications_complete'], 'publication completeness'), commits_truncated: boolean(v['commits_truncated'], 'commit truncation'),
    evidence_basis: string(v['evidence_basis'], 'evidence basis'), approval: string(v['approval'], 'approval'),
  }
}

export function decodeInsightPage(value: unknown): InsightPage {
  const v = object(value, 'insight page')
  return {
    version: version(v['version']), as_of: integer(v['as_of'], 'as of'), since: integer(v['since'], 'since'), until: integer(v['until'], 'until'), through: integer(v['through'], 'through'),
    next_after: nullableString(v['next_after'], 'next cursor'), complete: boolean(v['complete'], 'completeness'),
    items: array(v['items'], 'insight items', MAX_ITEMS).map(decodeWorkInsight),
  }
}

async function getJson(path: string, token: string, signal: AbortSignal): Promise<unknown> {
  const response = await fetch(path, { signal, cache: 'no-store', credentials: 'omit', headers: { Authorization: `Bearer ${token}` } })
  if (!response.ok) throw new ApiError(response.status, `HTTP ${response.status}`)
  const declared = Number(response.headers.get('content-length'))
  if (Number.isFinite(declared) && declared > MAX_RESPONSE_BYTES) {
    await response.body?.cancel()
    throw new Error('Work insights response is too large')
  }
  if (!response.body) throw new Error('Work insights response has no body')
  const reader = response.body.getReader()
  const chunks: Uint8Array[] = []
  let total = 0
  try {
    while (true) {
      const next = await reader.read()
      if (next.done) break
      total += next.value.byteLength
      if (total > MAX_RESPONSE_BYTES) {
        await reader.cancel()
        throw new Error('Work insights response is too large')
      }
      chunks.push(next.value)
    }
  } finally {
    reader.releaseLock()
  }
  const bytes = new Uint8Array(total)
  let offset = 0
  for (const chunk of chunks) {
    bytes.set(chunk, offset)
    offset += chunk.byteLength
  }
  return JSON.parse(new TextDecoder().decode(bytes)) as unknown
}

export async function fetchInsightPage(token: string, signal: AbortSignal, query: { since?: number; until?: number; through?: number; after?: string } = {}): Promise<InsightPage> {
  const params = new URLSearchParams({ limit: '20' })
  if (query.since !== undefined) params.set('since', String(query.since))
  if (query.until !== undefined) params.set('until', String(query.until))
  if (query.through !== undefined) params.set('through', String(query.through))
  if (query.after !== undefined) params.set('after', query.after)
  return decodeInsightPage(await getJson(`/v1/hub/standup/insights?${params}`, token, signal))
}

export async function fetchWorkInsight(id: string, token: string, signal: AbortSignal): Promise<WorkInsight> {
  return decodeWorkInsight(await getJson(`/v1/hub/standup/work/${encodeURIComponent(id)}/insights`, token, signal))
}
