import { ApiError } from './api.ts'

const MAX_RESPONSE_BYTES = 4 * 1024 * 1024
const MAX_PAGE = 100
const MAX_SCOPE = 256
const MAX_RELATED = 100
const MAX_PARTICIPANTS = 100
const MAX_REFS = 100
const MAX_OVERLAPS = (MAX_PAGE * (MAX_PAGE - 1)) / 2
const MAX_COORDINATION_UPDATES = MAX_PAGE * MAX_PAGE
const MAX_COORDINATION_REASONS = 4
const MAX_TEXT_BYTES = 500
const MAX_LABEL_BYTES = 255
const textEncoder = new TextEncoder()

export type WorkRole = 'lead' | 'helper' | 'reviewer'
export type WorkState = 'active' | 'blocked' | 'paused' | 'finished' | 'handover'
export type ClaimStatus = 'active' | 'stale' | 'unknown'
export type CheckpointKind = 'progress' | 'decision' | 'blocker' | 'pause' | 'finish' | 'handover'
export type WorkEventKind = 'work_started' | 'work_joined' | 'work_takeover' | 'work_checkpoint'
export type WorkOutcomeKind = 'decision' | 'completion'
export type CoordinationStatus = 'pending' | 'responded' | 'resolved' | 'withdrawn'
export type CoordinationResponseKind = 'propose_split' | 'join' | 'handover' | 'decline'

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

export interface WorkOutcome {
  seq: number
  at: number
  kind: WorkOutcomeKind
  item: WorkItem
}

export interface WorkPathPair {
  left_path: string
  right_path: string
}

export interface WorkOverlapPair {
  left: WorkItem
  right: WorkItem
  reasons: WorkPathPair[]
  reasons_complete: boolean
}

export interface WorkOverview {
  recent_since: number
  until: number
  current: WorkItem[]
  recent: WorkOutcome[]
  overlaps: WorkOverlapPair[]
  current_complete: boolean
  recent_complete: boolean
  overlaps_complete: boolean
  history_retention_gap: boolean
}

export interface CoordinationAgent {
  token: string
  agent: string
  session: string
  owner: string
  login: string
}

export interface ForgeWorkRef {
  provider: string
  host: string
  repository: string
  issue: string
}

export interface CoordinationProposal {
  work_id: string | null
  source: ForgeWorkRef | null
  request_key: string | null
  title: string
  scope: string[]
}

export interface CoordinationTarget {
  work_id: string
  lead: CoordinationAgent
  generation: number
  reasons: { your_path: string; their_path: string }[]
  reasons_complete: boolean
}

export interface CoordinationAcknowledgement {
  target_work_id: string | null
  agent: CoordinationAgent
  generation: number
  through_seq: number
  acknowledged_at: number
  seq: number
}

export interface CoordinationResponse {
  id: string
  operation_id: string
  target_work_id: string
  responder: CoordinationAgent
  target_generation: number
  kind: CoordinationResponseKind
  note: string | null
  created_at: number
  seq: number
}

export interface CoordinationRequest {
  id: string
  requester: CoordinationAgent
  proposal: CoordinationProposal
  targets: CoordinationTarget[]
  status: CoordinationStatus
  acknowledgements: CoordinationAcknowledgement[]
  acknowledgements_complete: boolean
  responses: CoordinationResponse[]
  responses_complete: boolean
  created_at: number
  updated_at: number
  created_seq: number
  updated_seq: number
}

export interface CoordinationBriefing {
  from: number
  through: number
  next_after: number
  requests: CoordinationRequest[]
  complete: boolean
  retention_gap: boolean
}

export function hasMoreBriefing(briefing: StandupBriefing, view: 'current' | 'changes'): boolean {
  return !briefing.complete && (view === 'current' ? !briefing.next_work_done : briefing.next_after < briefing.through)
}

export function workMatchesSearch(item: WorkItem, search: string): boolean {
  const normalized = search.trim().toLocaleLowerCase()
  if (!normalized) return true
  const source = item.source.kind === 'native' ? item.source.request_key : `${item.source.provider} ${item.source.repository} ${item.source.issue}`
  return [item.title, item.owner_login, item.blocker?.summary, item.next_action, source,
    ...item.scope,
    ...item.participants.flatMap((participant) => [participant.agent, participant.login])]
    .some((value) => value?.toLocaleLowerCase().includes(normalized))
}

