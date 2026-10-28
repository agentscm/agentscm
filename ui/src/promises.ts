import { workspaceMutation, workspaceRequest, type WorkspaceSession } from './workspace.ts'

export type EvidenceStage = 'design' | 'code' | 'review' | 'release' | 'post_release'
export type EvidenceBasis = 'rehearsal' | 'observed' | 'caller_reported'
export type Assurance = 'caller_asserted' | 'observed_trusted_local' | 'protected_execution' | 'independent_verification'

export interface EvidenceRequirement {
  id: string
  criterion_id: string
  stage: EvidenceStage
  basis: EvidenceBasis
  producer: string
  check_key: string
  check_digest: string
  scenario_digest: string | null
  workflow_digest: string | null
  environment_digest: string | null
  accepted_assurance: Assurance[]
}
export interface PromiseCriterion { id: string; text: string; evidence_requirements: EvidenceRequirement[] }
export interface PromiseConstraint { id: string; text: string }
export interface PromiseRepository { repository_id: string; relevance: string }
export interface PromiseForgeAdmission { provider: string; host: string; forge_repository_id: number }
export interface PromiseRepositoryAdmission { repository_id: string; forge: PromiseForgeAdmission | null; native_repository_ids: string[] }
export interface PromiseDefinition { original_request: string; commitment: string; criteria: PromiseCriterion[]; constraints: PromiseConstraint[]; repositories: PromiseRepository[] }
export interface PromiseContent { commitment: string; criteria: PromiseCriterion[]; constraints: PromiseConstraint[]; repositories: PromiseRepository[] }
export interface PromiseRef { workspace_id: string; promise_id: string; version: number; digest: string }
export type PromiseState = 'active' | 'retired'
export type PromiseVersionKind = 'create' | 'adopt' | 'amend' | 'correct' | 'retire' | 'transfer' | 'recovery_transfer'
export interface PromiseVersion {
  ref: PromiseRef
  prior_digest: string | null
  steward_user: string
  state: PromiseState
  kind: PromiseVersionKind
  reason: string | null
  definition: PromiseDefinition
  repository_admissions: PromiseRepositoryAdmission[]
  created_by: string
  proposal_id: string | null
  created_at: number
}
export interface PromiseView { current: PromiseVersion }
export interface PromisePage { promises: PromiseView[]; next: string | null }
export interface PromiseHistoryPage { versions: PromiseVersion[]; next: number | null }
export interface PromiseReceipt { ref: PromiseRef; replayed: boolean }
export interface PromiseAdmissionPreflight { admissions: PromiseRepositoryAdmission[]; digest: string }
export interface PromiseChangeCapabilities { amend: boolean; correct: boolean; retire: boolean; transfer: boolean; recovery_transfer: boolean }
export type PromiseInput =
  | { kind: 'proposal'; operation_id: string; repository_id: string; original_request: string; commitment: string; criteria: PromiseCriterion[]; constraints: PromiseConstraint[] }
  | { kind: 'report'; operation_id: string; promise_id: string; promise_version: number; promise_digest: string; repository_id: string; summary: string }
export interface PromiseInputView { input_id: string; repository_id: string; actor_user: string; submitted_by_agent: boolean; payload: PromiseInput; adopted_promise: string | null; created_at: number }
export interface PromiseInputPage { inputs: PromiseInputView[]; next: string | null }

export type PromiseChange =
  | { kind: 'amend' | 'correct'; content: PromiseContent }
  | { kind: 'retire' }
  | { kind: 'transfer' | 'recovery_transfer'; to_user: string }

export class InvalidPromiseResponse extends Error {}

const stages = new Set<EvidenceStage>(['design', 'code', 'review', 'release', 'post_release'])
const bases = new Set<EvidenceBasis>(['rehearsal', 'observed', 'caller_reported'])
const assurances = new Set<Assurance>(['caller_asserted', 'observed_trusted_local', 'protected_execution', 'independent_verification'])
const states = new Set<PromiseState>(['active', 'retired'])
const kinds = new Set<PromiseVersionKind>(['create', 'adopt', 'amend', 'correct', 'retire', 'transfer', 'recovery_transfer'])

