import { ApiError } from './api.ts'
import { validWorkId } from './standup.ts'
import { withTimeout } from './timeout.ts'

const MAX_RESPONSE_BYTES = 48 * 1024
const MAX_IDENTIFIER_BYTES = 255
const MAX_PATH_BYTES = 1024
const MAX_SCOPE = 16
const MAX_SOURCE_REFS = 16
const MAX_SESSION_SETUPS = 16
const MAX_ARTIFACT_BYTES = 1024 * 1024 * 1024
const textEncoder = new TextEncoder()
const hiddenText = /[\p{Cc}\u00ad\u061c\u200b-\u200f\u2028-\u202e\u2060-\u2064\u2066-\u206f\ufeff\u{e0000}-\u{e007f}]/u

export type ChangeRecordResult = 'passed' | 'failed' | 'incomplete' | 'unknown'
export type ChangeRecordActionKind = 'execution' | 'verification'
export type ChangeRecordOutcome = 'succeeded' | 'failed' | 'incomplete' | 'unknown'
export type ChangeRecordRecipeProvenance = 'observed_native_input' | 'configured_exact_match'
export type ChangeRecordApplicability = 'current' | 'stale' | 'unknown'
export type ChangeRecordAssurance = 'caller_asserted' | 'observed_trusted_local' | 'protected_execution' | 'independent_verification'
export type ChangeRecordAvailability = 'local' | 'shared' | 'external' | 'mixed' | 'missing' | 'expired' | 'deleted' | 'denied' | 'integrity_failure'
export type ChangeRecordCoverage = 'complete' | 'partial' | 'unknown'
export type ChangeRecordDrift = 'none_observed' | 'source_changed' | 'setup_changed' | 'recipe_changed' | 'unknown'
export type ChangeRecordDecision = 'missing' | 'proposed' | 'approved' | 'rejected' | 'superseded'
export type ChangeRecordDecisionAuthor = 'agent' | 'human'
export type ChangeRecordUnknown = 'capture_coverage' | 'source_applicability' | 'setup_identity' | 'recipe_identity' | 'result' | 'usage' | 'human_decision'
export type ChangeRecordOmission = 'untracked_content' | 'ignored_content' | 'working_tree_content' | 'index_state' | 'submodule_content' | 'symlink_target'
export type ChangeRecordMedia = 'json' | 'text' | 'json_lines' | 'binary'
export type ChangeRecordRetention = 'ephemeral_local' | 'work_local' | 'shared_approved'

export interface ChangeRecordArtifact {
  digest: string
  size_bytes: number
  media: ChangeRecordMedia
  retention: ChangeRecordRetention
  availability: ChangeRecordAvailability
}

export type ChangeRecordSource =
  | {
      kind: 'native'
      repository: string
      revision: string
      change_id: string
      context_digest: string
      source_generation: number
    }
  | {
      kind: 'git'
      repository: string
      worktree_id: string
      head: string
      tree: string
      base: string
      dirty: boolean
      untracked_count: number
      omissions: ChangeRecordOmission[]
      captured_snapshot: ChangeRecordArtifact | null
      snapshot_coverage: ChangeRecordCoverage
    }

export type ChangeRecordDecisionRef =
  | { kind: 'repository_path'; path: string; line: number | null }
  | { kind: 'commit'; oid: string; path: string | null }
  | { kind: 'work'; work_id: string }
  | { kind: 'captured_event'; session_id: string; event_id: string }
  | { kind: 'prior_work_version'; work_id: string; event_seq: number }

/** Latest observed setup of one session. `null` means the setup event did not report the field. */
export interface ChangeRecordSessionSetup {
  session_id: string
  parent_session_id: string | null
  model: string | null
  permission_mode: string | null
  effort: string | null
}

