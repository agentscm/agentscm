import assert from 'node:assert/strict'
import { decodeInsightPage, decodeWorkInsight } from '../src/insights.ts'

const item = {
  version: 1, as_of: 200, work_id: `wrk_${'1'.repeat(32)}`, title: '<img src=x onerror=alert(1)>', state: 'finished', claim_status: 'unknown',
  created_at: 100, updated_at: 190, reported_finished: true, history_complete: false, history_retention_gap: true, scanned_events: 3,
  timing: { elapsed_seconds: 90, active_seconds: 60, blocked_seconds: 0, paused_seconds: 0, handover_seconds: 0, finished_seconds: 30, unknown_seconds: 20 },
  signals: { takeovers: 1, handovers: 0, reopened: 1 },
  usage: {
    observations: 1, coverage: 'partial', input_tokens: 10, output_tokens: null, input_known_observations: 1, output_known_observations: 0,
    missing_cost_observations: 1, reported_failed_attempts: 1, groups_complete: true,
    token_groups: [{ agent: 'codex', provider: 'unknown', model: 'unknown', usage_basis: 'reported', observations: 1, input_tokens: 10, output_tokens: null, input_known_observations: 1, output_known_observations: 0 }],
    cost_groups: [],
  },
  publications: [{ publication_id: 'pub_1', commit: 'a'.repeat(40), branch_state: 'open', pr_number: 4, merge_commit: null, observed_at: 180, checks: [], association: 'claimed' }],
  publications_complete: true, commits_truncated: false, evidence_basis: 'reported_and_recorded', approval: 'not_established',
}

assert.deepEqual(decodeWorkInsight(item), item)
const page = { version: 1, as_of: 200, since: 100, until: 200, through: 9, next_after: null, complete: true, items: [item] }
assert.equal(decodeInsightPage(page).items[0]?.title, item.title)
assert.throws(() => decodeInsightPage({ ...page, complete: 'yes' }), /Invalid completeness/)
assert.throws(() => decodeInsightPage({ ...page, version: 2 }), /Unsupported work insight version/)
assert.throws(() => decodeInsightPage({ ...page, items: Array(21).fill(item) }), /Invalid insight items/)
assert.throws(() => decodeWorkInsight({ ...item, usage: { ...item.usage, coverage: 'complete' } }), /Invalid coverage/)
assert.throws(() => decodeWorkInsight({ ...item, timing: { ...item.timing, elapsed_seconds: -1 } }), /Invalid elapsed seconds/)

console.log('work insight parser checks passed')
