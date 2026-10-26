import { ApiError } from './api.ts'
import { withTimeout } from './timeout.ts'

const MAX_RESPONSE_BYTES = 1024 * 1024
const MAX_TEXT_BYTES = 4 * 1024
const MAX_SOURCES = 100
const MAX_WARNINGS = 20
const MAX_FACTS = 6
const MAX_IMPLICATIONS = 4
const MAX_REFERENCES = 4
const textEncoder = new TextEncoder()
const hiddenText = /[\p{Cc}\u00ad\u061c\u200b-\u200f\u2028-\u202e\u2060-\u2064\u2066-\u206f\ufeff\u{e0000}-\u{e007f}]/u
const COMMIT_ID = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/
const WORK_ID = /^wrk_[0-9a-f]{32}$/

export type NarrativeStatus = 'pending' | 'generating' | 'ready' | 'failed'

export interface NarrativeWindow {
  event_from: number
  event_through: number
  merge_from: number
  merge_through: number
  subscriber: string
  complete: boolean
}

export interface NarrativeSource {
  id: string
  label: string
  work_id: string | null
  commit: string | null
  pull_number: number | null
}

export interface NarrativeStatement {
  text: string
  references: string[]
}

export interface NarrativeOutput {
  headline: string
  headline_references: string[]
  facts: NarrativeStatement[]
  implications: NarrativeStatement[]
}

export interface NarrativeView {
  id: string
  digest: string
  status: NarrativeStatus
  generated_at: number | null
  generated_by: string | null
  window: NarrativeWindow
  sources: NarrativeSource[]
  warnings: string[]
  output: NarrativeOutput | null
}

export interface NarrativeQuery {
  subscriber?: string
  task?: string
}

export class InvalidNarrativeResponse extends Error {}

const fail = (field: string): never => {
  throw new InvalidNarrativeResponse(`Invalid agent briefing response (${field})`)
}
const record = (value: unknown, field: string): Record<string, unknown> =>
  value !== null && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : fail(field)
const integer = (value: unknown, field: string): number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : fail(field)
const bool = (value: unknown, field: string): boolean => typeof value === 'boolean' ? value : fail(field)
const list = (value: unknown, field: string, max: number): unknown[] =>
  Array.isArray(value) && value.length <= max ? value : fail(field)
const boundedText = (value: unknown, field: string, maxBytes = MAX_TEXT_BYTES): string => {
  if (typeof value !== 'string' || value.length === 0 || textEncoder.encode(value).byteLength > maxBytes || hiddenText.test(value)) fail(field)
  return value as string
}
const nullableText = (value: unknown, field: string, maxBytes: number): string | null =>
  value === null ? null : boundedText(value, field, maxBytes)

function statement(value: unknown, field: string, sourceIds: Set<string>): NarrativeStatement {
  const row = record(value, field)
  const references = sourceReferences(row['references'], `${field}.references`, sourceIds)
  return {
    text: boundedText(row['text'], `${field}.text`, 600),
    references,
  }
}

function sourceReferences(value: unknown, field: string, sourceIds: Set<string>): string[] {
  const references = list(value, field, MAX_REFERENCES)
    .map((reference) => boundedText(reference, `${field}.item`, 200))
  if (new Set(references).size !== references.length || references.some((reference) => !sourceIds.has(reference))) {
    fail(field)
  }
  return references
}

