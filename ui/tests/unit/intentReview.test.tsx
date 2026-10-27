import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { decodeIntentReview, fetchIntentReview, submitIntentDisposition, type IntentReview, type Subject, type IntentReference } from '../../src/intentReview.ts'
import { adoptFrontDoorLink } from '../../src/frontDoor.ts'
import { IntentReviewPanel } from '../../src/IntentReviewPanel.tsx'

const digest = (letter: string) => letter.repeat(64)
const reference = (id: string, kind: string, text: string | null): IntentReference => ({ id, kind, source: `message:${id}`, author: kind === 'agent_plan' ? 'agent' : 'human', recorded_at_ms: 1_800_000_000_000, sha256: digest('a'), coverage: 'complete', text, reconstructed: false })
const subject = (): Subject => ({
  repository: 'acme/widgets', work_id: 'wrk-intent', root_work_id: 'wrk-intent', contract_digest: digest('b'),
  scope: { parent_work_id: null, checkpoint: null, criteria: ['account-limit'], coverage: 'complete', gaps: [], source_exclusions: [] },
  source: { kind: 'git', repository: 'acme/widgets', worktree_id: 'wt-1', head: '1'.repeat(40), tree: '2'.repeat(40), base: '3'.repeat(40), dirty: false, untracked_count: 0, omissions: [], captured_snapshot: null, snapshot_coverage: 'complete' },
  evidence: [{ id: 'test-result', sha256: digest('c'), origin: 'cargo test --locked', candidate_digest: digest('d'), coverage: 'complete', assurance: 'observed_trusted_local', available: true }],
  skill: { workflow_version_id: 'wf-v1', step_id: 'intent-review', skill_version_id: 'skill-v1', skill_sha256: digest('e'), package_digest: digest('f'), entrypoint: 'SKILL.md' },
})
const fixture = (): IntentReview => ({
  record: {
    subject: subject(), contracts: [{ version: 1, previous_digest: null, request: [reference('request-1', 'human_message', 'Limit requests per account.')], amendments: [], criteria: [{ id: 'account-limit', description: 'Enforce an account-level request limit.', sources: ['request-1'], required: true, constraint: false }], focus: [reference('focus-1', 'agent_plan', 'Use a shared account key.')], coverage: 'complete', gaps: [] }],
    limits: { max_correction_rounds: 2, max_invocations: 5, wall_time_ms: 600_000 }, started_at_ms: 1_800_000_000_000, invocations: 2, correction_rounds: 1, cancelled: false, active_work_id: null,
    attempts: [{ attempt_id: 'attempt-1', fence: 1, phase: 'assessment', subject: subject(), started_at_ms: 1_800_000_001_000, lease_expires_at_ms: 1_800_000_002_000, state: 'finished', requested_model: null, observed_model: 'model-a', execution: 'succeeded', assessment: { criteria: [{ criterion: 'account-limit', status: 'mismatch', explanation: 'Counters are keyed by client IP.', evidence_ids: ['test-result'], corrective_outcome: 'Share counters by account.', recheck: 'Test the same account from two IPs.' }], coverage: 'complete', gaps: [] }, resulting_candidate_digest: digest('d'), detail: null }], dispositions: [],
  }, current: true, current_subject_digest: digest('9'), outcome: 'needs_correction', next: 'correct', root_invocations: 2, root_correction_rounds: 1, admission: 'not_assessed',
})
const response = (status: number, value?: unknown) => new Response(value === undefined ? null : JSON.stringify(value), { status, headers: { 'content-type': 'application/json' } })
const token = async () => {
  window.localStorage.setItem('agentscm.token', 'local:grant:acme/widgets')
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(200, { instance: digest('a').slice(0, 32) })))
  await adoptFrontDoorLink()
  vi.unstubAllGlobals()
}

afterEach(() => {
  vi.unstubAllGlobals()
  window.localStorage.clear()
  window.sessionStorage.clear()
})