export interface ChangeRecordSummaryV1 {
  schema_version: 1
  work_id: string
  generation: number
  as_of: number
  intent: { title: string; scope: string[]; scope_omitted: number }
  subject: { source_id: string; source_kind: 'native' | 'git'; source: ChangeRecordSource } | null
  sessions: { count: number; coverage: ChangeRecordCoverage; gaps: number }
  session_setups: ChangeRecordSessionSetup[]
  session_setups_omitted: number
  latest_action: {
    action_id: string
    source_id: string
    kind: ChangeRecordActionKind
    outcome: ChangeRecordOutcome
    recipe_provenance: ChangeRecordRecipeProvenance
    recipe_digest: string
    setup_digest: string
    result: ChangeRecordResult
    applicability: ChangeRecordApplicability
    assurance: ChangeRecordAssurance
    evidence_availability: ChangeRecordAvailability
    coverage: ChangeRecordCoverage
    started_at: number
    finished_at: number | null
    drift: ChangeRecordDrift
  } | null
  decision: {
    status: ChangeRecordDecision
    reference: string | null
    summary: string | null
    source_refs: ChangeRecordDecisionRef[]
    source_refs_omitted: number
    author_kind: ChangeRecordDecisionAuthor | null
    author_agent: string | null
    at: number | null
    generation: number | null
  }
  next_action: string | null
  completeness: {
    events: ChangeRecordCoverage
    source: ChangeRecordCoverage
    setup: ChangeRecordCoverage
    recipe: ChangeRecordCoverage
    result: ChangeRecordCoverage
    usage: ChangeRecordCoverage
  }
  unknowns: ChangeRecordUnknown[]
}

export interface ChangeRecordResponseV1 {
  version: 1
  availability: 'available'
  summary: ChangeRecordSummaryV1
}

export class InvalidChangeRecordResponse extends Error {}

const RESULT = new Set<ChangeRecordResult>(['passed', 'failed', 'incomplete', 'unknown'])
const ACTION_KIND = new Set<ChangeRecordActionKind>(['execution', 'verification'])
const OUTCOME = new Set<ChangeRecordOutcome>(['succeeded', 'failed', 'incomplete', 'unknown'])
const RECIPE_PROVENANCE = new Set<ChangeRecordRecipeProvenance>(['observed_native_input', 'configured_exact_match'])
const APPLICABILITY = new Set<ChangeRecordApplicability>(['current', 'stale', 'unknown'])
const ASSURANCE = new Set<ChangeRecordAssurance>(['caller_asserted', 'observed_trusted_local', 'protected_execution', 'independent_verification'])
const AVAILABILITY = new Set<ChangeRecordAvailability>(['local', 'shared', 'external', 'mixed', 'missing', 'expired', 'deleted', 'denied', 'integrity_failure'])
const COVERAGE = new Set<ChangeRecordCoverage>(['complete', 'partial', 'unknown'])
const DRIFT = new Set<ChangeRecordDrift>(['none_observed', 'source_changed', 'setup_changed', 'recipe_changed', 'unknown'])
const DECISION = new Set<ChangeRecordDecision>(['missing', 'proposed', 'approved', 'rejected', 'superseded'])
const DECISION_AUTHOR = new Set<ChangeRecordDecisionAuthor>(['agent', 'human'])
const UNKNOWN = new Set<ChangeRecordUnknown>(['capture_coverage', 'source_applicability', 'setup_identity', 'recipe_identity', 'result', 'usage', 'human_decision'])
const SUBJECT_KIND = new Set(['native', 'git'] as const)
const OMISSION = new Set<ChangeRecordOmission>(['untracked_content', 'ignored_content', 'working_tree_content', 'index_state', 'submodule_content', 'symlink_target'])
const MEDIA = new Set<ChangeRecordMedia>(['json', 'text', 'json_lines', 'binary'])
const RETENTION = new Set<ChangeRecordRetention>(['ephemeral_local', 'work_local', 'shared_approved'])

const fail = (field: string): never => {
  throw new InvalidChangeRecordResponse(`Invalid change record response (${field})`)
}
const record = (value: unknown, field: string): Record<string, unknown> =>
  value !== null && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, unknown>) : fail(field)
