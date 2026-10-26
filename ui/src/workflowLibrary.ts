import { localCredential, loadLocalBase } from './localApi.ts'

export type DiagnosticTone = 'error' | 'warning' | 'info'
export interface Diagnostic {
  severity: DiagnosticTone
  message: string
  path?: string
}

export type WorkflowStep = {
  id: string
  label: string
  needs: string[]
  required: boolean
} & (
  | { kind: 'skill'; skill_version_id: string }
  | { kind: 'build'; skill_version_id: string; runner: string }
  | { kind: 'deploy'; skill_version_id: string; resource: string; target: string }
  | { kind: 'action'; skill_version_id: string; resource: string }
  | { kind: 'workflow'; workflow_version_id: string; inputs: Record<string, string> }
)

export interface WorkflowTrigger {
  kind: 'confirmed_merge'
  target: string
  labels_all: string[]
}

export interface WorkflowFollowOn {
  workflow_version_id: string
  when: 'success' | 'failure' | 'always'
  inputs: Record<string, string>
}

export interface WorkflowDraft {
  id: string
  name: string
  summary: string
  generation: number
  steps: WorkflowStep[]
  trigger: WorkflowTrigger | null
  follow_on: WorkflowFollowOn | null
  updated_at: number
  archived: boolean
}

export interface WorkflowVersion {
  id: string
  workflow_id: string
  version: number
  digest: string
  definition: WorkflowDraft
  skill_versions: string[]
  created_at: number
  archived: boolean
}

export interface WorkflowRecord {
  draft: WorkflowDraft
  versions: WorkflowVersion[]
}

export interface SkillFile {
  path: string
  sha256: string
  size: number
}

export interface SkillVersion {
  id: string
  name: string
  version: number
  generation: number
  digest: string
  entrypoint: string
  files: SkillFile[]
  provenance: {
    kind: 'markdown' | 'github' | 'npx_descriptor'
    locator: string
    commit?: string
    path?: string
    command?: string
  }
  created_at: number
  archived: boolean
}

export interface SkillPreview {
  preview_id: string
  digest: string
  expires_at: number
  skill: {
    name: string
    entrypoint: string
    provenance: SkillVersion['provenance']
  }
  files: SkillFile[]
  warnings: Diagnostic[]
  blocked: boolean
}

export interface Simulation {
  outcome: 'passed' | 'passed_with_warnings' | 'failed'
  steps: { id: string; status: 'would_run' | 'would_skip' | 'blocked'; requires: string[]; detail: string }[]
  trigger_match: boolean | null
  diagnostics: Diagnostic[]
  side_effects: 'none'
  admission: 'not_assessed'
  deployment: 'not_attempted'
}

export interface Guidance {
  observed_files: SkillFile[]
  suggestions: { kind: 'lint' | 'build' | 'test'; argv: string[]; reason: string; sources: string[] }[]
  uncovered: string[]
  diagnostics: Diagnostic[]
}

export interface Attachment {
  id: string
  repository: string
  workflow_version_id: string
  generation: number
  state: 'draft' | 'blocked' | 'validated' | 'export_required' | 'active' | 'archived'
  source_binding?: unknown
  pipeline_rule?: unknown
  diagnostics: Diagnostic[]
}

export interface AttachmentSetup {
  runnable: boolean
  files: { path: string; content: string }[]
  pipeline_recipe: Record<string, unknown>
  pipeline_install_template: Record<string, unknown> | null
  library_follow_on: WorkflowFollowOn | null
  unmet_bindings: string[]
  activation: 'operator_local_cli'
}

export interface AttachmentValidation extends Attachment {
  setup: AttachmentSetup
  next_actions: string[]
}

