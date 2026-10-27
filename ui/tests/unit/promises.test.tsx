import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { OwnerChange, PromisesPage } from '../../src/pages/Promises.tsx'
import { decodeAdmissionPreflight, decodePromiseInputView, decodePromisePage, decodePromiseVersion, promiseIdFromHash, type PromiseVersion } from '../../src/promises.ts'
import { workspaceMutation, workspacePendingMutations, type WorkspaceSession } from '../../src/workspace.ts'

const DIGEST_A = 'a'.repeat(64)
const DIGEST_B = 'b'.repeat(64)
const definition = {
  original_request: 'Keep existing settings unchanged.',
  commitment: 'Existing settings stay unchanged.',
  criteria: [{ id: 'settings', text: 'Settings are preserved.', evidence_requirements: [] }],
  constraints: [],
  repositories: [{ repository_id: 'repo_01', relevance: 'Owns settings.' }],
}
const version = (number: number, digest: string): PromiseVersion => ({
  ref: { workspace_id: 'workspace', promise_id: 'prm_01', version: number, digest },
  prior_digest: number === 1 ? null : DIGEST_A,
  steward_user: 'usr_owner', state: 'active', kind: number === 1 ? 'create' : 'amend', reason: number === 1 ? null : 'new version', definition, repository_admissions: [{ repository_id: 'repo_01', forge: null, native_repository_ids: [] }], created_by: 'usr_owner', proposal_id: null, created_at: number,
})
const session: WorkspaceSession = {
  workspace: { id: 'workspace', name: 'Workspace', generation: 1, max_concurrent_runs: 1 },
  actor: { user: { id: 'usr_owner', login: 'owner', identity: { provider: 'github', host: 'github.com', user_id: 1 } }, roles: ['reader', 'contributor'], repository_ids: ['repo_01'], generation: 1 },
  capabilities: { manage_workspace: false, read_repositories: true, execute_workflows: true, approve_revisions: false, integrate_revisions: false },
  csrf_token: 'csrf', login_providers: ['github'],
}
const response = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } })
const ownerCapabilities = { amend: true, correct: true, retire: true, transfer: true, recovery_transfer: false }
const preflight = { admissions: [{ repository_id: 'repo_01', forge: null, native_repository_ids: [] }], digest: DIGEST_B }
const isPreflight = (input: RequestInfo | URL) => String(input).endsWith('/promises/admission-preflight')

beforeEach(() => {
  sessionStorage.clear()
  vi.restoreAllMocks()
})