const integer = (value: unknown, field: string, minimum = 0, maximum = Number.MAX_SAFE_INTEGER): number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= minimum && value <= maximum ? value : fail(field)
const boolean = (value: unknown, field: string): boolean => (typeof value === 'boolean' ? value : fail(field))
const identifier = (value: unknown, field: string): string => {
  const result = typeof value === 'string' ? value : fail(field)
  if (result.length === 0 || textEncoder.encode(result).byteLength > MAX_IDENTIFIER_BYTES || !/^[A-Za-z0-9._:@-]+$/.test(result)) fail(field)
  return result
}
const boundedText = (value: unknown, field: string, maxBytes = 500): string => {
  const result = typeof value === 'string' ? value : fail(field)
  if (result.length === 0 || textEncoder.encode(result).byteLength > maxBytes || hiddenText.test(result)) fail(field)
  return result
}
const nullableText = (value: unknown, field: string, maxBytes = 500): string | null => (value === null ? null : boundedText(value, field, maxBytes))
const nullableIdentifier = (value: unknown, field: string): string | null => (value === null ? null : identifier(value, field))
const digest = (value: unknown, field: string): string =>
  typeof value === 'string' && /^[0-9a-f]{64}$/.test(value) ? value : fail(field)
const oid = (value: unknown, field: string): string =>
  typeof value === 'string' && /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(value) ? value : fail(field)
const repository = (value: unknown, field: string): string => {
  const result = typeof value === 'string' ? value : fail(field)
  const segments = result.split('/')
  if (textEncoder.encode(result).byteLength > MAX_IDENTIFIER_BYTES || segments.length < 2 || segments.some((part) => !part || part === '.' || part === '..' || !/^[A-Za-z0-9._-]+$/.test(part))) fail(field)
  return result
}
const pathSpec = (value: unknown, field: string): string => {
  const result = boundedText(value, field, MAX_PATH_BYTES)
  const path = result.endsWith('/**') ? result.slice(0, -3) : result
  if (result.startsWith('/') || result.includes('\\') || path.includes('*') || path.split('/').some((part) => !part || part === '.' || part === '..') || /^[A-Za-z]:/.test(result)) fail(field)
  return result
}
const timestamp = (value: unknown, field: string): number => integer(value, field, 0, 8_640_000_000_000)
const nullableTimestamp = (value: unknown, field: string): number | null => (value === null ? null : timestamp(value, field))
const nullablePositiveInteger = (value: unknown, field: string): number | null => (value === null ? null : integer(value, field, 1))
const enumValue = <T extends string>(value: unknown, allowed: Set<T>, field: string): T =>
  typeof value === 'string' && allowed.has(value as T) ? (value as T) : fail(field)

function artifact(value: unknown, field: string): ChangeRecordArtifact {
  const item = record(value, field)
  return {
    digest: digest(item['digest'], `${field}.digest`),
    size_bytes: integer(item['size_bytes'], `${field}.size_bytes`, 0, MAX_ARTIFACT_BYTES),
    media: enumValue(item['media'], MEDIA, `${field}.media`),
    retention: enumValue(item['retention'], RETENTION, `${field}.retention`),
    availability: enumValue(item['availability'], AVAILABILITY, `${field}.availability`),
  }
}

function sourceBinding(value: unknown, field: string): ChangeRecordSource {
  const item = record(value, field)
  const kind = item['kind']
  if (kind === 'native') {
    return {
      kind,
      repository: repository(item['repository'], `${field}.repository`),
      revision: digest(item['revision'], `${field}.revision`),
      change_id: identifier(item['change_id'], `${field}.change_id`),
      context_digest: digest(item['context_digest'], `${field}.context_digest`),
      source_generation: integer(item['source_generation'], `${field}.source_generation`, 1),
    }
  }
  if (kind === 'git') {
    const rawOmissions = Array.isArray(item['omissions']) ? item['omissions'] : fail(`${field}.omissions`)
    if (rawOmissions.length > 8) fail(`${field}.omissions`)
    const omissions = rawOmissions.map((value, index) => enumValue(value, OMISSION, `${field}.omissions[${index}]`))
    const capturedSnapshot = item['captured_snapshot'] === null ? null : artifact(item['captured_snapshot'], `${field}.captured_snapshot`)
    const dirty = boolean(item['dirty'], `${field}.dirty`)
    const untrackedCount = integer(item['untracked_count'], `${field}.untracked_count`)
    const snapshotCoverage = enumValue(item['snapshot_coverage'], COVERAGE, `${field}.snapshot_coverage`)
    if ((dirty || untrackedCount > 0) && snapshotCoverage === 'complete' && capturedSnapshot === null) fail(`${field}.captured_snapshot`)
    return {
      kind,
      repository: repository(item['repository'], `${field}.repository`),
      worktree_id: identifier(item['worktree_id'], `${field}.worktree_id`),
      head: oid(item['head'], `${field}.head`),
      tree: oid(item['tree'], `${field}.tree`),
      base: oid(item['base'], `${field}.base`),
      dirty,
      untracked_count: untrackedCount,
      omissions,
      captured_snapshot: capturedSnapshot,
      snapshot_coverage: snapshotCoverage,
    }
  }
  return fail(`${field}.kind`)
}

