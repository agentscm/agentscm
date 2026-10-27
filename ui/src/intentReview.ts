import { localCredential, loadLocalBase } from './localApi.ts'
import { onFrontDoor } from './frontDoor.ts'

type Json = Record<string, unknown>
const rec = (value: unknown): Json => value && typeof value === 'object' && !Array.isArray(value) ? value as Json : {}
const list = (value: unknown, limit: number): unknown[] => {
  if (!Array.isArray(value) || value.length > limit) throw new Error('Invalid intent review list')
  return value
}
const requiredText = (value: unknown, field: string, max = 4096): string => {
  if (typeof value !== 'string' || value.length > max) throw new Error(`Invalid intent review ${field}`)
  return value
}
const optionalText = (value: unknown, field: string, max = 4096): string | null => value === null ? null : requiredText(value, field, max)
const bool = (value: unknown, field: string): boolean => {
  if (typeof value !== 'boolean') throw new Error(`Invalid intent review ${field}`)
  return value
}
const integer = (value: unknown, field: string): number => {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) throw new Error(`Invalid intent review ${field}`)
  return value
}
const oneOf = <T extends string>(value: unknown, values: readonly T[], field: string): T => {
  if (typeof value !== 'string' || !values.includes(value as T)) throw new Error(`Invalid intent review ${field}`)
  return value as T
}
const strings = (value: unknown, field: string, maxItems = 128, maxText = 2048): string[] => {
  if (!Array.isArray(value) || value.length > maxItems) throw new Error(`Invalid intent review ${field}`)
  return value.map((item) => requiredText(item, field, maxText))
}
const digest = (value: unknown, field: string): string => {
  const text = requiredText(value, field, 64)
  if (!/^[0-9a-f]{64}$/.test(text)) throw new Error(`Invalid intent review ${field}`)
  return text
}

export type Coverage = 'complete' | 'partial' | 'unknown'
export type FindingStatus = 'supported' | 'mismatch' | 'unknown'
export type IntentOutcome = 'meets_recorded_criteria' | 'needs_correction' | 'needs_evidence' | 'operational_error' | 'stale' | 'interrupted' | 'disputed' | 'exception'
export type IntentNext = 'assess' | 'correct' | 'investigate' | 'in_progress' | 'complete' | 'human_disposition' | 'recovery_required' | 'budget_exhausted' | 'cancelled'

export interface IntentReference {
  id: string; kind: string; source: string; author: string; recorded_at_ms: number; sha256: string
  coverage: Coverage; text: string | null; reconstructed: boolean
}
export interface Criterion { id: string; description: string; sources: string[]; required: boolean; constraint: boolean }
export interface Finding { criterion: string; status: FindingStatus; explanation: string; evidence_ids: string[]; corrective_outcome: string | null; recheck: string | null }
export interface Evidence { id: string; sha256: string; origin: string; candidate_digest: string | null; coverage: Coverage; assurance: string; available: boolean }
export interface Source {
  kind: 'git'; repository: string; worktree_id: string; head: string; tree: string; base: string; dirty: boolean; untracked_count: number
  omissions: string[]; captured_snapshot: { digest: string; size_bytes: number; availability: string } | null; snapshot_coverage: Coverage
}
export interface Subject {
  repository: string; work_id: string; root_work_id: string; contract_digest: string
  scope: { parent_work_id: string | null; checkpoint: string | null; criteria: string[]; coverage: Coverage; gaps: string[]; source_exclusions: { omission: string; reason: string; authorization_reference: string }[] }
  source: Source; evidence: Evidence[]; skill: { workflow_version_id: string; step_id: string; skill_version_id: string; skill_sha256: string; package_digest: string; entrypoint: string }
}
export interface Attempt {
  attempt_id: string; fence: number; phase: 'assessment' | 'correction'; subject: Subject; started_at_ms: number
  lease_expires_at_ms: number; state: 'leased' | 'finished' | 'cancelled' | 'recovered'; requested_model: string | null
  observed_model: string | null; execution: 'succeeded' | 'failed' | 'cancelled' | 'uncertain' | null
  assessment: { criteria: Finding[]; coverage: Coverage; gaps: string[] } | null; resulting_candidate_digest: string | null; detail: string | null
}
export interface IntentReview {
  record: { subject: Subject; contracts: { version: number; previous_digest: string | null; request: IntentReference[]; amendments: IntentReference[]; criteria: Criterion[]; focus: IntentReference[]; coverage: Coverage; gaps: string[] }[]
    limits: { max_correction_rounds: number; max_invocations: number; wall_time_ms: number }; started_at_ms: number; invocations: number
    correction_rounds: number; cancelled: boolean; active_work_id: string | null; attempts: Attempt[]; dispositions: { kind: string; criteria: string[]; reason: string; authorization: IntentReference; subject_digest: string }[] }
  current: boolean; current_subject_digest: string; outcome: IntentOutcome | null; next: IntentNext; root_invocations: number; root_correction_rounds: number; admission: string
}