export interface BriefingQuery {
  after?: number
  workAfter?: string
  workDone?: boolean
  through?: number
  limit?: number
}

export interface OverviewQuery {
  recentSince?: number
  limit?: number
}

export interface CoordinationQuery {
  activeOnly?: boolean
  after?: number
  through?: number
  limit?: number
}

export class InvalidStandupResponse extends Error {}

const fail = (field: string): never => {
  throw new InvalidStandupResponse(`Invalid stand-up response (${field})`)
}
const record = (value: unknown, field: string): Record<string, unknown> =>
  value !== null && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, unknown>) : fail(field)
const hiddenText = /[\p{Cc}\u00ad\u061c\u200b-\u200f\u2028-\u202e\u2060-\u2064\u2066-\u206f\ufeff\u{e0000}-\u{e007f}]/u
const boundedText = (value: unknown, field: string, maxBytes = MAX_TEXT_BYTES): string => {
  const result = typeof value === 'string' ? value : fail(field)
  if (textEncoder.encode(result).byteLength > maxBytes || hiddenText.test(result)) fail(field)
  return result
}
const nullableText = (value: unknown, field: string, maxBytes = MAX_TEXT_BYTES): string | null =>
  value === null ? null : boundedText(value, field, maxBytes)
const safeInteger = (value: unknown, field: string): number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : fail(field)
const timestamp = (value: unknown, field: string): number => {
  const result = safeInteger(value, field)
  return result <= 8_640_000_000_000 ? result : fail(field)
}
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
const OUTCOME_KINDS = new Set<WorkOutcomeKind>(['decision', 'completion'])
const COORDINATION_STATUSES = new Set<CoordinationStatus>(['pending', 'responded', 'resolved', 'withdrawn'])
const COORDINATION_RESPONSE_KINDS = new Set<CoordinationResponseKind>(['propose_split', 'join', 'handover', 'decline'])
const COORDINATION_ID = /^crq_[0-9a-f]{32}$/
const COORDINATION_RESPONSE_ID = /^crs_[0-9a-f]{32}$/

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
    at: timestamp(item['at'], 'work.current.at'),
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
    last_checkin_at: timestamp(item['last_checkin_at'], 'work.last_checkin_at'),
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
    created_at: timestamp(item['created_at'], 'work.created_at'),
    updated_at: timestamp(item['updated_at'], 'work.updated_at'),
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
    at: timestamp(item['at'], 'briefing.event.at'),
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

function outcome(value: unknown): WorkOutcome {
  const item = record(value, 'overview.recent.item')
  const kind = enumValue(item['kind'], OUTCOME_KINDS, 'overview.recent.kind')
  const work = decodeWorkItem(item['item'])
  if (kind === 'completion' && work.state !== 'finished') fail('overview.recent.completion.state')
  return {
    seq: safeInteger(item['seq'], 'overview.recent.seq'),
    at: timestamp(item['at'], 'overview.recent.at'),
    kind,
    item: work,
  }
}

function pathPair(value: unknown): WorkPathPair {
  const item = record(value, 'overview.overlap.reason')
  return {
    left_path: repositoryPath(item['left_path'], 'overview.overlap.reason.left_path'),
    right_path: repositoryPath(item['right_path'], 'overview.overlap.reason.right_path'),
  }
}

function overlapPair(value: unknown): WorkOverlapPair {
  const item = record(value, 'overview.overlap')
  const left = decodeWorkItem(item['left'])
  const right = decodeWorkItem(item['right'])
  if (left.id === right.id) fail('overview.overlap.work')
  const reasons = list(item['reasons'], MAX_COORDINATION_REASONS, 'overview.overlap.reasons').map(pathPair)
  if (reasons.length === 0) fail('overview.overlap.reasons')
  return { left, right, reasons, reasons_complete: bool(item['reasons_complete'], 'overview.overlap.reasons_complete') }
}

