// Reads for two local-only features: reusable workflow templates (#64) and the workspace snapshot
// timeline (#67). Both live in a developer's local data directory, so they come from `agentscm serve` on
// this machine, not the hub. Shapes mirror the CLI's JSON (`skill-workflow library inspect`,
// `skill-workflow handoff`, `workspace timeline|entry|diff`).
//
// shortcut: the routes below are provisional until the local read API ships; when a route does not
// answer, the adapter returns the fixtures and marks them `preview`, and every screen says so.
import examples from './localExamples.json'
import { clean } from './publications.ts'
import { decodeSkillWorkflowListEnvelope } from './skillWorkflow.ts'
import type { SkillWorkflowListEnvelope } from './types.ts'

export type Source = 'live' | 'preview'
export interface Read<T> {
  source: Source
  value: T
  /** Why preview data is showing, in plain words. */
  reason?: string
}

// ---------- #64 templates

export type TemplateAction =
  | { kind: 'source_paths'; expectations: { path: string; [k: string]: unknown }[] }
  | { kind: 'require_capability'; capability: string }
  | { kind: 'command'; runner: string }
  | { kind: 'claude_code_review'; paths: string[]; model: string }
  | { kind: 'unknown' }

export interface TemplateStep {
  id: string
  needs: string[]
  required: boolean
  action: TemplateAction
}

export interface TemplateInspect {
  reference: { repository: string; revision: string; path: string; sha256: string; name: string; workflow_version: number }
  template: { version: number; name: string; workflow_version: number; skill: { entrypoint: string; resources: string[] }; steps: TemplateStep[] }
  admission: 'not_assessed'
}

export type RunState = 'pending' | 'running' | 'succeeded' | 'completed_with_omissions' | 'failed' | 'blocked' | 'cancelled' | 'unknown'
export interface Handoff {
  run: { run_id: string; state: RunState; source: { repository: string; revision: string }; steps: { id: string; state: string; required: boolean; detail: string | null }[] }
  next_action: string
  subject_scope: 'source_revision_only' | 'source_revision_and_command_contexts' | 'unknown'
  command_context_bindings: { change: string; change_version: number; context_digest: string }[]
  admission: 'not_assessed'
}

// ---------- #67 timeline

export type Trigger = 'manual' | 'turn_end' | 'check_start' | 'mcp_explicit' | 'unknown'
export type TimelineEntry =
  | { kind: 'snapshot'; id: number; trigger: Trigger; recorded_at: number; revision: string; base_revision: string; scope: string[] | null; changed_paths: number; admitted_bytes: number }
  | { kind: 'check_link'; id: number; recorded_at: number; source_revision: string; tested_revision: string; scope: string[] | null; change: string; change_version: number; target: string; run_id: string }

export interface TimelinePage {
  entries: TimelineEntry[]
  next_before: number | null
  coverage: { earliest_recorded_id: number | null; includes_pre_feature_history: boolean }
}

export type EvidenceState = 'not_run' | 'available' | 'binding_mismatch' | 'unavailable'
export interface EntryView {
  entry: TimelineEntry
  evidence: EvidenceState
}

export interface DiffEntry {
  path: string
  change: 'added' | 'removed' | 'changed'
}
export interface DiffPage {
  left: number
  right: number
  entries: DiffEntry[]
  next: string | null
  /** Contents the comparison needed but this computer doesn't hold: those files may be missing here. */
  missing: number
}

// ---------- Decoding (the local server is trusted more than the hub, but its text is still file names
// and agent-supplied notes, so it is cleaned the same way)

