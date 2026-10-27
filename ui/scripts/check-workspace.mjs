// Zero-dependency regression checks for the real workspace mutation client.
import assert from 'node:assert/strict'

class MemoryStorage {
  #values = new Map()
  getItem(key) { return this.#values.get(key) ?? null }
  setItem(key, value) { this.#values.set(String(key), String(value)) }
  removeItem(key) { this.#values.delete(key) }
  clear() { this.#values.clear() }
  key(index) { return [...this.#values.keys()][index] ?? null }
  get length() { return this.#values.size }
}

const browser = new EventTarget()
const retainedStorage = new MemoryStorage()
Object.defineProperties(browser, {
  location: { configurable: true, value: { origin: 'https://workspace.test' } },
  sessionStorage: { configurable: true, get: () => retainedStorage },
})
globalThis.window = browser

const calls = []
let responder = async () => { throw new Error('unexpected fetch') }
globalThis.fetch = async (url, options) => {
  const body = options.body === undefined ? undefined : JSON.parse(options.body)
  const call = { url, options, body }
  calls.push(call)
  return responder(call)
}

const response = (status, body) => ({ ok: status >= 200 && status < 300, status, json: async () => body })
const session = (workspaceId, userId, csrf) => ({
  workspace: { id: workspaceId, name: 'Workspace', generation: 1, max_concurrent_runs: 1 },
  actor: {
    user: { id: userId, identity: { provider: 'github', host: 'github.com', user_id: 101 }, login: userId },
    roles: ['reader', 'contributor', 'reviewer', 'integrator', 'administrator'],
    repository_ids: ['owner/repository'],
    generation: 7,
  },
  capabilities: { manage_workspace: true, read_repositories: true, execute_workflows: true, approve_revisions: true, integrate_revisions: true },
  csrf_token: csrf,
  login_providers: ['github'],
})
const subject = {
  change_id: 'change-1',
  change_version: 3,
  context_digest: `sha256:${'c'.repeat(64)}`,
  revision: 'a'.repeat(64),
  requirement_digest: `sha256:${'d'.repeat(64)}`,
}
const runPath = '/repositories/owner%2Frepository/runs'
const runBody = { subject, profile_id: 'required-review', expected_generation: 7 }
const runExpectation = { kind: 'start', repositoryId: 'owner/repository', profileId: 'required-review', subject }
const start = (client, activeSession, body = runBody) => client.workspaceMutation(activeSession, runPath, body, { expectation: runExpectation })
const runResponse = (runId, overrides = {}) => ({
  run_id: runId,
  repository_id: 'owner/repository',
  profile_id: 'required-review',
  actor_id: 'user-1',
  generation: 7,
  state: 'queued',
  subject,
  native_run_id: 'native-run-1',
  receipt: null,
  detail: null,
  ...overrides,
})

const first = await import(`../src/workspace.ts?workspace-check-first=${Date.now()}`)
const originalSession = session('workspace-1', 'user-1', 'csrf-old')
responder = async () => { throw new TypeError('connection lost after dispatch') }
await assert.rejects(() => start(first, originalSession), TypeError)
assert.equal(calls.length, 1)
const firstRequest = calls[0]
const originalRequestId = firstRequest.body.request_id
assert.equal(typeof originalRequestId, 'string')
assert.deepEqual(firstRequest.body, { ...runBody, request_id: originalRequestId })
assert.equal(first.workspacePendingMutations(originalSession).length, 1)

// A fresh module instance models a reload. Storage remains, module state does not.
const reloaded = await import(`../src/workspace.ts?workspace-check-reload=${Date.now()}`)
const refreshedSession = session('workspace-1', 'user-1', 'csrf-before-reauth')
assert.equal(reloaded.workspacePendingMutations(refreshedSession)[0].requestId, originalRequestId)
assert.equal(reloaded.workspacePendingMutations(refreshedSession)[0].uncertain, true)

responder = async () => response(401, { error: 'unauthorized' })
await assert.rejects(
  () => start(reloaded, refreshedSession),
  error => error instanceof reloaded.WorkspaceError && error.status === 401,
)
assert.equal(reloaded.workspacePendingMutations(refreshedSession)[0].requestId, originalRequestId, 'a later authentication rejection cannot erase earlier uncertainty')

const reauthenticated = await import(`../src/workspace.ts?workspace-check-reauth=${Date.now()}`)
const reauthenticatedSession = session('workspace-1', 'user-1', 'csrf-current')
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession)[0].requestId, originalRequestId)

const callsBeforeChangedBody = calls.length
await assert.rejects(
  () => start(reauthenticated, reauthenticatedSession, { ...runBody, expected_generation: 8 }),
  error => error instanceof reauthenticated.WorkspaceError && error.code === 'pending_mutation',
)
assert.equal(calls.length, callsBeforeChangedBody, 'changed fields are blocked before transport')

assert.equal(reauthenticated.workspacePendingMutations(session('workspace-2', 'user-1', 'csrf-other')).length, 0, 'another workspace cannot see the intent')
assert.equal(reauthenticated.workspacePendingMutations(session('workspace-1', 'user-2', 'csrf-other')).length, 0, 'another user cannot see the intent')

const otherUserSession = session('workspace-1', 'user-2', 'csrf-other')
responder = async () => response(400, { error: 'invalid_request' })
await assert.rejects(() => start(reauthenticated, otherUserSession), error => error instanceof reauthenticated.WorkspaceError && error.status === 400)
assert.notEqual(calls.at(-1).body.request_id, originalRequestId, 'another user cannot replay the retained request ID')
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession)[0].requestId, originalRequestId, 'another user cannot release the original intent')

responder = async () => response(200, {})
await assert.rejects(
  () => start(reauthenticated, reauthenticatedSession),
  error => error instanceof reauthenticated.WorkspaceError && error.code === 'response_mismatch',
)
assert.equal(calls.at(-1).body.request_id, originalRequestId, 'malformed success after reload retains the original request ID')
assert.deepEqual(calls.at(-1).body, firstRequest.body, 'malformed success after reload retains the original request body')

responder = async () => response(200, runResponse('run-1'))
await start(reauthenticated, reauthenticatedSession)
const retriedRun = calls.at(-1)
assert.equal(retriedRun.body.request_id, originalRequestId)
assert.deepEqual(retriedRun.body, firstRequest.body, 'reload retry preserves the complete request body')
assert.equal(retriedRun.options.headers['x-csrf-token'], 'csrf-current', 'reload retry uses the current CSRF token')
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession).length, 0)

responder = async () => response(200, runResponse('run-wrong-repository', { repository_id: 'another/repository' }))
await assert.rejects(
  () => start(reauthenticated, reauthenticatedSession),
  error => error instanceof reauthenticated.WorkspaceError && error.code === 'response_mismatch',
)
const inMemoryStartId = calls.at(-1).body.request_id
const inMemoryStartBody = calls.at(-1).body
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession)[0].requestId, inMemoryStartId)
responder = async () => response(200, runResponse('run-in-memory-retry'))
await start(reauthenticated, reauthenticatedSession)
assert.equal(calls.at(-1).body.request_id, inMemoryStartId, 'in-memory malformed success retains the request ID')
assert.deepEqual(calls.at(-1).body, inMemoryStartBody, 'in-memory malformed success retains the request body')
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession).length, 0)