describe('promise wire decoding and routes', () => {
  it('rejects unknown response fields', () => {
    expect(() => decodePromisePage({ promises: [], next: null, hidden_count: 3 })).toThrow(/fields/)
  })

  it('requires server-derived agent provenance on proposal reads', () => {
    const input = { input_id: 'inp_01', repository_id: 'repo_01', actor_user: 'usr_owner', submitted_by_agent: true, payload: { kind: 'proposal', operation_id: 'op_01', repository_id: 'repo_01', original_request: definition.original_request, commitment: definition.commitment, criteria: definition.criteria, constraints: [] }, adopted_promise: null, created_at: 1 }
    expect(decodePromiseInputView(input).submitted_by_agent).toBe(true)
    expect(() => decodePromiseInputView({ ...input, submitted_by_agent: 'true' })).toThrow(/actor kind/)
    const missing: Record<string, unknown> = { ...input }
    delete missing.submitted_by_agent
    expect(() => decodePromiseInputView(missing)).toThrow(/fields/)
  })

  it('requires immutable source admissions to match the version scope', () => {
    const current = version(1, DIGEST_A)
    expect(decodePromiseVersion(current).repository_admissions).toEqual([{ repository_id: 'repo_01', forge: null, native_repository_ids: [] }])
    const missing: Record<string, unknown> = { ...current }
    delete missing.repository_admissions
    expect(() => decodePromiseVersion(missing)).toThrow(/fields/)
    expect(() => decodePromiseVersion({ ...current, repository_admissions: [] })).toThrow(/scope/)
    expect(() => decodePromiseVersion({ ...current, repository_admissions: [{ repository_id: 'repo_other', forge: null, native_repository_ids: [] }] })).toThrow(/scope/)
    expect(() => decodePromiseVersion({ ...current, repository_admissions: [{ repository_id: 'repo_01', forge: null, native_repository_ids: ['native_b', 'native_a'] }] })).toThrow(/native repository admission/)
    expect(decodePromiseVersion({ ...current, repository_admissions: [{ repository_id: 'repo_01', forge: { provider: 'github', host: 'github.com', forge_repository_id: 42 }, native_repository_ids: ['native_a'] }] }).repository_admissions[0]?.forge?.forge_repository_id).toBe(42)
    expect(() => decodePromiseVersion({ ...current, repository_admissions: [{ repository_id: 'repo_01', forge: { provider: 'git\0hub', host: 'github.com', forge_repository_id: 42 }, native_repository_ids: [] }] })).toThrow(/forge provider/)
    expect(() => decodePromiseVersion({ ...current, repository_admissions: [{ repository_id: 'repo_01', forge: { provider: 'é'.repeat(33), host: 'github.com', forge_repository_id: 42 }, native_repository_ids: [] }] })).toThrow(/forge provider/)
  })

  it('rejects malformed or mismatched admission preflight before a reviewed mutation', () => {
    expect(decodeAdmissionPreflight(preflight, ['repo_01'])).toEqual(preflight)
    expect(() => decodeAdmissionPreflight({ ...preflight, digest: 'BAD' }, ['repo_01'])).toThrow(/digest/)
    expect(() => decodeAdmissionPreflight({ ...preflight, admissions: [] }, ['repo_01'])).toThrow(/scope/)
    expect(() => decodeAdmissionPreflight({ ...preflight, admissions: [preflight.admissions[0], preflight.admissions[0]] }, ['repo_01'])).toThrow(/scope/)
    expect(() => decodeAdmissionPreflight(preflight, ['repo_02'])).toThrow(/scope/)
    expect(() => decodeAdmissionPreflight(preflight, ['repo_02', 'repo_01'])).toThrow(/scope/)
    expect(() => decodeAdmissionPreflight({ ...preflight, extra: true }, ['repo_01'])).toThrow(/fields/)
    expect(() => decodeAdmissionPreflight({ ...preflight, admissions: [{ repository_id: 'repo_01', forge: { provider: 'github', host: 'git\0hub.com', forge_repository_id: 42 }, native_repository_ids: [] }] }, ['repo_01'])).toThrow(/forge host/)
    expect(() => decodeAdmissionPreflight({ ...preflight, admissions: [{ repository_id: 'repo_01', forge: { provider: 'github', host: 'é'.repeat(128), forge_repository_id: 42 }, native_repository_ids: [] }] }, ['repo_01'])).toThrow(/forge host/)
  })

  it('rejects semantically invalid retained evidence mappings and definition bounds', () => {
    const current = version(1, DIGEST_A)
    const requirement = { id: 'check_settings', criterion_id: 'settings', stage: 'code', basis: 'observed', producer: 'test', check_key: 'settings', check_digest: DIGEST_A, scenario_digest: null, workflow_digest: null, environment_digest: null, accepted_assurance: ['observed_trusted_local'] }
    const withRequirement = (patch: Record<string, unknown>) => ({ ...current, definition: { ...definition, criteria: [{ ...definition.criteria[0], evidence_requirements: [{ ...requirement, ...patch }] }] } })
    expect(decodePromiseVersion(withRequirement({})).definition.criteria[0]?.evidence_requirements).toHaveLength(1)
    expect(() => decodePromiseVersion(withRequirement({ check_digest: 'not-a-digest' }))).toThrow(/digest/)
    expect(() => decodePromiseVersion(withRequirement({ criterion_id: 'other' }))).toThrow(/owning criterion/)
    expect(() => decodePromiseVersion(withRequirement({ accepted_assurance: [] }))).toThrow(/accepted assurance/)
    expect(() => decodePromiseVersion(withRequirement({ accepted_assurance: ['observed_trusted_local', 'observed_trusted_local'] }))).toThrow(/accepted assurance/)
    expect(() => decodePromiseVersion(withRequirement({ basis: 'rehearsal', scenario_digest: DIGEST_A, workflow_digest: null, environment_digest: DIGEST_A }))).toThrow(/Rehearsal/)
    expect(() => decodePromiseVersion({ ...current, definition: { ...definition, commitment: 'x'.repeat(16 * 1024 + 1) } })).toThrow(/16 KiB/)
    expect(() => decodePromiseVersion({ ...current, definition: { ...definition, repositories: [...definition.repositories, ...definition.repositories] } })).toThrow(/unique/)
  })

  it('rejects malformed proposal and report semantics before showing advisory input', () => {
    const proposal = { input_id: 'inp_01', repository_id: 'repo_01', actor_user: 'usr_owner', submitted_by_agent: false, payload: { kind: 'proposal', operation_id: 'op_01', repository_id: 'repo_01', original_request: definition.original_request, commitment: definition.commitment, criteria: definition.criteria, constraints: [] }, adopted_promise: null, created_at: 1 }
    const badCriterion = { ...definition.criteria[0], evidence_requirements: [{ id: 'check_settings', criterion_id: 'other', stage: 'code', basis: 'observed', producer: 'test', check_key: 'settings', check_digest: DIGEST_A, scenario_digest: null, workflow_digest: null, environment_digest: null, accepted_assurance: ['observed_trusted_local'] }] }
    expect(() => decodePromiseInputView({ ...proposal, payload: { ...proposal.payload, criteria: [badCriterion] } })).toThrow(/owning criterion/)
    expect(() => decodePromiseInputView({ ...proposal, payload: { ...proposal.payload, operation_id: 'bad operation' } })).toThrow(/operation ID/)
    const report = { ...proposal, payload: { kind: 'report', operation_id: 'op_02', promise_id: 'prm_01', promise_version: 1, promise_digest: 'bad-digest', repository_id: 'repo_01', summary: 'Possible mismatch.' } }
    expect(() => decodePromiseInputView(report)).toThrow(/promise digest/)
  })

  it('parses only bounded promise detail routes', () => {
    expect(promiseIdFromHash('#promises/prm_01')).toBe('prm_01')
    expect(promiseIdFromHash('#promises/%E0%A4%A')).toBeNull()
    expect(promiseIdFromHash('#promises/not allowed')).toBeNull()
  })
})