const rec = (v: unknown): Record<string, unknown> => (v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : {})
const num = (v: unknown) => (typeof v === 'number' && Number.isFinite(v) ? v : 0)
const strs = (v: unknown, max = 500): string[] => (Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string').slice(0, max).map((x) => clean(x, 500)) : [])
const oneOf = <T extends string>(v: unknown, allowed: readonly T[]): T | 'unknown' => (typeof v === 'string' && (allowed as readonly string[]).includes(v) ? (v as T) : 'unknown')

function decodeAction(v: unknown): TemplateAction {
  const a = rec(v)
  switch (a['kind']) {
    case 'source_paths':
      return { kind: 'source_paths', expectations: (Array.isArray(a['expectations']) ? a['expectations'] : []).slice(0, 200).map((e) => ({ ...rec(e), path: clean(rec(e)['path'], 500) })) }
    case 'require_capability':
      return { kind: 'require_capability', capability: clean(a['capability']) }
    case 'command':
      return { kind: 'command', runner: clean(a['runner']) }
    case 'claude_code_review':
      return { kind: 'claude_code_review', paths: strs(a['paths']), model: clean(a['model']) }
    default:
      return { kind: 'unknown' }
  }
}

export function decodeInspect(v: unknown): TemplateInspect {
  const root = rec(v)
  const ref = rec(root['reference'])
  const t = rec(root['template'])
  const skill = rec(t['skill'])
  return {
    reference: { repository: clean(ref['repository']), revision: clean(ref['revision'], 128), path: clean(ref['path'], 500), sha256: clean(ref['sha256'], 64), name: clean(ref['name']), workflow_version: num(ref['workflow_version']) },
    template: {
      version: num(t['version']),
      name: clean(t['name']),
      workflow_version: num(t['workflow_version']),
      skill: { entrypoint: clean(skill['entrypoint'], 500), resources: strs(skill['resources']) },
      steps: (Array.isArray(t['steps']) ? t['steps'] : []).slice(0, 100).map((s) => {
        const r = rec(s)
        return { id: clean(r['id']), needs: strs(r['needs'], 100), required: r['required'] !== false, action: decodeAction(r['action']) }
      }),
    },
    admission: 'not_assessed',
  }
}

const RUN_STATES = ['pending', 'running', 'succeeded', 'completed_with_omissions', 'failed', 'blocked', 'cancelled'] as const
export function decodeHandoff(v: unknown): Handoff {
  const root = rec(v)
  const run = rec(root['run'])
  const source = rec(run['source'])
  return {
    run: {
      run_id: clean(run['run_id']),
      state: oneOf(run['state'], RUN_STATES),
      source: { repository: clean(source['repository']), revision: clean(source['revision'], 128) },
      steps: (Array.isArray(run['steps']) ? run['steps'] : []).slice(0, 100).map((s) => {
        const r = rec(s)
        return { id: clean(r['id']), state: clean(r['state'], 32), required: r['required'] !== false, detail: typeof r['detail'] === 'string' ? clean(r['detail'], 500) : null }
      }),
    },
    next_action: clean(root['next_action'], 1000),
    subject_scope: oneOf(root['subject_scope'], ['source_revision_only', 'source_revision_and_command_contexts'] as const),
    command_context_bindings: (Array.isArray(root['command_context_bindings']) ? root['command_context_bindings'] : []).slice(0, 50).map((b) => {
      const r = rec(b)
      const change = r['change']
      return { change: typeof change === 'string' ? clean(change) : clean(rec(change)['id']), change_version: num(r['change_version']), context_digest: clean(r['context_digest'], 128) }
    }),
    admission: 'not_assessed',
  }
}

function decodeEntry(v: unknown): TimelineEntry | null {
  const e = rec(v)
  const scope = e['scope'] === undefined || e['scope'] === null ? null : strs(e['scope'])
  if (e['kind'] === 'snapshot')
    return { kind: 'snapshot', id: num(e['id']), trigger: oneOf(e['trigger'], ['manual', 'turn_end', 'check_start', 'mcp_explicit'] as const), recorded_at: num(e['recorded_at']), revision: clean(e['revision'], 128), base_revision: clean(e['base_revision'], 128), scope, changed_paths: num(e['changed_paths']), admitted_bytes: num(e['admitted_bytes']) }
  if (e['kind'] === 'check_link') {
    const change = e['change']
    return { kind: 'check_link', id: num(e['id']), recorded_at: num(e['recorded_at']), source_revision: clean(e['source_revision'], 128), tested_revision: clean(e['tested_revision'], 128), scope, change: typeof change === 'string' ? clean(change) : clean(rec(change)['id']), change_version: num(e['change_version']), target: clean(e['target']), run_id: clean(e['run_id']) }
  }
  return null
}

export function decodeTimeline(v: unknown): TimelinePage {
  const t = rec(rec(v)['timeline'])
  const cov = rec(t['coverage'])
  return {
    entries: (Array.isArray(t['entries']) ? t['entries'] : []).slice(0, 100).flatMap((e) => {
      const d = decodeEntry(e)
      return d ? [d] : []
    }),
    next_before: typeof t['next_before'] === 'number' ? t['next_before'] : null,
    coverage: { earliest_recorded_id: typeof cov['earliest_recorded_id'] === 'number' ? cov['earliest_recorded_id'] : null, includes_pre_feature_history: cov['includes_pre_feature_history'] === true },
  }
}

export function decodeEntryView(v: unknown): EntryView | null {
  const root = rec(v)
  const entry = decodeEntry(root['entry'])
  if (!entry) return null
  const evidence = oneOf(rec(root['evidence'])['state'], ['not_run', 'available', 'binding_mismatch', 'unavailable'] as const)
  return { entry, evidence: evidence === 'unknown' ? 'unavailable' : evidence }
}

function decodeEntryStrict(v: unknown): EntryView {
  const view = decodeEntryView(v)
  if (!view) throw new Error('The local server returned an entry this page cannot read')
  return view
}

export function decodeDiff(v: unknown): DiffPage {
  const root = rec(v)
  const d = rec(root['diff'])
  return {
    left: num(root['left']),
    right: num(root['right']),
    entries: (Array.isArray(d['entries']) ? d['entries'] : []).slice(0, 100).map((e) => {
      const r = rec(e)
      const before = typeof r['before'] === 'string' ? r['before'] : null
      const after = typeof r['after'] === 'string' ? r['after'] : null
      return { path: clean(r['path'], 500), change: before === null ? 'added' : after === null ? 'removed' : 'changed' }
    }),
    next: typeof d['next'] === 'string' && d['next'] !== '' ? clean(d['next'], 200) : null,
    missing: Array.isArray(d['needed_objects']) ? d['needed_objects'].length : 0,
  }
}

// ---------- Where the local API lives

const BASE_KEY = 'agentscm.hub.localApi'
const TOKEN_KEY = 'agentscm.hub.localApiToken'
/** The address of `agentscm serve` on this computer; empty means this page's own origin (the dev proxy). */
export function loadLocalBase(): string {
  try {
    return window.localStorage.getItem(BASE_KEY) ?? ''
  } catch {
    return ''
  }
}
export function saveLocalBase(v: string): void {
  try {
    if (v) window.localStorage.setItem(BASE_KEY, v)
    else window.localStorage.removeItem(BASE_KEY)
  } catch {
    /* not saved in this browser */
  }
}
/** The local service secret (AGENTSCM_SERVE_READ_TOKEN), never the hub token; kept for this tab only. */
export function loadLocalToken(): string {
  try {
    return window.sessionStorage.getItem(TOKEN_KEY) ?? ''
  } catch {
    return ''
  }
}
export function saveLocalToken(v: string): void {
  try {
    if (v) window.sessionStorage.setItem(TOKEN_KEY, v)
    else window.sessionStorage.removeItem(TOKEN_KEY)
  } catch {
    /* not kept */
  }
}
/** Only loopback http(s) addresses are accepted: this must be a server on this computer. */
export const validLocalBase = (v: string) => v === '' || /^https?:\/\/(127\.0\.0\.1|localhost|\[::1\])(:\d{1,5})?$/.test(v)

/** What a person should do about a local server error, in plain words. */
function adviceFor(status: number): string {
  if (status === 404) return 'Not found. For Templates, check the change ID and library path. If Timeline never loads, start agentscm serve with --timeline-workspace.'
  if (status === 400) return 'The local server could not use that value. Check the ID and try again.'
  if (status === 401) return 'The local server needs its read token. Add it in Settings → This computer.'
  if (status === 421) return 'The local read API refused this page’s address; open the hub UI on localhost.'
  if (status === 403) return 'The local server refused this page’s address. Start agentscm serve with --browser-origin for it, or open the page through the dev proxy.'
  if (status === 409) return 'The workspace changed while reading. Try again.'
  if (status === 503) return 'The local server is busy. Try again in a moment.'
  return `The local server had a problem (HTTP ${status}).`
}

/** The local server answered with its own `{"error": …}`: shown as advice, never replaced by sample data. */
export class LocalError extends Error {
  status: number
  code: string
  constructor(status: number, code: string) {
    super(adviceFor(status))
    this.status = status
    this.code = code
  }
}

const BUSY_RETRIES = [150, 400, 900, 1800]

async function read<T>(path: string, decode: (v: unknown) => T, fixture: () => T, signal: AbortSignal): Promise<Read<T>> {
  const base = loadLocalBase()
  const cross = base !== '' && validLocalBase(base)
  const token = cross ? loadLocalToken() : ''
  let res: Response
  // serve handles one read at a time by default and answers 503 when busy; that is a retry, not an error.
  for (let attempt = 0; ; attempt++) {
    try {
      res = await fetch(`${cross ? base : ''}${path}`, { signal, cache: 'no-store', credentials: 'omit', headers: token ? { Authorization: `Bearer ${token}` } : {} })
    } catch (e) {
      if (signal.aborted) throw e
      return { source: 'preview', value: fixture(), reason: `Nothing answered at ${base || 'this page’s address'}.` }
    }
    if (res.status !== 503 || attempt === BUSY_RETRIES.length) break
    await new Promise((resolve) => setTimeout(resolve, BUSY_RETRIES[attempt]))
    if (signal.aborted) throw new DOMException('Aborted', 'AbortError')
  }
  if (res.ok) return { source: 'live', value: decode(await res.json()) }
  // Only a JSON `{"error"}` is the local server speaking; anything else (a proxy page, 502, 405 from an
  // older serve) means the read API isn't there yet.
  const body: unknown = await res.json().catch(() => null)
  const code = body && typeof body === 'object' && typeof (body as Record<string, unknown>)['error'] === 'string' ? clean((body as Record<string, unknown>)['error'], 100) : null
  if (code !== null && res.status !== 405) throw new LocalError(res.status, code)
  return { source: 'preview', value: fixture(), reason: `The local read API isn’t at ${base || 'this page’s address'} yet.` }
}

const q = (o: Record<string, string | number | undefined>) => new URLSearchParams(Object.entries(o).filter((e): e is [string, string | number] => e[1] !== undefined).map(([k, v]) => [k, String(v)])).toString()

// The timeline workspace is pinned when `agentscm serve` starts. `library` is a path inside the Change's own source.
export const inspectTemplate = (change: string, library: string, signal: AbortSignal) =>
  read(`/v1/local/skill-workflow-library?${q({ change, library })}`, decodeInspect, () => decodeInspect(FIXTURE_INSPECT()), signal)
export const fetchHandoff = (runId: string, signal: AbortSignal) =>
  read(`/v1/local/skill-workflows/${encodeURIComponent(runId)}/handoff`, decodeHandoff, () => decodeHandoff(FIXTURE_HANDOFF()), signal)
/** Retained runs on this computer, newest first (16 a page); `q` matches ID, name, revision or state. */
export const fetchLocalRuns = (signal: AbortSignal, query: { before?: string; q?: string } = {}) =>
  read(`/v1/local/skill-workflows?${q({ before: query.before, q: query.q || undefined })}`, decodeSkillWorkflowListEnvelope, () => FIXTURE_RUNS(query.before, query.q), signal)

export interface LocalChanges {
  changes: { id: string; version: number }[]
  next_after: string | null
}
export function decodeLocalChanges(v: unknown): LocalChanges {
  const root = rec(v)
  return {
    changes: (Array.isArray(root['changes']) ? root['changes'] : []).slice(0, 100).flatMap((c) => {
      const r = rec(c)
      const id = clean(r['id'], 200)
      return id ? [{ id, version: num(r['version']) }] : []
    }),
    next_after: typeof root['next_after'] === 'string' && root['next_after'] !== '' ? clean(root['next_after'], 200) : null,
  }
}
export const fetchLocalChanges = (signal: AbortSignal, after?: string) =>
  read(`/v1/local/changes?${q({ limit: 100, after })}`, decodeLocalChanges, () => FIXTURE_CHANGES(after), signal)

export const fetchTimeline = (signal: AbortSignal, before?: number) =>
  read(`/v1/local/workspace/timeline?${q({ limit: 25, before })}`, decodeTimeline, () => decodeTimeline(FIXTURE_TIMELINE(before)), signal)
export const fetchEntry = (id: number, signal: AbortSignal) =>
  read(`/v1/local/workspace/timeline/${id}`, decodeEntryStrict, () => decodeEntryStrict(FIXTURE_ENTRY(id)), signal)
export const fetchDiff = (left: number, right: number, signal: AbortSignal, after?: string) =>
  read(`/v1/local/workspace/diff?${q({ left, right, limit: 100, after })}`, decodeDiff, () => decodeDiff(FIXTURE_DIFF(left, right)), signal)

/** One cheap read for Settings: is the local server there, and is the timeline turned on? */
export async function localStatus(signal: AbortSignal): Promise<'live' | 'preview' | 'off' | 'refused' | 'error'> {
  try {
    return (await fetchTimeline(signal)).source
  } catch (e) {
    if (signal.aborted) throw e
    if (e instanceof LocalError) return e.status === 404 ? 'off' : e.status === 401 || e.status === 403 || e.status === 421 ? 'refused' : 'error'
    return 'error'
  }
}

// ---------- Preview data: real replies from a seeded local store (the API session's examples), verbatim

const all = examples as Record<string, { request: string; response: Record<string, unknown> }>
const reply = (name: string) => structuredClone(all[name]!.response)
type Timeline = { timeline: { entries: Record<string, unknown>[] } }

export const FIXTURE_INSPECT = () => reply('inspect')
export const FIXTURE_HANDOFF = () => reply('handoff')
/** Two real pages: the first stops at `next_before: 2`, the second holds the oldest entry. */
export const FIXTURE_TIMELINE = (before?: number) => reply(before === undefined ? 'timeline' : 'timelineOlder')
/** Entry 3 is the real check-link reply; a snapshot's evidence is always `not_run`. */
export function FIXTURE_ENTRY(id: number) {
  if (id === 3) return reply('entry3')
  const entry = [...(reply('timeline') as Timeline).timeline.entries, ...(reply('timelineOlder') as Timeline).timeline.entries].find((e) => e['id'] === id)
  return { version: 1, entry, evidence: { state: 'not_run' } }
}
export const FIXTURE_DIFF = (left: number, right: number) => ({ ...reply('diff'), left, right })

const FIXTURE_STATES = ['succeeded', 'failed', 'running', 'blocked', 'completed_with_omissions', 'cancelled', 'pending'] as const
const FIXTURE_NAMES = ['Check the change', 'Review the change', 'Release readiness']
const FIXTURE_RUN_LIST = Array.from({ length: 40 }, (_, i) => ({
  run_id: `preview-run-${String(40 - i).padStart(3, '0')}`,
  revision: (i % 16).toString(16).repeat(64),
  state: FIXTURE_STATES[i % FIXTURE_STATES.length]!,
  name: FIXTURE_NAMES[i % FIXTURE_NAMES.length]!,
  sequence: String(900 - i),
}))
/** 16 a page like the real list; with `q`, a case-insensitive match on ID, name, revision or state. */
export function FIXTURE_RUNS(before?: string, query?: string): SkillWorkflowListEnvelope {
  const needle = query?.trim().toLowerCase()
  const rows = FIXTURE_RUN_LIST.filter((r) => (before === undefined || Number(r.sequence) < Number(before)) && (!needle || [r.run_id, r.name, r.revision, r.state].some((v) => v.toLowerCase().includes(needle))))
  const runs = rows.slice(0, 16)
  return { version: 1, availability: 'available', runs, next_cursor: rows.length > runs.length ? runs.at(-1)!.sequence : null }
}
const FIXTURE_CHANGE_LIST = Array.from({ length: 130 }, (_, i) => ({ id: `change-preview-${String(i + 1).padStart(3, '0')}`, version: (i % 5) + 1 }))
export function FIXTURE_CHANGES(after?: string): LocalChanges {
  const start = after ? FIXTURE_CHANGE_LIST.findIndex((c) => c.id === after) + 1 : 0
  const changes = FIXTURE_CHANGE_LIST.slice(start, start + 100)
  return { changes, next_after: start + 100 < FIXTURE_CHANGE_LIST.length ? changes.at(-1)!.id : null }
}
