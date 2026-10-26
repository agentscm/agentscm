import type { Page, Route } from '@playwright/test'

const now = 1_798_000_000
const json = (route: Route, body: unknown, status = 200) => route.fulfill({ status, contentType: 'application/json', body: JSON.stringify(body) })
const body = async (route: Route) => JSON.parse(route.request().postData() || '{}') as Record<string, unknown>

export const LOCAL_TOKEN = `local:${'a'.repeat(64)}:acme/widgets`

export interface LibraryMockState {
  workflows: { draft: Record<string, unknown>; versions: Record<string, unknown>[] }[]
  skills: Record<string, unknown>[]
  attachments: Record<string, unknown>[]
  updates: Record<string, unknown>[]
  failNextPublish: boolean
  nextValidationRunnable: boolean
}

export async function installWorkflowLibraryMock(page: Page): Promise<LibraryMockState> {
  const state: LibraryMockState = { workflows: [], skills: [], attachments: [], updates: [], failNextPublish: false, nextValidationRunnable: false }
  let sequence = 0
  await page.route('**/v1/**', async (route) => {
    const request = route.request()
    const url = new URL(request.url())
    const path = url.pathname
    const method = request.method()
    if (path === '/v1/hub/whoami') return json(route, { token: { id: 'tkn_workflow_library', name: 'status-page', repository: 'acme/widgets', expires_at: null, read_only: true }, owner: { provider: 'github', host: 'github.com', login: 'synthetic-user' } })
    if (path === '/v1/hub/health') return json(route, { status: 'ok', storage: 'sqlite', coordination: { denied: 0, repeated: 0, rate_limited: 0, fail_open: 0, hook_p95_ms: 2 } })
    if (path === '/v1/hub/repos/acme%2Fwidgets/work') return json(route, { intents: [], intents_truncated: false, pull_requests: [], connected: true })
    if (path === '/v1/hub/repos/acme%2Fwidgets/activity') return json(route, { sessions: [], events: [], next_before: null })
    if (path === '/v1/hub/standup/overview') return json(route, { recent_since: now - 90_000, until: now, current: [], recent: [], overlaps: [], current_complete: true, recent_complete: true, overlaps_complete: true, history_retention_gap: false })
    if (path === '/v1/local/identity') return json(route, { instance: 'b'.repeat(32), computer: 'Test Mac', repository: 'acme/widgets', hub: { url: 'http://127.0.0.1:7332', signed_in: true }, expires_at: null })
    if (path === '/v1/local/repositories') return json(route, { repositories: [{ repository: 'acme/widgets', hub: 'http://127.0.0.1:7332', folder: 'widgets' }, { repository: 'beta/gadgets', hub: 'http://127.0.0.1:7332', folder: 'gadgets' }] })
    if (path === '/v1/local/workflow-library' && method === 'GET') return json(route, { version: 1, workflows: state.workflows })
    if (path === '/v1/local/workflow-library' && method === 'POST') {
      const input = await body(route)
      const id = `workflow-${++sequence}`
      const draft = { id, name: input['name'], summary: input['summary'], generation: 1, steps: [], trigger: null, follow_on: null, updated_at: now + sequence, archived: false }
      state.workflows.unshift({ draft, versions: [] })
      return json(route, draft, 201)
    }
    const workflow = path.match(/^\/v1\/local\/workflow-library\/([^/]+)$/)
    if (workflow && method === 'PATCH') {
      const input = await body(route)
      state.updates.push(input)
      const record = state.workflows.find((w) => w.draft['id'] === workflow[1])!
      record.draft = { ...record.draft, ...input, generation: Number(record.draft['generation']) + 1, updated_at: now + ++sequence }
      delete record.draft['expected_generation']
      return json(route, record.draft)
    }
    if (workflow && method === 'DELETE') {
      state.workflows = state.workflows.filter((w) => w.draft['id'] !== workflow[1])
      return route.fulfill({ status: 204 })
    }
    const publish = path.match(/^\/v1\/local\/workflow-library\/([^/]+)\/versions$/)
    if (publish && method === 'POST') {
      if (state.failNextPublish) {
        state.failNextPublish = false
        return json(route, { error: 'invalid_workflow', message: 'workflow has blocking diagnostics', diagnostics: [{ level: 'error', code: 'invalid_composition', message: 'not found: skill resource missing.json', path: null }] }, 400)
      }
      const record = state.workflows.find((w) => w.draft['id'] === publish[1])!
      const version = { id: `${publish[1]}-v${record.versions.length + 1}`, workflow_id: publish[1], version: record.versions.length + 1, digest: 'd'.repeat(64), definition: record.draft, skill_versions: state.skills.map((s) => s['id']), created_at: now + ++sequence, archived: false }
      record.versions.unshift(version)
      return json(route, { version, diagnostics: [] }, 201)
    }
    const archive = path.match(/^\/v1\/local\/workflow-library\/([^/]+)\/archive$/)
    if (archive && method === 'POST') {
      const record = state.workflows.find((w) => w.draft['id'] === archive[1])!
      record.draft = { ...record.draft, archived: true, generation: Number(record.draft['generation']) + 1, updated_at: now + ++sequence }
      return json(route, record.draft)
    }
    if (path === '/v1/local/skill-library' && method === 'GET') return json(route, { version: 1, skills: state.skills })
    if (path === '/v1/local/skill-imports/preview' && method === 'POST') {
      const input = await body(route)
      const source = input['source'] as Record<string, unknown>
      const files = [{ path: 'SKILL.md', sha256: 'e'.repeat(64), size: String(source['markdown'] || '').length }]
      const provenance = { kind: source['kind'], locator: source['kind'] === 'markdown' ? 'pasted' : source['repository'] || 'descriptor', commit: source['commit'], path: source['path'], command: source['command'] }
      return json(route, { preview_id: 'preview-1', digest: 'c'.repeat(64), expires_at: now + 600, skill: { name: source['name'] || 'Imported skill', entrypoint: 'SKILL.md', provenance }, files, warnings: [], blocked: false })
    }
    if (path === '/v1/local/skill-imports/accept' && method === 'POST') {
      const skill = state.skills.find((value) => value['digest'] === 'c'.repeat(64)) ?? { id: `skill-${++sequence}`, name: 'Release helper', version: 1, generation: 1, digest: 'c'.repeat(64), entrypoint: 'SKILL.md', files: [{ path: 'SKILL.md', sha256: 'e'.repeat(64), size: 72 }], provenance: { kind: 'markdown', locator: 'pasted' }, created_at: now, archived: false }
      if (!state.skills.includes(skill)) state.skills.unshift(skill)
      return json(route, skill, 201)
    }
    if (path === '/v1/local/workflow-simulations' && method === 'POST') {
      const input = await body(route)
      const record = state.workflows.find((w) => w.versions.some((v) => v['id'] === input['workflow_version_id']))!
      const steps = record.draft['steps'] as Record<string, unknown>[]
      return json(route, { outcome: 'passed_with_warnings', steps: steps.map((s) => ({ id: s['id'], status: 'would_run', requires: ['operator_profile'], detail: `${s['label']} is structurally ready.` })), trigger_match: true, diagnostics: [{ severity: 'warning', message: 'Operator runtime is not configured in this browser fixture.' }], side_effects: 'none', admission: 'not_assessed', deployment: 'not_attempted' })
    }
    if (path === '/v1/local/repository-guidance' && method === 'POST') return json(route, { observed_files: [{ path: 'package.json', sha256: 'f'.repeat(64), size: 1200 }], suggestions: [{ kind: 'lint', argv: ['npm', 'run', 'lint'], reason: 'package.json declares a lint script', sources: ['package.json'] }], uncovered: ['Deploy parity was not established.'], diagnostics: [] })
    if (/\/workflow-attachments$/.test(path) && method === 'GET') return json(route, state.attachments)
    const attach = path.match(/\/workflow-attachments\/([^/]+)$/)
    if (attach && method === 'PUT') {
      const input = await body(route)
      const current = state.attachments.find((item) => item['id'] === attach[1])
      const value = { id: attach[1], repository: 'acme/widgets', workflow_version_id: input['workflow_version_id'], generation: current ? Number(current['generation']) + 1 : 1, state: 'draft', diagnostics: [] }
      state.attachments = [value]
      return json(route, value)
    }
    const validate = path.match(/\/workflow-attachments\/([^/]+)\/validate$/)
    if (validate && method === 'POST') {
      const current = state.attachments.find((item) => item['id'] === validate[1])!
      const runnable = state.nextValidationRunnable
      state.nextValidationRunnable = false
      const attachment = { ...current, generation: Number(current['generation']) + 1, state: runnable ? 'export_required' : 'blocked', diagnostics: runnable ? [] : [{ level: 'error', code: 'unmet_runtime_binding', message: 'operator_profile', path: null }] }
      state.attachments = [attachment]
      return json(route, {
        attachment,
        state: attachment.state,
        setup: {
          runnable,
          files: [{ path: '.agentscm/workflows/major-feature.json', content: '{"version":1}' }],
          pipeline_recipe: { trigger: 'confirmed_merge' },
          pipeline_install_template: runnable ? { command: 'agentscm pipeline install' } : null,
          library_follow_on: null,
          unmet_bindings: runnable ? [] : ['operator_profile'],
          activation: 'operator_local_cli',
        },
        next_actions: runnable ? ['Commit the exact exported package.'] : [],
      })
    }
    return json(route, { error: `not found: unhandled local fixture route ${method} ${path}` }, 404)
  })
  return state
}