describe('promise authorization and exact retries', () => {
  it('does not request hidden promise data before workspace sign-in', async () => {
    const fetch = vi.spyOn(globalThis, 'fetch').mockResolvedValue(response(401, { error: 'unauthorized' }))
    render(<PromisesPage />)
    await screen.findByText('Sign in to read promises')
    expect(screen.getByText(/No promise details or counts were requested/)).toBeTruthy()
    expect(fetch).toHaveBeenCalledTimes(1)
    expect(fetch.mock.calls[0]?.[0]).toBe('/v1/hub/workspace/session')
    expect(fetch.mock.calls[0]?.[1]).toMatchObject({ credentials: 'same-origin', method: 'GET' })
  })

  it('gates a remounted page on the new actor before showing cached promise data', async () => {
    window.location.hash = '#promises'
    let resolveSecondList!: (value: Response) => void
    const secondList = new Promise<Response>((resolve) => { resolveSecondList = resolve })
    const otherSession = { ...session, actor: { ...session.actor, user: { ...session.actor.user, id: 'usr_other', login: 'other' }, generation: 2 } }
    let sessions = 0
    let lists = 0
    const fetch = vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => {
      const path = String(input)
      if (path.endsWith('/session')) return response(200, ++sessions === 1 ? session : otherSession)
      if (path.includes('/promises?')) return ++lists === 1 ? response(200, { promises: [{ current: version(1, DIGEST_A) }], next: null }) : secondList
      throw new Error(`Unexpected request: ${path}`)
    })
    const first = render(<PromisesPage />)
    await screen.findByText('Existing settings stay unchanged.')
    first.unmount()
    render(<PromisesPage />)
    await screen.findByText(/You are signed in as other/)
    expect(screen.queryByText('Existing settings stay unchanged.')).toBeNull()
    expect(lists).toBe(2)
    resolveSecondList(response(200, { promises: [], next: null }))
    await screen.findByText('No readable promises on this page')
    expect(fetch.mock.calls.filter(([input]) => String(input).includes('/promises?'))).toHaveLength(2)
  })

  it('initializes adoption editing from the exact fetched proposal', async () => {
    window.location.hash = '#promises'
    const input = { input_id: 'inp_01', repository_id: 'repo_01', actor_user: 'usr_owner', submitted_by_agent: true, payload: { kind: 'proposal', operation_id: 'op_01', repository_id: 'repo_01', original_request: 'Keep the saved preference.', commitment: 'Existing preference remains.', criteria: definition.criteria, constraints: [] }, adopted_promise: null, created_at: 1 }
    vi.spyOn(globalThis, 'fetch').mockImplementation(async (request) => {
      const path = String(request)
      if (path.endsWith('/session')) return response(200, session)
      if (path.includes('/promises?')) return response(200, { promises: [], next: null })
      if (path.includes('/promises/inputs/proposals?')) return response(200, { inputs: [input], next: null })
      if (path.endsWith('/promises/inputs/inp_01')) return response(200, input)
      throw new Error(`Unexpected request: ${path}`)
    })
    render(<PromisesPage />)
    await screen.findByText('No readable promises on this page')
    fireEvent.change(screen.getByLabelText('Workspace repository key'), { target: { value: 'repo_01' } })
    fireEvent.click(screen.getByRole('button', { name: 'Load proposals' }))
    fireEvent.click(await screen.findByRole('button', { name: 'Existing preference remains.' }))
    await screen.findByText('Inspect and adopt proposal')
    expect((screen.getAllByLabelText('Original request').at(-1) as HTMLTextAreaElement).value).toBe('Keep the saved preference.')
    expect((screen.getAllByLabelText('Commitment').at(-1) as HTMLTextAreaElement).value).toBe('Existing preference remains.')
    expect(screen.getByText(/Agent for/)).toBeTruthy()
    expect((screen.getByRole('button', { name: 'Review adoption' }) as HTMLButtonElement).disabled).toBe(true)
  })

  it('refuses a change when admission review is denied or malformed, without journaling a mutation', async () => {
    const fetch = vi.spyOn(globalThis, 'fetch').mockResolvedValueOnce(response(403, { error: 'forbidden' })).mockResolvedValueOnce(response(200, { ...preflight, digest: 'bad' }))
    render(<OwnerChange session={session} current={version(1, DIGEST_A)} capabilities={ownerCapabilities} onChanged={() => {}} onRefresh={() => {}} />)
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'Clarify scope.' } })
    fireEvent.click(screen.getByRole('button', { name: 'Review exact change' }))
    await screen.findByText(/membership does not allow this action/)
    expect(screen.queryByRole('button', { name: 'Record amend' })).toBeNull()
    fireEvent.click(screen.getByRole('button', { name: 'Review exact change' }))
    await screen.findByText(/invalid admission preflight/)
    expect(workspacePendingMutations(session)).toHaveLength(0)
    expect(fetch).toHaveBeenCalledTimes(2)
    for (const [input, init] of fetch.mock.calls) {
      expect(isPreflight(input)).toBe(true)
      expect(init?.headers).toMatchObject({ 'x-csrf-token': 'csrf' })
      expect(JSON.parse(String(init?.body))).toEqual({ repository_ids: ['repo_01'] })
    }
  })

  it('retains the exact admission digest across reload and hidden retry, then replays under restored authority', async () => {
    const bodies: Record<string, unknown>[] = []
    let preflights = 0
    vi.spyOn(globalThis, 'fetch').mockImplementation(async (input, init) => {
      if (isPreflight(input)) { preflights++; return response(200, preflight) }
      bodies.push(JSON.parse(String(init?.body)) as Record<string, unknown>)
      if (bodies.length === 1) throw new TypeError('connection lost')
      if (bodies.length === 2) return response(404, { error: 'not_found' })
      return response(200, { ref: { workspace_id: 'workspace', promise_id: 'prm_01', version: 2, digest: DIGEST_B }, replayed: true })
    })
    const view = render(<OwnerChange session={session} current={version(1, DIGEST_A)} capabilities={ownerCapabilities} onChanged={() => {}} onRefresh={() => {}} />)
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'Clarify scope.' } })
    fireEvent.click(screen.getByRole('button', { name: 'Review exact change' }))
    await screen.findByRole('button', { name: 'Record amend' })
    fireEvent.click(screen.getByRole('button', { name: 'Record amend' }))
    await screen.findByRole('button', { name: 'Retry exact change' })
    expect(workspacePendingMutations(session)).toHaveLength(1)
    view.unmount()
    const narrowed = { ...session, actor: { ...session.actor, repository_ids: [], generation: 2 } }
    const pending = workspacePendingMutations(narrowed)[0]!
    await expect(workspaceMutation(narrowed, pending.path, pending.body, { expectation: pending.expectation })).rejects.toMatchObject({ status: 404 })
    expect(workspacePendingMutations(narrowed)).toHaveLength(1)
    const restored = workspacePendingMutations(session)[0]!
    await workspaceMutation(session, restored.path, restored.body, { expectation: restored.expectation })
    expect(workspacePendingMutations(session)).toHaveLength(0)
    expect(preflights).toBe(1)
    expect(bodies).toHaveLength(3)
    expect(bodies[0]).toEqual(bodies[1])
    expect(bodies[0]).toEqual(bodies[2])
    expect(bodies[0]?.expected_admissions_digest).toBe(DIGEST_B)
  })

  it('releases a no-receipt denied retry on conclusive conflict for a new review', async () => {
    let mutations = 0
    vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => {
      if (isPreflight(input)) return response(200, preflight)
      mutations++
      if (mutations === 1) throw new TypeError('connection lost')
      return response(409, { error: 'promise_conflict' })
    })
    const view = render(<OwnerChange session={session} current={version(1, DIGEST_A)} capabilities={ownerCapabilities} onChanged={() => {}} onRefresh={() => {}} />)
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'Clarify scope.' } })
    fireEvent.click(screen.getByRole('button', { name: 'Review exact change' }))
    await screen.findByRole('button', { name: 'Record amend' })
    fireEvent.click(screen.getByRole('button', { name: 'Record amend' }))
    await screen.findByRole('button', { name: 'Retry exact change' })
    view.unmount()
    const narrowed = { ...session, actor: { ...session.actor, repository_ids: [], generation: 2 } }
    const pending = workspacePendingMutations(narrowed)[0]!
    await expect(workspaceMutation(narrowed, pending.path, pending.body, { expectation: pending.expectation })).rejects.toMatchObject({ status: 409 })
    expect(workspacePendingMutations(narrowed)).toHaveLength(0)
    expect(mutations).toBe(2)
  })

  it('replays an uncertain owner change with the same operation and frozen version after rerender', async () => {
    const calls: { body: Record<string, unknown> }[] = []
    const fetch = vi.spyOn(globalThis, 'fetch').mockImplementation(async (input, init) => {
      if (isPreflight(input)) return response(200, preflight)
      calls.push({ body: JSON.parse(String(init?.body)) })
      if (calls.length === 1) throw new TypeError('connection lost')
      return response(200, { ref: { workspace_id: 'workspace', promise_id: 'prm_01', version: 2, digest: DIGEST_B }, replayed: true })
    })
    const changed = vi.fn()
    const view = render(<OwnerChange session={session} current={version(1, DIGEST_A)} capabilities={ownerCapabilities} onChanged={changed} onRefresh={() => {}} />)
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'Clarify the commitment.' } })
    fireEvent.click(screen.getByRole('button', { name: 'Review exact change' }))
    await screen.findByRole('button', { name: 'Record amend' })
    fireEvent.click(screen.getByRole('button', { name: 'Record amend' }))
    await screen.findByRole('button', { name: 'Retry exact change' })
    view.rerender(<OwnerChange session={session} current={version(2, DIGEST_B)} capabilities={ownerCapabilities} onChanged={changed} onRefresh={() => {}} />)
    expect(screen.getByText(/From version 1, exact digest/)).toBeTruthy()
    fireEvent.click(screen.getByRole('button', { name: 'Retry exact change' }))
    await waitFor(() => expect(changed).toHaveBeenCalledTimes(1))
    expect(fetch).toHaveBeenCalledTimes(3)
    expect(calls[0]!.body.operation_id).toBe(calls[1]!.body.operation_id)
    expect(calls[1]!.body.expected_version).toBe(1)
    expect(calls[1]!.body.expected_digest).toBe(DIGEST_A)
    expect(calls[1]!.body.expected_admissions_digest).toBe(DIGEST_B)
    expect(calls[1]!.body).toEqual(calls[0]!.body)
    expect(screen.queryByRole('button', { name: 'Retry exact change' })).toBeNull()
    expect((screen.getByLabelText('Reason') as HTMLTextAreaElement).value).toBe('')
  })

  it('clears a stale reviewed change after a conclusive conflict', async () => {
    vi.spyOn(globalThis, 'fetch').mockImplementation(async (input) => isPreflight(input) ? response(200, preflight) : response(409, { error: 'promise_conflict' }))
    const refresh = vi.fn()
    const view = render(<OwnerChange session={session} current={version(1, DIGEST_A)} capabilities={ownerCapabilities} onChanged={() => {}} onRefresh={refresh} />)
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'Clarify the commitment.' } })
    fireEvent.click(screen.getByRole('button', { name: 'Review exact change' }))
    await screen.findByRole('button', { name: 'Record amend' })
    fireEvent.click(screen.getByRole('button', { name: 'Record amend' }))
    await waitFor(() => expect(refresh).toHaveBeenCalledTimes(1))
    expect(screen.queryByRole('button', { name: 'Record amend' })).toBeNull()
    view.rerender(<OwnerChange session={session} current={version(2, DIGEST_B)} capabilities={ownerCapabilities} onChanged={() => {}} onRefresh={refresh} />)
    await waitFor(() => expect((screen.getByLabelText('Reason') as HTMLTextAreaElement).value).toBe(''))
    expect((screen.getByRole('button', { name: 'Review exact change' }) as HTMLButtonElement).disabled).toBe(true)
  })

  it('starts an empty editor on a new conclusive version without a pending retry', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(response(200, preflight))
    const view = render(<OwnerChange session={session} current={version(1, DIGEST_A)} capabilities={ownerCapabilities} onChanged={() => {}} onRefresh={() => {}} />)
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'Unsaved old reason.' } })
    fireEvent.click(screen.getByRole('button', { name: 'Review exact change' }))
    await screen.findByRole('button', { name: 'Record amend' })
    view.rerender(<OwnerChange session={session} current={version(2, DIGEST_B)} capabilities={ownerCapabilities} onChanged={() => {}} onRefresh={() => {}} />)
    expect(screen.queryByRole('button', { name: 'Record amend' })).toBeNull()
    expect((screen.getByLabelText('Reason') as HTMLTextAreaElement).value).toBe('')
    expect((screen.getByRole('button', { name: 'Review exact change' }) as HTMLButtonElement).disabled).toBe(true)
  })

  it('uses server-granted recovery authority and binds transfer to the reviewed version', async () => {
    let sent: Record<string, unknown> | null = null
    vi.spyOn(globalThis, 'fetch').mockImplementation(async (input, init) => {
      if (isPreflight(input)) return response(200, preflight)
      sent = JSON.parse(String(init?.body)) as Record<string, unknown>
      return response(200, { ref: { workspace_id: 'workspace', promise_id: 'prm_01', version: 2, digest: DIGEST_B }, replayed: false })
    })
    render(<OwnerChange session={session} current={version(1, DIGEST_A)} capabilities={{ amend: false, correct: false, retire: false, transfer: false, recovery_transfer: true }} onChanged={() => {}} onRefresh={() => {}} />)
    expect(screen.queryByRole('option', { name: 'Amend commitment or scope' })).toBeNull()
    expect(screen.getByRole('option', { name: 'Recovery transfer stewardship' })).toBeTruthy()
    fireEvent.change(screen.getByLabelText('Eligible target user ID'), { target: { value: 'usr_successor' } })
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'Steward account retired.' } })
    fireEvent.click(screen.getByRole('button', { name: 'Review exact change' }))
    await screen.findByRole('button', { name: 'Record recovery transfer' })
    fireEvent.click(screen.getByRole('button', { name: 'Record recovery transfer' }))
    await waitFor(() => expect(sent).not.toBeNull())
    expect(sent).toMatchObject({ expected_version: 1, expected_digest: DIGEST_A, expected_admissions_digest: DIGEST_B, reason: 'Steward account retired.', action: { kind: 'recovery_transfer', to_user: 'usr_successor' } })
  })
})