const definitionDigest = /^[0-9a-f]{64}$/
export const invalidPromiseText = (value: string) => [...value].some((character) => {
  const code = character.codePointAt(0)!
  return (code < 32 && character !== '\n' && character !== '\r' && character !== '\t') || (code >= 127 && code <= 159)
})

export const promiseDefinitionProblems = (definition: PromiseDefinition, includeOriginal = true, sizeValue: unknown = definition): string[] => {
  const out: string[] = []
  if (includeOriginal && !definition.original_request.trim()) out.push('Enter the original request.')
  if (!definition.commitment.trim()) out.push('Enter the commitment.')
  if (new TextEncoder().encode(definition.original_request).length > 16 * 1024 || new TextEncoder().encode(definition.commitment).length > 16 * 1024) out.push('The original request and commitment must each fit within 16 KiB.')
  if (new TextEncoder().encode(JSON.stringify(sizeValue)).length > 64 * 1024) out.push('The complete promise definition must fit within 64 KiB.')
  if (!definition.criteria.length || definition.criteria.length > 64) out.push('Add 1 to 64 criteria.')
  if (definition.constraints.length > 64) out.push('Use at most 64 constraints.')
  if (!definition.repositories.length || definition.repositories.length > 8) out.push('Add 1 to 8 repositories.')
  const criterionIds = definition.criteria.map((item) => item.id)
  const constraintIds = definition.constraints.map((item) => item.id)
  const requirementIds = definition.criteria.flatMap((item) => item.evidence_requirements.map((requirement) => requirement.id))
  if ([...criterionIds, ...constraintIds, ...requirementIds].some((value) => !validPromiseId(value))) out.push('Every criterion, constraint and requirement needs a valid ID.')
  if (new Set(criterionIds).size !== criterionIds.length || new Set(constraintIds).size !== constraintIds.length || new Set(requirementIds).size !== requirementIds.length) out.push('IDs must be unique within criteria, constraints and evidence requirements.')
  if (requirementIds.length > 64) out.push('Use at most 64 evidence requirements.')
  if (definition.criteria.some((item) => !item.text.trim())) out.push('Every criterion needs text.')
  if (definition.constraints.some((item) => !item.text.trim())) out.push('Every constraint needs text.')
  if (definition.repositories.some((item) => !validPromiseId(item.repository_id) || !item.relevance.trim())) out.push('Every repository needs its stable ID and relevance.')
  if ([definition.original_request, definition.commitment, ...definition.criteria.map((item) => item.text), ...definition.constraints.map((item) => item.text), ...definition.repositories.map((item) => item.relevance)].some(invalidPromiseText)) out.push('Promise text cannot contain control characters other than tabs and line breaks.')
  if (new Set(definition.repositories.map((item) => item.repository_id)).size !== definition.repositories.length) out.push('Repository IDs must be unique.')
  if (definition.criteria.some((item) => new TextEncoder().encode(item.text).length > 2048) || definition.constraints.some((item) => new TextEncoder().encode(item.text).length > 2048) || definition.repositories.some((item) => new TextEncoder().encode(item.relevance).length > 2048)) out.push('Criterion, constraint and repository relevance text must each fit within 2 KiB.')
  for (const criterion of definition.criteria) for (const requirement of criterion.evidence_requirements) {
    if (requirement.criterion_id !== criterion.id) out.push(`Evidence requirement ${requirement.id || '(unnamed)'} must name its owning criterion.`)
    if (!validPromiseId(requirement.producer) || !validPromiseId(requirement.check_key) || !definitionDigest.test(requirement.check_digest)) out.push(`Evidence requirement ${requirement.id || '(unnamed)'} needs a valid producer, check key and 64-character lowercase digest.`)
    if ([requirement.scenario_digest, requirement.workflow_digest, requirement.environment_digest].some((value) => value !== null && !definitionDigest.test(value))) out.push(`Evidence requirement ${requirement.id || '(unnamed)'} has a malformed optional digest.`)
    if (!requirement.accepted_assurance.length || new Set(requirement.accepted_assurance).size !== requirement.accepted_assurance.length) out.push(`Evidence requirement ${requirement.id || '(unnamed)'} needs unique accepted assurance values.`)
    if (requirement.basis === 'rehearsal' && ![requirement.scenario_digest, requirement.workflow_digest, requirement.environment_digest].every((value) => value !== null && definitionDigest.test(value))) out.push(`Rehearsal requirement ${requirement.id || '(unnamed)'} needs scenario, workflow and environment digests.`)
  }
  return [...new Set(out)]
}

