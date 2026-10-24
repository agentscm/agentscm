import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { decodeChangeRecordResponse, fetchChangeRecord, InvalidChangeRecordResponse } from '../../src/changeRecord.ts'
import { ChangeRecordPage, changeRecordHref, changeRecordIdFromHash } from '../../src/pages/ChangeRecord.tsx'
import { json } from '../fixtures/mockFetch.ts'

const WORK_ID = `wrk_${'a'.repeat(32)}`
const OTHER_WORK_ID = `wrk_${'b'.repeat(32)}`
const SOURCE_ID = 'c'.repeat(64)
const RECIPE_DIGEST = 'd'.repeat(64)
const SETUP_DIGEST = 'e'.repeat(64)
const SNAPSHOT_DIGEST = 'f'.repeat(64)
const HEAD = '1'.repeat(40)
const TREE = '2'.repeat(40)
const BASE = '3'.repeat(40)
const MAX_RESPONSE_BYTES = 48 * 1024
const bytes = (value: string) => new TextEncoder().encode(value).byteLength

const fixture = () => ({
  version: 1,
  availability: 'available',
  summary: {
    schema_version: 1,
    work_id: WORK_ID,
    generation: 3,
    as_of: 1_798_000_300,
    intent: {
      title: 'Capture a bounded change-record handoff',
      scope: ['ui/src/**', 'src/hub/mcp.rs'],
      scope_omitted: 0,
    },
    subject: {
      source_id: SOURCE_ID,
      source_kind: 'git',
      source: {
        kind: 'git',
        repository: 'counterbranch/agentscm',
        worktree_id: 'worktree-1',
        head: HEAD,
        tree: TREE,
        base: BASE,
        dirty: true,
        untracked_count: 2,
        omissions: ['ignored_content'],
        captured_snapshot: {
          digest: SNAPSHOT_DIGEST,
          size_bytes: 12_345,
          media: 'json',
          retention: 'work_local',
          availability: 'local',
        },
        snapshot_coverage: 'partial',
      },
    },
    sessions: { count: 2, coverage: 'partial', gaps: 1 },
    latest_action: {
      action_id: 'action-1',
      source_id: SOURCE_ID,
      kind: 'verification',
      outcome: 'succeeded',
      recipe_provenance: 'configured_exact_match',
      recipe_digest: RECIPE_DIGEST,
      setup_digest: SETUP_DIGEST,
      result: 'passed',
      applicability: 'stale',
      assurance: 'caller_asserted',
      evidence_availability: 'local',
      coverage: 'partial',
      started_at: 1_798_000_000,
      finished_at: 1_798_000_120,
      drift: 'source_changed',
    },
    decision: {
      status: 'proposed',
      reference: 'decision-1',
      summary: 'Keep the read surface bounded and expose source coverage.',
      source_refs: [
        { kind: 'repository_path', path: 'ui/src/changeRecord.ts', line: 42 },
        { kind: 'commit', oid: HEAD, path: 'ui/src/pages/ChangeRecord.tsx' },
        { kind: 'work', work_id: OTHER_WORK_ID },
        { kind: 'captured_event', session_id: 'session-7', event_id: 'event:42' },
        { kind: 'prior_work_version', work_id: WORK_ID, event_seq: 91 },
      ],
      source_refs_omitted: 0,
      author_kind: 'agent',
      author_agent: 'review-agent',
      at: 1_798_000_060,
      generation: 3,
    },
    next_action: 'Run the focused UI check against the current source.',
    completeness: { events: 'partial', source: 'complete', setup: 'unknown', recipe: 'partial', result: 'complete', usage: 'unknown' },
    unknowns: ['usage', 'human_decision'],
    transcript_body: 'private transcript must never render',
  },
})

const executionFixture = () => {
  const value = fixture()
  value.summary.latest_action.kind = 'execution'
  value.summary.latest_action.outcome = 'succeeded'
  value.summary.latest_action.recipe_provenance = 'observed_native_input'
  value.summary.latest_action.result = 'unknown'
  value.summary.completeness.recipe = 'unknown'
  value.summary.completeness.result = 'unknown'
  value.summary.unknowns = ['result', 'usage', 'human_decision']
  return value
}