function decisionRef(value: unknown, field: string): ChangeRecordDecisionRef {
  const item = record(value, field)
  if (item['kind'] === 'repository_path') {
    return {
      kind: 'repository_path',
      path: pathSpec(item['path'], `${field}.path`),
      line: item['line'] === null ? null : integer(item['line'], `${field}.line`, 1),
    }
  }
  if (item['kind'] === 'commit') {
    return {
      kind: 'commit',
      oid: oid(item['oid'], `${field}.oid`),
      path: item['path'] === null ? null : pathSpec(item['path'], `${field}.path`),
    }
  }
  if (item['kind'] === 'work') {
    const workId = item['work_id']
    return { kind: 'work', work_id: typeof workId === 'string' && validWorkId(workId) ? workId : fail(`${field}.work_id`) }
  }
  if (item['kind'] === 'captured_event') {
    return {
      kind: 'captured_event',
      session_id: identifier(item['session_id'], `${field}.session_id`),
      event_id: identifier(item['event_id'], `${field}.event_id`),
    }
  }
  if (item['kind'] === 'prior_work_version') {
    const workId = item['work_id']
    return {
      kind: 'prior_work_version',
      work_id: typeof workId === 'string' && validWorkId(workId) ? workId : fail(`${field}.work_id`),
      event_seq: integer(item['event_seq'], `${field}.event_seq`, 1),
    }
  }
  return fail(`${field}.kind`)
}