const COVERAGE = ['complete', 'partial', 'unknown'] as const
const OUTCOMES = ['meets_recorded_criteria', 'needs_correction', 'needs_evidence', 'operational_error', 'stale', 'interrupted', 'disputed', 'exception'] as const
const NEXT = ['assess', 'correct', 'investigate', 'in_progress', 'complete', 'human_disposition', 'recovery_required', 'budget_exhausted', 'cancelled'] as const

function reference(value: unknown, field: string): IntentReference {
  const r = rec(value)
  const text = r['text'] === null ? null : requiredText(r['text'], `${field}.text`, 16 * 1024)
  return { id: requiredText(r['id'], `${field}.id`, 256), kind: requiredText(r['kind'], `${field}.kind`, 64), source: requiredText(r['source'], `${field}.source`, 2048), author: requiredText(r['author'], `${field}.author`, 256), recorded_at_ms: integer(r['recorded_at_ms'], `${field}.recorded_at_ms`), sha256: digest(r['sha256'], `${field}.sha256`), coverage: oneOf(r['coverage'], COVERAGE, `${field}.coverage`), text, reconstructed: bool(r['reconstructed'], `${field}.reconstructed`) }
}
function criterion(value: unknown, field: string): Criterion {
  const r = rec(value)
  return { id: requiredText(r['id'], `${field}.id`, 256), description: requiredText(r['description'], `${field}.description`, 2048), sources: strings(r['sources'], `${field}.sources`, 16, 256), required: bool(r['required'], `${field}.required`), constraint: bool(r['constraint'], `${field}.constraint`) }
}
function finding(value: unknown, field: string): Finding {
  const r = rec(value)
  return { criterion: requiredText(r['criterion'], `${field}.criterion`, 256), status: oneOf(r['status'], ['supported', 'mismatch', 'unknown'] as const, `${field}.status`), explanation: requiredText(r['explanation'], `${field}.explanation`, 2048), evidence_ids: strings(r['evidence_ids'], `${field}.evidence_ids`, 16, 256), corrective_outcome: optionalText(r['corrective_outcome'], `${field}.corrective_outcome`, 2048), recheck: optionalText(r['recheck'], `${field}.recheck`, 2048) }
}
function subject(value: unknown, field: string): Subject {
  const r = rec(value), scope = rec(r['scope']), source = rec(r['source']), skill = rec(r['skill'])
  if (source['kind'] !== 'git') throw new Error(`Invalid intent review ${field}.source.kind`)
  const snapshot = source['captured_snapshot'] === null ? null : rec(source['captured_snapshot'])
  return {
    repository: requiredText(r['repository'], `${field}.repository`, 256), work_id: requiredText(r['work_id'], `${field}.work_id`, 256), root_work_id: requiredText(r['root_work_id'], `${field}.root_work_id`, 256), contract_digest: digest(r['contract_digest'], `${field}.contract_digest`),
    scope: { parent_work_id: optionalText(scope['parent_work_id'], `${field}.scope.parent_work_id`, 256), checkpoint: optionalText(scope['checkpoint'], `${field}.scope.checkpoint`, 256), criteria: strings(scope['criteria'], `${field}.scope.criteria`, 64, 256), coverage: oneOf(scope['coverage'], COVERAGE, `${field}.scope.coverage`), gaps: strings(scope['gaps'], `${field}.scope.gaps`), source_exclusions: list(scope['source_exclusions'], 8).map((v) => { const e = rec(v); return { omission: requiredText(e['omission'], `${field}.source_exclusions.omission`, 64), reason: requiredText(e['reason'], `${field}.source_exclusions.reason`, 2048), authorization_reference: requiredText(e['authorization_reference'], `${field}.source_exclusions.authorization_reference`, 256) } }) },
    source: { kind: 'git', repository: requiredText(source['repository'], `${field}.source.repository`, 256), worktree_id: requiredText(source['worktree_id'], `${field}.source.worktree_id`, 256), head: requiredText(source['head'], `${field}.source.head`, 64), tree: requiredText(source['tree'], `${field}.source.tree`, 64), base: requiredText(source['base'], `${field}.source.base`, 64), dirty: bool(source['dirty'], `${field}.source.dirty`), untracked_count: integer(source['untracked_count'], `${field}.source.untracked_count`), omissions: strings(source['omissions'], `${field}.source.omissions`, 8, 64), captured_snapshot: snapshot === null ? null : { digest: digest(snapshot['digest'], `${field}.source.snapshot.digest`), size_bytes: integer(snapshot['size_bytes'], `${field}.source.snapshot.size_bytes`), availability: requiredText(snapshot['availability'], `${field}.source.snapshot.availability`, 64) }, snapshot_coverage: oneOf(source['snapshot_coverage'], COVERAGE, `${field}.source.snapshot_coverage`) },
    evidence: list(r['evidence'], 128).map((v, i) => { const e = rec(v); return { id: requiredText(e['id'], `${field}.evidence[${i}].id`, 256), sha256: digest(e['sha256'], `${field}.evidence[${i}].sha256`), origin: requiredText(e['origin'], `${field}.evidence[${i}].origin`, 2048), candidate_digest: e['candidate_digest'] === null ? null : digest(e['candidate_digest'], `${field}.evidence[${i}].candidate_digest`), coverage: oneOf(e['coverage'], COVERAGE, `${field}.evidence[${i}].coverage`), assurance: requiredText(e['assurance'], `${field}.evidence[${i}].assurance`, 64), available: bool(e['available'], `${field}.evidence[${i}].available`) } }),
    skill: { workflow_version_id: requiredText(skill['workflow_version_id'], `${field}.skill.workflow_version_id`, 256), step_id: requiredText(skill['step_id'], `${field}.skill.step_id`, 256), skill_version_id: requiredText(skill['skill_version_id'], `${field}.skill.skill_version_id`, 256), skill_sha256: digest(skill['skill_sha256'], `${field}.skill.skill_sha256`), package_digest: digest(skill['package_digest'], `${field}.skill.package_digest`), entrypoint: requiredText(skill['entrypoint'], `${field}.skill.entrypoint`, 1024) },
  }
}
function assessment(value: unknown, field: string): Attempt['assessment'] {
  if (value === null) return null
  const r = rec(value)
  return { criteria: list(r['criteria'], 64).map((v, i) => finding(v, `${field}.criteria[${i}]`)), coverage: oneOf(r['coverage'], COVERAGE, `${field}.coverage`), gaps: strings(r['gaps'], `${field}.gaps`) }
}
function attempt(value: unknown, index: number): Attempt {
  const field = `record.attempts[${index}]`, r = rec(value)
  return { attempt_id: requiredText(r['attempt_id'], `${field}.attempt_id`, 256), fence: integer(r['fence'], `${field}.fence`), phase: oneOf(r['phase'], ['assessment', 'correction'] as const, `${field}.phase`), subject: subject(r['subject'], `${field}.subject`), started_at_ms: integer(r['started_at_ms'], `${field}.started_at_ms`), lease_expires_at_ms: integer(r['lease_expires_at_ms'], `${field}.lease_expires_at_ms`), state: oneOf(r['state'], ['leased', 'finished', 'cancelled', 'recovered'] as const, `${field}.state`), requested_model: optionalText(r['requested_model'], `${field}.requested_model`, 256), observed_model: optionalText(r['observed_model'], `${field}.observed_model`, 256), execution: r['execution'] === null ? null : oneOf(r['execution'], ['succeeded', 'failed', 'cancelled', 'uncertain'] as const, `${field}.execution`), assessment: assessment(r['assessment'], field), resulting_candidate_digest: r['resulting_candidate_digest'] === null ? null : digest(r['resulting_candidate_digest'], `${field}.resulting_candidate_digest`), detail: optionalText(r['detail'], `${field}.detail`, 2048) }
}