describe('intent review response', () => {
  it('retains historical findings when a human amendment changes the current criteria', () => {
    const value = fixture()
    value.outcome = 'stale'
    value.next = 'assess'
    value.record.subject.contract_digest = digest('8')
    value.record.subject.scope.criteria = ['alternate-endpoint']
    value.record.contracts.push({ ...value.record.contracts[0]!, version: 2, previous_digest: digest('b'), amendments: [reference('amendment-1', 'human_amendment', 'Apply the limit to the alternate endpoint.')], criteria: [{ id: 'alternate-endpoint', description: 'Limit the alternate endpoint by account.', sources: ['amendment-1'], required: true, constraint: false }] })
    expect(decodeIntentReview(value).record.attempts[0]!.assessment!.criteria[0]!.criterion).toBe('account-limit')
    value.outcome = 'meets_recorded_criteria'
    expect(() => decodeIntentReview(value)).toThrow()
  })

  it('decodes the current assessment and tolerates additive fields', () => {
    const value = { ...fixture(), future_field: 'ignored', record: { ...fixture().record, future_field: true } }
    expect(decodeIntentReview(value)).toMatchObject({ current: true, outcome: 'needs_correction', record: { contracts: [{ request: [{ text: 'Limit requests per account.' }] }] } })
  })

  it('rejects incomplete findings when the response claims success', () => {
    const value = fixture()
    value.outcome = 'meets_recorded_criteria'
    value.record.attempts[0]!.assessment!.criteria = []
    expect(() => decodeIntentReview(value)).toThrow()
  })

  it('does not accept duplicate criterion findings or citations to absent evidence', () => {
    const duplicate = fixture()
    const findings = duplicate.record.attempts[0]!.assessment!.criteria
    findings.push({ ...findings[0]! })
    expect(() => decodeIntentReview(duplicate)).toThrow()
    const missing = fixture()
    missing.record.attempts[0]!.assessment!.criteria[0]!.evidence_ids = ['made-up']
    expect(() => decodeIntentReview(missing)).toThrow()
  })
})

describe('local intent review endpoint', () => {
  it('uses the selected local credential and returns the retained view', async () => {
    await token()
    const sent: { url: string; init?: RequestInit }[] = []
    vi.stubGlobal('fetch', async (input: RequestInfo | URL, init?: RequestInit) => {
      sent.push({ url: String(input), init })
      return response(200, fixture())
    })
    expect(await fetchIntentReview('wrk/1', new AbortController().signal)).toMatchObject({ outcome: 'needs_correction' })
    expect(sent[0]!.url).toContain('/v1/local/intent-reviews/wrk%2F1')
    expect(new Headers(sent[0]!.init?.headers).get('authorization')).toBe('Bearer local:grant:acme/widgets')
    expect(sent[0]!.init?.method).toBeUndefined()
  })

  it('distinguishes absence from denied and unavailable access', async () => {
    await token()
    vi.stubGlobal('fetch', vi.fn().mockResolvedValueOnce(response(404)).mockResolvedValueOnce(response(401)).mockResolvedValueOnce(response(403)).mockResolvedValueOnce(response(503)))
    expect(await fetchIntentReview('wrk-intent', new AbortController().signal)).toBeNull()
    await expect(fetchIntentReview('wrk-intent', new AbortController().signal)).rejects.toMatchObject({ status: 401 })
    await expect(fetchIntentReview('wrk-intent', new AbortController().signal)).rejects.toMatchObject({ status: 403 })
    await expect(fetchIntentReview('wrk-intent', new AbortController().signal)).rejects.toMatchObject({ status: 503 })
  })

  it('posts a disposition fenced to the current subject and rejects a stale subject', async () => {
    await token()
    const sent: { url: string; init?: RequestInit }[] = []
    vi.stubGlobal('fetch', async (input: RequestInfo | URL, init?: RequestInit) => {
      sent.push({ url: String(input), init })
      return response(200, { view: fixture() })
    })
    await submitIntentDisposition('wrk-intent', fixture(), 'dispute', ['account-limit'], 'The test evidence is incomplete.', new AbortController().signal)
    expect(sent[0]!.url).toContain('/v1/local/intent-reviews')
    expect(sent[0]!.init?.method).toBe('POST')
    const body = JSON.parse(String(sent[0]!.init?.body))
    expect(body).toMatchObject({ operation: 'disposition', expected_subject_digest: digest('9'), kind: 'dispute', criteria: ['account-limit'], reason: 'The test evidence is incomplete.', authorization: { kind: 'human_message', source: 'local_browser', text: 'The test evidence is incomplete.' } })
    const stale = fixture()
    stale.current = false
    await expect(submitIntentDisposition('wrk-intent', stale, 'exception', ['account-limit'], 'reason', new AbortController().signal)).rejects.toMatchObject({ status: 409 })
  })

  it('does not use a local grant away from the front door', async () => {
    window.localStorage.setItem('agentscm.token', 'local:grant:acme/widgets')
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(404)))
    await adoptFrontDoorLink()
    const fetch = vi.fn()
    vi.stubGlobal('fetch', fetch)
    await expect(fetchIntentReview('wrk-intent', new AbortController().signal)).rejects.toMatchObject({ status: 403 })
    await expect(submitIntentDisposition('wrk-intent', fixture(), 'dispute', ['account-limit'], 'reason', new AbortController().signal)).rejects.toMatchObject({ status: 403 })
    expect(fetch).not.toHaveBeenCalled()
  })
})