const approvalPath = '/repositories/owner%2Frepository/profiles/required-review/approve'
const approvalBody = { subject, expected_generation: 7 }
const approvalExpectation = { kind: 'approval', revision: subject.revision, contextDigest: subject.context_digest }
let approvalRequestId
responder = async call => {
  approvalRequestId = call.body.request_id
  return response(200, {
    approval_id: `ws-approve-user-1-${approvalRequestId}-wrong`,
    revision: subject.revision,
    context_digest: subject.context_digest,
    actor_id: 'user-1',
  })
}
await assert.rejects(
  () => reauthenticated.workspaceMutation(reauthenticatedSession, approvalPath, approvalBody, { expectation: approvalExpectation }),
  error => error instanceof reauthenticated.WorkspaceError && error.code === 'response_mismatch',
)
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession)[0].requestId, approvalRequestId)

responder = async call => response(200, {
  approval_id: `ws-approve-user-1-${call.body.request_id}`,
  revision: subject.revision,
  context_digest: subject.context_digest,
  actor_id: 'another-user',
})
await assert.rejects(
  () => reauthenticated.workspaceMutation(reauthenticatedSession, approvalPath, approvalBody, { expectation: approvalExpectation }),
  error => error instanceof reauthenticated.WorkspaceError && error.code === 'response_mismatch',
)
assert.equal(calls.at(-1).body.request_id, approvalRequestId, 'actor mismatch retains the approval request ID')