export function decodeOverview(value: unknown): WorkOverview {
  const item = record(value, 'overview')
  const recentSince = timestamp(item['recent_since'], 'overview.recent_since')
  const until = timestamp(item['until'], 'overview.until')
  if (recentSince > until) fail('overview.window')
  const current = list(item['current'], MAX_PAGE, 'overview.current').map((entry) => decodeWorkItem(entry))
  if (current.some((work) => work.state === 'finished')) fail('overview.current.state')
  if (new Set(current.map((work) => work.id)).size !== current.length) fail('overview.current.id')
  const recent = list(item['recent'], MAX_PAGE, 'overview.recent').map(outcome)
  for (let index = 1; index < recent.length; index++) {
    if (recent[index - 1]!.seq <= recent[index]!.seq) fail('overview.recent.order')
  }
  if (recent.some((entry) => entry.at < recentSince || entry.at > until)) fail('overview.recent.at')
  const overlaps = list(item['overlaps'], MAX_OVERLAPS, 'overview.overlaps').map(overlapPair)
  const currentIds = new Set(current.map((work) => work.id))
  const overlapKeys = new Set<string>()
  for (const overlap of overlaps) {
    if (!currentIds.has(overlap.left.id) || !currentIds.has(overlap.right.id)) fail('overview.overlap.current')
    const key = [overlap.left.id, overlap.right.id].sort().join(':')
    if (overlapKeys.has(key)) fail('overview.overlap.duplicate')
    overlapKeys.add(key)
  }
  const currentComplete = bool(item['current_complete'], 'overview.current_complete')
  const recentComplete = bool(item['recent_complete'], 'overview.recent_complete')
  const overlapsComplete = bool(item['overlaps_complete'], 'overview.overlaps_complete')
  const historyRetentionGap = bool(item['history_retention_gap'], 'overview.history_retention_gap')
  if (overlapsComplete && !currentComplete) fail('overview.overlaps_complete')
  if (historyRetentionGap && recentComplete) fail('overview.recent_complete')
  return {
    recent_since: recentSince,
    until,
    current,
    recent,
    overlaps,
    current_complete: currentComplete,
    recent_complete: recentComplete,
    overlaps_complete: overlapsComplete,
    history_retention_gap: historyRetentionGap,
  }
}

function coordinationAgent(value: unknown, field: string): CoordinationAgent {
  const item = record(value, field)
  return {
    token: label(item['token'], `${field}.token`),
    agent: label(item['agent'], `${field}.agent`),
    session: label(item['session'], `${field}.session`),
    owner: label(item['owner'], `${field}.owner`),
    login: label(item['login'], `${field}.login`),
  }
}

function forgeWorkRef(value: unknown, field: string): ForgeWorkRef {
  const item = record(value, field)
  return {
    provider: label(item['provider'], `${field}.provider`),
    host: label(item['host'], `${field}.host`),
    repository: label(item['repository'], `${field}.repository`),
    issue: label(item['issue'], `${field}.issue`),
  }
}

