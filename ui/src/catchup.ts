import { ApiError } from './api.ts'
import { decodeWorkItem, type WorkItem } from './standup.ts'
import { withTimeout } from './timeout.ts'

const MAX_RESPONSE_BYTES = 4 * 1024 * 1024
const MAX_PAGE = 50
const MAX_PATHS = 256
const MAX_TEXT_BYTES = 500
const textEncoder = new TextEncoder()
const hiddenText = /[\p{Cc}\u00ad\u061c\u200b-\u200f\u2028-\u202e\u2060-\u2064\u2066-\u206f\ufeff\u{e0000}-\u{e007f}]/u
const DELIVERY_ID = /^cup_[0-9a-f]{32}$/
const COMMIT_ID = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/
const DELIVERY_CLIENT = /^hub_[A-Za-z0-9_-]{16,100}$/
const DELIVERY_CLIENT_STORAGE = 'agentscm.catchup.delivery-client.v1.'
const DELIVERY_CLIENT_LOCK = 'agentscm.catchup.delivery-client:'
const DELIVERY_CLIENT_SLOTS = 8
const leasedDeliveryClientSlots = new Set<number>()

export type CatchupMergeSource = 'tracked_publication' | 'forge_pull'
export type CatchupCoverageState = 'observed_only' | 'incomplete' | 'unavailable'
export type CatchupAckStatus = 'advanced' | 'already_acknowledged' | 'conflict'

export interface CatchupWorkEvent {
  seq: number
  at: number
  kind: string
  work_id: string
  item: WorkItem
}

export interface CatchupCoordinationEvent {
  seq: number
  at: number
  kind: string
  request_id: string
  title: string
  status: string
}

export interface CatchupMergeObservation {
  id: string
  seq: number
  source: CatchupMergeSource
  source_id: string
  occurred_at: number | null
  observed_at: number
  title: string | null
  branch: string
  base: string
  head_commit: string
  merge_commit: string
  pull_number: number
  paths: string[]
  paths_complete: boolean
}

export interface CatchupRetention {
  work_gap: boolean
  coordination_gap: boolean
  merge_gap: boolean
  event_available_from: number
  merge_available_from: number
}

export interface CatchupSourceCoverage {
  source: CatchupMergeSource
  state: CatchupCoverageState
  checked_at: number | null
  unread_pages: number
  deferred: number
}

export interface CatchupPage {
  subscriber: string
  delivery_client: string
  delivery: string
  event_from: number
  event_through: number
  event_after: number
  next_event_after: number
  merge_from: number
  merge_through: number
  merge_after: number
  next_merge_after: number
  work_events: CatchupWorkEvent[]
  coordination_events: CatchupCoordinationEvent[]
  merges: CatchupMergeObservation[]
  retention: CatchupRetention
  source_coverage: CatchupSourceCoverage[]
  complete: boolean
}

export interface CatchupAckResult {
  status: CatchupAckStatus
  event_through: number
  merge_through: number
  current_event_ack: number
  current_merge_ack: number
}

export interface CatchupQuery {
  subscriber?: string
  deliveryClient?: string
  delivery?: string
  eventAfter?: number
  eventThrough?: number
  mergeAfter?: number
  mergeThrough?: number
  limit?: number
}

export class InvalidCatchupResponse extends Error {}

export interface CatchupDeliveryClientLease {
  client: string
  slot: number | null
  releaseLock: (() => void) | null
}

export function newCatchupDeliveryClient(prefix: 'hub' | 'mcp' = 'hub'): string {
  const cryptoApi = globalThis.crypto
  if (typeof cryptoApi?.randomUUID === 'function') return `${prefix}_${cryptoApi.randomUUID()}`
  if (typeof cryptoApi?.getRandomValues === 'function') {
    const bytes = cryptoApi.getRandomValues(new Uint8Array(16))
    return `${prefix}_${Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('')}`
  }
  return `${prefix}_${Date.now().toString(36)}_${Math.random().toString(36).slice(2)}_${Math.random().toString(36).slice(2)}`
}

