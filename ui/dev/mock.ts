// Dev-only: answers status and workspace GET endpoints from synthetic fixtures.
// Workspace fixtures are visibly labelled; mutations return 501 and never execute.
// Used by `MOCK=1 npm run dev`; never imported by app code.
//   MOCK=1      acme/widgets with live agents, open PRs and retained workflows
//   MOCK=empty  a connected-nothing repository: no agents, events or PRs, no forge connection
//   MOCK=down   the hub is failing: health 503, work and activity 503
//   MOCK=demo   `agentscm demo` hub: health.demo true, simulated agents, no forge connection, no PRs
// While running, GET /__mock/down and /__mock/up flip the failure on and off; /__mock/reset un-revokes computers.
import type { Plugin } from 'vite'

const START = Date.now()
const ago = (s: number) => Math.floor((Date.now() - s * 1000) / 1000)

const A = {
  mariaClaude: { token: 'tok_synth_1', name: 'claude-code' },
  mariaCodex: { token: 'tok_synth_2', name: 'codex' },
  devCodex: { token: 'tok_synth_3', name: 'codex' },
  devCursor: { token: 'tok_synth_4', name: 'cursor' },
  samCopilot: { token: 'tok_synth_5', name: 'copilot' },
}
const maria = { id: 'own_1', provider: 'github', host: 'github.com', user_id: '1001', login: 'maria' }
const dev = { id: 'own_2', provider: 'github', host: 'github.com', user_id: '1002', login: 'dev' }
const REPO = 'acme/widgets'

function intents() {
  const base = { repository: REPO, base: 'main', expires_at: ago(-3600) }
  return [
    { ...base, id: 'int_dev_codex', task: 'Fix invoice rounding when a customer has multiple currencies', branch: 'dev/invoice-rounding', machine: 'synthetic-laptop-2', paths: ['src/billing.rs', 'tests/billing_test.rs'], via: ['hook:codex'], created_at: ago(1500), updated_at: ago(40), agent: A.devCodex, owner: dev },
    { ...base, id: 'int_maria_claude', task: 'Refactor the billing module to split tax calculation from invoice totals, keeping the public interface intact and adding focused unit tests for each rounding mode we support today', branch: 'feat/sync-retry-budget', machine: 'synthetic-laptop-1', paths: ['src/billing.rs', 'src/invoice/**', 'src/tax.rs', 'src/currency.rs', 'src/ledger.rs', 'src/receipts.rs'], via: ['hook:claude'], created_at: ago(900), updated_at: ago(12), agent: A.mariaClaude, owner: maria },
    { ...base, id: 'int_maria_codex', task: 'Write the webhook retry documentation', branch: 'maria/docs-webhooks', machine: 'synthetic-laptop-1', paths: ['docs/webhooks.md'], via: ['hook:codex'], created_at: ago(700), updated_at: ago(260), agent: A.mariaCodex, owner: maria },
    { ...base, id: 'int_dev_cursor', task: 'Tidy the settings page layout', branch: null, machine: null, paths: ['web/settings/**'], via: [], created_at: ago(400), updated_at: ago(380), agent: A.devCursor, owner: dev },
  ]
}

function work() {
  return {
    intents: intents(),
    intents_truncated: false,
    connected: true,
    branches: [],
    truncated: {},
    pull_requests: [
      { number: 42, title: 'Round invoice totals per currency', state: 'open', draft: false, head_ref: 'sam/round-per-currency', head_sha: 'a'.repeat(40), base_ref: 'main', author: { login: 'sam', id: 1003 }, html_url: 'https://example.invalid/acme/widgets/pull/42', files: ['src/billing.rs', 'src/invoice/totals.rs'], files_truncated: false },
      { number: 43, title: 'Document webhook signing', state: 'open', draft: true, head_ref: 'sam/webhook-docs', head_sha: 'b'.repeat(40), base_ref: 'main', author: { login: 'sam', id: 1003 }, html_url: 'https://example.invalid/acme/widgets/pull/43', files: ['docs/signing.md'], files_truncated: false },
      { number: 40, title: 'Bump synthetic dependency', state: 'closed', draft: false, head_ref: 'bot/bump', head_sha: 'c'.repeat(40), base_ref: 'main', author: { login: 'sam', id: 1003 }, html_url: 'https://example.invalid/acme/widgets/pull/40', files: ['Cargo.toml'], files_truncated: false },
    ],
  }
}

/** Synthetic work overview: one item per progress lane, two recent outcomes and one declared-scope overlap. */
let catchupAcked = false
// Computer sign-in fixtures. Revoke only changes this dev server's memory.
const revokedDevices = new Map<string, number>()
// The synthetic browser session: GET /v1/hub/login/github signs in, DELETE /v1/hub/session signs out.
let browserSignedIn = true
// Read tokens the Hub handed the web pages (POST /v1/hub/session/read-token), counted so tests can see the page didn't over-ask.
let readTokensMinted = 0
let rejectNextWebRead = false
let rejectNextHubSignOut = false
let rejectNextAgentRead = false
// A repository the signed-in person can list but whose role is too low: the mint refuses it.
const REFUSED_REPO = 'acme/secret-plans'
const webToken = (repo: string, serial: number) => `webread:${repo.replace('/', ':')}:${serial}`
function devices() {
  const mk = (n: string, name: string, owner: { login: string }, created: number, used: number | null, repositories: string[]) => ({ id: `dev_${n.repeat(32).slice(0, 32)}`, name, created_at: ago(created), last_used_at: used === null ? null : ago(used), revoked_at: revokedDevices.get(`dev_${n.repeat(32).slice(0, 32)}`) ?? null, current: false, repositories, owner: { id: `usr_${owner.login}`, login: owner.login, provider: 'github' } })
  return [
    mk('a', 'maria-mbp', maria, 86400 * 12, 40, [REPO, 'acme/billing-docs', REFUSED_REPO]),
    mk('b', 'maria-desktop', maria, 86400 * 40, 86400 * 9, [REPO]),
    mk('c', 'ci-runner-old', maria, 86400 * 80, null, []),
    mk('d', 'dev-thinkpad', dev, 86400 * 3, 300, [REPO]),
  ]
}

