// Dev-only: answers status and workspace GET endpoints from synthetic fixtures.
// Workspace fixtures are visibly labelled; mutations return 501 and never execute.
// Used by `MOCK=1 npm run dev`; never imported by app code.
//   MOCK=1      acme/widgets with live agents, open PRs and retained workflows
//   MOCK=empty  a connected-nothing repository: no agents, events or PRs, no forge connection
//   MOCK=down   the hub is failing: health 503, work and activity 503
//   MOCK=demo   `agentscm demo` hub: health.demo true, simulated agents, no forge connection, no PRs
// While running, GET /__mock/down and /__mock/up flip the failure on and off.
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
    { ...base, id: 'int_maria_claude', task: 'Refactor the billing module to split tax calculation from invoice totals, keeping the public interface intact and adding focused unit tests for each rounding mode we support today', branch: 'maria/billing-split', machine: 'synthetic-laptop-1', paths: ['src/billing.rs', 'src/invoice/**', 'src/tax.rs', 'src/currency.rs', 'src/ledger.rs', 'src/receipts.rs'], via: ['hook:claude'], created_at: ago(900), updated_at: ago(12), agent: A.mariaClaude, owner: maria },
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
  return list
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
const WORKFLOW_RUNS = Array.from({ length: 45 }, (_, index) => {
  const special = [
    { run_id: 'claude-review-failed', state: 'failed', name: 'Claude code quality gate' },
    { run_id: 'claude-review-stale', state: 'succeeded', name: 'Billing change verification' },
    { run_id: 'claude-review-passed', state: 'succeeded', name: 'Claude code quality gate' },
  ][index]
  return {
    run_id: special?.run_id ?? `retained-run-${String(45 - index).padStart(3, '0')}`,
    revision: (index % 16).toString(16).repeat(64),
    state: special?.state ?? WORKFLOW_STATES[index % WORKFLOW_STATES.length],
    name: special?.name ?? WORKFLOW_NAMES[index % WORKFLOW_NAMES.length],
    sequence: String(5000 - index),
  }
})

function workflow(runId: string): Record<string, unknown> | null {
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

export type Scenario = 'full' | 'empty' | 'down' | 'demo'

export function mockHub(scenario: Scenario = 'full'): Plugin {
  let down = scenario === 'down'
  return {
    name: 'agentscm-mock-hub',
    apply: 'serve',
    configureServer(server) {
      server.middlewares.use('/__mock', (req, res) => {
        down = req.url?.startsWith('/down') ?? false
        res.end(down ? 'hub is now failing\n' : 'hub is now healthy\n')
      })
      server.middlewares.use('/v1/hub', (req, res) => {
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
          if (url.pathname === '/workspace/settings') return send(200, settings)
          if (url.pathname === '/workspace/members') return send(200, { members: [{ identity, roles: ['reader', 'contributor', 'administrator'], repository_ids: ['*'], enabled: true, generation: 1 }, { identity: { ...identity, user_id: 1002 }, roles: ['reader', 'reviewer'], repository_ids: [REPO], enabled: true, generation: 1 }], generation: 1, next_cursor: null })
          if (url.pathname === '/workspace/repositories') return send(200, { repositories: scenario === 'empty' ? [] : [{ repository_id: REPO, profiles: [{ profile_id: 'quality-gate', target: 'main', requirement_digest: subject.requirement_digest, readiness: 'configured', limitations: 'Synthetic profile for interface development only.' }] }] })
          const base = `/workspace/repositories/${encodeURIComponent(REPO)}`
          if (url.pathname === `${base}/profiles/quality-gate/subjects`) return send(200, { subjects: [subject], next_cursor: null })
          if (url.pathname === `${base}/profiles/quality-gate/subject`) return send(200, { subject })
          if (url.pathname === `${base}/runs`) return send(200, { runs: scenario === 'empty' ? [] : [run, { ...run, run_id: 'synthetic-unknown-run', generation: 4, state: 'unknown', receipt: null, detail: 'Synthetic unknown outcome; requires operator inspection.' }], next_cursor: null })
          if (url.pathname === `${base}/runs/synthetic-workspace-run`) return send(200, run)
          if (url.pathname === `${base}/runs/synthetic-workspace-run/workflow`) return send(200, workflow(run.native_run_id))
          return send(404, { error: 'synthetic_workspace_route_not_found' })
        }
        const token = /^Bearer (.+)$/.exec(req.headers.authorization ?? '')?.[1]
        if (!token || token === 'expired') return send(401, { error: 'unauthorized' })
        if (url.pathname === '/whoami') {
          return send(200, { token: { id: 'tkn_1', name: 'status-page', repository: REPO, expires_at: ago(-86400 * 30), read_only: !token.startsWith('agent') }, owner: maria })
        }
        if (down) return send(503, { error: 'unavailable' })
        const empty = scenario === 'empty'
        if (url.pathname === '/skill-workflows') {
          if (empty || scenario === 'demo') return send(200, { version: 1, availability: 'available', runs: [], next_cursor: null })
          const before = url.searchParams.get('before')
          if (before !== null && !/^[1-9][0-9]{0,18}$/.test(before)) return send(400, { error: 'invalid before cursor' })
          const candidates = before === null ? WORKFLOW_RUNS : WORKFLOW_RUNS.filter((run) => BigInt(run.sequence) < BigInt(before))
          const runs = candidates.slice(0, 16)
          return send(200, {
            version: 1,
            availability: 'available',
            runs,
            next_cursor: candidates.length > runs.length ? runs.at(-1)?.sequence ?? null : null,
          })
        }
        if (url.pathname.startsWith('/skill-workflows/')) {
          const runId = decodeURIComponent(url.pathname.slice('/skill-workflows/'.length))
          const retained = workflow(runId)
          return retained ? send(200, retained) : send(404, { error: 'not found' })
        }
        if (url.pathname === `/repos/${encodeURIComponent(REPO)}/work`) {
          if (scenario === 'demo') return send(200, { ...work(), pull_requests: [], connected: false })
          return send(200, empty ? { intents: [], intents_truncated: false, pull_requests: [], branches: [], connected: false, truncated: {} } : work())
        }
        if (url.pathname === `/repos/${encodeURIComponent(REPO)}/activity`) return send(200, empty ? { sessions: [], events: [] } : activity())
        send(404, { error: 'not found' })
      })
    },
  }
}