responder = async call => response(200, {
  approval_id: `ws-approve-user-1-${call.body.request_id}`,
  revision: subject.revision,
  context_digest: subject.context_digest,
  actor_id: 'user-1',
})
await reauthenticated.workspaceMutation(reauthenticatedSession, approvalPath, approvalBody, { expectation: approvalExpectation })
assert.equal(calls.at(-1).body.request_id, approvalRequestId, 'matching approval retries with its original ID')
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession).length, 0)

const landingPath = '/repositories/owner%2Frepository/profiles/required-review/integrate'
const landingBody = { subject, expected_generation: 7, approval_id: `ws-approve-user-1-${approvalRequestId}` }
const landingExpectation = { kind: 'landing', revision: subject.revision, contextDigest: subject.context_digest, target: 'main' }
let landingRequestId
responder = async call => {
  landingRequestId = call.body.request_id
  return response(200, {
    operation_id: `ws-integrate-user-1-${landingRequestId}-wrong`,
    target: 'main',
    after: { revision: subject.revision, version: 2 },
    approval_id: landingBody.approval_id,
    context_digest: subject.context_digest,
  })
}
await assert.rejects(
  () => reauthenticated.workspaceMutation(reauthenticatedSession, landingPath, landingBody, { expectation: landingExpectation }),
  error => error instanceof reauthenticated.WorkspaceError && error.code === 'response_mismatch',
)
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession)[0].requestId, landingRequestId)

responder = async call => response(200, {
  operation_id: `ws-integrate-user-1-${call.body.request_id}`,
  target: 'main',
  after: { revision: subject.revision, version: 2 },
  approval_id: landingBody.approval_id,
  context_digest: 'wrong-context',
})
await assert.rejects(
  () => reauthenticated.workspaceMutation(reauthenticatedSession, landingPath, landingBody, { expectation: landingExpectation }),
  error => error instanceof reauthenticated.WorkspaceError && error.code === 'response_mismatch',
)
assert.equal(calls.at(-1).body.request_id, landingRequestId, 'context mismatch retains the landing request ID')

responder = async call => response(200, {
  operation_id: `ws-integrate-user-1-${call.body.request_id}`,
  target: 'main',
  after: { revision: subject.revision, version: 2 },
  approval_id: landingBody.approval_id,
  context_digest: subject.context_digest,
})
await reauthenticated.workspaceMutation(reauthenticatedSession, landingPath, landingBody, { expectation: landingExpectation })
assert.equal(calls.at(-1).body.request_id, landingRequestId, 'matching landing retries with its original ID')
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession).length, 0)