function overview() {
  const wid = (c: string) => `wrk_${c.repeat(32)}`
  const cp = (kind: string, summary: string, next: string | null, secs: number, agent: string) => ({ kind, summary, next_action: next, source_refs: [], at: ago(secs), token: `tok_${agent}`, agent, session: `${agent}-session`, generation: 1 })
  const item = (c: string, title: string, state: string, login: string, agent: string, scope: string[], secs: number, extra: Record<string, unknown> = {}) => ({
    id: wid(c), repository: REPO, owner: `github:github.com:${login}`, owner_login: login, source: { kind: 'native', request_key: `req-${c}` }, title, state, claim_status: state === 'finished' ? 'unknown' : secs > 3600 ? 'stale' : 'active',
    last_checkin_at: ago(secs), generation: 1, lead_token: `tok_${agent}`, lead_agent: agent, lead_session: `${agent}-session`,
    participants: [{ owner: `github:github.com:${login}`, login, token: `tok_${agent}`, agent, session: `${agent}-session`, role: 'lead' }],
    scope, branch: null, worktree: null, next_action: null, current: null, blocker: null, depends_on: [], related_work: [],
    created_at: ago(secs + 3600), updated_at: ago(secs), created_seq: 1, updated_seq: 2, ...extra,
  })
  const billing = item('a', 'Refactor the billing module', 'active', 'maria', 'claude-code', ['src/billing.rs', 'src/tax.rs'], 60, { branch: 'feat/sync-retry-budget', next_action: 'Split tax calculation out of invoice totals', current: cp('progress', 'Tax module extracted; tests green', 'Split tax calculation out of invoice totals', 60, 'claude-code') })
  const rounding = item('b', 'Fix invoice rounding', 'active', 'dev', 'codex', ['src/billing.rs'], 300, { branch: 'dev/invoice-rounding', next_action: 'Add a regression test for JPY totals' })
  const docs = item('c', 'Write the webhook retry documentation', 'blocked', 'maria', 'codex', ['docs/webhooks.md'], 900, { blocker: cp('blocker', 'Waiting on the retry policy decision from the API team', null, 900, 'codex') })
  const settings = item('d', 'Tidy the settings page layout', 'handover', 'dev', 'cursor', ['web/settings/**'], 1800, { next_action: 'Pick up the responsive pass from the handover note' })
  const ledger = item('e', 'Reconcile ledger exports', 'paused', 'sam', 'copilot', ['src/ledger.rs'], 7200, { next_action: 'Resume after the quarter-end freeze' })
  // Branches match skill run run_41 and PUBLICATIONS[0] (merged, PR #220), so the task page's lifecycle strip has evidence.
  const shipped = item('f', 'Round invoice totals per currency', 'finished', 'sam', 'copilot', ['src/invoice/totals.rs'], 1200, { branch: 'agent/claude-code/invoice-rounding-120' })
  return {
    recent_since: ago(86400), until: ago(0),
    current: [billing, rounding, docs, settings, ledger],
    recent: [{ seq: 9, at: ago(1200), kind: 'completion', item: shipped }, { seq: 8, at: ago(2400), kind: 'decision', item: billing }],
    overlaps: [{ left: billing, right: rounding, reasons: [{ left_path: 'src/billing.rs', right_path: 'src/billing.rs' }], reasons_complete: true }],
    current_complete: true, recent_complete: true, overlaps_complete: true, history_retention_gap: false,
  }
}

function events() {
  const e = (kind: string, secs: number, agent: { token: string; name: string }, login: string, data: Record<string, unknown>) => ({ id: `evt_${kind}_${secs}_${agent.token}`, kind, at: Math.floor(START / 1000) - secs, agent, owner: { login }, data })
  const list = [
    e('session_started', 1700, A.devCodex, 'dev', { via: 'hook:codex' }),
    e('intent_announced', 1500, A.devCodex, 'dev', { task: 'Fix invoice rounding', paths: ['src/billing.rs', 'tests/billing_test.rs'] }),
    e('session_started', 1000, A.mariaClaude, 'maria', { via: 'hook:claude' }),
    e('intent_announced', 900, A.mariaClaude, 'maria', { task: 'Refactor the billing module', paths: ['src/billing.rs', 'src/invoice/**'] }),
    e('intent_notice', 898, A.mariaClaude, 'maria', { agent: 'codex', owner: { login: 'dev' }, task: 'Fix invoice rounding', their_paths: ['src/billing.rs'], your_paths: ['src/billing.rs'] }),
    e('session_connected', 880, A.mariaClaude, 'maria', { via: 'mcp' }),
    e('session_started', 700, A.mariaCodex, 'maria', { via: 'hook:codex' }),
    e('intent_announced', 690, A.mariaCodex, 'maria', { task: 'Write the webhook retry documentation', paths: ['docs/webhooks.md'] }),
    e('intent_announced', 400, A.devCursor, 'dev', { task: 'Tidy the settings page layout', paths: ['web/settings/**'] }),
    e('pr_notice', 330, A.mariaClaude, 'maria', { number: 42, title: 'Round invoice totals per currency', author: 'sam', html_url: 'https://example.invalid/acme/widgets/pull/42' }),
    e('edit_denied', 360, A.mariaClaude, 'maria', { paths: ['src/billing.rs'], holders: ['int_dev_codex'] }),
    e('edit_repeated', 300, A.mariaClaude, 'maria', { paths: ['src/billing.rs'] }),
    e('pr_notice', 240, A.mariaClaude, 'maria', { number: 41, title: 'Split tax rounding', author: { login: 'sam' }, merged: true, base_ref: 'main', their_paths: ['src/tax.rs'] }),
    e('task_overlap', 200, A.devCursor, 'dev', { task: 'Round invoice totals per currency', work: ['wrk_synth_1'], pulls: [42] }),
    // settled: dev's codex asked maria's claude-code for src/tax.rs, which was handed over
    e('handoff_request', 140, A.devCodex, 'dev', { request: 'hof_synth_1', paths: ['src/tax.rs'], note: 'Rounding fix needs a one-line change here <b>please</b>', to: { agent: 'claude-code', login: 'maria' } }),
    e('handoff_grant', 110, A.mariaClaude, 'maria', { request: 'hof_synth_1', paths: ['src/tax.rs'], from: { agent: 'codex', login: 'dev' } }),
    // pending: dev's cursor asked maria's claude-code for src/currency.rs
    e('handoff_request', 60, A.devCursor, 'dev', { request: 'hof_synth_2', paths: ['src/currency.rs'], note: null, to: { agent: 'claude-code', login: 'maria' } }),
    // kept: dev's codex asked for src/ledger.rs and maria's claude-code said no
    e('handoff_request', 520, A.devCodex, 'dev', { request: 'hof_synth_3', paths: ['src/ledger.rs'], note: 'Need to post the rounding adjustment', to: { agent: 'claude-code', login: 'maria' } }),
    e('handoff_decline', 470, A.mariaClaude, 'maria', { request: 'hof_synth_3', paths: ['src/ledger.rs'], from: { agent: 'codex', login: 'dev' }, reason: 'Still mid-refactor, ask again after the tax split lands' }),
    // lapsed: nobody answered within 30 minutes
    e('handoff_request', 2400, A.devCursor, 'dev', { request: 'hof_synth_4', paths: ['src/receipts.rs'], note: 'Receipts footer typo', to: { agent: 'claude-code', login: 'maria' } }),
    e('session_started', 240, A.samCopilot, 'sam', { via: 'hook:copilot' }),
    e('session_ended', 200, A.samCopilot, 'sam', {}),
    e('session_started', 150, A.samCopilot, 'sam', { via: 'hook:copilot' }),
    e('hold_expired', 120, A.devCursor, 'dev', { cause: 'idle' }),
    e('intent_released', 90, A.mariaCodex, 'maria', {}),
  ]
  // One extra synthetic event every 30 s (at most 6), rotating kinds, so the live-update animation is visible.
  const extra = Math.min(Math.floor((Date.now() - START) / 30_000), 6)
  const gen: [string, { token: string; name: string }, string, Record<string, unknown>][] = [
    ['session_started', A.samCopilot, 'sam', { via: ['hook:copilot'] }],
    ['edit_denied', A.devCursor, 'dev', { paths: ['src/tax.rs'], holders: ['int_maria_claude'] }],
    ['edit_repeated', A.devCursor, 'dev', { paths: ['src/tax.rs'] }],
    ['intent_notice', A.mariaCodex, 'maria', { agent: 'cursor', owner: { login: 'dev' }, task: 'Tidy the settings page layout', their_paths: ['web/settings/**'], your_paths: ['web/settings/theme.ts'] }],
  ]
  for (let n = 1; n <= extra; n++) {
    const [kind, agent, login, data] = gen[(n - 1) % gen.length]!
    list.push({ id: `evt_extra_${n}`, kind, at: Math.floor(START / 1000) + n * 30, agent, owner: { login }, data })
  }
  return [...list, ...OLD_EVENTS]
    .sort((a, b) => b.at - a.at)
    .map((ev, i, all) => ({ seq: all.length - i, ...ev }))
}