describe('intent review panel', () => {
  it('shows mismatch, evidence provenance, budget and execution separately from assessment', async () => {
    await token()
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(200, fixture())))
    render(<IntentReviewPanel workId="wrk-intent" />)
    expect(await screen.findByText('Needs correction')).toBeTruthy()
    expect(screen.getByText('Mismatch')).toBeTruthy()
    expect(screen.getByText(/Counters are keyed by client IP/)).toBeTruthy()
    expect(screen.getByText(/cargo test --locked · observed trusted local · complete/)).toBeTruthy()
    expect(screen.getByText(/Execution: succeeded · candidate/)).toBeTruthy()
    expect(screen.getByText(/2 of 5 agent invocations/)).toBeTruthy()
  })

  it('shows criteria outside a checkpoint as unassessed and excludes them from dispositions', async () => {
    await token()
    const value = fixture()
    value.record.subject.scope.checkpoint = 'Account-key boundary'
    value.record.subject.scope.coverage = 'complete'
    value.record.subject.scope.gaps = []
    value.record.contracts[0]!.criteria.push({ id: 'retry-bound', description: 'Bound retries per account.', sources: ['request-1'], required: true, constraint: false })
    value.record.attempts[0]!.subject.scope = { ...value.record.subject.scope }
    value.record.attempts[0]!.assessment!.criteria[0]!.status = 'supported'
    value.record.attempts[0]!.assessment!.criteria[0]!.corrective_outcome = null
    value.record.attempts[0]!.assessment!.criteria[0]!.recheck = null
    value.record.attempts[0]!.assessment!.coverage = 'complete'
    value.record.attempts[0]!.assessment!.gaps = []
    value.outcome = 'meets_recorded_criteria'
    value.next = 'complete'
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(200, value)))
    render(<IntentReviewPanel workId="wrk-intent" />)
    expect(await screen.findByText('Meets recorded criteria')).toBeTruthy()
    expect(screen.getByText('Supported')).toBeTruthy()
    expect(screen.getByText('Not assessed in this scope')).toBeTruthy()
    expect(screen.getByText(/Bound retries per account/)).toBeTruthy()
    expect(screen.getByText(/Support here does not establish support for the full Work intent/)).toBeTruthy()
    expect(screen.queryByLabelText('Bound retries per account.')).toBeNull()
    expect(screen.getByLabelText('Enforce an account-level request limit.')).toBeTruthy()
  })

  it('renders hostile excerpts as text and labels a missing assessment', async () => {
    await token()
    const value = fixture()
    value.record.contracts[0]!.request[0]!.text = '<script>hostile()</script>'
    value.record.attempts = []
    value.outcome = null
    value.next = 'assess'
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(200, value)))
    render(<IntentReviewPanel workId="wrk-intent" />)
    expect(await screen.findByText('<script>hostile()</script>')).toBeTruthy()
    expect(document.querySelector('script')).toBeNull()
    expect(await screen.findByText('No completed assessment')).toBeTruthy()
  })

  it('labels a missing record without substituting sample data', async () => {
    await token()
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(404)))
    render(<IntentReviewPanel workId="wrk-missing" />)
    expect(await screen.findByText('No retained intent assessment for this Work')).toBeTruthy()
    expect(screen.queryByText('Meets recorded criteria')).toBeNull()
  })

  it('lets a human dispute or except selected criteria and reports stale-subject conflicts', async () => {
    await token()
    const sent: { init?: RequestInit }[] = []
    vi.stubGlobal('fetch', async (_input: RequestInfo | URL, init?: RequestInit) => {
      sent.push({ init })
      return init?.method === 'POST' ? response(409) : response(200, fixture())
    })
    render(<IntentReviewPanel workId="wrk-intent" />)
    await screen.findByText('Needs correction')
    fireEvent.click(screen.getByLabelText('Enforce an account-level request limit.'))
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'The test evidence is incomplete.' } })
    fireEvent.click(screen.getByRole('button', { name: 'Record disposition' }))
    expect(await screen.findByText(/candidate or context changed/i)).toBeTruthy()
    await waitFor(() => expect(sent.some((item) => item.init?.method === 'POST')).toBe(true))
    const payload = JSON.parse(String(sent.find((item) => item.init?.method === 'POST')!.init?.body))
    expect(payload.expected_subject_digest).toBe(digest('9'))
  })

  it('refetches the retained view after a successful human disposition', async () => {
    await token()
    const methods: string[] = []
    vi.stubGlobal('fetch', async (_input: RequestInfo | URL, init?: RequestInit) => {
      methods.push(init?.method ?? 'GET')
      return init?.method === 'POST' ? response(200, { view: fixture() }) : response(200, fixture())
    })
    render(<IntentReviewPanel workId="wrk-intent" />)
    await screen.findByText('Needs correction')
    fireEvent.click(screen.getByLabelText('Enforce an account-level request limit.'))
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'The evidence does not test the requested behavior.' } })
    fireEvent.click(screen.getByRole('button', { name: 'Record disposition' }))
    expect(await screen.findByText('Disposition recorded')).toBeTruthy()
    await waitFor(() => expect(methods).toEqual(['GET', 'POST', 'GET']))
  })
})