responder = async () => { throw new TypeError('connection lost after dispatch') }
await assert.rejects(() => start(reauthenticated, reauthenticatedSession), TypeError)
const busyRecoveryRequestId = calls.at(-1).body.request_id
responder = async () => response(503, { error: 'busy' })
await assert.rejects(
  () => start(reauthenticated, reauthenticatedSession),
  error => error instanceof reauthenticated.WorkspaceError && error.code === 'busy',
)
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession)[0].requestId, busyRecoveryRequestId, 'busy cannot erase an already-uncertain request')
responder = async () => response(200, runResponse('run-after-busy'))
await start(reauthenticated, reauthenticatedSession)
assert.equal(calls.at(-1).body.request_id, busyRecoveryRequestId)
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession).length, 0)

responder = async () => response(503, { error: 'busy' })
await assert.rejects(
  () => start(reauthenticated, reauthenticatedSession),
  error => error instanceof reauthenticated.WorkspaceError && error.code === 'busy' && reauthenticated.workspaceError(error).includes('queue is full'),
)
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession).length, 0, 'a first-attempt busy response releases the intent')

const promisePath = '/promises/prm_01/changes'
const promiseBody = { expected_version: 1, expected_digest: 'a'.repeat(64), reason: 'Clarify the commitment.', action: { kind: 'retire' } }
const promiseExpectation = { kind: 'promise', promiseId: 'prm_01', expectedVersion: 2 }
responder = async () => { throw new TypeError('connection lost after dispatch') }
await assert.rejects(() => reauthenticated.workspaceMutation(reauthenticatedSession, promisePath, promiseBody, { expectation: promiseExpectation }), TypeError)
const promiseRequest = calls.at(-1).body
assert.equal(typeof promiseRequest.operation_id, 'string')
assert.equal(promiseRequest.request_id, undefined, 'promise mutations use the backend operation_id field')
responder = async () => response(200, { ref: { workspace_id: 'workspace-1', promise_id: 'prm_01', version: 2, digest: 'b'.repeat(64) }, replayed: true })
await reauthenticated.workspaceMutation(reauthenticatedSession, promisePath, promiseBody, { expectation: promiseExpectation })
assert.deepEqual(calls.at(-1).body, promiseRequest, 'an uncertain promise mutation replays the exact body and operation ID')
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession).length, 0)

const nextPromiseBody = { ...promiseBody, expected_version: 2, expected_digest: 'b'.repeat(64), reason: 'Retire after review.' }
const nextPromiseExpectation = { ...promiseExpectation, expectedVersion: 3 }
responder = async () => response(200, { ref: { workspace_id: 'workspace-1', promise_id: 'prm_01', version: 3, digest: 'c'.repeat(64) }, replayed: false, hidden: true })
await assert.rejects(
  () => reauthenticated.workspaceMutation(reauthenticatedSession, promisePath, nextPromiseBody, { expectation: nextPromiseExpectation }),
  error => error instanceof reauthenticated.WorkspaceError && error.code === 'response_mismatch',
)
const malformedPromiseRequest = calls.at(-1).body
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession)[0].requestId, malformedPromiseRequest.operation_id, 'malformed promise success retains the operation ID')
responder = async () => response(200, { ref: { workspace_id: 'workspace-1', promise_id: 'prm_01', version: 3, digest: 'c'.repeat(64) }, replayed: true })
await reauthenticated.workspaceMutation(reauthenticatedSession, promisePath, nextPromiseBody, { expectation: nextPromiseExpectation })
assert.deepEqual(calls.at(-1).body, malformedPromiseRequest, 'malformed promise success keeps an exact retry')
assert.equal(reauthenticated.workspacePendingMutations(reauthenticatedSession).length, 0)

const fetchesBeforeUnavailableStorage = calls.length
Object.defineProperty(browser, 'sessionStorage', { configurable: true, get: () => { throw new Error('storage disabled') } })
await assert.rejects(
  () => start(reauthenticated, reauthenticatedSession),
  error => error instanceof reauthenticated.WorkspaceError && error.code === 'mutation_journal_unavailable',
)
assert.equal(calls.length, fetchesBeforeUnavailableStorage, 'storage failure sends no request')

console.log('workspace mutation recovery checks passed')