function activity() {
  const s = (key: string, agent: { token: string; name: string }, login: string, intent: string | null, task: string | null, via: string[], upd: number) => ({ key, agent, owner: { login }, intent, task, via, created_at: ago(upd + 600), updated_at: ago(upd) })
  return {
    sessions: [
      s('ses_1', A.mariaClaude, 'maria', 'int_maria_claude', 'Refactor the billing module', ['hook:claude'], 12),
      s('ses_2', A.mariaCodex, 'maria', 'int_maria_codex', null, ['hook:codex'], 260),
      s('ses_3', A.devCodex, 'dev', 'int_dev_codex', 'Fix invoice rounding', ['hook:codex'], 40),
      s('ses_4', A.samCopilot, 'sam', null, null, ['hook:copilot'], 150),
    ],
    events: events(),
  }
}

const WORKFLOW_NAMES = [
  'Claude code quality gate',
  'Billing change verification',
  'Release readiness',
  'Dependency review',
  'Documentation checks',
]
const WORKFLOW_STATES = ['succeeded', 'failed', 'running', 'blocked', 'completed_with_omissions', 'cancelled', 'pending'] as const
const WORKFLOW_RUNS = Array.from({ length: 60 }, (_, index) => {
  const special = [
    { run_id: 'claude-review-failed', state: 'failed', name: 'Claude code quality gate' },
    { run_id: 'claude-review-stale', state: 'succeeded', name: 'Billing change verification' },
    { run_id: 'claude-review-passed', state: 'succeeded', name: 'Claude code quality gate' },
  ][index]
  return {
    run_id: special?.run_id ?? `retained-run-${String(60 - index).padStart(3, '0')}`,
    revision: (index % 16).toString(16).repeat(64),
    state: special?.state ?? WORKFLOW_STATES[index % WORKFLOW_STATES.length],
    name: special?.name ?? WORKFLOW_NAMES[index % WORKFLOW_NAMES.length],
    sequence: String(5000 - index),
  }
})

function workflow(runId: string): Record<string, unknown> | null {
  if (runId === 'productive-artifacts') {
    const revision = 'a'.repeat(64)
    return {
      version: 1, availability: 'available',
      projection: {
        version: 1,
        run: {
          version: 1, run_id: runId, plan_digest: `sha256:${'b'.repeat(64)}`, skill_digest: `sha256:${'c'.repeat(64)}`,
          source: { repository: 'd'.repeat(32), revision, policy_epoch: 1 }, state: 'succeeded', admission: 'not_assessed',
          steps: ['explain', 'draft', 'package'].map((id, index, ids) => ({
            id, needs: index ? [ids[index - 1]] : [], required: true, state: 'succeeded', observations: [],
            detail: 'Synthetic artifact metadata for interface development; no agent was executed.',
            action: {
              profile_digest: 'e'.repeat(64), outcome: 'succeeded', verification: 'not_independently_verified',
              observed_model: index < 2 ? 'synthetic-model' : null,
              artifact: { name: id, media_type: index < 2 ? 'text/markdown' : 'application/json', sha256: 'f'.repeat(64), bytes: 1240, run_id: runId, step_id: id, revision },
            },
          })),
        },
        present_applicability: { basis: 'current_authority_repository_policy_epoch_and_immutable_revision', state: 'current', reason: null },
        present_landing_binding: null,
      },
    }
  }
  const summary = WORKFLOW_RUNS.find((run) => run.run_id === runId)
  if (!summary) return null
  const failedReview = runId === 'claude-review-failed'
  const stale = runId === 'claude-review-stale'
  const passed = summary.state === 'succeeded' || summary.state === 'completed_with_omissions'
  const failed = summary.state === 'failed'
  const blocked = summary.state === 'blocked'
  const running = summary.state === 'running'
  const pending = summary.state === 'pending'
  const cancelled = summary.state === 'cancelled'
  const stateFor = (id: string) => {
    if (passed) return id === 'optional-docs' && summary.state === 'completed_with_omissions' ? 'cancelled' : 'succeeded'
    if (failedReview) return id === 'claude-review' ? 'failed' : ['evidence-bundle', 'landing-readiness'].includes(id) ? 'blocked' : 'succeeded'
    if (failed) return id === 'unit-tests' ? 'failed' : ['integration-tests', 'package-check', 'evidence-bundle', 'landing-readiness'].includes(id) ? 'blocked' : 'succeeded'
    if (blocked) return ['policy-check', 'evidence-bundle', 'landing-readiness'].includes(id) ? 'blocked' : 'succeeded'
    if (running) return ['source-snapshot', 'lint'].includes(id) ? 'succeeded' : id === 'unit-tests' ? 'running' : 'pending'
    if (cancelled) return id === 'source-snapshot' ? 'succeeded' : 'cancelled'
    return pending ? 'pending' : summary.state
  }
  const execution = (id: string) => {
    const state = stateFor(id)
    if (state === 'blocked' || state === 'pending' || state === 'cancelled') return null
    return {
      attempt_id: `attempt-${summary.sequence}-${id}`,
      generation: 1,
      state: state === 'running' ? 'running' : 'finished',
      outcome: state === 'succeeded' ? 'success' : state === 'failed' ? 'failure' : null,
      record_digest: state === 'running' ? null : (Number(summary.sequence) % 16).toString(16).repeat(64),
    }
  }
  const reviewState = stateFor('claude-review')
  const review = reviewState === 'succeeded' || reviewState === 'failed' ? {
    model_requested: 'sonnet',
    model_observed: 'claude-sonnet-synthetic',
    status: reviewState,
    stop: 'finish',
    findings: failedReview ? [{
      path: 'src/billing.rs', start_line: 118, end_line: 121, severity: 'error',
      title: 'Preserve the currency-specific rounding mode',
      detail: 'This fallback applies the default rounding mode after the currency lookup fails, which can change an existing invoice total. Return the lookup error or retain the invoice currency before computing the total.',
    }, {
      path: 'tests/billing_test.rs', start_line: 44, end_line: 44, severity: 'note',
      title: 'Add a regression example',
      detail: 'A focused case for an unsupported currency would make the intended failure behavior explicit.',
    }] : [],
    unresolved: failedReview ? ['The unsupported-currency behavior is not covered by the current test result.'] : [],
    covered_paths: ['src/billing.rs', 'src/invoice/totals.rs', 'tests/billing_test.rs', 'tests/integration/invoice.rs'],
  } : null
  const step = (id: string, needs: string[], required = true, extra: Record<string, unknown> = {}) => ({
    id, needs, required, state: stateFor(id), observations: [], detail: null, execution: null, review: null, ...extra,
  })
  const source = { repository: '0123456789abcdef0123456789abcdef', revision: summary.revision, policy_epoch: 4 }
  const binding = {
    basis: 'current_change_version_review_context_target_and_policy_epoch',
    state: 'current', reason: null, change_id: 'change-invoice-rounding', change_version: 3,
    context_digest: 'c'.repeat(64), target: 'main',
  }
  const applicability = {
    basis: 'current_authority_repository_policy_epoch_and_immutable_revision', state: 'current', reason: null,
  }
  return {
    version: 1,
    availability: 'available',
    projection: {
      version: 1,
      run: {
        version: 1,
        run_id: runId,
        plan_digest: `sha256:${'d'.repeat(64)}`,
        skill_digest: `sha256:${'e'.repeat(64)}`,
        source,
        state: summary.state,
        steps: [
          step('source-snapshot', [], true, {
            observations: [
              { path: 'src/billing.rs', sha256: 'f'.repeat(64), bytes: 1842 },
              { path: 'tests/billing_test.rs', sha256: 'a'.repeat(64), bytes: 3920 },
            ],
            detail: 'Required source paths matched the retained revision.',
          }),
          step('lint', ['source-snapshot'], true, { execution: execution('lint'), detail: 'The packaged lint command completed.' }),
          step('unit-tests', ['source-snapshot'], true, {
            execution: execution('unit-tests'),
            detail: failed && !failedReview ? 'The packaged unit test command reported a failure.' : 'The packaged unit test command completed.',
          }),
          step('integration-tests', ['unit-tests'], true, { execution: execution('integration-tests'), detail: stateFor('integration-tests') === 'blocked' ? 'Did not run because unit-tests did not succeed.' : 'Integration checks use the retained unit-test result.' }),
          step('package-check', ['lint', 'unit-tests'], true, { execution: execution('package-check'), detail: stateFor('package-check') === 'blocked' ? 'Did not run because a required test step did not succeed.' : 'Package checks completed against the retained revision.' }),
          step('claude-review', ['source-snapshot'], true, { review, detail: review === null ? 'No structured review result is retained yet.' : failedReview ? 'The structured review returned a blocking finding.' : 'The structured review completed.' }),
          step('policy-check', ['source-snapshot'], true, { detail: blocked ? 'A required native capability is unavailable.' : 'Required native capabilities were present.' }),
          step('optional-docs', ['source-snapshot'], false, { execution: execution('optional-docs'), detail: 'Optional documentation checks are retained separately.' }),
          step('evidence-bundle', ['integration-tests', 'package-check', 'claude-review', 'policy-check'], true, { detail: stateFor('evidence-bundle') === 'blocked' ? 'Did not complete because a required predecessor did not succeed.' : 'Required predecessor results are bound into the retained run.' }),
          step('landing-readiness', ['evidence-bundle'], true, { detail: stateFor('landing-readiness') === 'blocked' ? 'Did not complete because the evidence bundle is blocked.' : 'This step records workflow readiness; native landing revalidates independently.' }),
        ],
        admission: 'not_assessed',
      },
      present_applicability: stale ? { ...applicability, state: 'stale', reason: 'the current policy epoch no longer matches this run' } : applicability,
      present_landing_binding: stale ? { ...binding, state: 'stale', reason: 'the Change or its review context changed after this run' } : binding,
    },
  }
}