export function decodeIntentReview(value: unknown): IntentReview {
  const root = rec(value), record = rec(root['record'])
  const contracts = list(record['contracts'], 64).map((value, index) => {
    const r = rec(value), field = `record.contracts[${index}]`
    return { version: integer(r['version'], `${field}.version`), previous_digest: r['previous_digest'] === null ? null : digest(r['previous_digest'], `${field}.previous_digest`), request: list(r['request'], 128).map((v, i) => reference(v, `${field}.request[${i}]`)), amendments: list(r['amendments'], 128).map((v, i) => reference(v, `${field}.amendments[${i}]`)), criteria: list(r['criteria'], 64).map((v, i) => criterion(v, `${field}.criteria[${i}]`)), focus: list(r['focus'], 128).map((v, i) => reference(v, `${field}.focus[${i}]`)), coverage: oneOf(r['coverage'], COVERAGE, `${field}.coverage`), gaps: strings(r['gaps'], `${field}.gaps`) }
  })
  const limits = rec(record['limits'])
  const dispositions = list(record['dispositions'], 64).map((value, index) => { const r = rec(value), field = `record.dispositions[${index}]`; return { kind: requiredText(r['kind'], `${field}.kind`, 32), criteria: strings(r['criteria'], `${field}.criteria`, 64, 256), reason: requiredText(r['reason'], `${field}.reason`, 2048), authorization: reference(r['authorization'], `${field}.authorization`), subject_digest: digest(r['subject_digest'], `${field}.subject_digest`) } })
  const view: IntentReview = {
    record: { subject: subject(record['subject'], 'record.subject'), contracts, limits: { max_correction_rounds: integer(limits['max_correction_rounds'], 'record.limits.max_correction_rounds'), max_invocations: integer(limits['max_invocations'], 'record.limits.max_invocations'), wall_time_ms: integer(limits['wall_time_ms'], 'record.limits.wall_time_ms') }, started_at_ms: integer(record['started_at_ms'], 'record.started_at_ms'), invocations: integer(record['invocations'], 'record.invocations'), correction_rounds: integer(record['correction_rounds'], 'record.correction_rounds'), cancelled: bool(record['cancelled'], 'record.cancelled'), active_work_id: optionalText(record['active_work_id'], 'record.active_work_id', 256), attempts: list(record['attempts'], 64).map(attempt), dispositions },
    current: bool(root['current'], 'current'), current_subject_digest: digest(root['current_subject_digest'], 'current_subject_digest'), outcome: root['outcome'] === null ? null : oneOf(root['outcome'], OUTCOMES, 'outcome'), next: oneOf(root['next'], NEXT, 'next'), root_invocations: integer(root['root_invocations'], 'root_invocations'), root_correction_rounds: integer(root['root_correction_rounds'], 'root_correction_rounds'), admission: 'not_assessed',
  }
  if (!contracts.length) throw new Error('Invalid intent review contract history')
  const contract = contracts.at(-1)!
  const criteria = new Set(contract.criteria.map((item) => item.id))
  if (criteria.size !== contract.criteria.length || view.record.subject.scope.criteria.some((id) => !criteria.has(id))) throw new Error('Invalid intent review criterion binding')
  const humanRefs = [...contract.request, ...contract.amendments]
  const humanIds = new Set(humanRefs.map((item) => item.id))
  if (humanIds.size !== humanRefs.length || contract.criteria.some((item) => !item.sources.length || item.sources.some((id) => !humanIds.has(id)))) throw new Error('Invalid intent review human provenance')
  const latestAssessment = [...view.record.attempts].reverse().find((item) => item.phase === 'assessment' && item.assessment)
  const latestAttempt = view.record.attempts.at(-1)
  if (latestAssessment?.assessment) {
    const findings = latestAssessment.assessment.criteria
    const assessedCriteria = latestAssessment.subject.scope.criteria
    if (new Set(findings.map((item) => item.criterion)).size !== findings.length || findings.length !== assessedCriteria.length || findings.some((item) => !assessedCriteria.includes(item.criterion))) throw new Error('Invalid intent review finding coverage')
    const cited = new Map(latestAssessment.subject.evidence.map((item) => [item.id, item]))
    if (findings.some((item) => item.evidence_ids.some((id) => !cited.has(id)))) throw new Error('Invalid intent review evidence reference')
    if (findings.some((item) => item.status !== 'unknown' && (!item.evidence_ids.length || item.evidence_ids.some((id) => {
      const evidence = cited.get(id)!
      return !evidence.available || evidence.coverage !== 'complete' || !latestAssessment.resulting_candidate_digest || evidence.candidate_digest !== latestAssessment.resulting_candidate_digest
    })))) throw new Error('Intent review finding claims support without current complete evidence')
    if (findings.some((item) => item.status === 'mismatch' && (!item.corrective_outcome || !item.recheck))) throw new Error('Intent review mismatch lacks corrective guidance')
  }
  if (view.outcome === 'meets_recorded_criteria' && (!view.current || latestAttempt?.attempt_id !== latestAssessment?.attempt_id || !latestAssessment?.assessment || latestAssessment.execution !== 'succeeded' || latestAssessment.subject.contract_digest !== view.record.subject.contract_digest || latestAssessment.subject.source.head !== view.record.subject.source.head || latestAssessment.subject.source.tree !== view.record.subject.source.tree || latestAssessment.assessment.coverage !== 'complete' || latestAssessment.assessment.gaps.length > 0 || latestAssessment.assessment.criteria.some((item) => item.status !== 'supported') || contract.coverage !== 'complete' || !contract.request.length || humanRefs.some((item) => item.coverage !== 'complete' || item.reconstructed) || view.record.subject.scope.coverage !== 'complete' || view.record.subject.scope.gaps.length > 0)) throw new Error('Intent review claims support without complete current findings')
  return view
}

