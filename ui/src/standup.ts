import { ApiError } from './api.ts'

const MAX_RESPONSE_BYTES = 4 * 1024 * 1024
const MAX_PAGE = 100
const MAX_SCOPE = 256
const MAX_RELATED = 100
const MAX_PARTICIPANTS = 100
const MAX_REFS = 100
const MAX_TEXT_BYTES = 500
const MAX_LABEL_BYTES = 255
const textEncoder = new TextEncoder()

export type WorkRole = 'lead' | 'helper' | 'reviewer'
export type WorkState = 'active' | 'blocked' | 'paused' | 'finished' | 'handover'
export type ClaimStatus = 'active' | 'stale' | 'unknown'
export type CheckpointKind = 'progress' | 'decision' | 'blocker' | 'pause' | 'finish' | 'handover'
export type WorkEventKind = 'work_started' | 'work_joined' | 'work_takeover' | 'work_checkpoint'

export type WorkSource =
  | { kind: 'native'; request_key: string }
  | { kind: 'forge'; provider: string; host: string; repository: string; issue: string }

export type WorkSourceRef =
  | { kind: 'repository_path'; path: string; line: number | null }
  | { kind: 'commit'; oid: string; path: string | null }
  | { kind: 'work'; work_id: string }

export interface WorkParticipant {
  owner: string
  login: string
  token: string
  agent: string
  session: string
  role: WorkRole
}

export interface WorkCheckpoint {
  kind: CheckpointKind
  summary: string
  next_action: string | null
  source_refs: WorkSourceRef[]
  at: number
  token: string
  agent: string
  session: string
  generation: number
}

export interface WorkItem {
  id: string
  repository: string
  owner: string
  owner_login: string
  source: WorkSource
  title: string
  state: WorkState
  claim_status: ClaimStatus
  last_checkin_at: number
  generation: number
  lead_token: string
  lead_agent: string
  lead_session: string
  participants: WorkParticipant[]
  scope: string[]
  branch: string | null
  worktree: string | null
  next_action: string | null
  current: WorkCheckpoint | null
  blocker: WorkCheckpoint | null
  depends_on: string[]
  related_work: string[]
  created_at: number
  updated_at: number
  created_seq: number
  updated_seq: number
}

export function currentLeadParticipant(item: WorkItem): WorkParticipant {
  const lead = item.participants.find((participant) =>
    participant.role === 'lead'
    && participant.token === item.lead_token
    && participant.session === item.lead_session
    && participant.agent === item.lead_agent)
  return lead ?? fail('work.lead')
}

export interface BriefingEvent {
  seq: number
  at: number
  kind: WorkEventKind
  work_id: string
  item: WorkItem
}

export interface StandupBriefing {
  delivery: string | null
  from: number
  through: number
  after: number
  work_after: string | null
  next_after: number
  next_work_after: string | null
  next_work_done: boolean
  items: WorkItem[]
  events: BriefingEvent[]
  retention_gap: boolean
  complete: boolean
}

export interface BriefingQuery {
  after?: number
  workAfter?: string
  workDone?: boolean
  through?: number
  limit?: number
}

export class InvalidStandupResponse extends Error {}

const fail = (field: string): never => {
  throw new InvalidStandupResponse(`Invalid stand-up response (${field})`)
}
const record = (value: unknown, field: string): Record<string, unknown> =>
  value !== null && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, unknown>) : fail(field)
const boundedText = (value: unknown, field: string, maxBytes = MAX_TEXT_BYTES): string => {
  const result = typeof value === 'string' ? value : fail(field)
  if (textEncoder.encode(result).byteLength > maxBytes) fail(field)
  return result
}
const nullableText = (value: unknown, field: string, maxBytes = MAX_TEXT_BYTES): string | null =>
  value === null ? null : boundedText(value, field, maxBytes)
const safeInteger = (value: unknown, field: string): number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : fail(field)
const positiveInteger = (value: unknown, field: string): number => {
  const result = safeInteger(value, field)
  return result > 0 ? result : fail(field)
}
const bool = (value: unknown, field: string): boolean => typeof value === 'boolean' ? value : fail(field)
const list = (value: unknown, max: number, field: string): unknown[] =>
  Array.isArray(value) && value.length <= max ? value : fail(field)
const enumValue = <T extends string>(value: unknown, values: ReadonlySet<T>, field: string): T =>
  typeof value === 'string' && values.has(value as T) ? value as T : fail(field)