const AGENT_NAMES = ['claude-code', 'codex', 'cursor', 'copilot']
const TOPICS = ['invoice rounding', 'webhook retries', 'settings layout', 'tax split', 'ledger export', 'receipt footer', 'currency lookup', 'audit log', 'cache warmup', 'retry backoff']
const hex32 = (n: number) => n.toString(16).padStart(32, '0')

/** ~120 synthetic published changes, newest first. */
const PUBLICATIONS = Array.from({ length: 120 }, (_, i) => {
  const topic = TOPICS[i % TOPICS.length]!
  const agent = AGENT_NAMES[i % AGENT_NAMES.length]!
  const merged = i % 3 === 0
  const prNumber = i % 4 === 3 ? null : 100 + (120 - i)
  return {
    id: `pub_${hex32(0xabc000 + 120 - i)}`,
    repository: REPO,
    revision: (i % 16).toString(16).repeat(64),
    commit: (i % 16).toString(16).repeat(40),
    branch: `agent/${agent}/${topic.replace(/ /g, '-')}-${120 - i}`,
    title: `Synthetic ${topic} change ${120 - i}`,
    base: 'main',
    event_seq: 1000 - i,
    created_at: ago(3600 + i * 3600),
    updated_at: ago(1800 + i * 3600),
    states: {
      branch: { state: 'created', message: null },
      native_landed: true,
      pull_request: prNumber === null ? { state: 'pending' } : { state: merged ? 'merged' : 'opened', number: prNumber, html_url: `https://example.invalid/acme/widgets/pull/${prNumber}`, ...(merged ? { merge_commit: 'f'.repeat(40) } : {}) },
    },
  }
})

/** ~150 synthetic work items, most recently updated first. */
const WORK_ITEMS = Array.from({ length: 150 }, (_, i) => {
  const topic = TOPICS[i % TOPICS.length]!
  return {
    id: `wrk_${hex32(0xdef000 + 150 - i)}`,
    title: `Synthetic ${topic} task ${150 - i}`,
    state: (['active', 'blocked', 'handover', 'paused', 'finished'] as const)[i % 5],
    lead_agent: AGENT_NAMES[i % AGENT_NAMES.length]!,
    owner: i % 7 === 6 ? null : (['maria', 'dev', 'sam'] as const)[i % 3],
    branch: i % 4 === 0 ? null : `agent/${topic.replace(/ /g, '-')}-${150 - i}`,
    source_issue: i % 6 === 0 ? String(200 + i) : null,
    updated_at: ago(600 + i * 1800),
  }
})

/** ~200 older activity events (seq 1..200 are older than the live ones, which are numbered above them). */
const OLD_EVENTS = Array.from({ length: 200 }, (_, i) => {
  const agent = [A.mariaClaude, A.devCodex, A.mariaCodex, A.devCursor][i % 4]!
  const login = ['maria', 'dev', 'maria', 'dev'][i % 4]!
  const kind = ['session_started', 'intent_announced', 'edit_repeated', 'intent_released'][i % 4]!
  const data = kind === 'session_started' ? { via: 'hook:claude' } : kind === 'intent_announced' ? { task: `Synthetic ${TOPICS[i % TOPICS.length]}`, paths: ['src/billing.rs'] } : kind === 'edit_repeated' ? { paths: ['src/tax.rs'] } : {}
  return { id: `evt_old_${i}`, kind, at: Math.floor(START / 1000) - 7200 - (200 - i) * 120, agent, owner: { login }, data }
})

const clampLimit = (raw: string | null, dflt: number, max: number) => Math.min(max, Math.max(1, Number(raw ?? dflt) || dflt))