type Json = Record<string, unknown>
const rec = (v: unknown): Json => (v && typeof v === 'object' && !Array.isArray(v) ? v as Json : {})
const array = (v: unknown): unknown[] => Array.isArray(v) ? v : []
const text = (v: unknown, fallback = ''): string => typeof v === 'string' ? v.slice(0, 4096) : fallback
const bool = (v: unknown): boolean => v === true
const number = (v: unknown): number => typeof v === 'number' && Number.isFinite(v) ? v : 0
const strings = (v: unknown, limit = 128): string[] => array(v).slice(0, limit).flatMap((x) => typeof x === 'string' ? [x.slice(0, 1024)] : [])
const stringMap = (v: unknown): Record<string, string> => Object.fromEntries(Object.entries(rec(v)).slice(0, 128).flatMap(([k, x]) => typeof x === 'string' ? [[k.slice(0, 128), x.slice(0, 1024)]] : []))

function diagnostic(v: unknown): Diagnostic {
  if (typeof v === 'string') return { severity: 'error', message: v.slice(0, 4096) }
  const r = rec(v)
  const raw = text(r['severity'] || r['level'], 'info')
  const severity: DiagnosticTone = raw === 'error' || raw === 'warning' ? raw : 'info'
  const path = text(r['path'])
  return { severity, message: text(r['message'] || r['detail'] || r['code'], 'Unknown diagnostic'), ...(path ? { path } : {}) }
}
const diagnostics = (v: unknown): Diagnostic[] => array(v).slice(0, 256).map(diagnostic)

function step(v: unknown): WorkflowStep | null {
  const r = rec(v)
  const common = { id: text(r['id']), label: text(r['label'], text(r['id'])), needs: strings(r['needs'], 64), required: r['required'] !== false }
  if (!common.id) return null
  const kind = r['kind']
  if (kind === 'skill') return { ...common, kind, skill_version_id: text(r['skill_version_id']) }
  if (kind === 'build') return { ...common, kind, skill_version_id: text(r['skill_version_id']), runner: text(r['runner']) }
  if (kind === 'deploy') return { ...common, kind, skill_version_id: text(r['skill_version_id']), resource: text(r['resource']), target: text(r['target']) }
  if (kind === 'action') return { ...common, kind, skill_version_id: text(r['skill_version_id']), resource: text(r['resource']) }
  if (kind === 'workflow') return { ...common, kind, workflow_version_id: text(r['workflow_version_id']), inputs: stringMap(r['inputs']) }
  return null
}

function trigger(v: unknown): WorkflowTrigger | null {
  const r = rec(v)
  return r['kind'] === 'confirmed_merge' ? { kind: 'confirmed_merge', target: text(r['target']), labels_all: strings(r['labels_all'], 32) } : null
}

function followOn(v: unknown): WorkflowFollowOn | null {
  const r = rec(v)
  const when = r['when']
  return typeof r['workflow_version_id'] === 'string' && (when === 'success' || when === 'failure' || when === 'always')
    ? { workflow_version_id: text(r['workflow_version_id']), when, inputs: stringMap(r['inputs']) }
    : null
}

function draft(v: unknown): WorkflowDraft {
  const r = rec(v)
  return {
    id: text(r['id']), name: text(r['name'], 'Untitled workflow'), summary: text(r['summary']), generation: number(r['generation']),
    steps: array(r['steps']).slice(0, 128).flatMap((x) => { const s = step(x); return s ? [s] : [] }),
    trigger: trigger(r['trigger']), follow_on: followOn(r['follow_on']), updated_at: number(r['updated_at']), archived: bool(r['archived']),
  }
}

function version(v: unknown): WorkflowVersion {
  const r = rec(v)
  return {
    id: text(r['id']), workflow_id: text(r['workflow_id']), version: number(r['version']), digest: text(r['digest']),
    definition: draft(r['definition']),
    skill_versions: strings(r['skill_versions']), created_at: number(r['created_at']), archived: bool(r['archived']),
  }
}

function skill(v: unknown): SkillVersion {
  const r = rec(v)
  const p = rec(r['provenance'])
  const kind = p['kind'] === 'github' || p['kind'] === 'npx_descriptor' ? p['kind'] : 'markdown'
  return {
    id: text(r['id']), name: text(r['name'], 'Unnamed skill'), version: number(r['version']), generation: number(r['generation']), digest: text(r['digest']), entrypoint: text(r['entrypoint']),
    files: array(r['files']).slice(0, 128).map(file),
    provenance: { kind, locator: text(p['locator']), ...(text(p['commit']) ? { commit: text(p['commit']) } : {}), ...(text(p['path']) ? { path: text(p['path']) } : {}), ...(text(p['command']) ? { command: text(p['command']) } : {}) },
    created_at: number(r['created_at']), archived: bool(r['archived']),
  }
}