function coordinationRequest(value: unknown): CoordinationRequest {
  const item = record(value, 'coordination.request')
  const id = boundedText(item['id'], 'coordination.request.id', 36)
  if (!COORDINATION_ID.test(id)) fail('coordination.request.id')
  const proposalValue = record(item['proposal'], 'coordination.request.proposal')
  const proposal: CoordinationProposal = {
    work_id: proposalValue['work_id'] === null ? null : workId(proposalValue['work_id'], 'coordination.request.proposal.work_id'),
    source: proposalValue['source'] === null ? null : forgeWorkRef(proposalValue['source'], 'coordination.request.proposal.source'),
    request_key: nullableText(proposalValue['request_key'], 'coordination.request.proposal.request_key', MAX_LABEL_BYTES),
    title: boundedText(proposalValue['title'], 'coordination.request.proposal.title'),
    scope: list(proposalValue['scope'], MAX_SCOPE, 'coordination.request.proposal.scope').map((entry) => repositoryPath(entry, 'coordination.request.proposal.scope.item')),
  }
  const targets = list(item['targets'], MAX_PAGE, 'coordination.request.targets').map((entry) => {
    const target = record(entry, 'coordination.request.target')
    const reasons = list(target['reasons'], MAX_COORDINATION_REASONS, 'coordination.request.target.reasons').map((reasonValue) => {
      const reason = record(reasonValue, 'coordination.request.target.reason')
      return {
        your_path: repositoryPath(reason['your_path'], 'coordination.request.target.reason.your_path'),
        their_path: repositoryPath(reason['their_path'], 'coordination.request.target.reason.their_path'),
      }
    })
    if (reasons.length === 0) fail('coordination.request.target.reasons')
    return {
      work_id: workId(target['work_id'], 'coordination.request.target.work_id'),
      lead: coordinationAgent(target['lead'], 'coordination.request.target.lead'),
      generation: safeInteger(target['generation'], 'coordination.request.target.generation'),
      reasons,
      reasons_complete: bool(target['reasons_complete'], 'coordination.request.target.reasons_complete'),
    }
  })
  if (targets.length === 0 || new Set(targets.map((target) => target.work_id)).size !== targets.length) fail('coordination.request.targets')
  const targetIds = new Set(targets.map((target) => target.work_id))
  const acknowledgements = list(item['acknowledgements'], MAX_COORDINATION_UPDATES, 'coordination.request.acknowledgements').map((entry) => {
    const acknowledgement = record(entry, 'coordination.request.acknowledgement')
    const targetWorkId = acknowledgement['target_work_id'] === null ? null : workId(acknowledgement['target_work_id'], 'coordination.request.acknowledgement.target_work_id')
    if (targetWorkId !== null && !targetIds.has(targetWorkId)) fail('coordination.request.acknowledgement.target_work_id')
    return {
      target_work_id: targetWorkId,
      agent: coordinationAgent(acknowledgement['agent'], 'coordination.request.acknowledgement.agent'),
      generation: safeInteger(acknowledgement['generation'], 'coordination.request.acknowledgement.generation'),
      through_seq: safeInteger(acknowledgement['through_seq'], 'coordination.request.acknowledgement.through_seq'),
      acknowledged_at: timestamp(acknowledgement['acknowledged_at'], 'coordination.request.acknowledgement.acknowledged_at'),
      seq: safeInteger(acknowledgement['seq'], 'coordination.request.acknowledgement.seq'),
    }
  })
  const responses = list(item['responses'], MAX_COORDINATION_UPDATES, 'coordination.request.responses').map((entry) => {
    const response = record(entry, 'coordination.request.response')
    const targetWorkId = workId(response['target_work_id'], 'coordination.request.response.target_work_id')
    if (!targetIds.has(targetWorkId)) fail('coordination.request.response.target_work_id')
    const responseId = boundedText(response['id'], 'coordination.request.response.id', 36)
    if (!COORDINATION_RESPONSE_ID.test(responseId)) fail('coordination.request.response.id')
    return {
      id: responseId,
      operation_id: label(response['operation_id'], 'coordination.request.response.operation_id'),
      target_work_id: targetWorkId,
      responder: coordinationAgent(response['responder'], 'coordination.request.response.responder'),
      target_generation: safeInteger(response['target_generation'], 'coordination.request.response.target_generation'),
      kind: enumValue(response['kind'], COORDINATION_RESPONSE_KINDS, 'coordination.request.response.kind'),
      note: nullableText(response['note'], 'coordination.request.response.note'),
      created_at: timestamp(response['created_at'], 'coordination.request.response.created_at'),
      seq: safeInteger(response['seq'], 'coordination.request.response.seq'),
    }
  })
  const createdAt = timestamp(item['created_at'], 'coordination.request.created_at')
  const updatedAt = timestamp(item['updated_at'], 'coordination.request.updated_at')
  const createdSeq = safeInteger(item['created_seq'], 'coordination.request.created_seq')
  const updatedSeq = safeInteger(item['updated_seq'], 'coordination.request.updated_seq')
  if (createdAt > updatedAt || createdSeq > updatedSeq) fail('coordination.request.updated')
  return {
    id,
    requester: coordinationAgent(item['requester'], 'coordination.request.requester'),
    proposal,
    targets,
    status: enumValue(item['status'], COORDINATION_STATUSES, 'coordination.request.status'),
    acknowledgements,
    acknowledgements_complete: bool(item['acknowledgements_complete'], 'coordination.request.acknowledgements_complete'),
    responses,
    responses_complete: bool(item['responses_complete'], 'coordination.request.responses_complete'),
    created_at: createdAt,
    updated_at: updatedAt,
    created_seq: createdSeq,
    updated_seq: updatedSeq,
  }
}