// ---------- supported skills (synthetic)
const SHA_PR_GATE = '3f9a1c0de5b7a2486f0c9d1e3b5a7c9e1f3a5b7c9d1e3f5a7b9c1d3e5f7a9b1c'
const SHA_POST_MERGE = 'a71c4e2b6d8f0a1c3e5b7d9f1a3c5e7b9d1f3a5c7e9b1d3f5a7c9e1b3d5f7a90'
const PR_GATES = [
  { id: 'branch', by: 'author', enforce: ['advise'] }, { id: 'tests', by: 'author', enforce: ['stop'] }, { id: 'review', by: 'reviewer', enforce: ['stop'] }, { id: 'fix', by: 'author', enforce: ['stop'] },
  { id: 'ci', by: 'author', enforce: ['forge_status'] }, { id: 'approval', by: 'maintainer', enforce: ['forge_status'] }, { id: 'merge', by: 'maintainer', enforce: ['advise'] }, { id: 'post_merge', by: 'author', enforce: ['signal'] },
]
// Written by hand from the manifest walkthrough rules; the real lines come from the hub.
const SELF = 'self-reported or stronger'
const STOP = "Blocks if the opt-in Stop hook is installed: the author's session cannot finish until this is recorded."
const FORGE = 'Warns only for now: a merge check (forge status) is planned but not built.'
const walk = (merge: string): string[] => [
  `1. Branch: The author works on a branch made for this task, never on the default branch. Done by the author agent. Needs: the task branch (${SELF}). Warns only.`,
  `2. Tests: The author runs the tests on the exact head commit. Done by the author agent. Needs: test results for the current head (${SELF}). ${STOP}`,
  `3. Review: A different session reviews that exact commit and reports what it finds. Done by a different session (not the author). Needs: an independent review of the current head (${SELF}). ${STOP}`,
  `4. Fix: The author fixes every blocking finding, or dismisses it with a written reason. Done by the author agent. Needs: every blocking finding fixed or dismissed with a reason (${SELF}). ${STOP}`,
  `5. CI: The hosted checks run on the current commit and must pass. Done by the author agent. Needs: green CI on the current head (${SELF}). ${FORGE}`,
  `6. Approval: A maintainer looks at the current commit and approves it. Done by a maintainer (a person). Needs: maintainer approval of the current head (${SELF}). ${FORGE}`,
  `7. Merge: A maintainer merges the pull request. Done by a maintainer (a person). Needs: the merge${merge} (${SELF}). Warns only.`,
  `8. Post merge: After the merge, the author checks the default branch still passes. Done by the author agent. Needs: a regression pass on the default branch after the merge (${SELF}). Recorded only; the planned alert for a red default branch is not built yet.`,
]
const SKILL_CATALOG = [
  {
    name: 'pr-gate', workflow_version: 1, sha256: SHA_PR_GATE, first_party: true, summary: 'Every pull request passes tests, an independent review, CI and approval before it merges.',
    gates: PR_GATES.map(({ id, enforce }) => ({ id, enforce })),
    walkthrough: walk(''),
    procedure: '---\nname: pr-gate\n---\n\n# pr-gate v1\n\n3. **review**: the hub refuses a review recorded with the same agent token and session as an author.',
    applicability: { triggers: ['opening a pull request', 'before merging a pull request'], prerequisites: ['a GitHub or GitLab repository', 'a test command agents can run'], not_for: ['direct pushes to the default branch by a person'] },
    parameters: [
      { name: 'ci_label', description: 'Label that requests the extended CI run.', type: 'string', default: null },
      { name: 'merge_method', description: 'Required merge method.', type: 'string', default: 'any', enum: ['merge', 'squash', 'rebase', 'any'] },
    ],
  },
  {
    name: 'post-merge-check', workflow_version: 1, sha256: SHA_POST_MERGE, first_party: true, summary: 'Run a regression pass on the target branch after each merge.',
    gates: [{ id: 'merge', enforce: ['advise'] }, { id: 'post_merge', enforce: ['signal'] }],
    walkthrough: walk('').slice(6).map((l, i) => l.replace(/^\d+/, String(i + 1))),
    procedure: '# post-merge-check v1\n\nRun the regression pass on the default branch.',
    applicability: { triggers: ['after a pull request merges'], prerequisites: ['a command that checks the target branch'], not_for: ['repositories without a stable default branch'] },
    parameters: [],
  },
]
const SKILL_RUNS = (() => {
  const skill = { name: 'pr-gate', workflow_version: 1, sha256: SHA_PR_GATE }
  const ev = (id: string, kind: string, head: string, summary: string, mins: number, assurance = 'self_reported') => ({ id, kind, head_sha: head, assurance, summary, url: null, session: 'ses_75a86944', agent: 'claude', recorded_at: ago(mins * 60), detail: null })
  const gates = (states: string[], evidence: Record<string, ReturnType<typeof ev>[]>) =>
    PR_GATES.map((g, i) => {
      const e = evidence[g.id] ?? []
      return { id: g.id, state: states[i], by: g.by, enforce: g.enforce, min_assurance: 'self_reported', assurance: states[i] === 'done' ? (e.some((x) => x.assurance === 'self_reported') ? 'self_reported' : (e[0]?.assurance ?? 'self_reported')) : null, evidence: e }
    })
  const H1 = 'a1b2c3d4e5f60718293a4b5c6d7e8f9012345678', H2 = '9e8d7f6a5b4c3d2e1f0a9b8c7d6e5f4a3b2c1d0e', H3 = '55af0e1b2c3d4e5f60718293a4b5c6d7e8f90123'
  const D = 'done', C = 'current', N = 'not_reached'
  return [
    { run_id: 'run_41', repo: REPO, branch: 'feat/sync-retry-budget', pull_request: 41, head_sha: H1, state: 'open', skill, params: { ci_label: null, merge_method: 'squash' }, current_gate: 'review', next_action: 'Needs an independent review of a1b2c3d by another session.', blocks_finish: true,
      gates: gates([D, D, C, N, N, N, N, N], { branch: [ev('e1', 'task_branch', H1, 'feat/sync-retry-budget from main', 40, 'observed')], tests: [ev('e2', 'revision_tests', H1, 'cargo nextest run exit 0 on a1b2c3d', 12)] }),
      stale_evidence: [ev('e0', 'revision_tests', 'f00dfeed00000000000000000000000000000000', 'cargo nextest run exit 0 on f00dfee', 55)], updated_at: ago(12 * 60) },
    { run_id: 'run_40', repo: REPO, branch: 'fix/cache-parsed-manifests', pull_request: 40, head_sha: H2, state: 'open', skill, params: { ci_label: null, merge_method: 'squash' }, current_gate: 'ci', next_action: 'Waiting for CI on 9e8d7f6: 4 of 6 checks are green.', blocks_finish: false,
      gates: gates([D, D, D, D, C, N, N, N], { branch: [ev('f1', 'task_branch', H2, 'fix/cache-parsed-manifests from main', 180)], tests: [ev('f2', 'revision_tests', H2, 'npm test passed on 9e8d7f6', 150)], review: [ev('f3', 'independent_review', H2, 'Reviewed by another session, no blocking findings', 120)], fix: [ev('f4', 'findings_resolved', H2, 'No blocking findings to resolve', 118)] }),
      stale_evidence: [], updated_at: ago(30 * 60) },
    { run_id: 'run_38', repo: REPO, branch: 'chore/tighten-upload-limits', pull_request: 38, head_sha: H3, state: 'completed', skill, params: { ci_label: null, merge_method: 'squash' }, current_gate: null, next_action: 'Complete. Nothing to do.', blocks_finish: false,
      gates: gates([D, D, D, D, D, D, D, D], Object.fromEntries(PR_GATES.map((g, i) => [g.id, [ev(`g${i}`, g.id === 'branch' ? 'task_branch' : 'workflow_receipt', H3, `${g.id} recorded on 55af0e1`, 3000 - i * 100, g.id === 'review' ? 'receipt' : g.id === 'branch' ? 'observed' : 'self_reported')]]))),
      stale_evidence: [], updated_at: ago(2 * 3600) },
  ]
})()
const LOCK_PIN = (over: Record<string, unknown> = {}) => ({ name: 'pr-gate', workflow_version: 1, path: null, sha256: SHA_PR_GATE, status: 'adopted', params: { merge_method: 'squash' }, walkthrough: walk(' (squash)'), source: { kind: 'first_party' }, ...over })

export type Scenario = 'full' | 'empty' | 'down' | 'demo'