function file(v: unknown): SkillFile {
  const r = rec(v)
  return { path: text(r['path']), sha256: text(r['sha256']), size: number(r['size']) }
}

export class WorkflowLibraryError extends Error {
  status: number
  code: string
  diagnostics: Diagnostic[]
  constructor(status: number, code: string, message: string, items: Diagnostic[] = []) {
    super(message)
    this.status = status
    this.code = code
    this.diagnostics = items
  }
}

function connectionAdvice(code: string, message: string): string {
  if (code === 'not_connected') return 'Open this repository with `agentscm open`, then return to Workflow library.'
  if (code === 'repository_unavailable') return 'The selected checkout is no longer connected. Choose another repository at the top of the page, or run `agentscm open` in its checkout.'
  if (code === 'generation_conflict') return 'This item changed on this computer. Reload it before saving again.'
  return message || 'The local workflow library could not complete the request.'
}

async function request(path: string, init: RequestInit, signal?: AbortSignal): Promise<unknown> {
  if (loadLocalBase()) throw new WorkflowLibraryError(403, 'not_connected', 'Workflow editing is available only in the page opened by `agentscm open`.')
  const token = localCredential()
  if (!token) throw new WorkflowLibraryError(401, 'not_connected', connectionAdvice('not_connected', ''))
  let response: Response
  try {
    response = await fetch(path, {
      ...init, signal, cache: 'no-store', credentials: 'omit',
      headers: { 'content-type': 'application/json', Authorization: `Bearer ${token}`, ...init.headers },
    })
  } catch (error) {
    if (signal?.aborted) throw error
    throw new WorkflowLibraryError(0, 'unavailable', 'Nothing answered on this computer. Run `agentscm open` in the repository and try again.')
  }
  const body: unknown = response.status === 204 ? null : await response.json().catch(() => null)
  if (response.ok) return body
  const r = rec(body)
  const wireError = text(r['error'])
  const wireMessage = text(r['message'])
  const code = wireMessage ? wireError || `http_${response.status}` : `http_${response.status}`
  const message = wireMessage ? connectionAdvice(code, wireMessage) : wireError || `The local workflow library returned HTTP ${response.status}.`
  throw new WorkflowLibraryError(response.status, code, message, diagnostics(r['diagnostics']))
}

