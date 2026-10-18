/** Browser workspace requests use the HttpOnly session and CSRF, never agent tokens. */
export interface WorkspaceIdentity { provider: string; host: string; user_id: number }
export interface WorkspaceMember { identity: WorkspaceIdentity; roles: string[]; repository_ids: string[]; enabled: boolean; generation: number }
export interface WorkspaceSettings { id: string; name: string; generation: number; max_concurrent_runs: number }
export interface WorkspaceSession {
  workspace: WorkspaceSettings
  actor: { user: { id: string; identity: WorkspaceIdentity; login: string }; roles: string[]; repository_ids: string[]; generation: number }
  capabilities: { manage_workspace: boolean; read_repositories: boolean; execute_workflows: boolean; approve_revisions: boolean; integrate_revisions: boolean }
  csrf_token: string
  login_providers: string[]
  synthetic_fixture?: boolean
}
export interface WorkspaceRepository { repository_id: string; profiles: WorkspaceProfile[] }
export interface WorkspaceProfile { profile_id: string; target: string; requirement_digest: string; readiness: string; limitations?: string }
export interface WorkspaceSubject { change_id: string; change_version: number; context_digest: string; revision: string; requirement_digest: string }
export interface WorkspaceRun { run_id: string; repository_id: string; profile_id: string; actor_id: string; generation: number; state: string; subject: WorkspaceSubject; native_run_id: string | null; receipt: string | null; detail: string | null }

export type WorkspaceMutationExpectation =
  | { kind: 'start'; repositoryId: string; profileId: string; subject: WorkspaceSubject }
  | { kind: 'approval'; revision: string; contextDigest: string }
  | { kind: 'landing'; revision: string; contextDigest: string; target: string }

export interface PendingWorkspaceMutation {
  path: string
  method: string
  body: Record<string, unknown>
  requestId: string
  createdAt: number
  uncertain: boolean
  expectation?: WorkspaceMutationExpectation
}

export class WorkspaceError extends Error {
  status: number
  code?: string
  constructor(status: number, message: string, code?: string) { super(message); this.status = status; this.code = code }
}

export const workspaceError = (error: unknown): string => {
  if (error instanceof WorkspaceError) {
    if (error.code === 'pending_mutation') return error.message
    if (error.code === 'busy') return 'The workflow queue is full. Wait for active work to finish or cancel old work, then retry.'
    if (error.code === 'mutation_journal_unavailable') return error.message
    if (error.code === 'response_mismatch') return error.message
    if (error.code === 'last_administrator') return 'Keep at least one enabled administrator. Add another administrator before disabling this membership or removing its role.'
    if (error.code === 'idempotency_conflict') return 'This request ID was already used for different fields. Refresh and review the current state before making a new change.'
    if (error.code === 'member_limit') return 'The workspace member limit has been reached. Ask the operator to review its membership configuration.'
    if (error.code === 'csrf_required') return 'Your workspace security token is no longer accepted. Refresh the workspace before retrying.'
    if (error.status === 401) return 'Your workspace session has ended. Sign in again to continue.'
    if (error.status === 403) return 'Your current membership does not allow this action. Refresh to load your permissions.'
    if (error.status === 409) return 'The workspace changed while you were editing. Reload the latest data, then review your changes before saving again.'
    if (error.status === 404 || error.status === 503) return 'Workspace access is unavailable. Ask the operator to check the workspace configuration.'
    if (error.status === 429) return 'The hub is busy. Wait a moment, then try again.'
    return error.message
  }
  return 'Could not reach the hub. Check your connection, then retry. A mutation may have reached the hub; refresh before submitting again.'
}

export async function workspaceRequest<T>(path: string, options: { signal?: AbortSignal; method?: string; body?: unknown; csrf?: string } = {}): Promise<T> {
  const method = options.method ?? 'GET'
  const response = await fetch(`/v1/hub/workspace${path}`, {
    method, credentials: 'same-origin', cache: 'no-store', signal: options.signal,
    headers: { ...(options.body !== undefined ? { 'Content-Type': 'application/json' } : {}), ...(options.csrf ? { 'x-csrf-token': options.csrf } : {}) },
    ...(options.body !== undefined ? { body: JSON.stringify(options.body) } : {}),
  })
  if (!response.ok) {
    const message = `The hub rejected the request (HTTP ${response.status}). Refresh and check your inputs before retrying.`
    let code: string | undefined
    try { const detail: unknown = await response.json(); if (detail && typeof detail === 'object' && 'error' in detail && typeof detail.error === 'string') code = detail.error.slice(0, 100) } catch { /* A non-JSON error still has a useful HTTP status. */ }
    throw new WorkspaceError(response.status, message, code)
  }
  if (response.status === 204) return undefined as T
  return response.json() as Promise<T>
}