const requireValidDefinition = (definition: PromiseDefinition, sizeValue: unknown = definition): void => {
  const issue = promiseDefinitionProblems(definition, true, sizeValue)[0]
  if (issue) throw new InvalidPromiseResponse(`Invalid promise definition: ${issue}`)
}

const requireValidText = (value: string, name: string, maxBytes: number): void => {
  if (!value.trim() || new TextEncoder().encode(value).length > maxBytes || invalidPromiseText(value)) throw new InvalidPromiseResponse(`Invalid ${name}.`)
}

const requireValidId = (value: string, name: string): void => {
  if (!validPromiseId(value)) throw new InvalidPromiseResponse(`Invalid ${name}.`)
}

const requireValidDigest = (value: string, name: string): void => {
  if (!definitionDigest.test(value)) throw new InvalidPromiseResponse(`Invalid ${name}.`)
}

const record = (value: unknown, keys: string[], name: string): Record<string, unknown> => {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) throw new InvalidPromiseResponse(`Invalid ${name}.`)
  const actual = Object.keys(value)
  if (actual.length !== keys.length || actual.some((key) => !keys.includes(key))) throw new InvalidPromiseResponse(`Invalid ${name} fields.`)
  return value as Record<string, unknown>
}
const string = (value: unknown, name: string) => {
  if (typeof value !== 'string') throw new InvalidPromiseResponse(`Invalid ${name}.`)
  return value
}
const nullableString = (value: unknown, name: string) => value === null ? null : string(value, name)
const integer = (value: unknown, name: string, positive = false) => {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < (positive ? 1 : 0)) throw new InvalidPromiseResponse(`Invalid ${name}.`)
  return value
}
const list = <T>(value: unknown, name: string, decode: (item: unknown) => T): T[] => {
  if (!Array.isArray(value)) throw new InvalidPromiseResponse(`Invalid ${name}.`)
  return value.map(decode)
}
const member = <T extends string>(value: unknown, values: Set<T>, name: string): T => {
  const item = string(value, name) as T
  if (!values.has(item)) throw new InvalidPromiseResponse(`Invalid ${name}.`)
  return item
}