function holdCatchupDeliveryClient(client: string): Promise<(() => void) | null> {
  const locks = globalThis.navigator?.locks
  if (locks === undefined || typeof locks.request !== 'function') return Promise.resolve(null)
  return new Promise((resolve) => {
    let settled = false
    let unlock!: () => void
    const hold = new Promise<void>((done) => { unlock = done })
    void locks.request(`${DELIVERY_CLIENT_LOCK}${client}`, { mode: 'exclusive', ifAvailable: true }, (lock) => {
      settled = true
      if (lock === null) {
        resolve(null)
        return
      }
      resolve(unlock)
      return hold
    }).catch(() => { if (!settled) resolve(null) })
  })
}

export async function acquireCatchupDeliveryClient(): Promise<CatchupDeliveryClientLease> {
  const locks = globalThis.navigator?.locks
  if (locks === undefined || typeof locks.request !== 'function') {
    return { client: newCatchupDeliveryClient(), slot: null, releaseLock: null }
  }
  for (let slot = 0; slot < DELIVERY_CLIENT_SLOTS; slot++) {
    if (leasedDeliveryClientSlots.has(slot)) continue
    leasedDeliveryClientSlots.add(slot)
    const key = `${DELIVERY_CLIENT_STORAGE}${slot}`
    let stored: string | null
    try {
      stored = globalThis.sessionStorage.getItem(key)
    } catch {
      leasedDeliveryClientSlots.delete(slot)
      return { client: newCatchupDeliveryClient(), slot: null, releaseLock: null }
    }
    if (stored !== null && DELIVERY_CLIENT.test(stored)) {
      let releaseLock = await holdCatchupDeliveryClient(stored)
      if (releaseLock === null) {
        await new Promise<void>((resolve) => { setTimeout(resolve, 0) })
        releaseLock = await holdCatchupDeliveryClient(stored)
      }
      if (releaseLock !== null) return { client: stored, slot, releaseLock }
    }
    const client = newCatchupDeliveryClient()
    const releaseLock = await holdCatchupDeliveryClient(client)
    if (releaseLock === null) {
      leasedDeliveryClientSlots.delete(slot)
      return { client: newCatchupDeliveryClient(), slot: null, releaseLock: null }
    }
    try {
      globalThis.sessionStorage.setItem(key, client)
      return { client, slot, releaseLock }
    } catch {
      releaseLock()
      leasedDeliveryClientSlots.delete(slot)
      return { client: newCatchupDeliveryClient(), slot: null, releaseLock: null }
    }
  }
  return { client: newCatchupDeliveryClient(), slot: null, releaseLock: null }
}

export function releaseCatchupDeliveryClient(lease: CatchupDeliveryClientLease): void {
  if (lease.slot !== null) leasedDeliveryClientSlots.delete(lease.slot)
  lease.releaseLock?.()
}

const fail = (field: string): never => {
  throw new InvalidCatchupResponse(`Invalid catch-up response (${field})`)
}
const record = (value: unknown, field: string): Record<string, unknown> =>
  value !== null && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : fail(field)
const integer = (value: unknown, field: string): number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : fail(field)
const bool = (value: unknown, field: string): boolean => typeof value === 'boolean' ? value : fail(field)
const boundedText = (value: unknown, field: string, maxBytes = MAX_TEXT_BYTES): string => {
  if (typeof value !== 'string' || value.length === 0 || textEncoder.encode(value).byteLength > maxBytes || hiddenText.test(value)) fail(field)
  return value as string
}
const nullableText = (value: unknown, field: string): string | null => value === null ? null : boundedText(value, field, 256)
const list = (value: unknown, field: string, max: number): unknown[] =>
  Array.isArray(value) && value.length <= max ? value : fail(field)

function relativePath(value: unknown, field: string): string {
  const path = boundedText(value, field, 4096)
  const parts = path.split('/')
  if (path.startsWith('/') || path.includes('\\') || parts.some((part) => part === '' || part === '.' || part === '..')) fail(field)
  return path
}

