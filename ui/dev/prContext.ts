// Synthetic, fixed-time context packets for local preview and UI boundary tests.
// These are examples of the shared Rust contract, not live GitHub observations.
import type { PrContext } from '../src/prContext.ts'

const T = 1_793_387_700
const sha = (c: string) => c.repeat(40)
export const CONTEXT_PULLS = [
  { number: 42, title: 'Round invoice totals per currency', head: 'a', scenario: 'retained context' },
  { number: 43, title: 'Refresh retry handling', head: 'b', scenario: 'head changed' },
  { number: 44, title: 'Inspect restricted repository data', head: 'c', scenario: 'access unavailable' },
  { number: 45, title: 'Add bounded invoice retries', head: 'd', scenario: 'reported blocker' },
]
export function contextFixture(number: number): PrContext {
  const chosen = CONTEXT_PULLS.find((p) => p.number === number)
  if (!chosen) return { version: 1, availability: 'not_observed', coverage: { files_partial: true, work_complete: true, work_observed_at: T, pull_requests_complete: false, workflows_complete: false, last_sync_partial: null, retained_pr_scan_saturated: false } }
  const changed = number === 43
  const blocked = number === 45
  const head = sha(changed ? 'e' : chosen.head)
  return {
    version: 1, availability: 'available', repository: 'acme/widgets',
    pull_request: { number, title: chosen.title, author: 'sam', head_ref: `feature/context-${number}`, head_sha: head, base_ref: 'main', base_sha: null, draft: false, url: `https://example.invalid/acme/widgets/pull/${number}`, observed_at_ms: T * 1000, basis: 'provider_observed' },
    expected_head: sha(chosen.head), head_relation: changed ? 'changed' : 'same',
    pull_request_overlaps: number === 42 ? [{ number: 45, title: 'Add bounded invoice retries', author: 'sam', head_sha: sha('d'), draft: true, observed_at_ms: T * 1000, paths: ['src/billing.rs'], paths_omitted: 0, files_partial: false }] : [],
    pull_request_overlaps_omitted: 0,
    work_overlaps: number === 42 || blocked ? [{ work_id: `wrk_${'b'.repeat(32)}`, title: 'Fix invoice rounding', agent: 'codex', owner: 'dev', updated_at: T - 100, claim_status: 'active', blocker: null, next_action: 'Add a regression test for JPY totals', paths: ['src/billing.rs'], paths_partial: false }] : [],
    work_overlaps_omitted: 0,
    workflows: [{
      run_id: `context_${number}`, head_sha: sha(chosen.head), applicability: changed ? 'earlier_or_other_head' : 'matches_observed_head',
      current_gate: blocked ? 'review' : 'ci', next_action: blocked ? 'Independent review is still needed for this head.' : changed ? 'A test pass was reported for the previous head. Repeat the applicable checks.' : 'Inspect GitHub checks and review requests before deciding what to do next.',
      blocks_finish: blocked, updated_at: T - 120, basis: 'self_reported',
    }],
    workflows_omitted: 0,
    coverage: { files_partial: false, work_complete: true, work_observed_at: T, pull_requests_complete: false, workflows_complete: false, last_sync_partial: false, retained_pr_scan_saturated: false },
  }
}

// Wire-format example of provider evidence; it is decoded by the normal UI boundary.
export function expandedContextFixture(number: number) {
  const context = contextFixture(number)
  if (!context.pull_request) return context
  const head = context.pull_request.head_sha!
  const base = sha('f')
  const category = (items: unknown[], view: 'provider_list' | 'latest_by_name' | 'latest_observed' = 'provider_list') => ({ availability: 'available', view, complete: true, items, omitted: 0, malformed: 0, pages_read: 1, unread_pages: 0 })
  return { ...context, repository_id: 100,
    pull_request: { ...context.pull_request, base_sha: base },
    provider_evidence: { availability: 'observed', requester_access: 'hub_token_scope', live_requester_access: 'not_revalidated', snapshot: {
      version: 1, provider: 'github', repository_id: 100, number, head_sha: head, base_sha: base, observed_at: T * 1000,
      atomic_snapshot: false, attempt_history: false, retention_truncated: false,
      permissions: { availability: 'observed', pull_requests: 'available', checks: 'available', statuses: 'available', requester_access: 'not_asserted' },
      reviews: category([{ provider_id: 10, reviewer: 'alex', state: 'CHANGES_REQUESTED', commit_sha: head, head_relation: 'same', attempt: null, tested_merge_sha: null, provider_submitted_at: '2026-10-30T20:00:00Z', source_url: `https://example.invalid/acme/widgets/pull/${number}#pullrequestreview-10` }], 'latest_observed'),
      check_runs: category([{ provider_id: 11, name: 'Invoice regression tests', state: 'completed', conclusion: 'failure', commit_sha: head, head_relation: 'same', attempt: null, tested_merge_sha: null, provider_completed_at: '2026-10-30T20:01:00Z', source_url: 'https://example.invalid/acme/widgets/runs/11' }], 'latest_by_name'),
      commit_statuses: category([]),
    } },
  }
}