const WORK_ID = /^wrk_[0-9a-f]{32}$/
const DELIVERY_ID = /^dlv_[0-9a-f]{32}$/
const COMMIT_ID = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/i
const ROLES = new Set<WorkRole>(['lead', 'helper', 'reviewer'])
const STATES = new Set<WorkState>(['active', 'blocked', 'paused', 'finished', 'handover'])
const CLAIM_STATUSES = new Set<ClaimStatus>(['active', 'stale', 'unknown'])
const CHECKPOINT_KINDS = new Set<CheckpointKind>(['progress', 'decision', 'blocker', 'pause', 'finish', 'handover'])
const EVENT_KINDS = new Set<WorkEventKind>(['work_started', 'work_joined', 'work_takeover', 'work_checkpoint'])

export const validWorkId = (value: string): boolean => WORK_ID.test(value)

const workId = (value: unknown, field: string): string => {
  const result = boundedText(value, field, 36)
  return validWorkId(result) ? result : fail(field)
}

const label = (value: unknown, field: string): string => boundedText(value, field, MAX_LABEL_BYTES)

const repositoryPath = (value: unknown, field: string): string => {
  const path = boundedText(value, field, 1024)
  const key = path.endsWith('/**') ? path.slice(0, -3) : path.endsWith('/') ? path.slice(0, -1) : path
  const parts = key.split('/')
  const drive = key.length >= 2 && /^[A-Za-z]:/.test(key)
  const control = [...path].some((character) => character.charCodeAt(0) < 32 || character.charCodeAt(0) === 127)
  if (!key || path.startsWith('/') || drive || path.includes('\\') || control || key.includes('*') || parts.some((part) => part === '' || part === '.' || part === '..')) fail(field)
  return path
}

const validRepositoryId = (value: string): boolean => {
  const parts = value.split('/')
  return parts.length >= 2 && parts.every((part) => part !== '' && part !== '.' && part !== '..' && /^[A-Za-z0-9_.-]+$/.test(part))
}

function source(value: unknown): WorkSource {
  const item = record(value, 'work.source')
  if (item['kind'] === 'native') return { kind: 'native', request_key: label(item['request_key'], 'work.source.request_key') }
  if (item['kind'] === 'forge') {
    return {
      kind: 'forge',
      provider: label(item['provider'], 'work.source.provider'),
      host: label(item['host'], 'work.source.host'),
      repository: label(item['repository'], 'work.source.repository'),
      issue: label(item['issue'], 'work.source.issue'),
    }
  }
  return fail('work.source.kind')
}

function sourceRef(value: unknown): WorkSourceRef {
  const item = record(value, 'checkpoint.source_ref')
  if (item['kind'] === 'repository_path') {
    return {
      kind: 'repository_path',
      path: repositoryPath(item['path'], 'checkpoint.source_ref.path'),
      line: item['line'] === null ? null : positiveInteger(item['line'], 'checkpoint.source_ref.line'),
    }
  }
  if (item['kind'] === 'commit') {
    const oid = boundedText(item['oid'], 'checkpoint.source_ref.oid', 64)
    if (!COMMIT_ID.test(oid)) fail('checkpoint.source_ref.oid')
    return {
      kind: 'commit',
      oid,
      path: item['path'] === null ? null : repositoryPath(item['path'], 'checkpoint.source_ref.path'),
    }
  }
  if (item['kind'] === 'work') return { kind: 'work', work_id: workId(item['work_id'], 'checkpoint.source_ref.work_id') }
  return fail('checkpoint.source_ref.kind')
}

function participant(value: unknown): WorkParticipant {
  const item = record(value, 'work.participant')
  return {
    owner: label(item['owner'], 'work.participant.owner'),
    login: label(item['login'], 'work.participant.login'),
    token: label(item['token'], 'work.participant.token'),
    agent: label(item['agent'], 'work.participant.agent'),
    session: label(item['session'], 'work.participant.session'),
    role: enumValue(item['role'], ROLES, 'work.participant.role'),
  }
}

function checkpoint(value: unknown): WorkCheckpoint | null {
  if (value === null) return null
  const item = record(value, 'work.current')
  return {
    kind: enumValue(item['kind'], CHECKPOINT_KINDS, 'work.current.kind'),
    summary: boundedText(item['summary'], 'work.current.summary'),
    next_action: nullableText(item['next_action'], 'work.current.next_action'),
    source_refs: list(item['source_refs'], MAX_REFS, 'work.current.source_refs').map(sourceRef),
    at: safeInteger(item['at'], 'work.current.at'),
    token: label(item['token'], 'work.current.token'),
    agent: label(item['agent'], 'work.current.agent'),
    session: label(item['session'], 'work.current.session'),
    generation: safeInteger(item['generation'], 'work.current.generation'),
  }
}