const JOURNAL_VERSION = 1
const JOURNAL_LIMIT = 12
const JOURNAL_BYTES = 64 * 1024
const BODY_BYTES = 16 * 1024
const pendingMutationEvent = 'agentscm:workspace-pending-mutations'
const sensitiveField = /(^|_)(authorization|bearer|csrf|password|secret|session|token)($|_)/i
const validMutationPath = (path: string): boolean => path === '/members' || path === '/settings' || /^\/repositories\/[^/?#]+\/runs(?:\/[^/?#]+\/cancel)?$/.test(path) || /^\/repositories\/[^/?#]+\/profiles\/[^/?#]+\/(approve|integrate)$/.test(path)

const isRecord = (value: unknown): value is Record<string, unknown> => value !== null && typeof value === 'object' && !Array.isArray(value)
const canonicalJson = (value: unknown): string => JSON.stringify(value, (_key, nested: unknown) => {
  if (!isRecord(nested)) return nested
  return Object.keys(nested).sort().reduce<Record<string, unknown>>((sorted, key) => { sorted[key] = nested[key]; return sorted }, {})
})
const containsSensitiveField = (value: unknown): boolean => {
  if (Array.isArray(value)) return value.some(containsSensitiveField)
  if (!isRecord(value)) return false
  return Object.entries(value).some(([key, nested]) => sensitiveField.test(key) || containsSensitiveField(nested))
}
const journalKey = (session: WorkspaceSession): string => `agentscm.workspace.pending.v${JOURNAL_VERSION}:${encodeURIComponent(window.location.origin)}:${encodeURIComponent(session.workspace.id)}:${encodeURIComponent(session.actor.user.id)}`
const emitJournalChange = () => window.dispatchEvent(new Event(pendingMutationEvent))
const storage = (): Storage | null => { try { return window.sessionStorage } catch { return null } }
const validSubject = (value: unknown): value is WorkspaceSubject => isRecord(value) && typeof value.change_id === 'string' && typeof value.change_version === 'number' && typeof value.context_digest === 'string' && typeof value.revision === 'string' && typeof value.requirement_digest === 'string'
const validExpectation = (value: unknown): value is WorkspaceMutationExpectation => {
  if (!isRecord(value) || typeof value.kind !== 'string') return false
  if (value.kind === 'start') return typeof value.repositoryId === 'string' && typeof value.profileId === 'string' && validSubject(value.subject)
  if (typeof value.revision !== 'string' || typeof value.contextDigest !== 'string') return false
  return value.kind === 'approval' || value.kind === 'landing' && typeof value.target === 'string'
}
const validPendingMutation = (value: unknown): value is PendingWorkspaceMutation => {
  if (!isRecord(value) || typeof value.path !== 'string' || value.path.length > 1024 || !validMutationPath(value.path)) return false
  if (typeof value.method !== 'string' || !['POST', 'PUT', 'DELETE'].includes(value.method)) return false
  if (!isRecord(value.body) || typeof value.requestId !== 'string' || value.requestId.length < 1 || value.requestId.length > 128) return false
  if (typeof value.createdAt !== 'number' || !Number.isSafeInteger(value.createdAt) || value.createdAt < 0) return false
  if (typeof value.uncertain !== 'boolean') return false
  if (value.expectation !== undefined && !validExpectation(value.expectation)) return false
  try { return canonicalJson(value.body).length <= BODY_BYTES && !containsSensitiveField(value.body) } catch { return false }
}
const readJournal = (session: WorkspaceSession): PendingWorkspaceMutation[] => {
  const target = storage()
  if (!target) return []
  const key = journalKey(session)
  try {
    const raw = target.getItem(key)
    if (!raw) return []
    if (raw.length > JOURNAL_BYTES) { target.removeItem(key); return [] }
    const parsed: unknown = JSON.parse(raw)
    if (!isRecord(parsed) || parsed.version !== JOURNAL_VERSION || !Array.isArray(parsed.entries) || parsed.entries.length > JOURNAL_LIMIT || !parsed.entries.every(validPendingMutation)) { target.removeItem(key); return [] }
    return parsed.entries
  } catch { try { target.removeItem(key) } catch { /* Storage may be unavailable. */ } return [] }
}
const writeJournal = (session: WorkspaceSession, entries: PendingWorkspaceMutation[]): void => {
  const target = storage()
  if (!target) throw new WorkspaceError(0, 'This browser cannot retain a safe retry in this tab. No request was sent.', 'mutation_journal_unavailable')
  const raw = JSON.stringify({ version: JOURNAL_VERSION, entries })
  if (entries.length > JOURNAL_LIMIT || raw.length > JOURNAL_BYTES) throw new WorkspaceError(0, 'Too many operations have unknown outcomes in this tab. Resolve them before submitting another change.', 'mutation_journal_unavailable')
  try { target.setItem(journalKey(session), raw) } catch { throw new WorkspaceError(0, 'This browser cannot retain a safe retry in this tab. No request was sent.', 'mutation_journal_unavailable') }
}
const removePendingMutation = (session: WorkspaceSession, path: string, method: string): void => {
  const entries = readJournal(session).filter(entry => entry.path !== path || entry.method !== method)
  const target = storage()
  if (!target) return
  try {
    if (entries.length) target.setItem(journalKey(session), JSON.stringify({ version: JOURNAL_VERSION, entries }))
    else target.removeItem(journalKey(session))
    emitJournalChange()
  } catch { /* A conclusive response does not become uncertain if cleanup storage fails. */ }
}
const copyBody = (body: Record<string, unknown>): { body: Record<string, unknown>; key: string } => {
  let key: string
  try { key = canonicalJson(body) } catch { throw new WorkspaceError(400, 'This request cannot be safely retained because its fields are not valid JSON.') }
  if (key.length > BODY_BYTES) throw new WorkspaceError(400, 'This request is too large to retain for a safe retry.')
  const copy: unknown = JSON.parse(key)
  if (!isRecord(copy) || containsSensitiveField(copy)) throw new WorkspaceError(400, 'Workspace mutations cannot retain credentials or session secrets.')
  return { body: copy, key }
}
const validateRetainedResult = (result: unknown, expectation: WorkspaceMutationExpectation | undefined, session: WorkspaceSession, retained: PendingWorkspaceMutation): void => {
  if (!expectation) return
  if (!isRecord(result)) throw new WorkspaceError(500, 'The hub returned a success response that did not match the retained operation. Its exact retry remains available.', 'response_mismatch')
  if (expectation.kind === 'start') {
    const validStates = ['queued', 'running', 'succeeded', 'failed', 'cancelled', 'unknown', 'stale']
    if (typeof result.run_id !== 'string' || result.run_id.trim().length === 0 || result.actor_id !== session.actor.user.id || result.repository_id !== expectation.repositoryId || result.profile_id !== expectation.profileId || typeof result.state !== 'string' || !validStates.includes(result.state) || !validSubject(result.subject) || canonicalJson(result.subject) !== canonicalJson(expectation.subject)) throw new WorkspaceError(500, 'The workflow start response did not match the retained actor, repository, profile and exact subject. Its exact retry remains available.', 'response_mismatch')
    return
  }
  if (expectation.kind === 'approval') {
    const expectedId = `ws-approve-${session.actor.user.id}-${retained.requestId}`
    if (result.approval_id !== expectedId || result.actor_id !== session.actor.user.id || result.revision !== expectation.revision || result.context_digest !== expectation.contextDigest) throw new WorkspaceError(500, 'The approval response did not match the retained request, actor, revision and review context. Its exact retry remains available.', 'response_mismatch')
    return
  }
  const after = result.after
  const expectedId = `ws-integrate-${session.actor.user.id}-${retained.requestId}`
  if (result.operation_id !== expectedId || result.target !== expectation.target || result.context_digest !== expectation.contextDigest || result.approval_id !== retained.body.approval_id || !isRecord(after) || after.revision !== expectation.revision) throw new WorkspaceError(500, 'The landing response did not match the retained request, approval, revision, review context and target. Its exact retry remains available.', 'response_mismatch')
}

export const workspacePendingMutations = (session: WorkspaceSession): PendingWorkspaceMutation[] => readJournal(session)
export const workspacePendingMutationEvent = pendingMutationEvent

// Persist only bounded, non-secret intent fields. A retry is always an explicit user action
// and uses the current CSRF token; conclusive responses release the retained request ID.
export async function workspaceMutation<T>(session: WorkspaceSession, path: string, body: Record<string, unknown>, options: { method?: string; expectation?: WorkspaceMutationExpectation } = {}): Promise<T> {
  const method = options.method ?? 'POST'
  if (!['POST', 'PUT', 'DELETE'].includes(method) || path.length > 1024 || !validMutationPath(path)) throw new WorkspaceError(400, 'This workspace mutation cannot be safely retained for retry.')
  const prepared = copyBody(body)
  const entries = readJournal(session)
  const pending = entries.find(entry => entry.path === path && entry.method === method)
  if (pending && (canonicalJson(pending.body) !== prepared.key || canonicalJson(pending.expectation) !== canonicalJson(options.expectation))) throw new WorkspaceError(409, 'An earlier request has an unknown outcome. Retry its exact retained fields from Pending operation recovery before making a different change.', 'pending_mutation')
  if (!pending && entries.length >= JOURNAL_LIMIT) throw new WorkspaceError(0, 'Too many operations have unknown outcomes in this tab. Resolve them before submitting another change.', 'mutation_journal_unavailable')
  const hadPriorUncertainty = pending?.uncertain === true
  const retained: PendingWorkspaceMutation = pending ?? { path, method, body: prepared.body, requestId: crypto.randomUUID(), createdAt: Date.now(), uncertain: true, ...(options.expectation ? { expectation: options.expectation } : {}) }
  if (!pending) writeJournal(session, [...entries, retained])
  try {
    const result = await workspaceRequest<T>(path, { method, csrf: session.csrf_token, body: { ...retained.body, request_id: retained.requestId } })
    validateRetainedResult(result, retained.expectation, session, retained)
    removePendingMutation(session, path, method)
    return result
  } catch (error) {
    if (!hadPriorUncertainty && error instanceof WorkspaceError && (error.status < 500 || error.code === 'busy')) removePendingMutation(session, path, method)
    else emitJournalChange()
    throw error
  }
}
export const identityKey = (identity: WorkspaceIdentity): string => `${identity.provider}:${identity.host}:${identity.user_id}`