const decodeRef = (value: unknown): PromiseRef => {
  const r = record(value, ['workspace_id', 'promise_id', 'version', 'digest'], 'promise reference')
  const digest = string(r.digest, 'promise digest')
  if (!/^[0-9a-f]{64}$/.test(digest)) throw new InvalidPromiseResponse('Invalid promise digest.')
  const workspace_id = string(r.workspace_id, 'workspace ID')
  const promise_id = string(r.promise_id, 'promise ID')
  if (!validPromiseId(workspace_id) || !validPromiseId(promise_id)) throw new InvalidPromiseResponse('Invalid promise reference ID.')
  return { workspace_id, promise_id, version: integer(r.version, 'promise version', true), digest }
}
const decodeRequirement = (value: unknown): EvidenceRequirement => {
  const r = record(value, ['id', 'criterion_id', 'stage', 'basis', 'producer', 'check_key', 'check_digest', 'scenario_digest', 'workflow_digest', 'environment_digest', 'accepted_assurance'], 'evidence requirement')
  return {
    id: string(r.id, 'requirement ID'), criterion_id: string(r.criterion_id, 'requirement criterion ID'), stage: member(r.stage, stages, 'evidence stage'), basis: member(r.basis, bases, 'evidence basis'), producer: string(r.producer, 'evidence producer'), check_key: string(r.check_key, 'evidence check key'), check_digest: string(r.check_digest, 'evidence check digest'), scenario_digest: nullableString(r.scenario_digest, 'scenario digest'), workflow_digest: nullableString(r.workflow_digest, 'workflow digest'), environment_digest: nullableString(r.environment_digest, 'environment digest'), accepted_assurance: list(r.accepted_assurance, 'accepted assurance', (item) => member(item, assurances, 'assurance')),
  }
}
const decodeCriterion = (value: unknown): PromiseCriterion => {
  const r = record(value, ['id', 'text', 'evidence_requirements'], 'criterion')
  return { id: string(r.id, 'criterion ID'), text: string(r.text, 'criterion text'), evidence_requirements: list(r.evidence_requirements, 'evidence requirements', decodeRequirement) }
}
const decodeConstraint = (value: unknown): PromiseConstraint => {
  const r = record(value, ['id', 'text'], 'constraint')
  return { id: string(r.id, 'constraint ID'), text: string(r.text, 'constraint text') }
}
const decodeRepository = (value: unknown): PromiseRepository => {
  const r = record(value, ['repository_id', 'relevance'], 'repository')
  return { repository_id: string(r.repository_id, 'repository ID'), relevance: string(r.relevance, 'repository relevance') }
}
const decodeDefinition = (value: unknown): PromiseDefinition => {
  const r = record(value, ['original_request', 'commitment', 'criteria', 'constraints', 'repositories'], 'promise definition')
  return { original_request: string(r.original_request, 'original request'), commitment: string(r.commitment, 'commitment'), criteria: list(r.criteria, 'criteria', decodeCriterion), constraints: list(r.constraints, 'constraints', decodeConstraint), repositories: list(r.repositories, 'repositories', decodeRepository) }
}
const decodeAdmission = (value: unknown): PromiseRepositoryAdmission => {
  const r = record(value, ['repository_id', 'forge', 'native_repository_ids'], 'repository admission')
  const repository_id = string(r.repository_id, 'admission repository ID')
  if (!validPromiseId(repository_id)) throw new InvalidPromiseResponse('Invalid admission repository ID.')
  let forge: PromiseForgeAdmission | null = null
  if (r.forge !== null) {
    const source = record(r.forge, ['provider', 'host', 'forge_repository_id'], 'forge admission')
    const provider = string(source.provider, 'forge provider')
    const host = string(source.host, 'forge host')
    requireValidText(provider, 'forge provider', 64)
    requireValidText(host, 'forge host', 255)
    forge = { provider, host, forge_repository_id: integer(source.forge_repository_id, 'forge repository ID', true) }
  }
  const native_repository_ids = list(r.native_repository_ids, 'native repository IDs', (item) => string(item, 'native repository ID'))
  if (native_repository_ids.length > 32 || native_repository_ids.some((item) => !validPromiseId(item)) || native_repository_ids.some((item, index) => index > 0 && native_repository_ids[index - 1]! >= item)) throw new InvalidPromiseResponse('Invalid native repository admission identities.')
  return { repository_id, forge, native_repository_ids }
}
export const decodePromiseVersion = (value: unknown): PromiseVersion => {
  const r = record(value, ['ref', 'prior_digest', 'steward_user', 'state', 'kind', 'reason', 'definition', 'repository_admissions', 'created_by', 'proposal_id', 'created_at'], 'promise version')
  const definition = decodeDefinition(r.definition)
  requireValidDefinition(definition)
  const repository_admissions = list(r.repository_admissions, 'repository admissions', decodeAdmission)
  const expected = definition.repositories.map((repository) => repository.repository_id).sort()
  if (expected.length !== repository_admissions.length || repository_admissions.some((admission, index) => admission.repository_id !== expected[index])) throw new InvalidPromiseResponse('Promise source admissions do not match the owner-defined scope.')
  const prior_digest = nullableString(r.prior_digest, 'prior digest')
  if (prior_digest !== null) requireValidDigest(prior_digest, 'prior digest')
  const steward_user = string(r.steward_user, 'steward user')
  const created_by = string(r.created_by, 'creator')
  requireValidId(steward_user, 'steward user')
  requireValidId(created_by, 'creator')
  const reason = nullableString(r.reason, 'version reason')
  if (reason !== null) requireValidText(reason, 'version reason', 2048)
  const proposal_id = nullableString(r.proposal_id, 'proposal ID')
  if (proposal_id !== null) requireValidId(proposal_id, 'proposal ID')
  return { ref: decodeRef(r.ref), prior_digest, steward_user, state: member(r.state, states, 'promise state'), kind: member(r.kind, kinds, 'version kind'), reason, definition, repository_admissions, created_by, proposal_id, created_at: integer(r.created_at, 'created time') }
}
export const decodePromiseView = (value: unknown): PromiseView => {
  const r = record(value, ['current'], 'promise view')
  return { current: decodePromiseVersion(r.current) }
}
export const decodePromisePage = (value: unknown): PromisePage => {
  const r = record(value, ['promises', 'next'], 'promise page')
  return { promises: list(r.promises, 'promises', decodePromiseView), next: nullableString(r.next, 'next cursor') }
}
export const decodePromiseHistoryPage = (value: unknown): PromiseHistoryPage => {
  const r = record(value, ['versions', 'next'], 'promise history')
  return { versions: list(r.versions, 'versions', decodePromiseVersion), next: r.next === null ? null : integer(r.next, 'next version', true) }
}
export const decodePromiseReceipt = (value: unknown): PromiseReceipt => {
  const r = record(value, ['ref', 'replayed'], 'promise receipt')
  if (typeof r.replayed !== 'boolean') throw new InvalidPromiseResponse('Invalid replay marker.')
  return { ref: decodeRef(r.ref), replayed: r.replayed }
}
const decodeBoolean = (value: unknown, name: string): boolean => {
  if (typeof value !== 'boolean') throw new InvalidPromiseResponse(`Invalid ${name}.`)
  return value
}
export const decodeAdmissionPreflight = (value: unknown, repositoryIds: string[]): PromiseAdmissionPreflight => {
  if (repositoryIds.length < 1 || repositoryIds.length > 8 || repositoryIds.some((id, index) => !validPromiseId(id) || (index > 0 && repositoryIds[index - 1]! >= id))) throw new InvalidPromiseResponse('Invalid reviewed repository scope.')
  const r = record(value, ['admissions', 'digest'], 'admission preflight')
  const admissions = list(r.admissions, 'admissions', decodeAdmission)
  if (admissions.length !== repositoryIds.length || admissions.some((admission, index) => admission.repository_id !== repositoryIds[index])) throw new InvalidPromiseResponse('Admission preflight does not match the reviewed repository scope.')
  const digest = string(r.digest, 'admission digest')
  requireValidDigest(digest, 'admission digest')
  return { admissions, digest }
}
export const decodeChangeCapabilities = (value: unknown): PromiseChangeCapabilities => {
  const r = record(value, ['amend', 'correct', 'retire', 'transfer', 'recovery_transfer'], 'promise change capabilities')
  return { amend: decodeBoolean(r.amend, 'amend capability'), correct: decodeBoolean(r.correct, 'correct capability'), retire: decodeBoolean(r.retire, 'retire capability'), transfer: decodeBoolean(r.transfer, 'transfer capability'), recovery_transfer: decodeBoolean(r.recovery_transfer, 'recovery transfer capability') }
}
export const decodePromiseInputView = (value: unknown): PromiseInputView => {
  const r = record(value, ['input_id', 'repository_id', 'actor_user', 'submitted_by_agent', 'payload', 'adopted_promise', 'created_at'], 'promise input')
  const payload = r.payload
  if (payload === null || typeof payload !== 'object' || Array.isArray(payload)) throw new InvalidPromiseResponse('Invalid promise input payload.')
  let decoded: PromiseInput
  if ((payload as Record<string, unknown>).kind === 'proposal') {
    const p = record(payload, ['kind', 'operation_id', 'repository_id', 'original_request', 'commitment', 'criteria', 'constraints'], 'proposal payload')
    decoded = { kind: 'proposal', operation_id: string(p.operation_id, 'operation ID'), repository_id: string(p.repository_id, 'repository ID'), original_request: string(p.original_request, 'original request'), commitment: string(p.commitment, 'commitment'), criteria: list(p.criteria, 'criteria', decodeCriterion), constraints: list(p.constraints, 'constraints', decodeConstraint) }
    const proposalDefinition: PromiseDefinition = { original_request: decoded.original_request, commitment: decoded.commitment, criteria: decoded.criteria, constraints: decoded.constraints, repositories: [{ repository_id: decoded.repository_id, relevance: 'proposal scope' }] }
    requireValidDefinition(proposalDefinition, decoded)
  } else {
    const p = record(payload, ['kind', 'operation_id', 'promise_id', 'promise_version', 'promise_digest', 'repository_id', 'summary'], 'report payload')
    if (p.kind !== 'report') throw new InvalidPromiseResponse('Invalid input kind.')
    decoded = { kind: 'report', operation_id: string(p.operation_id, 'operation ID'), promise_id: string(p.promise_id, 'promise ID'), promise_version: integer(p.promise_version, 'promise version', true), promise_digest: string(p.promise_digest, 'promise digest'), repository_id: string(p.repository_id, 'repository ID'), summary: string(p.summary, 'report summary') }
    requireValidId(decoded.promise_id, 'report promise ID')
    requireValidDigest(decoded.promise_digest, 'report promise digest')
    requireValidText(decoded.summary, 'report summary', 8192)
  }
  const repository_id = string(r.repository_id, 'repository ID')
  if (decoded.repository_id !== repository_id) throw new InvalidPromiseResponse('Input repository mismatch.')
  requireValidId(decoded.operation_id, 'input operation ID')
  requireValidId(repository_id, 'input repository ID')
  const input_id = string(r.input_id, 'input ID')
  const actor_user = string(r.actor_user, 'actor user')
  const adopted_promise = nullableString(r.adopted_promise, 'adopted promise')
  requireValidId(input_id, 'input ID')
  requireValidId(actor_user, 'input actor user')
  if (adopted_promise !== null) requireValidId(adopted_promise, 'adopted promise ID')
  return { input_id, repository_id, actor_user, submitted_by_agent: decodeBoolean(r.submitted_by_agent, 'input actor kind'), payload: decoded, adopted_promise, created_at: integer(r.created_at, 'input time') }
}
export const decodePromiseInputPage = (value: unknown): PromiseInputPage => {
  const r = record(value, ['inputs', 'next'], 'promise input page')
  return { inputs: list(r.inputs, 'inputs', decodePromiseInputView), next: nullableString(r.next, 'next input cursor') }
}