export function decodeWorkItem(value: unknown, expectedId?: string): WorkItem {
  const item = record(value, 'work')
  const id = workId(item['id'], 'work.id')
  if (expectedId !== undefined && id !== expectedId) fail('work.id')
  const dependencies = list(item['depends_on'], MAX_RELATED, 'work.depends_on').map((entry) => workId(entry, 'work.depends_on.item'))
  const related = list(item['related_work'], MAX_RELATED, 'work.related_work').map((entry) => workId(entry, 'work.related_work.item'))
  if (new Set([...dependencies, ...related]).size !== dependencies.length + related.length) fail('work.relations')
  const decoded: WorkItem = {
    id,
    repository: label(item['repository'], 'work.repository'),
    owner: label(item['owner'], 'work.owner'),
    owner_login: label(item['owner_login'], 'work.owner_login'),
    source: source(item['source']),
    title: boundedText(item['title'], 'work.title'),
    state: enumValue(item['state'], STATES, 'work.state'),
    claim_status: enumValue(item['claim_status'], CLAIM_STATUSES, 'work.claim_status'),
    last_checkin_at: safeInteger(item['last_checkin_at'], 'work.last_checkin_at'),
    generation: safeInteger(item['generation'], 'work.generation'),
    lead_token: label(item['lead_token'], 'work.lead_token'),
    lead_agent: label(item['lead_agent'], 'work.lead_agent'),
    lead_session: label(item['lead_session'], 'work.lead_session'),
    participants: list(item['participants'], MAX_PARTICIPANTS, 'work.participants').map(participant),
    scope: list(item['scope'], MAX_SCOPE, 'work.scope').map((entry) => repositoryPath(entry, 'work.scope.item')),
    branch: nullableText(item['branch'], 'work.branch', MAX_LABEL_BYTES),
    worktree: nullableText(item['worktree'], 'work.worktree', MAX_LABEL_BYTES),
    next_action: nullableText(item['next_action'], 'work.next_action'),
    current: checkpoint(item['current']),
    blocker: checkpoint(item['blocker']),
    depends_on: dependencies,
    related_work: related,
    created_at: safeInteger(item['created_at'], 'work.created_at'),
    updated_at: safeInteger(item['updated_at'], 'work.updated_at'),
    created_seq: safeInteger(item['created_seq'], 'work.created_seq'),
    updated_seq: safeInteger(item['updated_seq'], 'work.updated_seq'),
  }
  const leads = decoded.participants.filter((participant) => participant.role === 'lead')
  if (leads.length !== 1) fail('work.participants.lead')
  currentLeadParticipant(decoded)
  return decoded
}

function event(value: unknown): BriefingEvent {
  const item = record(value, 'briefing.event')
  const id = workId(item['work_id'], 'briefing.event.work_id')
  return {
    seq: safeInteger(item['seq'], 'briefing.event.seq'),
    at: safeInteger(item['at'], 'briefing.event.at'),
    kind: enumValue(item['kind'], EVENT_KINDS, 'briefing.event.kind'),
    work_id: id,
    item: decodeWorkItem(item['item'], id),
  }
}

export function decodeBriefing(value: unknown): StandupBriefing {
  const item = record(value, 'briefing')
  const delivery = item['delivery'] === null ? null : boundedText(item['delivery'], 'briefing.delivery', 36)
  if (delivery !== null && !DELIVERY_ID.test(delivery)) fail('briefing.delivery')
  const through = safeInteger(item['through'], 'briefing.through')
  const after = safeInteger(item['after'], 'briefing.after')
  const nextAfter = safeInteger(item['next_after'], 'briefing.next_after')
  const from = safeInteger(item['from'], 'briefing.from')
  if (after > nextAfter || nextAfter > through || from > through) fail('briefing.sequence')
  const workAfter = item['work_after'] === null ? null : workId(item['work_after'], 'briefing.work_after')
  const nextWorkAfter = item['next_work_after'] === null ? null : workId(item['next_work_after'], 'briefing.next_work_after')
  const items = list(item['items'], MAX_PAGE, 'briefing.items').map((entry) => decodeWorkItem(entry))
  const events = list(item['events'], MAX_PAGE, 'briefing.events').map(event)
  if (new Set(items.map((work) => work.id)).size !== items.length) fail('briefing.items.id')
  for (let index = 1; index < events.length; index++) {
    if (events[index - 1]!.seq >= events[index]!.seq) fail('briefing.events.order')
  }
  if (events.some((entry) => entry.seq <= after || entry.seq > through)) fail('briefing.events.seq')
  return {
    delivery,
    from,
    through,
    after,
    work_after: workAfter,
    next_after: nextAfter,
    next_work_after: nextWorkAfter,
    next_work_done: bool(item['next_work_done'], 'briefing.next_work_done'),
    items,
    events,
    retention_gap: bool(item['retention_gap'], 'briefing.retention_gap'),
    complete: bool(item['complete'], 'briefing.complete'),
  }
}