export class IntentReviewError extends Error {
  readonly status?: number
  constructor(message: string, status?: number) {
    super(message)
    this.status = status
  }
}

export async function fetchIntentReview(workId: string, signal: AbortSignal): Promise<IntentReview | null> {
  if (!onFrontDoor()) throw new IntentReviewError('Intent review is available only from this computer’s AgentSCM page.', 403)
  if (loadLocalBase()) throw new IntentReviewError('Intent review is available only from the selected local checkout.', 403)
  const token = localCredential()
  if (!token) throw new IntentReviewError('Open this repository with `agentscm open` to read its local intent review.', 401)
  const path = `/v1/local/intent-reviews/${encodeURIComponent(workId)}`
  let response: Response
  try {
    response = await fetch(path, { signal, cache: 'no-store', credentials: 'omit', headers: { Authorization: `Bearer ${token}` } })
  } catch (error) {
    if (signal.aborted) throw error
    throw new IntentReviewError('The local intent review service is unavailable.')
  }
  if (response.status === 404) return null
  if (!response.ok) throw new IntentReviewError(response.status === 401 ? 'The local server refused this read token.' : response.status === 403 ? 'The local server refused access to this checkout.' : `The local intent review could not be read (HTTP ${response.status}).`, response.status)
  try { return decodeIntentReview(await response.json()) }
  catch (error) { throw new IntentReviewError(error instanceof Error ? error.message : 'The local server returned an invalid intent review.') }
}