export function decodeChangeRecordResponse(value: unknown, expectedWorkId?: string, expectedRepository?: string): ChangeRecordResponseV1 {
  const envelope = record(value, 'response')
  if (envelope['version'] !== 1) fail('version')
  if (envelope['availability'] !== 'available') fail('availability')
  const raw = record(envelope['summary'], 'summary')
  if (raw['schema_version'] !== 1) fail('summary.schema_version')
  const workId = typeof raw['work_id'] === 'string' && validWorkId(raw['work_id']) ? raw['work_id'] : fail('summary.work_id')
  if (expectedWorkId !== undefined && workId !== expectedWorkId) fail('summary.work_id_mismatch')

  const rawIntent = record(raw['intent'], 'summary.intent')
  const rawScope: unknown[] = Array.isArray(rawIntent['scope']) ? rawIntent['scope'] : fail('summary.intent.scope')
  if (rawScope.length > MAX_SCOPE) fail('summary.intent.scope')
  const intent = {
    title: boundedText(rawIntent['title'], 'summary.intent.title'),
    scope: rawScope.map((value, index) => pathSpec(value, `summary.intent.scope[${index}]`)),
    scope_omitted: integer(rawIntent['scope_omitted'], 'summary.intent.scope_omitted'),
  }

  const rawSubject = raw['subject']
  const subject = rawSubject === null
    ? null
    : (() => {
        const item = record(rawSubject, 'summary.subject')
        const sourceKind = enumValue(item['source_kind'], SUBJECT_KIND, 'summary.subject.source_kind')
        const source = sourceBinding(item['source'], 'summary.subject.source')
        if (source.kind !== sourceKind) fail('summary.subject.source_kind_mismatch')
        if (expectedRepository !== undefined && source.repository !== expectedRepository) fail('summary.subject.repository_mismatch')
        return {
          source_id: digest(item['source_id'], 'summary.subject.source_id'),
          source_kind: sourceKind,
          source,
        }
      })()

  const rawSessions = record(raw['sessions'], 'summary.sessions')
  // The Hub omits both fields when no session reported setup; absent decodes to none observed.
  const rawSetups = raw['session_setups'] === undefined ? [] : raw['session_setups']
  const setupsArray: unknown[] = Array.isArray(rawSetups) ? rawSetups : fail('summary.session_setups')
  if (setupsArray.length > MAX_SESSION_SETUPS) fail('summary.session_setups')
  const sessionSetups = setupsArray.map((value, index): ChangeRecordSessionSetup => {
    const field = `summary.session_setups[${index}]`
    const item = record(value, field)
    return {
      session_id: identifier(item['session_id'], `${field}.session_id`),
      parent_session_id: nullableIdentifier(item['parent_session_id'], `${field}.parent_session_id`),
      model: nullableIdentifier(item['model'], `${field}.model`),
      permission_mode: nullableIdentifier(item['permission_mode'], `${field}.permission_mode`),
      effort: nullableIdentifier(item['effort'], `${field}.effort`),
    }
  })
  const sessionSetupsOmitted = raw['session_setups_omitted'] === undefined ? 0 : integer(raw['session_setups_omitted'], 'summary.session_setups_omitted')
  const rawAction = raw['latest_action']
  const latestAction = rawAction === null
    ? null
    : (() => {
        const item = record(rawAction, 'summary.latest_action')
        const kind = enumValue(item['kind'], ACTION_KIND, 'summary.latest_action.kind')
        const outcome = enumValue(item['outcome'], OUTCOME, 'summary.latest_action.outcome')
        const recipeProvenance = enumValue(item['recipe_provenance'], RECIPE_PROVENANCE, 'summary.latest_action.recipe_provenance')
        const result = enumValue(item['result'], RESULT, 'summary.latest_action.result')
        if (kind === 'execution' && result !== 'unknown') fail('summary.latest_action.execution_result')
        if (result !== 'unknown' && (kind !== 'verification' || recipeProvenance !== 'configured_exact_match')) fail('summary.latest_action.verification_result')
        return {
          action_id: identifier(item['action_id'], 'summary.latest_action.action_id'),
          source_id: digest(item['source_id'], 'summary.latest_action.source_id'),
          kind,
          outcome,
          recipe_provenance: recipeProvenance,
          recipe_digest: digest(item['recipe_digest'], 'summary.latest_action.recipe_digest'),
          setup_digest: digest(item['setup_digest'], 'summary.latest_action.setup_digest'),
          result,
          applicability: enumValue(item['applicability'], APPLICABILITY, 'summary.latest_action.applicability'),
          assurance: enumValue(item['assurance'], ASSURANCE, 'summary.latest_action.assurance'),
          evidence_availability: enumValue(item['evidence_availability'], AVAILABILITY, 'summary.latest_action.evidence_availability'),
          coverage: enumValue(item['coverage'], COVERAGE, 'summary.latest_action.coverage'),
          started_at: timestamp(item['started_at'], 'summary.latest_action.started_at'),
          finished_at: nullableTimestamp(item['finished_at'], 'summary.latest_action.finished_at'),
          drift: enumValue(item['drift'], DRIFT, 'summary.latest_action.drift'),
        }
      })()
  if (latestAction && !subject) fail('summary.latest_action.subject_missing')
  if (subject && latestAction && subject.source_id !== latestAction.source_id) fail('summary.latest_action.source_id_mismatch')

  const rawDecision = record(raw['decision'], 'summary.decision')
  const decisionStatus = enumValue(rawDecision['status'], DECISION, 'summary.decision.status')
  const rawDecisionRefs: unknown[] = Array.isArray(rawDecision['source_refs']) ? rawDecision['source_refs'] : fail('summary.decision.source_refs')
  if (rawDecisionRefs.length > MAX_SOURCE_REFS) fail('summary.decision.source_refs')
  const decision = {
    status: decisionStatus,
    reference: nullableIdentifier(rawDecision['reference'], 'summary.decision.reference'),
    summary: nullableText(rawDecision['summary'], 'summary.decision.summary'),
    source_refs: rawDecisionRefs.map((value, index) => decisionRef(value, `summary.decision.source_refs[${index}]`)),
    source_refs_omitted: integer(rawDecision['source_refs_omitted'], 'summary.decision.source_refs_omitted'),
    author_kind: rawDecision['author_kind'] === null ? null : enumValue(rawDecision['author_kind'], DECISION_AUTHOR, 'summary.decision.author_kind'),
    author_agent: nullableIdentifier(rawDecision['author_agent'], 'summary.decision.author_agent'),
    at: nullableTimestamp(rawDecision['at'], 'summary.decision.at'),
    generation: nullablePositiveInteger(rawDecision['generation'], 'summary.decision.generation'),
  }
  if (decision.author_kind === 'agent' && decision.author_agent === null) fail('summary.decision.author_agent')
  if (decision.status === 'missing' && (decision.reference !== null || decision.summary !== null || decision.source_refs.length > 0 || decision.source_refs_omitted > 0 || decision.author_kind !== null || decision.author_agent !== null || decision.at !== null || decision.generation !== null)) fail('summary.decision.missing_fields')

  const rawCompleteness = record(raw['completeness'], 'summary.completeness')
  const completeness = {
    events: enumValue(rawCompleteness['events'], COVERAGE, 'summary.completeness.events'),
    source: enumValue(rawCompleteness['source'], COVERAGE, 'summary.completeness.source'),
    setup: enumValue(rawCompleteness['setup'], COVERAGE, 'summary.completeness.setup'),
    recipe: enumValue(rawCompleteness['recipe'], COVERAGE, 'summary.completeness.recipe'),
    result: enumValue(rawCompleteness['result'], COVERAGE, 'summary.completeness.result'),
    usage: enumValue(rawCompleteness['usage'], COVERAGE, 'summary.completeness.usage'),
  }
  if (completeness.setup === 'complete' || completeness.recipe === 'complete') fail('summary.completeness.provenance')

  const rawUnknowns = raw['unknowns']
  const unknownsArray: unknown[] = Array.isArray(rawUnknowns) ? rawUnknowns : fail('summary.unknowns')
  if (unknownsArray.length > 16) fail('summary.unknowns')
  const unknowns = unknownsArray.map((item, index) => enumValue(item, UNKNOWN, `summary.unknowns[${index}]`))
  const nextAction = raw['next_action'] === null ? null : boundedText(raw['next_action'], 'summary.next_action')

  return {
    version: 1,
    availability: 'available',
    summary: {
      schema_version: 1,
      work_id: workId,
      generation: integer(raw['generation'], 'summary.generation', 1),
      as_of: timestamp(raw['as_of'], 'summary.as_of'),
      intent,
      subject,
      sessions: {
        count: integer(rawSessions['count'], 'summary.sessions.count'),
        coverage: enumValue(rawSessions['coverage'], COVERAGE, 'summary.sessions.coverage'),
        gaps: integer(rawSessions['gaps'], 'summary.sessions.gaps'),
      },
      session_setups: sessionSetups,
      session_setups_omitted: sessionSetupsOmitted,
      latest_action: latestAction,
      decision,
      next_action: nextAction,
      completeness,
      unknowns,
    },
  }
}