export function decodeNarrative(value: unknown): NarrativeView {
  const row = record(value, 'narrative')
  const status = row['status']
  if (status !== 'pending' && status !== 'generating' && status !== 'ready' && status !== 'failed') fail('status')

  const windowRow = record(row['window'], 'window')
  const eventFrom = integer(windowRow['event_from'], 'window.event_from')
  const eventThrough = integer(windowRow['event_through'], 'window.event_through')
  const mergeFrom = integer(windowRow['merge_from'], 'window.merge_from')
  const mergeThrough = integer(windowRow['merge_through'], 'window.merge_through')
  if (eventFrom > eventThrough) fail('window.event_range')
  if (mergeFrom > mergeThrough) fail('window.merge_range')

  const sources = list(row['sources'], 'sources', MAX_SOURCES).map((value): NarrativeSource => {
    const source = record(value, 'sources.item')
    const workId = nullableText(source['work_id'], 'sources.work_id', 36)
    const commit = nullableText(source['commit'], 'sources.commit', 64)
    const pullNumber = source['pull_number'] === null ? null : integer(source['pull_number'], 'sources.pull_number')
    if (workId !== null && !WORK_ID.test(workId)) fail('sources.work_id')
    if (commit !== null && !COMMIT_ID.test(commit)) fail('sources.commit')
    if (pullNumber === 0) fail('sources.pull_number')
    return {
      id: boundedText(source['id'], 'sources.id', 200),
      label: boundedText(source['label'], 'sources.label', 600),
      work_id: workId,
      commit,
      pull_number: pullNumber,
    }
  })
  const sourceIds = new Set(sources.map((source) => source.id))
  if (sourceIds.size !== sources.length) fail('sources.id')

  const generatedAt = row['generated_at'] === null ? null : integer(row['generated_at'], 'generated_at')
  const generatedBy = nullableText(row['generated_by'], 'generated_by', 256)
  const output = row['output'] === null ? null : (() => {
    const outputRow = record(row['output'], 'output')
    return {
      headline: boundedText(outputRow['headline'], 'output.headline', 240),
      headline_references: sourceReferences(outputRow['headline_references'], 'output.headline_references', sourceIds),
      facts: list(outputRow['facts'], 'output.facts', MAX_FACTS)
        .map((item, index) => statement(item, `output.facts.${index}`, sourceIds)),
      implications: list(outputRow['implications'], 'output.implications', MAX_IMPLICATIONS)
        .map((item, index) => statement(item, `output.implications.${index}`, sourceIds)),
    }
  })()
  if (status === 'ready' ? output === null || (sources.length > 0 && output.facts.length === 0) : output !== null) fail('status_output')
  if (output !== null && sources.length === 0 && (output.headline_references.length > 0 || output.facts.length > 0 || output.implications.length > 0)) fail('output.sources')
  if (output !== null && sources.length > 0 && output.headline_references.length === 0) fail('output.headline_references')
  if (output !== null && [...output.facts, ...output.implications].some((item) => item.references.length === 0)) fail('output.references')

  return {
    id: boundedText(row['id'], 'id', 100),
    digest: boundedText(row['digest'], 'digest', 64),
    status: status as NarrativeStatus,
    generated_at: generatedAt,
    generated_by: generatedBy,
    window: {
      event_from: eventFrom,
      event_through: eventThrough,
      merge_from: mergeFrom,
      merge_through: mergeThrough,
      subscriber: boundedText(windowRow['subscriber'], 'window.subscriber', 255),
      complete: bool(windowRow['complete'], 'window.complete'),
    },
    sources,
    warnings: list(row['warnings'], 'warnings', MAX_WARNINGS)
      .map((warning) => boundedText(warning, 'warnings.item', 1000)),
    output,
  }
}

function narrativePath(query: NarrativeQuery): string {
  const params = new URLSearchParams()
  params.set('subscriber', boundedText(query.subscriber ?? 'human', 'query.subscriber', 128))
  if (query.task !== undefined) params.set('task', boundedText(query.task, 'query.task', 500))
  return `/v1/hub/standup/narrative?${params}`
}

export function validateNarrativeTask(value: string): string {
  return boundedText(value, 'query.task', 500)
}

async function boundedJson(response: Response): Promise<unknown> {
  if (!response.ok) throw new ApiError(response.status, `HTTP ${response.status}`)
  const declared = Number(response.headers.get('content-length'))
  if (Number.isFinite(declared) && declared > MAX_RESPONSE_BYTES) {
    await response.body?.cancel()
    throw new InvalidNarrativeResponse('Agent briefing response is too large')
  }
  if (!response.body) throw new InvalidNarrativeResponse('Agent briefing response has no body')
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
        throw new InvalidNarrativeResponse('Agent briefing response is too large')
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
    throw new InvalidNarrativeResponse('Agent briefing response is not valid JSON')
  }
}

export async function fetchNarrative(token: string, signal: AbortSignal, query: NarrativeQuery = {}): Promise<NarrativeView> {
  const response = await fetch(narrativePath(query), {
    method: 'GET',
    signal: withTimeout(signal),
    cache: 'no-store',
    credentials: 'omit',
    headers: { Authorization: `Bearer ${token}` },
  })
  const view = decodeNarrative(await boundedJson(response))
  if (view.window.subscriber !== (query.subscriber ?? 'human')) fail('window.subscriber')
  return view
}