export function decodeCoordinationBriefing(value: unknown): CoordinationBriefing {
  const item = record(value, 'coordination')
  const from = safeInteger(item['from'], 'coordination.from')
  const through = safeInteger(item['through'], 'coordination.through')
  const nextAfter = safeInteger(item['next_after'], 'coordination.next_after')
  if (from > nextAfter || nextAfter > through) fail('coordination.sequence')
  const requests = list(item['requests'], MAX_PAGE, 'coordination.requests').map(coordinationRequest)
  if (new Set(requests.map((request) => request.id)).size !== requests.length) fail('coordination.requests.id')
  for (let index = 0; index < requests.length; index++) {
    if (requests[index]!.updated_seq <= from || requests[index]!.updated_seq > through) fail('coordination.requests.seq')
    if (index > 0 && requests[index - 1]!.updated_seq >= requests[index]!.updated_seq) fail('coordination.requests.order')
  }
  if (requests.length > 0 && requests.at(-1)!.updated_seq !== nextAfter) fail('coordination.next_after')
  if (requests.length === 0 && nextAfter !== from) fail('coordination.next_after')
  const complete = bool(item['complete'], 'coordination.complete')
  if (!complete && requests.length === 0) fail('coordination.complete')
  return {
    from,
    through,
    next_after: nextAfter,
    requests,
    complete,
    retention_gap: bool(item['retention_gap'], 'coordination.retention_gap'),
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

export function overviewPath(query: OverviewQuery = {}): string {
  const limit = query.limit ?? MAX_PAGE
  if (!Number.isSafeInteger(limit) || limit < 1 || limit > MAX_PAGE) throw new RangeError('Stand-up overview limit must be between 1 and 100')
  const params = new URLSearchParams()
  if (query.recentSince !== undefined) params.set('recent_since', String(timestamp(query.recentSince, 'query.recent_since')))
  params.set('limit', String(limit))
  return `/v1/hub/standup/overview?${params.toString()}`
}

export function coordinationPath(query: CoordinationQuery = {}): string {
  const limit = query.limit ?? MAX_PAGE
  if (!Number.isSafeInteger(limit) || limit < 1 || limit > MAX_PAGE) throw new RangeError('Coordination page size must be between 1 and 100')
  const params = new URLSearchParams()
  if (query.after !== undefined) params.set('after', String(safeInteger(query.after, 'query.after')))
  if (query.through !== undefined) params.set('through', String(safeInteger(query.through, 'query.through')))
  if (query.activeOnly !== undefined) params.set('active_only', String(query.activeOnly))
  params.set('limit', String(limit))
  return `/v1/hub/standup/coordination?${params.toString()}`
}

export const workPath = (id: string): string => validWorkId(id)
  ? `/v1/hub/standup/work/${encodeURIComponent(id)}`
  : fail('work.id')

export const workHref = (id: string): string => validWorkId(id)
  ? `#work/${encodeURIComponent(id)}`
  : fail('work.id')

export function workIdFromHash(hash: string): string | null {
  const match = /^#(?:work|coordination\/work)\/([^/]+)$/.exec(hash)
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

export async function fetchOverview(token: string, signal: AbortSignal, query: OverviewQuery = {}): Promise<WorkOverview> {
  return decodeOverview(await getBoundedJson(overviewPath(query), token, signal))
}

export async function fetchCoordination(token: string, signal: AbortSignal, query: CoordinationQuery = {}): Promise<CoordinationBriefing> {
  return decodeCoordinationBriefing(await getBoundedJson(coordinationPath(query), token, signal))
}

export async function fetchStandupWork(id: string, token: string, signal: AbortSignal): Promise<WorkItem> {
  return decodeWorkItem(await getBoundedJson(workPath(id), token, signal), id)
}