async function boundedJson(path: string, token: string, signal: AbortSignal): Promise<unknown> {
  const response = await fetch(path, {
    signal: withTimeout(signal),
    cache: 'no-store',
    credentials: 'omit',
    headers: { Authorization: `Bearer ${token}` },
  })
  if (!response.ok) throw new ApiError(response.status, `HTTP ${response.status}`)
  const declared = Number(response.headers.get('content-length'))
  if (Number.isFinite(declared) && declared > MAX_RESPONSE_BYTES) {
    await response.body?.cancel()
    throw new InvalidChangeRecordResponse('Change record response is too large')
  }
  if (!response.body) throw new InvalidChangeRecordResponse('Change record response has no body')
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
        throw new InvalidChangeRecordResponse('Change record response is too large')
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
    throw new InvalidChangeRecordResponse('Change record response is not valid JSON')
  }
}

export async function fetchChangeRecord(repo: string, workId: string, token: string, signal: AbortSignal): Promise<ChangeRecordResponseV1> {
  if (!validWorkId(workId)) throw new InvalidChangeRecordResponse('Invalid work ID')
  const value = await boundedJson(`/v1/hub/repos/${encodeURIComponent(repo)}/change-records/${encodeURIComponent(workId)}`, token, signal)
  return decodeChangeRecordResponse(value, workId, repo)
}