export async function submitIntentDisposition(workId: string, view: IntentReview, kind: 'dispute' | 'exception', criteria: string[], reason: string, signal: AbortSignal): Promise<void> {
  if (!view.current || !view.current_subject_digest) throw new IntentReviewError('Reload this Work before recording a disposition.', 409)
  if (!criteria.length || !reason.trim() || reason.length > 2048) throw new IntentReviewError('Choose at least one criterion and enter a reason.')
  if (!onFrontDoor()) throw new IntentReviewError('Intent disposition is available only from this computer’s AgentSCM page.', 403)
  if (loadLocalBase()) throw new IntentReviewError('Intent disposition is available only from the selected local checkout.', 403)
  const token = localCredential()
  if (!token) throw new IntentReviewError('Open this repository with `agentscm open` to record a disposition.', 401)
  const requestId = `disposition-${globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random().toString(36).slice(2)}`}`
  const now = Date.now()
  const authorization = {
    id: requestId, kind: 'human_message', source: 'local_browser', author: 'local operator', recorded_at_ms: now,
    sha256: '0'.repeat(64), coverage: 'complete', text: reason.trim(), reconstructed: false,
  }
  let response: Response
  try {
    response = await fetch('/v1/local/intent-reviews', {
      method: 'POST', signal, cache: 'no-store', credentials: 'omit',
      headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
      body: JSON.stringify({ operation: 'disposition', request_id: requestId, work_id: workId, expected_subject_digest: view.current_subject_digest, kind, criteria, reason: reason.trim(), authorization }),
    })
  } catch (error) {
    if (signal.aborted) throw error
    throw new IntentReviewError('The local intent review service is unavailable.')
  }
  if (!response.ok) {
    if (response.status === 409) throw new IntentReviewError('The candidate or context changed. Reload Work detail before recording a disposition.', 409)
    throw new IntentReviewError(response.status === 401 ? 'The local server refused this read token.' : response.status === 403 ? 'The local server refused human disposition access to this checkout.' : `The disposition could not be recorded (HTTP ${response.status}).`, response.status)
  }
  const body: unknown = await response.json().catch(() => null)
  const result = rec(body)
  if (!result['view']) throw new IntentReviewError('The local server returned an invalid disposition response.')
  decodeIntentReview(result['view'])
}