afterEach(() => {
  vi.unstubAllGlobals()
  window.location.hash = ''
})

describe('change record response', () => {
  it('decodes the bounded projection with inspectable source, intent, decision and independent action dimensions', () => {
    const value = decodeChangeRecordResponse(fixture(), WORK_ID)
    expect(value.summary.intent).toEqual({
      title: 'Capture a bounded change-record handoff',
      scope: ['ui/src/**', 'src/hub/mcp.rs'],
      scope_omitted: 0,
    })
    expect(value.summary.subject?.source).toMatchObject({
      kind: 'git',
      head: HEAD,
      tree: TREE,
      dirty: true,
      snapshot_coverage: 'partial',
      captured_snapshot: { digest: SNAPSHOT_DIGEST, availability: 'local' },
    })
    expect(value.summary.latest_action).toMatchObject({
      kind: 'verification',
      outcome: 'succeeded',
      recipe_provenance: 'configured_exact_match',
      result: 'passed',
      applicability: 'stale',
      assurance: 'caller_asserted',
      evidence_availability: 'local',
      coverage: 'partial',
    })
    expect(value.summary.decision).toMatchObject({
      status: 'proposed',
      summary: 'Keep the read surface bounded and expose source coverage.',
      author_kind: 'agent',
      author_agent: 'review-agent',
    })
    expect(value.summary).not.toHaveProperty('transcript_body')

    const mixed = fixture()
    mixed.summary.latest_action.evidence_availability = 'mixed'
    expect(decodeChangeRecordResponse(mixed).summary.latest_action?.evidence_availability).toBe('mixed')
  })

  it('decodes native source facts without accepting host paths', () => {
    const value = fixture()
    value.summary.subject = {
      source_id: SOURCE_ID,
      source_kind: 'native',
      source: {
        kind: 'native',
        repository: 'counterbranch/agentscm',
        revision: '4'.repeat(64),
        change_id: 'change-7',
        context_digest: '5'.repeat(64),
        source_generation: 4,
      },
    } as unknown as typeof value.summary.subject
    expect(decodeChangeRecordResponse(value).summary.subject?.source).toMatchObject({
      kind: 'native',
      change_id: 'change-7',
      source_generation: 4,
    })
  })

  it('rejects execution receipts represented as passing verification', () => {
    const value = executionFixture()
    value.summary.latest_action.result = 'passed'
    expect(() => decodeChangeRecordResponse(value)).toThrow(/execution_result/)

    const unmatched = fixture()
    unmatched.summary.latest_action.recipe_provenance = 'observed_native_input'
    expect(() => decodeChangeRecordResponse(unmatched)).toThrow(/verification_result/)
  })

  it('rejects inconsistent source, unbounded provenance claims, hidden text and a mismatched work identity', () => {
    const wrongAvailability = fixture()
    wrongAvailability.summary.latest_action.evidence_availability = 'passed'
    expect(() => decodeChangeRecordResponse(wrongAvailability)).toThrow(InvalidChangeRecordResponse)

    const wrongKind = fixture()
    wrongKind.summary.subject.source_kind = 'native'
    expect(() => decodeChangeRecordResponse(wrongKind)).toThrow(/source_kind_mismatch/)

    const completeRecipe = fixture()
    completeRecipe.summary.completeness.recipe = 'complete'
    expect(() => decodeChangeRecordResponse(completeRecipe)).toThrow(/provenance/)

    const hidden = fixture()
    hidden.summary.next_action = 'safe\u202ehtml'
    expect(() => decodeChangeRecordResponse(hidden)).toThrow(/next_action/)
    expect(() => decodeChangeRecordResponse(fixture(), OTHER_WORK_ID)).toThrow(/work_id_mismatch/)
  })

  it('rejects malformed snapshot facts and decision references', () => {
    const missingDirtySnapshot = fixture()
    missingDirtySnapshot.summary.subject.source = {
      ...missingDirtySnapshot.summary.subject.source,
      captured_snapshot: null,
      snapshot_coverage: 'complete',
    } as unknown as typeof missingDirtySnapshot.summary.subject.source
    expect(() => decodeChangeRecordResponse(missingDirtySnapshot)).toThrow(/captured_snapshot/)

    const absoluteRef = fixture()
    absoluteRef.summary.decision.source_refs[0] = { kind: 'repository_path', path: '/private/source', line: 1 }
    expect(() => decodeChangeRecordResponse(absoluteRef)).toThrow(/source_refs\[0\].path/)

    const malformedCapturedEvent = fixture()
    malformedCapturedEvent.summary.decision.source_refs[3] = { kind: 'captured_event', session_id: 'session 7', event_id: 'event:42' }
    expect(() => decodeChangeRecordResponse(malformedCapturedEvent)).toThrow(/source_refs\[3\].session_id/)

    const missingPriorVersion = fixture()
    missingPriorVersion.summary.decision.source_refs[4] = { kind: 'prior_work_version', work_id: WORK_ID, event_seq: 0 }
    expect(() => decodeChangeRecordResponse(missingPriorVersion)).toThrow(/source_refs\[4\].event_seq/)
  })

  it('requires typed omission counts and rejects projected arrays above their shared bounds', () => {
    const missing = fixture()
    delete (missing.summary.intent as unknown as Record<string, unknown>)['scope_omitted']
    expect(() => decodeChangeRecordResponse(missing)).toThrow(/scope_omitted/)

    const negative = fixture()
    negative.summary.decision.source_refs_omitted = -1
    expect(() => decodeChangeRecordResponse(negative)).toThrow(/source_refs_omitted/)

    const excessiveScope = fixture()
    excessiveScope.summary.intent.scope = Array.from({ length: 17 }, (_, index) => `src/scope-${index}.rs`)
    expect(() => decodeChangeRecordResponse(excessiveScope)).toThrow(/summary.intent.scope/)

    const excessiveRefs = fixture()
    excessiveRefs.summary.decision.source_refs = Array.from({ length: 17 }, (_, index) => ({ kind: 'repository_path', path: `src/ref-${index}.rs`, line: index + 1 })) as typeof excessiveRefs.summary.decision.source_refs
    expect(() => decodeChangeRecordResponse(excessiveRefs)).toThrow(/summary.decision.source_refs/)
  })

  it('decodes optional session setups strictly and defaults to none observed', () => {
    const absent = decodeChangeRecordResponse(fixture(), WORK_ID)
    expect(absent.summary.session_setups).toEqual([])
    expect(absent.summary.session_setups_omitted).toBe(0)

    const present = fixture() as ReturnType<typeof fixture> & { summary: Record<string, unknown> }
    present.summary['session_setups'] = [
      { session_id: 'session-1', parent_session_id: null, model: 'model-a', permission_mode: null, effort: 'high' },
    ]
    present.summary['session_setups_omitted'] = 2
    const decoded = decodeChangeRecordResponse(present, WORK_ID)
    expect(decoded.summary.session_setups).toEqual([
      { session_id: 'session-1', parent_session_id: null, model: 'model-a', permission_mode: null, effort: 'high' },
    ])
    expect(decoded.summary.session_setups_omitted).toBe(2)

    const setup = (patch: Record<string, unknown>) => {
      const value = fixture() as ReturnType<typeof fixture> & { summary: Record<string, unknown> }
      value.summary['session_setups'] = [{ session_id: 'session-1', parent_session_id: null, model: null, permission_mode: null, effort: null, ...patch }]
      return value
    }
    expect(() => decodeChangeRecordResponse(setup({ model: '<img src=x onerror=alert(1)>' }))).toThrow(/session_setups\[0\]\.model/)
    expect(() => decodeChangeRecordResponse(setup({ effort: '' }))).toThrow(/effort/)
    expect(() => decodeChangeRecordResponse(setup({ permission_mode: 7 }))).toThrow(/permission_mode/)
    expect(() => decodeChangeRecordResponse(setup({ session_id: null }))).toThrow(/session_id/)
    expect(() => decodeChangeRecordResponse(setup({ parent_session_id: undefined }))).toThrow(/parent_session_id/)
    const tooMany = setup({})
    tooMany.summary['session_setups'] = Array.from({ length: 17 }, (_, index) => ({ session_id: `s-${index}`, parent_session_id: null, model: null, permission_mode: null, effort: null }))
    expect(() => decodeChangeRecordResponse(tooMany)).toThrow(/summary.session_setups/)
    const negative = setup({})
    negative.summary['session_setups_omitted'] = -1
    expect(() => decodeChangeRecordResponse(negative)).toThrow(/session_setups_omitted/)
  })

  it('accepts a worst-case escaped projection only after explicit deterministic omissions fit the shared 48 KiB envelope', async () => {
    const value = fixture()
    const escaped = '"'.repeat(1000)
    value.summary.intent.scope = Array.from({ length: 16 }, (_, index) => `s${index}-${escaped}`)
    value.summary.decision.source_refs = Array.from({ length: 16 }, (_, index) => ({ kind: 'repository_path', path: `r${index}-${escaped}`, line: index + 1 })) as typeof value.summary.decision.source_refs
    while (bytes(JSON.stringify(value)) > MAX_RESPONSE_BYTES) {
      const scope = value.summary.intent.scope.at(-1)
      const reference = value.summary.decision.source_refs.at(-1)
      const scopeBytes = scope === undefined ? -1 : bytes(JSON.stringify(scope))
      const referenceBytes = reference === undefined ? -1 : bytes(JSON.stringify(reference))
      if (scopeBytes >= referenceBytes && scope !== undefined) {
        value.summary.intent.scope.pop()
        value.summary.intent.scope_omitted++
      } else if (reference !== undefined) {
        value.summary.decision.source_refs.pop()
        value.summary.decision.source_refs_omitted++
      }
    }
    expect(value.summary.intent.scope_omitted + value.summary.decision.source_refs_omitted).toBeGreaterThan(0)
    expect(bytes(JSON.stringify(value))).toBeLessThanOrEqual(MAX_RESPONSE_BYTES)

    vi.stubGlobal('fetch', async () => new Response(JSON.stringify(value), { status: 200, headers: { 'content-type': 'application/json' } }))
    const decoded = await fetchChangeRecord('counterbranch/agentscm', WORK_ID, 'read-token', new AbortController().signal)
    expect(decoded.summary.intent.scope_omitted).toBe(value.summary.intent.scope_omitted)
    expect(decoded.summary.decision.source_refs_omitted).toBe(value.summary.decision.source_refs_omitted)
    expect(decoded.summary.intent.title).toBe('Capture a bounded change-record handoff')
    expect(decoded.summary.next_action).toBe('Run the focused UI check against the current source.')
  })

  it('uses one bounded read-only repository request', async () => {
    const sent: { url: string; init: RequestInit }[] = []
    vi.stubGlobal('fetch', async (input: string, init: RequestInit = {}) => {
      sent.push({ url: input, init })
      return json(200, fixture())
    })
    const result = await fetchChangeRecord('counterbranch/agentscm', WORK_ID, 'read-token', new AbortController().signal)
    expect(result.summary.work_id).toBe(WORK_ID)
    expect(sent).toHaveLength(1)
    expect(sent[0]!.url).toBe(`/v1/hub/repos/${encodeURIComponent('counterbranch/agentscm')}/change-records/${WORK_ID}`)
    expect((sent[0]!.init.method ?? 'GET').toUpperCase()).toBe('GET')
    expect(new Headers(sent[0]!.init.headers).get('authorization')).toBe('Bearer read-token')
    expect(sent[0]!.init.credentials).toBe('omit')
  })

  it('refuses a declared oversized response before parsing it', async () => {
    vi.stubGlobal('fetch', async () => json(200, fixture(), { 'content-length': String(MAX_RESPONSE_BYTES + 1) }))
    await expect(fetchChangeRecord('counterbranch/agentscm', WORK_ID, 'read-token', new AbortController().signal)).rejects.toThrow(/too large/)
  })

  it('accepts the exact 48 KiB wire bound and rejects a streamed byte beyond it', async () => {
    const encoded = JSON.stringify(fixture())
    expect(bytes(encoded)).toBeLessThan(MAX_RESPONSE_BYTES)
    const exact = encoded + ' '.repeat(MAX_RESPONSE_BYTES - bytes(encoded))
    vi.stubGlobal('fetch', async () => new Response(exact, { status: 200, headers: { 'content-type': 'application/json' } }))
    await expect(fetchChangeRecord('counterbranch/agentscm', WORK_ID, 'read-token', new AbortController().signal)).resolves.toMatchObject({ version: 1 })

    vi.stubGlobal('fetch', async () => new Response(' '.repeat(MAX_RESPONSE_BYTES + 1), { status: 200, headers: { 'content-type': 'application/json' } }))
    await expect(fetchChangeRecord('counterbranch/agentscm', WORK_ID, 'read-token', new AbortController().signal)).rejects.toThrow(/too large/)
  })
})

