// Dev-only: answers the hub's read-only status endpoints from synthetic fixtures.
// Used by `MOCK=1 npm run dev`; never imported by app code.
//   MOCK=1      acme/widgets with live agents, a contested file and open PRs
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
        const token = /^Bearer (.+)$/.exec(req.headers.authorization ?? '')?.[1]
        if (!token || token === 'expired') return send(401, { error: 'unauthorized' })
        if (url.pathname === '/whoami') {
          return send(200, { token: { id: 'tkn_1', name: 'status-page', repository: REPO, expires_at: ago(-86400 * 30), read_only: !token.startsWith('agent') }, owner: maria })
        }
        if (down) return send(503, { error: 'unavailable' })
        const empty = scenario === 'empty'
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
