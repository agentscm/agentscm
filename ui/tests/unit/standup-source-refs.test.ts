import { describe, expect, it } from 'vitest'
import { decodeWorkItem } from '../../src/standup.ts'

const WORK_ID = `wrk_${'a'.repeat(32)}`

const fixture = () => ({
  id: WORK_ID,
  repository: 'counterbranch/agentscm',
  owner: 'owner-1',
  owner_login: 'octocat',
  source: { kind: 'native', request_key: 'request-1' },
  title: 'Preserve intent provenance',
  state: 'active',
  claim_status: 'active',
  last_checkin_at: 1_798_000_000,
  generation: 1,
  lead_token: 'token-1',
  lead_agent: 'agent-1',
  lead_session: 'session-1',
  participants: [{
    owner: 'owner-1',
    login: 'octocat',
    token: 'token-1',
    agent: 'agent-1',
    session: 'session-1',
    role: 'lead',
  }],
  scope: ['src/**'],
  branch: null,
  worktree: null,
  next_action: 'Continue',
  current: {
    kind: 'decision',
    summary: 'Correct the prior checkpoint.',
    next_action: 'Continue',
    source_refs: [
      { kind: 'captured_event', session_id: 'session-1', event_id: 'event:42' },
      { kind: 'prior_work_version', work_id: WORK_ID, event_seq: 19 },
    ],
    at: 1_798_000_010,
    token: 'token-1',
    agent: 'agent-1',
    session: 'session-1',
    generation: 1,
  },
  blocker: null,
  depends_on: [],
  related_work: [],
  created_at: 1_798_000_000,
  updated_at: 1_798_000_010,
  created_seq: 10,
  updated_seq: 20,
})

describe('stand-up intent source references', () => {
  it('decodes declared captured-event metadata and an exact prior Work version', () => {
    expect(decodeWorkItem(fixture()).current?.source_refs).toEqual([
      { kind: 'captured_event', session_id: 'session-1', event_id: 'event:42' },
      { kind: 'prior_work_version', work_id: WORK_ID, event_seq: 19 },
    ])
  })

  it('rejects malformed captured-event identities and non-positive prior versions', () => {
    const malformedEvent = fixture()
    malformedEvent.current.source_refs[0] = { kind: 'captured_event', session_id: 'session 1', event_id: 'event:42' }
    expect(() => decodeWorkItem(malformedEvent)).toThrow(/source_ref.session_id/)

    const missingVersion = fixture()
    missingVersion.current.source_refs[1] = { kind: 'prior_work_version', work_id: WORK_ID, event_seq: 0 }
    expect(() => decodeWorkItem(missingVersion)).toThrow(/source_ref.event_seq/)
  })
})