function workEvent(value: unknown): CatchupWorkEvent {
  const row = record(value, 'work_events.item')
  const item = decodeWorkItem(row['item'])
  const workId = boundedText(row['work_id'], 'work_events.work_id', 36)
  if (workId !== item.id) fail('work_events.work_id')
  return {
    seq: integer(row['seq'], 'work_events.seq'),
    at: integer(row['at'], 'work_events.at'),
    kind: boundedText(row['kind'], 'work_events.kind', 64),
    work_id: workId,
    item,
  }
}

function coordinationEvent(value: unknown): CatchupCoordinationEvent {
  const row = record(value, 'coordination_events.item')
  return {
    seq: integer(row['seq'], 'coordination_events.seq'),
    at: integer(row['at'], 'coordination_events.at'),
    kind: boundedText(row['kind'], 'coordination_events.kind', 64),
    request_id: boundedText(row['request_id'], 'coordination_events.request_id', 64),
    title: boundedText(row['title'], 'coordination_events.title'),
    status: boundedText(row['status'], 'coordination_events.status', 64),
  }
}

function mergeObservation(value: unknown): CatchupMergeObservation {
  const row = record(value, 'merges.item')
  const source = row['source']
  if (source !== 'tracked_publication' && source !== 'forge_pull') fail('merges.source')
  const head = boundedText(row['head_commit'], 'merges.head_commit', 64)
  const merge = boundedText(row['merge_commit'], 'merges.merge_commit', 64)
  if (!COMMIT_ID.test(head) || !COMMIT_ID.test(merge)) fail('merges.commit')
  const paths = list(row['paths'], 'merges.paths', MAX_PATHS).map((path) => relativePath(path, 'merges.paths.item'))
  if (new Set(paths).size !== paths.length) fail('merges.paths')
  const pullNumber = integer(row['pull_number'], 'merges.pull_number')
  if (pullNumber === 0) fail('merges.pull_number')
  return {
    id: boundedText(row['id'], 'merges.id', 64),
    seq: integer(row['seq'], 'merges.seq'),
    source: source as CatchupMergeSource,
    source_id: boundedText(row['source_id'], 'merges.source_id', 255),
    occurred_at: row['occurred_at'] === null ? null : integer(row['occurred_at'], 'merges.occurred_at'),
    observed_at: integer(row['observed_at'], 'merges.observed_at'),
    title: nullableText(row['title'], 'merges.title'),
    branch: boundedText(row['branch'], 'merges.branch', 255),
    base: boundedText(row['base'], 'merges.base', 255),
    head_commit: head,
    merge_commit: merge,
    pull_number: pullNumber,
    paths,
    paths_complete: bool(row['paths_complete'], 'merges.paths_complete'),
  }
}

function strictlyIncreasing(values: number[], field: string): void {
  for (let index = 1; index < values.length; index++) {
    if (values[index - 1]! >= values[index]!) fail(field)
  }
}