export function mockHub(scenario: Scenario = 'full'): Plugin {
  let down = scenario === 'down'
  return {
    name: 'agentscm-mock-hub',
    apply: 'serve',
    configureServer(server) {
      server.middlewares.use('/__mock', (req, res) => {
        if (req.url?.startsWith('/reject-next-agent-read')) {
          rejectNextAgentRead = true
          return res.end('next pasted-token read rejected\n')
        }
        if (req.url?.startsWith('/reject-next-signout')) {
          rejectNextHubSignOut = true
          return res.end('next Hub sign-out rejected\n')
        }
        if (req.url?.startsWith('/reject-next-read')) {
          rejectNextWebRead = true
          return res.end('next web read rejected\n')
        }
        if (req.url?.startsWith('/signout')) {
          browserSignedIn = false
          return res.end('browser signed out\n')
        }
        if (req.url?.startsWith('/reset')) {
          revokedDevices.clear()
          browserSignedIn = true
          readTokensMinted = 0
          rejectNextWebRead = false
          rejectNextHubSignOut = false
          rejectNextAgentRead = false
          return res.end('computers reset\n')
        }
        down = req.url?.startsWith('/down') ?? false
        res.end(down ? 'hub is now failing\n' : 'hub is now healthy\n')
      })
      server.middlewares.use('/v1/hub', async (req, res) => {
        // MOCK_SLOW=1500 holds each sign-in step that long, so the opening screen can be seen and screenshotted.
        const slow = Number(process.env['MOCK_SLOW'] ?? 0)
        if (slow > 0 && /^\/(session|tokens|session\/read-token)$/.test(new URL(req.url ?? '/', 'http://localhost').pathname)) {
          await new Promise((r) => setTimeout(r, slow))
          // The page may have cancelled meanwhile (React runs effects twice); reading its body now would crash the server.
          if (req.destroyed) return
        }
        const url = new URL(req.url ?? '/', 'http://localhost')
        const send = (status: number, body: unknown) => {
          res.statusCode = status
          res.setHeader('content-type', 'application/json')
          res.setHeader('cache-control', 'no-store')
          res.end(JSON.stringify(body))
        }
        if (url.pathname === '/health') {
          if (down) return send(503, { status: 'unavailable' })
          if (scenario === 'empty') return send(200, { status: 'ok', storage: 'sqlite', coordination: { denied: 0, repeated: 0, rate_limited: 0, fail_open: 0, hook_p95_ms: 0 } })
          if (scenario === 'demo') return send(200, { status: 'ok', storage: 'sqlite', demo: true, coordination: { denied: 2, repeated: 0, rate_limited: 0, fail_open: 0, hook_p95_ms: 4 } })
          return send(200, { status: 'ok', storage: 'sqlite', coordination: { denied: 14, repeated: 9, rate_limited: 1, fail_open: 2, hook_p95_ms: 38 + Math.round(Math.random() * 30) } })
        }
        // Synthetic workspace viewing only. Production uses forge sessions and CSRF.
        if (url.pathname.startsWith('/workspace')) {
          if (down) return send(503, { error: 'unavailable' })
          if (req.method !== 'GET') return send(501, { error: 'synthetic_fixture_does_not_execute_mutations' })
          const settings = { id: 'synthetic-workspace', name: 'Synthetic workspace', generation: 1, max_concurrent_runs: 1 }
          const identity = { provider: 'github', host: 'github.com', user_id: 1001 }
          const subject = { change_id: 'change-invoice-rounding', change_version: 3, context_digest: 'c'.repeat(64), revision: '2'.repeat(64), requirement_digest: 'd'.repeat(64) }
          const run = { run_id: 'synthetic-workspace-run', repository_id: REPO, profile_id: 'quality-gate', actor_id: 'synthetic-member', generation: 3, state: 'succeeded', subject, native_run_id: 'claude-review-passed', receipt: 'synthetic-receipt', detail: 'Synthetic retained run for interface development; no execution occurred.' }
          if (url.pathname === '/workspace/login') return send(200, { providers: [] })
          if (url.pathname === '/workspace/session') return send(200, { workspace: settings, actor: { user: { id: 'synthetic-member', identity, login: 'synthetic-maintainer' }, roles: ['reader', 'contributor', 'administrator'], repository_ids: ['*'], generation: 1 }, capabilities: { manage_workspace: true, read_repositories: true, execute_workflows: true }, csrf_token: 'synthetic-csrf-no-authority', login_providers: [], synthetic_fixture: true })
          if (url.pathname === '/workspace/promises') return send(200, { promises: [], next: null })
          if (url.pathname === '/workspace/settings') return send(200, settings)
          if (url.pathname === '/workspace/members') return send(200, { members: [{ identity, roles: ['reader', 'contributor', 'administrator'], repository_ids: ['*'], enabled: true, generation: 1 }, { identity: { ...identity, user_id: 1002 }, roles: ['reader', 'reviewer'], repository_ids: [REPO], enabled: true, generation: 1 }], generation: 1, next_cursor: null })
          if (url.pathname === '/workspace/repositories') return send(200, { repositories: scenario === 'empty' ? [] : [{ repository_id: REPO, profiles: [{ profile_id: 'quality-gate', target: 'main', requirement_digest: subject.requirement_digest, server_execution: true, readiness: 'configured', limitations: 'Synthetic profile for interface development only.' }] }] })
          const base = `/workspace/repositories/${encodeURIComponent(REPO)}`
          if (url.pathname === `${base}/profiles/quality-gate/subjects`) return send(200, { subjects: [subject], next_cursor: null })
          if (url.pathname === `${base}/profiles/quality-gate/subject`) return send(200, { subject })
          if (url.pathname === `${base}/runs`) return send(200, { runs: scenario === 'empty' ? [] : [run, { ...run, run_id: 'synthetic-unknown-run', generation: 4, state: 'unknown', receipt: null, detail: 'Synthetic unknown outcome; requires operator inspection.' }], next_cursor: null })
          if (url.pathname === `${base}/runs/synthetic-workspace-run`) return send(200, run)
          if (url.pathname === `${base}/runs/synthetic-workspace-run/workflow`) return send(200, workflow(run.native_run_id))
          return send(404, { error: 'synthetic_workspace_route_not_found' })
        }
        // Synthetic browser session (an HttpOnly cookie in production) and the computer routes.
        // Like the Hub: the web sign-in starts only with ?web=1; without it a sign-in expects the CLI's loopback port.
        // MOCK_PROVIDERS=1 makes the mock advertise forge sign-in so the gated front door can be seen.
        if (url.pathname === '/login' && req.method === 'GET') return send(200, { providers: process.env['MOCK_PROVIDERS'] === '1' ? ['github', 'gitlab'] : [], browser_only: [] })
        if (url.pathname.startsWith('/login/') && req.method === 'GET' && url.searchParams.get('web') === '1') {
          browserSignedIn = true
          res.statusCode = 302
          res.setHeader('location', '/#home')
          return res.end()
        }
        if (url.pathname === '/session' && req.method === 'DELETE') {
          if (req.headers['x-csrf-token'] !== 'synthetic-csrf-no-authority') return send(403, { error: 'csrf', message: 'Reload the page and try again.' })
          if (rejectNextHubSignOut) {
            rejectNextHubSignOut = false
            return send(503, { error: 'unavailable', message: 'The Hub did not confirm sign-out.' })
          }
          browserSignedIn = false
          res.statusCode = 204
          return res.end()
        }
        if (url.pathname === '/session') return scenario === 'empty' || !browserSignedIn ? send(401, { error: 'sign_in_required' }) : send(200, { user: { id: 'usr_maria', login: 'maria', provider: 'github', host: 'github.com', user_id: 1001 }, admin: true, csrf_token: 'synthetic-csrf-no-authority', expires_at: ago(-86400 / 3) })
        if (url.pathname === '/session/repositories') {
          if (scenario === 'empty' || !browserSignedIn) return send(401, { error: 'sign_in_required' })
          const row = (repository: string, needs_you: number, total: number, moving: number, stuck: number, agents: number, idle: number | null, complete = true) => ({ repository, repository_id: repository.length, provider: 'github', unfinished: { total, moving, stuck }, needs_you, agents, last_activity_at: idle === null ? null : ago(idle), complete })
          // One row in the full scenario keeps the landing on #home that the e2e specs expect; demo shows several,
          // including the repository whose read token the mock refuses.
          const rows = scenario === 'demo'
            ? [row(REPO, 2, 5, 3, 2, 4, 120), row(REFUSED_REPO, 1, 2, 1, 1, 2, 3600), row('acme/billing-docs', 0, 3, 1, 2, 1, 7200, false), row('acme/infra', 0, 2, 2, 0, 3, 600), row('acme/design-system', 0, 0, 0, 0, 0, 86400 * 9), row('acme/handbook', 0, 0, 0, 0, 0, null)]
            : [row(REPO, 0, 1, 1, 0, 1, 300)]
          return send(200, { repositories: rows, unchecked: scenario === 'demo' ? 1 : 0, complete: true, checked_at: ago(0) })
        }
        if (url.pathname === '/devices') return send(200, { devices: scenario === 'empty' ? [] : devices().filter((d) => d.owner.login === maria.login).map((d) => ({ ...d, owner: undefined })) })
        if (url.pathname.startsWith('/devices/') && req.method === 'DELETE') {
          const id = url.pathname.slice('/devices/'.length)
          const known = devices().find((d) => d.id === id)
          if (!known || known.revoked_at) return send(404, { error: 'not_found' })
          revokedDevices.set(id, ago(0))
          res.statusCode = 204
          return res.end()
        }
        if (url.pathname === '/tokens' && req.method === 'GET') {
          if (scenario === 'empty' || !browserSignedIn) return send(401, { error: 'sign_in_required' })
          const live = { revoked: false, expires_at: ago(-86400 * 20), read_only: false, created_at: ago(86400) }
          return send(200, { tokens: [{ ...live, id: 'tok_1', name: 'claude', repository: REPO }, { ...live, id: 'tok_2', name: 'codex', repository: REFUSED_REPO }, { ...live, id: 'tok_3', name: 'old', repository: 'acme/archived', revoked: true }] })
        }
        if (url.pathname === '/session/read-token' && req.method === 'POST') {
          if (!browserSignedIn || scenario === 'empty') return send(401, { error: 'sign_in_required' })
          if (req.headers['x-csrf-token'] !== 'synthetic-csrf-no-authority') return send(403, { error: 'csrf', message: 'This change did not come from the Hub’s own page; reload the page and try again' })
          let raw = ''
          for await (const chunk of req) raw += chunk
          const repository = (JSON.parse(raw || '{}') as { repository?: string }).repository ?? ''
          if (repository === REFUSED_REPO) return send(403, { error: 'insufficient_role', login: 'maria', repository, visibility: 'private', has: 'read', needs: 'triage', message: `maria has Read on private repository ${repository}; ask a repository owner for Triage or higher` })
          readTokensMinted++
          return send(201, { token: webToken(repository, readTokensMinted), expires_at: ago(-1800) })
        }
        if (url.pathname === '/__count/read-tokens') return send(200, { minted: readTokensMinted })
        if (url.pathname === '/admin/devices') return send(200, { devices: scenario === 'empty' ? [] : devices() })
        if (url.pathname === '/admin/repositories') return send(200, { repositories: scenario === 'empty' ? [] : [{ full_name: REPO, provider: 'github', host: 'github.com', connection: 'con_synthetic', devices: 3, agent_tokens: 5 }, { full_name: 'acme/billing-docs', provider: 'github', host: 'github.com', connection: 'con_synthetic2', devices: 1, agent_tokens: 1 }] })
        if (url.pathname === '/admin/sign-in-refusals') return send(200, { refusals: scenario === 'empty' ? [] : [
          { at: ago(900), count: 3, provider: 'github', login: 'brandonfla', forge_user_id: 3001, reason: 'insufficient_role', via: 'device_mint' },
          { at: ago(86400 * 2), count: 1, provider: 'github', login: 'drive-by', forge_user_id: 3002, reason: 'not_allowed', via: 'sign_in' },
        ] })
        const token = /^Bearer (.+)$/.exec(req.headers.authorization ?? '')?.[1]
        if (!token || token === 'expired') return send(401, { error: 'unauthorized' })
        if (!token.startsWith('webread:') && rejectNextAgentRead && (url.pathname === '/whoami' || /^\/repos\/[^/]+\/(work|activity)$/.test(url.pathname))) {
          rejectNextAgentRead = false
          return send(401, { error: 'unauthorized' })
        }
        if (token.startsWith('webread:') && rejectNextWebRead && (url.pathname === '/whoami' || /^\/repos\/[^/]+\/(work|activity)$/.test(url.pathname))) {
          rejectNextWebRead = false
          return send(401, { error: 'unauthorized' })
        }
        if (url.pathname === '/whoami') {
          // A web read token names its repository; every other synthetic token reads acme/widgets.
          const webParts = /^webread:([^:]+):([^:]+):[0-9]+$/.exec(token)
          const web = webParts ? `${webParts[1]}/${webParts[2]}` : null
          return send(200, { token: { id: 'tkn_1', name: web ? 'hub-web' : 'status-page', repository: web ?? REPO, expires_at: web ? ago(-1800) : ago(-86400 * 30), read_only: !token.startsWith('agent') }, owner: maria })
        }
        if (down) return send(503, { error: 'unavailable' })
        const empty = scenario === 'empty'
        const billingDocs = token.startsWith('webread:acme:billing-docs:')
        if (url.pathname === '/standup/overview') return send(200, empty || billingDocs ? { ...overview(), current: [], recent: [], overlaps: [] } : overview())
        // A fixed personal delivery over the overview's work, two coordination events and two merges. Ack empties it once.
        if (url.pathname === '/standup/briefing') {
          const ov = overview()
          const items = empty ? [] : [...new Map([...ov.current, ...ov.recent.map((o) => o.item)].map((w) => [w.id, w])).values()]
          const events = items.map((item, i) => ({ seq: 10 + i, at: item.updated_at, kind: i % 3 === 0 ? 'work_started' : 'work_checkpoint', work_id: item.id, item }))
          return send(200, { delivery: null, from: 0, through: 40, after: 0, work_after: null, next_after: 40, next_work_after: null, next_work_done: true, items, events, retention_gap: false, complete: true })
        }
        if (url.pathname === '/standup/coordination') return send(200, { from: 0, through: 40, next_after: 0, requests: [], complete: true, retention_gap: false })
        if (url.pathname.startsWith('/standup/work/')) {
          const id = decodeURIComponent(url.pathname.slice('/standup/work/'.length))
          const ov = overview()
          const item = ov.current.find((w) => w.id === id) ?? ov.recent.find((o) => o.item.id === id)?.item
          return item && !empty ? send(200, item) : send(404, { error: 'not found' })
        }
        if (url.pathname === '/standup/catchup/ack' && req.method === 'POST') {
          catchupAcked = true
          return send(200, { status: 'advanced', event_through: 40, merge_through: 2, current_event_ack: 40, current_merge_ack: 2 })
        }
        if (url.pathname === '/standup/catchup') {
          const ov = overview()
          const quiet = empty || catchupAcked
          const work_events = quiet ? [] : ov.current.map((item, i) => ({ seq: 30 + i, at: item.updated_at, kind: i === 0 ? 'work_started' : 'work_checkpoint', work_id: item.id, item }))
          const coordination_events = quiet ? [] : [
            { seq: 28, at: ago(5400), kind: 'coordination_requested', request_id: 'crq_synthetic_1', title: 'Split the invoice scope', status: 'pending' },
            { seq: 29, at: ago(3000), kind: 'coordination_responded', request_id: 'crq_synthetic_1', title: 'Split the invoice scope', status: 'responded' },
          ]
          const merge = (n: number, title: string, secs: number, failed = false) => ({ id: `mob_synthetic_${n}`, seq: n, source: 'forge_pull', source_id: `pull_${n}`, occurred_at: ago(secs + 60), observed_at: ago(secs), title: failed ? `${title} (checks failed after merge)` : title, branch: `feat/${n}`, base: 'main', head_commit: String(n).repeat(40).slice(0, 40), merge_commit: `${n}f`.repeat(20).slice(0, 40), pull_number: 200 + n, paths: ['src/billing.rs'], paths_complete: true })
          const merges = quiet ? [] : [merge(1, 'Round invoice totals per currency', 6000), merge(2, 'Move tax rules into their own module', 1800)]
          return send(200, {
            subscriber: 'human', delivery_client: url.searchParams.get('delivery_client') ?? 'hub_synthetic-client-0001', delivery: `cup_${'a'.repeat(32)}`,
            event_from: 27, event_through: 40, event_after: 27, next_event_after: 40, merge_from: 0, merge_through: 2, merge_after: 0, next_merge_after: 2,
            work_events, coordination_events, merges,
            retention: { work_gap: false, coordination_gap: false, merge_gap: false, event_available_from: 0, merge_available_from: 0 },
            source_coverage: [
              { source: 'tracked_publication', state: 'observed_only', checked_at: ago(120), unread_pages: 0, deferred: 0 },
              { source: 'forge_pull', state: 'observed_only', checked_at: ago(120), unread_pages: 0, deferred: 0 },
            ],
            complete: true,
          })
        }
        if (url.pathname === '/onboarding') {
          const base = { owner: 'maria', expires_at: ago(-600), mcp_verified: true, native_verified: true, blocker: null, next_action: 'MCP and a native edit-hook check were observed for this connection and session. Continue work; this is coordination evidence, not a guarantee of future quota or complete tool coverage.' }
          return send(200, empty ? [] : [
            { ...base, agent: 'claude', key: 'a1b2c3d4e5f6', name: 'Claude Code' },
            { ...base, agent: 'codex', key: '9f8e7d6c5b4a', name: 'Codex', native_verified: false, blocker: 'trust', next_action: 'Review the generated AgentSCM plugin/hooks and MCP server in your host, approve them yourself, then start a fresh session. Never bypass trust.' },
          ])
        }
        if (url.pathname === '/skills') {
          if (empty) return send(200, { catalog: [], adoption: { repo: REPO, source: 'none', observed_at: null, default_branch: null, pins: [] }, counts: {} })
          return send(200, {
            catalog: SKILL_CATALOG,
            adoption: { repo: REPO, source: 'default_branch', observed_at: ago(300), default_branch: 'main', pins: [LOCK_PIN(), { name: 'legacy-review', workflow_version: 1, path: null, sha256: '0b2e'.padEnd(64, '0'), status: 'unknown_version', params: {}, source: { kind: 'first_party' } }] },
            counts: { 'pr-gate': { open: 2, at_stop_gate: 1, completed: 1 } },
          })
        }
        if (url.pathname.startsWith(`/repos/${encodeURIComponent(REPO)}/skills/runs`)) {
          const rest = url.pathname.slice(`/repos/${encodeURIComponent(REPO)}/skills/runs`.length)
          if (rest === '') return send(200, { runs: empty ? [] : SKILL_RUNS, truncated: false })
          const found = SKILL_RUNS.find((r) => `/${r.run_id}` === rest)
          return found && !empty ? send(200, found) : send(404, { error: 'not_found' })
        }
        if (url.pathname === '/skill-workflows') {
          if (empty || scenario === 'demo') return send(200, { version: 1, availability: 'available', runs: [], next_cursor: null })
          const before = url.searchParams.get('before')
          if (before !== null && !/^[1-9][0-9]{0,18}$/.test(before)) return send(400, { error: 'invalid before cursor' })
          const q = url.searchParams.get('q')?.trim().toLowerCase()
          const candidates = before === null ? WORKFLOW_RUNS : WORKFLOW_RUNS.filter((run) => BigInt(run.sequence) < BigInt(before))
          const matches = q ? candidates.filter((run) => [run.run_id, run.name, run.revision, run.state].some((v) => (v ?? '').toLowerCase().includes(q))) : candidates
          const runs = matches.slice(0, 16)
          return send(200, {
            version: 1,
            availability: 'available',
            runs,
            next_cursor: matches.length > runs.length ? runs.at(-1)?.sequence ?? null : null,
          })
        }
        if (url.pathname === '/standup/work') {
          if (empty || scenario === 'demo') return send(200, { items: [], next_cursor: null, complete: true })
          const q = url.searchParams.get('q')?.trim().toLowerCase().replace(/^#/, '')
          const cursor = url.searchParams.get('cursor')
          const limit = clampLimit(url.searchParams.get('limit'), 20, 50)
          const start = cursor === null ? 0 : WORK_ITEMS.findIndex((w) => w.id === cursor) + 1
          if (cursor !== null && start === 0) return send(400, { error: 'invalid' })
          const matches = WORK_ITEMS.slice(start).filter((w) => !q || [w.title, w.branch, w.lead_agent, w.owner, w.id, w.source_issue].some((v) => !!v && v.toLowerCase().includes(q)))
          const items = matches.slice(0, limit)
          const more = matches.length > items.length
          return send(200, { items, next_cursor: more ? items.at(-1)!.id : null, complete: !more })
        }
        if (url.pathname === `/repos/${encodeURIComponent(REPO)}/publications`) {
          if (empty || scenario === 'demo') return send(200, { publications: [], next_cursor: null, complete: true })
          const q = url.searchParams.get('q')?.trim().toLowerCase().replace(/^#/, '')
          const before = url.searchParams.get('before')
          const limit = clampLimit(url.searchParams.get('limit'), 25, 100)
          const start = before === null ? 0 : PUBLICATIONS.findIndex((p) => p.id === before) + 1
          if (before !== null && start === 0) return send(400, { error: 'invalid' })
          const matches = PUBLICATIONS.slice(start).filter((p) => !q || [p.branch, p.title, p.base, p.id, String((p.states.pull_request as { number?: number }).number ?? '')].some((v) => v.toLowerCase().includes(q)))
          const items = matches.slice(0, limit)
          const more = matches.length > items.length
          return send(200, { publications: items, next_cursor: more ? items.at(-1)!.id : null, complete: !more })
        }
        if (url.pathname.startsWith(`/repos/${encodeURIComponent(REPO)}/publications/`)) {
          const found = PUBLICATIONS.find((p) => url.pathname.endsWith(p.id))
          return found ? send(200, found) : send(404, { error: 'not found' })
        }
        if (url.pathname.startsWith('/skill-workflows/')) {
          const runId = decodeURIComponent(url.pathname.slice('/skill-workflows/'.length))
          const retained = workflow(runId)
          return retained ? send(200, retained) : send(404, { error: 'not found' })
        }
        // Any repository a read token names gets the synthetic team's work, so switching repositories shows a page.
        if (/^\/repos\/[^/]+\/work$/.test(url.pathname)) {
          if (scenario === 'demo') return send(200, { ...work(), pull_requests: [], connected: false })
          return send(200, empty || billingDocs ? { intents: [], intents_truncated: false, pull_requests: [], branches: [], connected: false, truncated: {} } : work())
        }
        if (/^\/repos\/[^/]+\/activity$/.test(url.pathname)) {
          if (empty || billingDocs) return send(200, { sessions: [], events: [], next_before: null })
          const all = activity()
          const limit = clampLimit(url.searchParams.get('limit'), 50, 200)
          const rawBefore = url.searchParams.get('before')
          if (rawBefore !== null && !/^[1-9][0-9]{0,15}$/.test(rawBefore)) return send(400, { error: 'invalid' })
          const older = rawBefore === null ? all.events : all.events.filter((e) => e.seq < Number(rawBefore))
          const events = older.slice(0, limit)
          return send(200, { ...all, events, next_before: older.length > events.length ? events.at(-1)!.seq : null })
        }
        send(404, { error: 'not found' })
      })
    },
  }
}
