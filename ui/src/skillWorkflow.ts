import type {
  SkillWorkflowAction,
  SkillWorkflowActionOutcome,
  SkillWorkflowApplicabilityState,
  SkillWorkflowEnvelope,
  SkillWorkflowListEnvelope,
  SkillWorkflowCommandExecution,
  SkillWorkflowObservation,
  SkillWorkflowReview,
  SkillWorkflowReviewFinding,
  SkillWorkflowReviewSeverity,
  SkillWorkflowReviewStatus,
  SkillWorkflowRunState,
  SkillWorkflowStep,
  SkillWorkflowStepState,
} from './types.ts'

const MAX_RUN_ID_BYTES = 64
const MAX_STEPS = 32
const MAX_RECENT_RUNS = 16
const MAX_OBSERVATIONS_PER_STEP = 32
const MAX_REVIEW_FINDINGS = 64
const MAX_REVIEW_LIST = 128
const MAX_TEXT = 4096
const MAX_WORKFLOW_NAME = 128
const APPLICABILITY_BASIS = 'current_authority_repository_policy_epoch_and_immutable_revision'
const LANDING_BASIS = 'current_change_version_review_context_target_and_policy_epoch'
const RUN_STATES = new Set<SkillWorkflowRunState>(['pending', 'running', 'succeeded', 'completed_with_omissions', 'failed', 'blocked', 'cancelled'])
const STEP_STATES = new Set<SkillWorkflowStepState>(['pending', 'running', 'succeeded', 'failed', 'blocked', 'cancelled'])
const APPLICABILITY_STATES = new Set<SkillWorkflowApplicabilityState>(['current', 'stale', 'unavailable'])
const REVIEW_STATUSES = new Set<SkillWorkflowReviewStatus>(['succeeded', 'failed', 'blocked', 'cancelled'])
const REVIEW_SEVERITIES = new Set<SkillWorkflowReviewSeverity>(['note', 'warning', 'error'])
const ACTION_OUTCOMES = new Set<SkillWorkflowActionOutcome>(['succeeded', 'failed', 'blocked', 'cancelled', 'uncertain', 'timed_out', 'output_limit', 'invalid_output'])
const ARTIFACT_MEDIA = new Set<NonNullable<SkillWorkflowAction['artifact']>['media_type']>(['text/plain', 'text/markdown', 'application/json'])

export class InvalidSkillWorkflowResponse extends Error {}

const fail = (field: string): never => {
  throw new InvalidSkillWorkflowResponse(`Invalid skill workflow response (${field})`)
}
const record = (value: unknown, field: string): Record<string, unknown> =>
  value !== null && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, unknown>) : fail(field)
const text = (value: unknown, field: string, max = MAX_TEXT): string =>
  typeof value === 'string' && value.length <= max ? value : fail(field)
const nullableText = (value: unknown, field: string): string | null => (value === null ? null : text(value, field))
const optionalText = (value: unknown, field: string, max = MAX_TEXT): string | null =>
  value === undefined || value === null ? null : text(value, field, max)
const integer = (value: unknown, field: string): number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : fail(field)
const versionOne = (value: unknown, field: string): 1 => (value === 1 ? 1 : fail(field))
const enumValue = <T extends string>(value: unknown, allowed: ReadonlySet<T>, field: string): T =>
  typeof value === 'string' && allowed.has(value as T) ? (value as T) : fail(field)
const boundedList = (value: unknown, max: number, field: string): unknown[] =>
  Array.isArray(value) && value.length <= max ? value : fail(field)