export function decodeCatchupPage(value: unknown): CatchupPage {
  const page = record(value, 'catchup')
  const delivery = boundedText(page['delivery'], 'delivery', 64)
  if (!DELIVERY_ID.test(delivery)) fail('delivery')
  const eventFrom = integer(page['event_from'], 'event_from')
  const eventThrough = integer(page['event_through'], 'event_through')
  const eventAfter = integer(page['event_after'], 'event_after')
  const nextEventAfter = integer(page['next_event_after'], 'next_event_after')
  const mergeFrom = integer(page['merge_from'], 'merge_from')
  const mergeThrough = integer(page['merge_through'], 'merge_through')
  const mergeAfter = integer(page['merge_after'], 'merge_after')
  const nextMergeAfter = integer(page['next_merge_after'], 'next_merge_after')
  if (!(eventFrom <= eventAfter && eventAfter <= nextEventAfter && nextEventAfter <= eventThrough)) fail('event_window')
  if (!(mergeFrom <= mergeAfter && mergeAfter <= nextMergeAfter && nextMergeAfter <= mergeThrough)) fail('merge_window')
  const workEvents = list(page['work_events'], 'work_events', MAX_PAGE).map(workEvent)
  const coordinationEvents = list(page['coordination_events'], 'coordination_events', MAX_PAGE).map(coordinationEvent)
  const eventSeqs = [...workEvents, ...coordinationEvents].map((event) => event.seq).sort((left, right) => left - right)
  strictlyIncreasing(eventSeqs, 'event_order')
  if (eventSeqs.some((seq) => seq <= eventAfter || seq > nextEventAfter)) fail('event_window')
  const merges = list(page['merges'], 'merges', MAX_PAGE).map(mergeObservation)
  strictlyIncreasing(merges.map((merge) => merge.seq), 'merge_order')
  if (merges.some((merge) => merge.seq <= mergeAfter || merge.seq > nextMergeAfter)) fail('merge_window')
  const retentionRow = record(page['retention'], 'retention')
  const retention = {
    work_gap: bool(retentionRow['work_gap'], 'retention.work_gap'),
    coordination_gap: bool(retentionRow['coordination_gap'], 'retention.coordination_gap'),
    merge_gap: bool(retentionRow['merge_gap'], 'retention.merge_gap'),
    event_available_from: integer(retentionRow['event_available_from'], 'retention.event_available_from'),
    merge_available_from: integer(retentionRow['merge_available_from'], 'retention.merge_available_from'),
  }
  const sourceCoverage = list(page['source_coverage'], 'source_coverage', 2).map((value): CatchupSourceCoverage => {
    const row = record(value, 'source_coverage.item')
    const source = row['source']
    if (source !== 'tracked_publication' && source !== 'forge_pull') fail('source_coverage.source')
    const state = row['state']
    if (state !== 'observed_only' && state !== 'incomplete' && state !== 'unavailable') fail('source_coverage.state')
    return {
      source: source as CatchupMergeSource,
      state: state as CatchupCoverageState,
      checked_at: row['checked_at'] === null ? null : integer(row['checked_at'], 'source_coverage.checked_at'),
      unread_pages: integer(row['unread_pages'], 'source_coverage.unread_pages'),
      deferred: integer(row['deferred'], 'source_coverage.deferred'),
    }
  })
  if (new Set(sourceCoverage.map((coverage) => coverage.source)).size !== sourceCoverage.length) fail('source_coverage.source')
  const complete = bool(page['complete'], 'complete')
  if (complete !== (nextEventAfter === eventThrough && nextMergeAfter === mergeThrough)) fail('complete')
  return {
    subscriber: boundedText(page['subscriber'], 'subscriber', 255),
    delivery_client: boundedText(page['delivery_client'], 'delivery_client', 200),
    delivery,
    event_from: eventFrom,
    event_through: eventThrough,
    event_after: eventAfter,
    next_event_after: nextEventAfter,
    merge_from: mergeFrom,
    merge_through: mergeThrough,
    merge_after: mergeAfter,
    next_merge_after: nextMergeAfter,
    work_events: workEvents,
    coordination_events: coordinationEvents,
    merges,
    retention,
    source_coverage: sourceCoverage,
    complete,
  }
}

export function appendCatchupPage(current: CatchupPage, next: CatchupPage): CatchupPage {
  if (current.complete
    || next.delivery !== current.delivery
    || next.subscriber !== current.subscriber
    || next.delivery_client !== current.delivery_client
    || next.event_from !== current.event_from
    || next.event_through !== current.event_through
    || next.merge_from !== current.merge_from
    || next.merge_through !== current.merge_through
    || next.event_after !== current.next_event_after
    || next.merge_after !== current.next_merge_after
    || JSON.stringify(next.retention) !== JSON.stringify(current.retention)) fail('continuation')
  return {
    ...next,
    event_after: current.event_after,
    merge_after: current.merge_after,
    work_events: [...current.work_events, ...next.work_events],
    coordination_events: [...current.coordination_events, ...next.coordination_events],
    merges: [...current.merges, ...next.merges],
  }
}