const json = (method: string, body?: unknown): RequestInit => ({ method, ...(body === undefined ? {} : { body: JSON.stringify(body) }) })
export const idempotencyKey = (prefix = 'ui') => `${prefix}-${globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random().toString(36).slice(2)}`}`.slice(0, 100)

export async function fetchWorkflowLibrary(signal?: AbortSignal): Promise<WorkflowRecord[]> {
  const root = rec(await request('/v1/local/workflow-library', json('GET'), signal))
  return array(root['workflows']).slice(0, 256).map((item) => {
    const r = rec(item)
    return { draft: draft(r['draft']), versions: array(r['versions']).slice(0, 128).map(version) }
  })
}

export async function fetchSkillLibrary(signal?: AbortSignal): Promise<SkillVersion[]> {
  const root = rec(await request('/v1/local/skill-library', json('GET'), signal))
  return array(root['skills']).slice(0, 256).map(skill)
}

export async function createWorkflow(name: string, summary: string, signal?: AbortSignal): Promise<WorkflowDraft> {
  const value = rec(await request('/v1/local/workflow-library', json('POST', { name, summary, idempotency_key: idempotencyKey('workflow') }), signal))
  return draft(value['draft'] ?? value)
}

export async function updateWorkflow(value: WorkflowDraft, signal?: AbortSignal): Promise<WorkflowDraft> {
  const body = { expected_generation: value.generation, name: value.name, summary: value.summary, steps: value.steps, trigger: value.trigger, follow_on: value.follow_on }
  const root = rec(await request(`/v1/local/workflow-library/${encodeURIComponent(value.id)}`, json('PATCH', body), signal))
  return draft(root['draft'] ?? root)
}

export const deleteWorkflow = (value: WorkflowDraft, signal?: AbortSignal) =>
  request(`/v1/local/workflow-library/${encodeURIComponent(value.id)}`, json('DELETE', { expected_generation: value.generation }), signal).then(() => undefined)

export async function archiveWorkflow(value: WorkflowDraft, signal?: AbortSignal): Promise<WorkflowDraft> {
  const root = rec(await request(`/v1/local/workflow-library/${encodeURIComponent(value.id)}/archive`, json('POST', { expected_generation: value.generation }), signal))
  return draft(root['draft'] ?? root)
}

export async function publishWorkflow(value: WorkflowDraft, signal?: AbortSignal): Promise<{ version: WorkflowVersion; diagnostics: Diagnostic[] }> {
  const root = rec(await request(`/v1/local/workflow-library/${encodeURIComponent(value.id)}/versions`, json('POST', { expected_generation: value.generation, idempotency_key: idempotencyKey('version') }), signal))
  return { version: version(root['version']), diagnostics: diagnostics(root['diagnostics']) }
}

export type SkillSource =
  | { kind: 'markdown'; name: string; markdown: string; resources: { path: string; content: string }[] }
  | { kind: 'github'; repository: string; commit: string; path: string; resources: string[] }
  | { kind: 'npx_descriptor'; command: string; commit: string; path?: string; resources: string[] }

export async function previewSkill(source: SkillSource, signal?: AbortSignal): Promise<SkillPreview> {
  const r = rec(await request('/v1/local/skill-imports/preview', json('POST', { source }), signal))
  const preview = rec(r['skill'])
  const p = rec(preview['provenance'])
  const kind = p['kind'] === 'github' || p['kind'] === 'npx_descriptor' ? p['kind'] : 'markdown'
  return {
    preview_id: text(r['preview_id']), digest: text(r['digest']), expires_at: number(r['expires_at']),
    skill: { name: text(preview['name'], 'Unnamed skill'), entrypoint: text(preview['entrypoint']), provenance: { kind, locator: text(p['locator']), ...(text(p['commit']) ? { commit: text(p['commit']) } : {}), ...(text(p['path']) ? { path: text(p['path']) } : {}), ...(text(p['command']) ? { command: text(p['command']) } : {}) } },
    files: array(r['files']).slice(0, 128).map(file), warnings: array(r['warnings']).slice(0, 256).map((item) => diagnostic(typeof item === 'string' ? { severity: 'warning', message: item } : item)), blocked: bool(r['blocked']),
  }
}

export async function acceptSkill(preview: SkillPreview, signal?: AbortSignal): Promise<SkillVersion> {
  const r = rec(await request('/v1/local/skill-imports/accept', json('POST', { preview_id: preview.preview_id, digest: preview.digest, idempotency_key: idempotencyKey('skill') }), signal))
  return skill(r)
}

export async function archiveSkill(value: SkillVersion, signal?: AbortSignal): Promise<SkillVersion> {
  const r = rec(await request(`/v1/local/skills/${encodeURIComponent(value.id)}/archive`, json('POST', { expected_generation: value.generation }), signal))
  return skill(r['skill'] ?? r)
}

export async function simulateWorkflow(workflowVersionId: string, repository?: string, sourceRevision?: string, signal?: AbortSignal): Promise<Simulation> {
  const r = rec(await request('/v1/local/workflow-simulations', json('POST', { workflow_version_id: workflowVersionId, ...(repository ? { repository } : {}), ...(sourceRevision ? { source_revision: sourceRevision } : {}) }), signal))
  const outcome = r['outcome'] === 'passed' || r['outcome'] === 'passed_with_warnings' ? r['outcome'] : 'failed'
  return {
    outcome, steps: array(r['steps']).slice(0, 128).map((item) => { const s = rec(item); const status = s['status'] === 'would_run' || s['status'] === 'would_skip' ? s['status'] : 'blocked'; return { id: text(s['id']), status, requires: strings(s['requires']), detail: text(s['detail']) } }),
    trigger_match: typeof r['trigger_match'] === 'boolean' ? r['trigger_match'] : null, diagnostics: diagnostics(r['diagnostics']), side_effects: 'none', admission: 'not_assessed', deployment: 'not_attempted',
  }
}

export async function fetchGuidance(_repository: string, signal?: AbortSignal): Promise<Guidance> {
  const r = rec(await request('/v1/local/repository-guidance', json('POST'), signal))
  return {
    observed_files: array(r['observed_files']).slice(0, 128).map(file),
    suggestions: array(r['suggestions']).slice(0, 128).map((item) => { const s = rec(item); const kind = s['kind'] === 'build' || s['kind'] === 'test' ? s['kind'] : 'lint'; return { kind, argv: strings(s['argv']), reason: text(s['reason']), sources: strings(s['sources']) } }),
    uncovered: strings(r['uncovered']), diagnostics: diagnostics(r['diagnostics']),
  }
}

export async function fetchAttachments(repository: string, signal?: AbortSignal): Promise<Attachment[]> {
  const value = await request(`/v1/local/repositories/${encodeURIComponent(repository)}/workflow-attachments`, json('GET'), signal)
  return array(value).slice(0, 256).map(attachment)
}

function attachment(v: unknown): Attachment {
  const r = rec(v)
  const state = r['state']
  return {
    id: text(r['id']), repository: text(r['repository']), workflow_version_id: text(r['workflow_version_id']), generation: number(r['generation']), state: state === 'blocked' || state === 'validated' || state === 'export_required' || state === 'active' || state === 'archived' ? state : 'draft',
    ...(r['source_binding'] == null ? {} : { source_binding: r['source_binding'] }),
    ...(r['pipeline_rule'] == null ? {} : { pipeline_rule: r['pipeline_rule'] }),
    diagnostics: diagnostics(r['diagnostics']),
  }
}

export async function putAttachment(repository: string, id: string, workflowVersionId: string, expectedGeneration?: number, signal?: AbortSignal): Promise<Attachment> {
  const body = { ...(expectedGeneration === undefined ? {} : { expected_generation: expectedGeneration }), workflow_version_id: workflowVersionId, params: {}, idempotency_key: idempotencyKey('attachment') }
  const r = rec(await request(`/v1/local/repositories/${encodeURIComponent(repository)}/workflow-attachments/${encodeURIComponent(id)}`, json('PUT', body), signal))
  return attachment(r['attachment'] ?? r)
}

export async function validateAttachment(repository: string, value: Attachment, sourceRevision: string, signal?: AbortSignal): Promise<AttachmentValidation> {
  const r = rec(await request(`/v1/local/repositories/${encodeURIComponent(repository)}/workflow-attachments/${encodeURIComponent(value.id)}/validate`, json('POST', { expected_generation: value.generation, source_revision: sourceRevision }), signal))
  const a = attachment(r['attachment'] ?? r)
  const setup = rec(r['setup'])
  const rawFollowOn = rec(setup['library_follow_on'])
  const when = text(rawFollowOn['when'])
  const followOn: WorkflowFollowOn | null = text(rawFollowOn['workflow_version_id']) && (when === 'success' || when === 'failure' || when === 'always') ? { workflow_version_id: text(rawFollowOn['workflow_version_id']), when, inputs: stringMap(rawFollowOn['inputs']) } : null
  return {
    ...a,
    setup: {
      runnable: bool(setup['runnable']),
      files: array(setup['files']).slice(0, 128).map((item) => { const f = rec(item); return { path: text(f['path']), content: typeof f['content'] === 'string' ? f['content'] : '' } }),
      pipeline_recipe: rec(setup['pipeline_recipe']),
      pipeline_install_template: setup['pipeline_install_template'] === null ? null : rec(setup['pipeline_install_template']),
      library_follow_on: followOn,
      unmet_bindings: strings(setup['unmet_bindings']),
      activation: 'operator_local_cli',
    },
    next_actions: strings(r['next_actions']),
  }
}
