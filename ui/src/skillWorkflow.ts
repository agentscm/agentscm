import type {
  SkillWorkflowApplicabilityState,
  SkillWorkflowEnvelope,
  SkillWorkflowObservation,
  SkillWorkflowRunState,
  SkillWorkflowStep,
  SkillWorkflowStepState,
} from './types.ts'

const MAX_RUN_ID_BYTES = 64
const MAX_STEPS = 32
const MAX_OBSERVATIONS_PER_STEP = 32
const MAX_TEXT = 4096
const APPLICABILITY_BASIS = 'current_authority_repository_policy_epoch_and_immutable_revision'
const RUN_STATES = new Set<SkillWorkflowRunState>(['pending', 'running', 'succeeded', 'completed_with_omissions', 'failed', 'blocked', 'cancelled'])
const STEP_STATES = new Set<SkillWorkflowStepState>(['pending', 'running', 'succeeded', 'failed', 'blocked', 'cancelled'])
const APPLICABILITY_STATES = new Set<SkillWorkflowApplicabilityState>(['current', 'stale', 'unavailable'])

export class InvalidSkillWorkflowResponse extends Error {}

const fail = (field: string): never => {
  throw new InvalidSkillWorkflowResponse(`Invalid skill workflow response (${field})`)
}
const record = (value: unknown, field: string): Record<string, unknown> =>
  value !== null && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, unknown>) : fail(field)
const text = (value: unknown, field: string, max = MAX_TEXT): string =>
  typeof value === 'string' && value.length <= max ? value : fail(field)
const nullableText = (value: unknown, field: string): string | null => (value === null ? null : text(value, field))
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

function observation(value: unknown): SkillWorkflowObservation {
  const item = record(value, 'observation')
  return {
    path: text(item['path'], 'observation.path'),
    sha256: lowerHex(item['sha256'], 64, 'observation.sha256'),
    bytes: integer(item['bytes'], 'observation.bytes'),
  }
}

function step(value: unknown): SkillWorkflowStep {
  const item = record(value, 'step')
  const required = item['required']
  if (typeof required !== 'boolean') return fail('step.required')
  return {
    id: stepId(item['id'], 'step.id'),
    required,
    state: enumValue(item['state'], STEP_STATES, 'step.state'),
    observations: boundedList(item['observations'], MAX_OBSERVATIONS_PER_STEP, 'step.observations').map(observation),
    detail: nullableText(item['detail'], 'step.detail'),
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
  if (run['admission'] !== 'not_assessed') fail('projection.run.admission')
  const steps = boundedList(run['steps'], MAX_STEPS, 'projection.run.steps')
  if (steps.length === 0) fail('projection.run.steps')
  const runId = canonicalId(run['run_id'], 'projection.run.run_id')
  if (expectedRunId !== undefined && runId !== expectedRunId) fail('projection.run.run_id')
  if (applicability['basis'] !== APPLICABILITY_BASIS) fail('projection.present_applicability.basis')
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
        steps: steps.map(step),
        admission: 'not_assessed',
      },
      present_applicability: {
        basis: APPLICABILITY_BASIS,
        state: enumValue(applicability['state'], APPLICABILITY_STATES, 'projection.present_applicability.state'),
        reason: nullableText(applicability['reason'], 'projection.present_applicability.reason'),
      },
    },
  }
}

export function validSkillWorkflowRunId(value: string): boolean {
  return /^[A-Za-z0-9._-]{1,64}$/.test(value)
}

export function validSkillWorkflowStepId(value: string): boolean {
  return /^[A-Za-z0-9._:/-]{1,64}$/.test(value)
}