export function catchupPath(query: CatchupQuery = {}): string {
  const params = new URLSearchParams()
  params.set('subscriber', query.subscriber ?? 'human')
  if (query.deliveryClient !== undefined) params.set('delivery_client', boundedText(query.deliveryClient, 'query.delivery_client', 200))
  const limit = query.limit ?? 20
  if (!Number.isSafeInteger(limit) || limit < 1 || limit > MAX_PAGE) throw new RangeError('Catch-up page size must be between 1 and 50')
  params.set('limit', String(limit))
  const continuation = [query.delivery, query.eventAfter, query.eventThrough, query.mergeAfter, query.mergeThrough]
  if (continuation.some((value) => value !== undefined)) {
    if (continuation.some((value) => value === undefined) || !DELIVERY_ID.test(query.delivery!)) throw new RangeError('Catch-up continuation requires every cursor')
    params.set('delivery', query.delivery!)
    params.set('event_after', String(integer(query.eventAfter, 'query.event_after')))
    params.set('event_through', String(integer(query.eventThrough, 'query.event_through')))
    params.set('merge_after', String(integer(query.mergeAfter, 'query.merge_after')))
    params.set('merge_through', String(integer(query.mergeThrough, 'query.merge_through')))
  }
  return `/v1/hub/standup/catchup?${params.toString()}`
}

async function boundedJson(response: Response): Promise<unknown> {
  if (!response.ok) throw new ApiError(response.status, `HTTP ${response.status}`)
  const declared = Number(response.headers.get('content-length'))
  if (Number.isFinite(declared) && declared > MAX_RESPONSE_BYTES) {
    await response.body?.cancel()
    throw new InvalidCatchupResponse('Catch-up response is too large')
  }
  if (!response.body) throw new InvalidCatchupResponse('Catch-up response has no body')
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
        throw new InvalidCatchupResponse('Catch-up response is too large')
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
    throw new InvalidCatchupResponse('Catch-up response is not valid JSON')
  }
}

export async function fetchCatchup(token: string, signal: AbortSignal, query: CatchupQuery = {}): Promise<CatchupPage> {
  const response = await fetch(catchupPath(query), {
    signal: withTimeout(signal),
    cache: 'no-store',
    credentials: 'omit',
    headers: { Authorization: `Bearer ${token}` },
  })
  return decodeCatchupPage(await boundedJson(response))
}

export function decodeCatchupAck(value: unknown): CatchupAckResult {
  const row = record(value, 'ack')
  const status = row['status']
  if (status !== 'advanced' && status !== 'already_acknowledged' && status !== 'conflict') fail('ack.status')
  const result: CatchupAckResult = {
    status: status as CatchupAckStatus,
    event_through: integer(row['event_through'], 'ack.event_through'),
    merge_through: integer(row['merge_through'], 'ack.merge_through'),
    current_event_ack: integer(row['current_event_ack'], 'ack.current_event_ack'),
    current_merge_ack: integer(row['current_merge_ack'], 'ack.current_merge_ack'),
  }
  if (result.current_event_ack < result.event_through && status !== 'conflict') fail('ack.current_event_ack')
  if (result.current_merge_ack < result.merge_through && status !== 'conflict') fail('ack.current_merge_ack')
  return result
}

export async function acknowledgeCatchup(token: string, signal: AbortSignal, page: CatchupPage): Promise<CatchupAckResult> {
  if (!page.complete) throw new InvalidCatchupResponse('A partial catch-up delivery cannot be marked reviewed')
  const response = await fetch('/v1/hub/standup/catchup/ack', {
    method: 'POST',
    signal: withTimeout(signal),
    cache: 'no-store',
    credentials: 'omit',
    headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
    body: JSON.stringify({ delivery: page.delivery, expected_event_from: page.event_from, expected_merge_from: page.merge_from }),
  })
  const result = decodeCatchupAck(await boundedJson(response))
  if (result.event_through !== page.event_through || result.merge_through !== page.merge_through) fail('ack.through')
  return result
}