const canonicalId = (value: unknown, field: string): string => {
  const valueText = text(value, field, MAX_RUN_ID_BYTES)
  return validSkillWorkflowRunId(valueText) ? valueText : fail(field)
}
const stepId = (value: unknown, field: string): string => {
  const valueText = text(value, field, MAX_RUN_ID_BYTES)
  return validSkillWorkflowStepId(valueText) ? valueText : fail(field)
}
const lowerHex = (value: unknown, bytes: number, field: string): string => {
  const valueText = text(value, field, bytes)
  return new RegExp(`^[0-9a-f]{${bytes}}$`).test(valueText) ? valueText : fail(field)
}
const sha256Identity = (value: unknown, field: string): string => {
  const valueText = text(value, field, 71)
  return /^sha256:[0-9a-f]{64}$/.test(valueText) ? valueText : fail(field)
}
const positiveDecimal = (value: unknown, field: string): string => {
  const valueText = text(value, field, 19)
  return /^[1-9][0-9]{0,18}$/.test(valueText) && (valueText.length < 19 || valueText <= '9223372036854775807')
    ? valueText
    : fail(field)
}

function observation(value: unknown): SkillWorkflowObservation {
  const item = record(value, 'observation')
  return {
    path: text(item['path'], 'observation.path'),
    sha256: lowerHex(item['sha256'], 64, 'observation.sha256'),
    bytes: integer(item['bytes'], 'observation.bytes'),
  }
}

function commandExecution(value: unknown): SkillWorkflowCommandExecution | null {
  if (value === undefined || value === null) return null
  const item = record(value, 'step.execution')
  return {
    attempt_id: text(item['attempt_id'], 'step.execution.attempt_id', 256),
    generation: integer(item['generation'], 'step.execution.generation'),
    state: text(item['state'], 'step.execution.state', 64),
    outcome: optionalText(item['outcome'], 'step.execution.outcome', 64),
    record_digest: item['record_digest'] === undefined || item['record_digest'] === null
      ? null
      : lowerHex(item['record_digest'], 64, 'step.execution.record_digest'),
    failure: optionalText(item['failure'], 'step.execution.failure'),
  }
}

function positiveLine(value: unknown, field: string): number | null {
  if (value === undefined || value === null) return null
  const line = integer(value, field)
  return line > 0 ? line : fail(field)
}

function reviewFinding(value: unknown): SkillWorkflowReviewFinding {
  const item = record(value, 'step.review.finding')
  const startLine = positiveLine(item['start_line'], 'step.review.finding.start_line')
  const endLine = positiveLine(item['end_line'], 'step.review.finding.end_line')
  if (endLine !== null && (startLine === null || endLine < startLine)) fail('step.review.finding.end_line')
  return {
    path: optionalText(item['path'], 'step.review.finding.path', 1024),
    start_line: startLine,
    end_line: endLine,
    title: text(item['title'], 'step.review.finding.title', 512),
    detail: text(item['detail'], 'step.review.finding.detail', 16 * 1024),
    severity: enumValue(item['severity'], REVIEW_SEVERITIES, 'step.review.finding.severity'),
  }
}

function review(value: unknown): SkillWorkflowReview | null {
  if (value === undefined || value === null) return null
  const item = record(value, 'step.review')
  return {
    model_requested: text(item['model_requested'], 'step.review.model_requested', 256),
    model_observed: optionalText(item['model_observed'], 'step.review.model_observed', 256),
    status: enumValue(item['status'], REVIEW_STATUSES, 'step.review.status'),
    stop: optionalText(item['stop'], 'step.review.stop', 1024),
    findings: boundedList(item['findings'], MAX_REVIEW_FINDINGS, 'step.review.findings').map(reviewFinding),
    unresolved: boundedList(item['unresolved'], MAX_REVIEW_LIST, 'step.review.unresolved')
      .map((entry) => text(entry, 'step.review.unresolved.item', 2048)),
    covered_paths: boundedList(item['covered_paths'], MAX_REVIEW_LIST, 'step.review.covered_paths')
      .map((entry) => text(entry, 'step.review.covered_paths.item', 1024)),
  }
}