export const validPromiseId = (value: string): boolean => value.length > 0 && value.length <= 128 && /^[A-Za-z0-9_./:-]+$/.test(value)
export const promiseHref = (id: string): string => `#promises/${encodeURIComponent(id)}`
export function promiseIdFromHash(hash = window.location.hash): string | null {
  const match = /^#promises\/([^/?#]+)$/.exec(hash.split('?')[0]!)
  if (!match) return null
  try { const id = decodeURIComponent(match[1]!); return validPromiseId(id) ? id : null } catch { return null }
}

export const listPromises = async (signal: AbortSignal, after?: string): Promise<PromisePage> => decodePromisePage(await workspaceRequest<unknown>(`/promises?limit=20${after ? `&after=${encodeURIComponent(after)}` : ''}`, { signal }))
export const readPromise = async (id: string, signal: AbortSignal): Promise<PromiseView> => decodePromiseView(await workspaceRequest<unknown>(`/promises/${encodeURIComponent(id)}`, { signal }))
export const promiseHistory = async (id: string, signal: AbortSignal, beforeVersion?: number): Promise<PromiseHistoryPage> => decodePromiseHistoryPage(await workspaceRequest<unknown>(`/promises/${encodeURIComponent(id)}/history?limit=20${beforeVersion ? `&before_version=${beforeVersion}` : ''}`, { signal }))
export const admissionPreflight = async (session: WorkspaceSession, repositoryIds: string[]): Promise<PromiseAdmissionPreflight> => {
  const sorted = [...repositoryIds].sort()
  if (sorted.length < 1 || sorted.length > 8 || sorted.some((id, index) => !validPromiseId(id) || (index > 0 && sorted[index - 1]! === id))) throw new InvalidPromiseResponse('Invalid reviewed repository scope.')
  return decodeAdmissionPreflight(await workspaceRequest<unknown>('/promises/admission-preflight', { method: 'POST', csrf: session.csrf_token, body: { repository_ids: sorted } }), sorted)
}
export const changeCapabilities = async (id: string, signal: AbortSignal): Promise<PromiseChangeCapabilities> => decodeChangeCapabilities(await workspaceRequest<unknown>(`/promises/${encodeURIComponent(id)}/capabilities`, { signal }))
export const proposalInputs = async (repositoryId: string, signal: AbortSignal, after?: string): Promise<PromiseInputPage> => decodePromiseInputPage(await workspaceRequest<unknown>(`/promises/inputs/proposals?repository_id=${encodeURIComponent(repositoryId)}&limit=20${after ? `&after=${encodeURIComponent(after)}` : ''}`, { signal }))
export const promiseReports = async (id: string, signal: AbortSignal, after?: string): Promise<PromiseInputPage> => decodePromiseInputPage(await workspaceRequest<unknown>(`/promises/${encodeURIComponent(id)}/reports?limit=20${after ? `&after=${encodeURIComponent(after)}` : ''}`, { signal }))
export const readPromiseInput = async (id: string, signal: AbortSignal): Promise<PromiseInputView> => decodePromiseInputView(await workspaceRequest<unknown>(`/promises/inputs/${encodeURIComponent(id)}`, { signal }))
export const createPromise = async (session: WorkspaceSession, definition: PromiseDefinition, admissionsDigest: string): Promise<PromiseReceipt> => decodePromiseReceipt(await workspaceMutation<unknown>(session, '/promises', { definition, expected_admissions_digest: admissionsDigest }, { expectation: { kind: 'promise', expectedVersion: 1 } }))
export const adoptPromise = async (session: WorkspaceSession, proposalId: string, definition: PromiseDefinition, admissionsDigest: string): Promise<PromiseReceipt> => decodePromiseReceipt(await workspaceMutation<unknown>(session, '/promises/adopt', { proposal_id: proposalId, definition, expected_admissions_digest: admissionsDigest }, { expectation: { kind: 'promise', expectedVersion: 1 } }))
export const changePromise = async (session: WorkspaceSession, current: PromiseVersion, reason: string, action: PromiseChange, admissionsDigest: string): Promise<PromiseReceipt> => decodePromiseReceipt(await workspaceMutation<unknown>(session, `/promises/${encodeURIComponent(current.ref.promise_id)}/changes`, { expected_version: current.ref.version, expected_digest: current.ref.digest, reason, action, expected_admissions_digest: admissionsDigest }, { expectation: { kind: 'promise', promiseId: current.ref.promise_id, expectedVersion: current.ref.version + 1 } }))