export function briefingPath(query: BriefingQuery = {}): string {
  const limit = query.limit ?? 50
  if (!Number.isSafeInteger(limit) || limit < 1 || limit > MAX_PAGE) throw new RangeError('Stand-up page size must be between 1 and 100')
  const params = new URLSearchParams()
  if (query.after !== undefined) params.set('after', String(safeInteger(query.after, 'query.after')))
  if (query.workAfter !== undefined) params.set('work_after', validWorkId(query.workAfter) ? query.workAfter : fail('query.work_after'))
  if (query.workDone !== undefined) {
    if (typeof query.workDone !== 'boolean') fail('query.work_done')
    params.set('work_done', String(query.workDone))
  }
  if (query.through !== undefined) params.set('through', String(safeInteger(query.through, 'query.through')))
  params.set('limit', String(limit))
  return `/v1/hub/standup/briefing?${params.toString()}`
}

export const workPath = (id: string): string => validWorkId(id)
  ? `/v1/hub/standup/work/${encodeURIComponent(id)}`
  : fail('work.id')

export const workHref = (id: string): string => validWorkId(id)
  ? `#coordination/work/${encodeURIComponent(id)}`
  : fail('work.id')

export function workIdFromHash(hash: string): string | null {
  const match = /^#coordination\/work\/([^/]+)$/.exec(hash)
  if (!match) return null
  try {
    const id = decodeURIComponent(match[1]!)
    return validWorkId(id) ? id : null
  } catch {
    return null
  }
}

export function safeForgeIssueHref(source: WorkSource): string | null {
  if (source.kind !== 'forge' || !validRepositoryId(source.repository) || !/^[1-9][0-9]*$/.test(source.issue)) return null
  if (source.provider === 'github' && source.host === 'github.com') return `https://github.com/${source.repository}/issues/${source.issue}`
  if (source.provider === 'gitlab' && source.host === 'gitlab.com') return `https://gitlab.com/${source.repository}/-/issues/${source.issue}`
  return null
}

export function safeCommitHref(source: WorkSource, oid: string): string | null {
  if (source.kind !== 'forge' || !COMMIT_ID.test(oid) || !validRepositoryId(source.repository)) return null
  if (source.provider === 'github' && source.host === 'github.com') return `https://github.com/${source.repository}/commit/${oid}`
  if (source.provider === 'gitlab' && source.host === 'gitlab.com') return `https://gitlab.com/${source.repository}/-/commit/${oid}`
  return null
}

async function getBoundedJson(path: string, token: string, signal: AbortSignal): Promise<unknown> {
  const response = await fetch(path, {
    signal,
    cache: 'no-store',
    credentials: 'omit',
    headers: { Authorization: `Bearer ${token}` },
  })
  if (!response.ok) throw new ApiError(response.status, `HTTP ${response.status}`)
  const declared = Number(response.headers.get('content-length'))
  if (Number.isFinite(declared) && declared > MAX_RESPONSE_BYTES) {
    await response.body?.cancel()
    throw new InvalidStandupResponse('Stand-up response is too large')
  }
  if (!response.body) throw new InvalidStandupResponse('Stand-up response has no body')
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
        throw new InvalidStandupResponse('Stand-up response is too large')
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
  try {
    return JSON.parse(new TextDecoder().decode(bytes)) as unknown
  } catch {
    throw new InvalidStandupResponse('Stand-up response is not valid JSON')
  }
}

export async function fetchBriefing(token: string, signal: AbortSignal, query: BriefingQuery = {}): Promise<StandupBriefing> {
  return decodeBriefing(await getBoundedJson(briefingPath(query), token, signal))
}

export async function fetchStandupWork(id: string, token: string, signal: AbortSignal): Promise<WorkItem> {
  return decodeWorkItem(await getBoundedJson(workPath(id), token, signal), id)
}