function productiveAction(value: unknown): SkillWorkflowAction | null {
  if (value === undefined || value === null) return null
  const item = record(value, 'step.action')
  if (item['verification'] !== 'not_independently_verified') fail('step.action.verification')
  const outcome = item['outcome'] === null ? null : enumValue(item['outcome'], ACTION_OUTCOMES, 'step.action.outcome')
  let artifact: SkillWorkflowAction['artifact'] = null
  if (item['artifact'] !== null) {
    const a = record(item['artifact'], 'step.action.artifact')
    const bytes = integer(a['bytes'], 'step.action.artifact.bytes')
    if (bytes === 0 || bytes > 256 * 1024 || outcome !== 'succeeded') fail('step.action.artifact')
    artifact = {
      name: stepId(a['name'], 'step.action.artifact.name'),
      media_type: enumValue(a['media_type'], ARTIFACT_MEDIA, 'step.action.artifact.media_type'),
      sha256: lowerHex(a['sha256'], 64, 'step.action.artifact.sha256'),
      bytes,
      run_id: canonicalId(a['run_id'], 'step.action.artifact.run_id'),
      step_id: stepId(a['step_id'], 'step.action.artifact.step_id'),
      revision: lowerHex(a['revision'], 64, 'step.action.artifact.revision'),
    }
  }
  if (outcome === 'succeeded' && artifact === null) fail('step.action.artifact')
  return {
    profile_digest: lowerHex(item['profile_digest'], 64, 'step.action.profile_digest'),
    outcome,
    artifact,
    observed_model: optionalText(item['observed_model'], 'step.action.observed_model', 256),
    verification: 'not_independently_verified',
  }
}

function step(value: unknown): SkillWorkflowStep {
  const item = record(value, 'step')
  const required = item['required']
  if (typeof required !== 'boolean') return fail('step.required')
  return {
    id: stepId(item['id'], 'step.id'),
    needs: boundedList(item['needs'], MAX_STEPS, 'step.needs').map((dependency) => stepId(dependency, 'step.needs.item')),
    required,
    state: enumValue(item['state'], STEP_STATES, 'step.state'),
    observations: boundedList(item['observations'], MAX_OBSERVATIONS_PER_STEP, 'step.observations').map(observation),
    detail: nullableText(item['detail'], 'step.detail'),
    execution: commandExecution(item['execution']),
    review: review(item['review']),
    action: productiveAction(item['action']),
  }
}