describe('change record route and page', () => {
  it('round-trips valid work IDs and rejects malformed routes', () => {
    expect(changeRecordHref(WORK_ID)).toBe(`#change-record/${WORK_ID}`)
    expect(changeRecordIdFromHash(`#change-record/${WORK_ID}`)).toBe(WORK_ID)
    expect(changeRecordIdFromHash('#change-record/%E0%A4%A')).toBeNull()
    expect(changeRecordIdFromHash('#change-record/not-a-work-id')).toBeNull()
  })

  it('renders inspectable tested source, intent, agent decision and independent verification dimensions', async () => {
    const response = fixture()
    response.summary.intent.scope_omitted = 240
    response.summary.decision.source_refs_omitted = 84
    const sent: { method: string; url: string }[] = []
    vi.stubGlobal('fetch', async (input: string, init: RequestInit = {}) => {
      sent.push({ method: (init.method ?? 'GET').toUpperCase(), url: input })
      return json(200, response)
    })
    window.location.hash = changeRecordHref(WORK_ID)
    render(<ChangeRecordPage repo="counterbranch/agentscm" token="read-token" />)

    await screen.findByRole('heading', { name: 'Latest verification' })
    expect(screen.getAllByText('Capture a bounded change-record handoff').length).toBeGreaterThan(0)
    expect(screen.getByText('ui/src/**')).toBeTruthy()
    expect(screen.getByText('240 additional scope entries omitted from this bounded projection.')).toBeTruthy()
    expect(screen.getByText('Git working source')).toBeTruthy()
    expect(screen.getByText(HEAD)).toBeTruthy()
    expect(screen.getByText(TREE)).toBeTruthy()
    expect(screen.getByText(BASE)).toBeTruthy()
    expect(screen.getByText(SNAPSHOT_DIGEST)).toBeTruthy()
    expect(screen.getByText('Ignored content')).toBeTruthy()
    expect(screen.getByText('Passed')).toBeTruthy()
    expect(screen.getByText('Verification')).toBeTruthy()
    expect(screen.getByText('Configured recipe match')).toBeTruthy()
    expect(screen.getByText('Stale')).toBeTruthy()
    expect(screen.getByText('Caller asserted')).toBeTruthy()
    expect(screen.getByText('Evidence local')).toBeTruthy()
    expect(screen.getAllByText('Partial').length).toBeGreaterThan(0)
    expect(screen.getByText('Decision proposed')).toBeTruthy()
    expect(screen.getByText('Agent proposal')).toBeTruthy()
    expect(screen.getByText(/This is not human approval/)).toBeTruthy()
    expect(screen.getByText('Keep the read surface bounded and expose source coverage.')).toBeTruthy()
    expect(screen.getByText(/Declared captured event/)).toBeTruthy()
    expect(screen.getByText(/session-7 \/ event:42/)).toBeTruthy()
    expect(screen.getByText(/Supersedes version/)).toBeTruthy()
    expect(screen.getByText('91')).toBeTruthy()
    expect(screen.getByText('84 additional source references omitted from this bounded projection.')).toBeTruthy()
    expect(screen.getByText('Human decision is unknown')).toBeTruthy()
    expect(screen.getAllByText('Run the focused UI check against the current source.').length).toBeGreaterThan(0)
    expect(screen.queryByText(/private transcript/)).toBeNull()
    expect(sent).toEqual([{ method: 'GET', url: `/v1/hub/repos/${encodeURIComponent('counterbranch/agentscm')}/change-records/${WORK_ID}` }])
  })

  it('lists model, effort and permission mode per session with unknown where absent', async () => {
    const response = fixture() as ReturnType<typeof fixture> & { summary: Record<string, unknown> }
    response.summary['session_setups'] = [
      { session_id: 'session-1', parent_session_id: null, model: 'model-a', permission_mode: 'plan', effort: 'high' },
      { session_id: 'session-2', parent_session_id: 'session-1', model: null, permission_mode: null, effort: null },
    ]
    response.summary['session_setups_omitted'] = 3
    vi.stubGlobal('fetch', async () => json(200, response))
    window.location.hash = changeRecordHref(WORK_ID)
    render(<ChangeRecordPage repo="counterbranch/agentscm" token="read-token" />)

    await screen.findByRole('heading', { name: 'Latest verification' })
    expect(screen.getByText('Session session-1')).toBeTruthy()
    expect(screen.getByText('Model model-a · effort high · permission mode plan')).toBeTruthy()
    expect(screen.getByText('Session session-2 (parent session-1)')).toBeTruthy()
    expect(screen.getByText('Model unknown · effort unknown · permission mode unknown')).toBeTruthy()
    expect(screen.getByText('3 additional sessions omitted from this bounded projection.')).toBeTruthy()
  })

  it('says session setup is unknown when none was reported', async () => {
    vi.stubGlobal('fetch', async () => json(200, fixture()))
    window.location.hash = changeRecordHref(WORK_ID)
    render(<ChangeRecordPage repo="counterbranch/agentscm" token="read-token" />)
    expect(await screen.findByText('No session setup reported')).toBeTruthy()
  })

  it('does not turn successful shell execution into a passing verification', async () => {
    vi.stubGlobal('fetch', async () => json(200, executionFixture()))
    window.location.hash = changeRecordHref(WORK_ID)
    render(<ChangeRecordPage repo="counterbranch/agentscm" token="read-token" />)

    await screen.findByRole('heading', { name: 'Latest execution' })
    expect(screen.getByText('Execution')).toBeTruthy()
    expect(screen.getByText('Succeeded')).toBeTruthy()
    expect(screen.getByText('Verification not established')).toBeTruthy()
    expect(screen.getByText('Observed command')).toBeTruthy()
    expect(screen.getByText(/no verification result was established/i)).toBeTruthy()
    expect(screen.queryByText('Passed')).toBeNull()
    expect(screen.queryByText(/Latest recorded check/)).toBeNull()
  })

  it('offers a read-only lookup and reports missing records plainly', async () => {
    vi.stubGlobal('fetch', async () => json(404, { error: 'not_found' }))
    render(<ChangeRecordPage repo="counterbranch/agentscm" token="read-token" />)
    const input = screen.getByRole('combobox', { name: 'Find a work item' })
    fireEvent.focus(input)
    fireEvent.change(input, { target: { value: WORK_ID } })
    fireEvent.keyDown(input, { key: 'Enter' })
    expect(window.location.hash).toBe(changeRecordHref(WORK_ID))

    await act(async () => window.dispatchEvent(new HashChangeEvent('hashchange')))
    await screen.findByText('No change record for this work item yet')
  })

  it('does not label an authorization denial as an empty record', async () => {
    vi.stubGlobal('fetch', async () => json(403, { error: 'forbidden' }))
    window.location.hash = changeRecordHref(WORK_ID)
    render(<ChangeRecordPage repo="counterbranch/agentscm" token="read-token" />)
    await waitFor(() => expect(screen.getByText('This token cannot read the change record')).toBeTruthy())
    expect(screen.queryByText(/No change record/)).toBeNull()
  })
})