/** Decode only the bounded fields the status card renders; reject partial or unfamiliar states. */
export function decodeSkillWorkflowEnvelope(value: unknown, expectedRunId?: string): SkillWorkflowEnvelope {
  const envelope = record(value, 'envelope')
  const version = versionOne(envelope['version'], 'envelope.version')
  if (envelope['availability'] === 'unavailable') {
    return { version, availability: 'unavailable', reason: text(envelope['reason'], 'envelope.reason', 128) }
  }
  if (envelope['availability'] !== 'available') fail('envelope.availability')
  const projection = record(envelope['projection'], 'projection')
  const run = record(projection['run'], 'projection.run')
  const source = record(run['source'], 'projection.run.source')
  const applicability = record(projection['present_applicability'], 'projection.present_applicability')
  const landingValue = projection['present_landing_binding']
  if (run['admission'] !== 'not_assessed') fail('projection.run.admission')
  const steps = boundedList(run['steps'], MAX_STEPS, 'projection.run.steps')
  if (steps.length === 0) fail('projection.run.steps')
  const runId = canonicalId(run['run_id'], 'projection.run.run_id')
  if (expectedRunId !== undefined && runId !== expectedRunId) fail('projection.run.run_id')
  if (applicability['basis'] !== APPLICABILITY_BASIS) fail('projection.present_applicability.basis')
  const landing = landingValue === undefined || landingValue === null ? null : record(landingValue, 'projection.present_landing_binding')
  if (landing && landing['basis'] !== LANDING_BASIS) fail('projection.present_landing_binding.basis')
  const decodedSteps = steps.map(step)
  const stepIds = new Set(decodedSteps.map((item) => item.id))
  if (stepIds.size !== decodedSteps.length) fail('projection.run.steps.id')
  for (const item of decodedSteps) {
    const dependencies = new Set(item.needs)
    if (dependencies.size !== item.needs.length || dependencies.has(item.id) || item.needs.some((dependency) => !stepIds.has(dependency))) {
      fail('projection.run.steps.needs')
    }
    const artifact = item.action?.artifact
    if (artifact && (artifact.run_id !== runId || artifact.step_id !== item.id || artifact.revision !== source['revision'])) {
      fail('projection.run.steps.action.artifact.binding')
    }
    if (item.action && item.state === 'succeeded' && item.action.outcome !== 'succeeded') fail('projection.run.steps.action.outcome')
  }
  return {
    version,
    availability: 'available',
    projection: {
      version: versionOne(projection['version'], 'projection.version'),
      run: {
        version: versionOne(run['version'], 'projection.run.version'),
        run_id: runId,
        plan_digest: sha256Identity(run['plan_digest'], 'projection.run.plan_digest'),
        skill_digest: sha256Identity(run['skill_digest'], 'projection.run.skill_digest'),
        source: {
          repository: lowerHex(source['repository'], 32, 'projection.run.source.repository'),
          revision: lowerHex(source['revision'], 64, 'projection.run.source.revision'),
          policy_epoch: integer(source['policy_epoch'], 'projection.run.source.policy_epoch'),
        },
        state: enumValue(run['state'], RUN_STATES, 'projection.run.state'),
        steps: decodedSteps,
        admission: 'not_assessed',
      },
      present_applicability: {
        basis: APPLICABILITY_BASIS,
        state: enumValue(applicability['state'], APPLICABILITY_STATES, 'projection.present_applicability.state'),
        reason: nullableText(applicability['reason'], 'projection.present_applicability.reason'),
      },
      present_landing_binding: landing === null ? null : {
        basis: LANDING_BASIS,
        state: enumValue(landing['state'], APPLICABILITY_STATES, 'projection.present_landing_binding.state'),
        reason: nullableText(landing['reason'], 'projection.present_landing_binding.reason'),
        change_id: optionalText(landing['change_id'], 'projection.present_landing_binding.change_id', 128),
        change_version: landing['change_version'] === null ? null : integer(landing['change_version'], 'projection.present_landing_binding.change_version'),
        context_digest: landing['context_digest'] === null ? null : lowerHex(landing['context_digest'], 64, 'projection.present_landing_binding.context_digest'),
        target: optionalText(landing['target'], 'projection.present_landing_binding.target', 128),
      },
    },
  }
}

export function decodeSkillWorkflowListEnvelope(value: unknown): SkillWorkflowListEnvelope {
  const envelope = record(value, 'workflow list')
  const version = versionOne(envelope['version'], 'workflow list.version')
  if (envelope['availability'] === 'unavailable') {
    return { version, availability: 'unavailable', reason: text(envelope['reason'], 'workflow list.reason', 128) }
  }
  if (envelope['availability'] !== 'available') fail('workflow list.availability')
  const seen = new Set<string>()
  const seenSequences = new Set<string>()
  const runs = boundedList(envelope['runs'], MAX_RECENT_RUNS, 'workflow list.runs').map((value) => {
    const item = record(value, 'workflow list.run')
    const runId = canonicalId(item['run_id'], 'workflow list.run.run_id')
    if (seen.has(runId)) fail('workflow list.run.run_id')
    seen.add(runId)
    const sequence = positiveDecimal(item['sequence'], 'workflow list.run.sequence')
    if (seenSequences.has(sequence)) fail('workflow list.run.sequence')
    seenSequences.add(sequence)
    return {
      run_id: runId,
      revision: lowerHex(item['revision'], 64, 'workflow list.run.revision'),
      state: enumValue(item['state'], RUN_STATES, 'workflow list.run.state'),
      name: text(item['name'], 'workflow list.run.name', MAX_WORKFLOW_NAME),
      sequence,
    }
  })
  const nextCursor = envelope['next_cursor'] === null ? null : positiveDecimal(envelope['next_cursor'], 'workflow list.next_cursor')
  return { version, availability: 'available', runs, next_cursor: nextCursor }
}

export function validSkillWorkflowRunId(value: string): boolean {
  return /^[A-Za-z0-9._-]{1,64}$/.test(value)
}

export function validSkillWorkflowStepId(value: string): boolean {
  return /^[A-Za-z0-9._:/-]{1,64}$/.test(value)
}
