// Supported skills (read-only): library, runs list and one run. Shapes follow the hub's skills
// contract; every string comes from agents or a repository, so decoding is defensive and the page only GETs.
import type { Step } from './publications.ts'
import type { Tone } from './ui.tsx'
import { withTimeout } from './timeout.ts'

export type GateState = 'done' | 'current' | 'not_reached'
export type AdoptionSource = 'default_branch' | 'reported_by_checkout' | 'none'
export interface Evidence { id: string; kind: string; head_sha: string; assurance: string; summary: string; url: string | null; session: string; agent: string; recorded_at: number }
export interface Gate { id: string; state: GateState; by: string; enforce: string[]; min_assurance: string; assurance: string | null; evidence: Evidence[] }
export interface RunView {
  run_id: string; repo: string; branch: string; pull_request: number | null; head_sha: string; state: string
  skill: { name: string; workflow_version: number; sha256: string }
  current_gate: string | null; next_action: string; blocks_finish: boolean
  gates: Gate[]; stale_evidence: Evidence[]; updated_at: number
  params: Record<string, unknown>
}
export interface Param { name: string; description: string; type: string; default: unknown; enum: string[] }
export interface CatalogEntry {
  name: string; workflow_version: number; sha256: string; summary: string; gates: { id: string; enforce: string[] }[]; first_party: boolean
  applicability: { triggers: string[]; prerequisites: string[]; not_for: string[] }; parameters: Param[]
  /** One plain-English line per gate, numbered by the hub ("1. Branch: ..."). */
  walkthrough: string[]
  /** The agent-facing procedure (SKILL.md); empty when an older hub does not send it. */
  procedure: string
}
export type PinStatus = 'adopted' | 'unknown_version' | 'unsupported_source' | 'invalid_params'
export interface Pin { name: string; workflow_version: number; path: string | null; sha256: string; status: PinStatus; params: Record<string, unknown>; source: string; walkthrough: string[] }
export interface Library {
  catalog: CatalogEntry[]
  adoption: { repo: string; source: AdoptionSource; observed_at: number | null; default_branch: string | null; pins: Pin[] }
  counts: Record<string, { open: number; at_stop_gate: number; completed: number }>
}

// ---------- routes

export type SkillsRoute = { kind: 'library' } | { kind: 'runs' } | { kind: 'run'; runId: string }
const RUN_ID = /^[A-Za-z0-9_.:-]{1,128}$/
export const validRunId = (id: string) => RUN_ID.test(id)
export const skillsHref = '#skills'
export const skillRunsHref = '#skills/runs'
export const skillRunHref = (id: string) => `#skills/runs/${encodeURIComponent(id)}`

/** `#skills` is the library, `#skills/runs` the list, `#skills/runs/<run_id>` one run. A malformed id is the list. */
export function skillsRoute(hash = window.location.hash): SkillsRoute {
  const parts = hash.split('/')
  if (parts[0] !== '#skills' || parts[1] !== 'runs') return { kind: 'library' }
  if (parts.length !== 3) return { kind: 'runs' }
  try {
    const runId = decodeURIComponent(parts[2]!)
    return validRunId(runId) ? { kind: 'run', runId } : { kind: 'runs' }
  } catch {
    return { kind: 'runs' }
  }
}

// ---------- labels

const GATE_LABEL: Record<string, string> = { branch: 'Branch', tests: 'Tests', review: 'Review', fix: 'Fix', ci: 'CI', approval: 'Approval', merge: 'Merge', post_merge: 'Post-merge check' }
export const gateLabel = (id: string) => GATE_LABEL[id] ?? id
const WAITING = new Set(['ci', 'approval', 'merge', 'post_merge'])
/** "At review" when an agent has to act, "Waiting on ci" when the forge or a maintainer does. */
export function runStateLabel(run: Pick<RunView, 'state' | 'current_gate'>): string {
  if (run.state === 'completed') return 'Completed'
  if (run.state === 'abandoned') return 'Abandoned'
  if (!run.current_gate) return 'Open'
  return `${WAITING.has(run.current_gate) ? 'Waiting on' : 'At'} ${run.current_gate === 'post_merge' ? 'post-merge check' : run.current_gate}`
}
export const runStateTone = (run: Pick<RunView, 'state'>): Tone => (run.state === 'completed' ? 'ok' : run.state === 'abandoned' ? 'muted' : 'brand')

/** The rule that keeps self-reported evidence from looking like a receipt: warn tone, its own icon, and the words. */
export const isSelfReported = (assurance: string | null) => assurance === 'self_reported'
export function assuranceView(assurance: string | null): { label: string; tone: Tone; icon: string; tip: string } | null {
  if (assurance === null) return null
  if (assurance === 'self_reported') return { label: 'Self-reported', tone: 'warn', icon: 'person-check', tip: 'An agent recorded this itself. The hub did not check it.' }
  if (assurance === 'observed') return { label: 'Observed', tone: 'brand', icon: 'eye', tip: 'The hub observed this on GitHub or GitLab.' }
  if (assurance === 'receipt') return { label: 'Receipt', tone: 'ok', icon: 'patch-check', tip: 'A signed receipt backs this evidence.' }
  return { label: `Unrecognised: ${assurance.slice(0, 40)}`, tone: 'muted', icon: 'question-circle', tip: 'This hub does not know this assurance level.' }
}
export const GATE_STATE_VIEW: Record<GateState, { label: string; tone: Tone; icon: string }> = {
  done: { label: 'Done', tone: 'ok', icon: 'check-circle' },
  current: { label: 'Current', tone: 'brand', icon: 'record-circle' },
  not_reached: { label: 'Not reached', tone: 'muted', icon: 'circle' },
}

/** The run's gate track: a done gate on self-reported evidence says so in the step itself. */
export function gateSteps(gates: Gate[]): Step[] {
  return gates.map((g) => {
    const label = gateLabel(g.id)
    if (g.state === 'done') {
      const self = isSelfReported(g.assurance)
      return { key: g.id, label, icon: self ? 'person-check' : 'check-circle', tone: self ? 'warn' : 'ok', word: self ? 'Self-reported' : 'Done', detail: self ? 'Done on evidence an agent recorded itself. The hub did not check it.' : 'Done.' }
    }
    if (g.state === 'current') return { key: g.id, label, icon: 'record-circle', tone: 'brand', word: 'Current', detail: 'The next gate without evidence.' }
    return { key: g.id, label, icon: 'circle', tone: 'muted', word: 'Not reached' }
  })
}

export const selfReportedCount = (run: Pick<RunView, 'gates'>) => run.gates.filter((g) => g.state === 'done' && isSelfReported(g.assurance)).length

export function adoptionLabel(source: AdoptionSource, defaultBranch: string | null): string {
  if (source === 'default_branch') return `Adoption: read from ${defaultBranch ?? 'the default branch'} on GitHub or GitLab`
  if (source === 'reported_by_checkout') return 'Adoption: reported by checkout'
  return 'Not adopted'
}
const ENFORCED: Record<string, string> = { stop: 'Stop hook' }
const PLANNED: Record<string, string> = { forge_status: 'merge check', native: 'native approval and landing' }
/** Only the Stop hook is built (and it is opt-in); forge_status and native are planned; advise and signal are neither. */
export function enforcement(gates: { enforce: string[] }[]): { enforced: string[]; planned: string[] } {
  const all = new Set(gates.flatMap((g) => g.enforce))
  return { enforced: Object.keys(ENFORCED).filter((k) => all.has(k)).map((k) => ENFORCED[k]!), planned: Object.keys(PLANNED).filter((k) => all.has(k)).map((k) => PLANNED[k]!) }
}
/** Steps (gates) across the adopted skills, and how many can stop an agent finishing: only those the opt-in Stop hook covers. */
export function stepCounts(lib: Library): { steps: number; canStop: number } {
  const gates = lib.adoption.pins
    .filter((p) => p.status === 'adopted')
    .flatMap((p) => lib.catalog.find((c) => c.name === p.name && c.sha256 === p.sha256)?.gates ?? [])
  return { steps: gates.length, canStop: gates.filter((g) => g.enforce.includes('stop')).length }
}
/** The adopted skills by name, and their open runs and runs at a stop gate, from the counts the hub already sends. */
export function adoptedSummary(lib: Library): { names: string[]; open: number; atStop: number } {
  const names = [...new Set(lib.adoption.pins.filter((p) => p.status === 'adopted').map((p) => p.name))]
  const sum = (k: 'open' | 'at_stop_gate') => names.reduce((t, n) => t + (lib.counts[n]?.[k] ?? 0), 0)
  return { names, open: sum('open'), atStop: sum('at_stop_gate') }
}
/** The hub numbers its lines; the page numbers them with <ol>. */
export const stepText = (line: string) => line.replace(/^\d+\.\s+/, '')

export type StepEnforce = 'block' | 'warn' | 'record'
export interface StepView { name: string; plain: string; who: string; needs: string; enforce: StepEnforce; detail: string }
/** One hub walkthrough line ("N. Name: plain. Done by X. Needs: Y. Blocks…|Warns…|Recorded…") split into its parts; null when the hub wrote it another way. */
export function parseStep(line: string): StepView | null {
  const m = /^(?:\d+\.\s+)?([^:]+):\s+(.+?\.)\s+Done by (.+?)\.\s+Needs: (.+?)\.\s*((?:Blocks|Warns|Recorded).*)$/.exec(line)
  if (!m) return null
  const [, name, plain, by, needs, rest] = m as unknown as [string, string, string, string, string, string]
  const enforce: StepEnforce = rest.startsWith('Blocks') ? 'block' : rest.startsWith('Warns') ? 'warn' : 'record'
  const who = by.replace(/^(the|a)\s+/i, '')
  return { name, plain, who: who.charAt(0).toUpperCase() + who.slice(1), needs, enforce, detail: rest.replace(/^[^:;]*[:;]\s*/, '').replace(/\.$/, '') }
}
export const shortSha = (s: string) => s.slice(0, 7)
const SAFE_ARG = /^[A-Za-z0-9_.:/@+=-]+$/
const shellArg = (v: string) => (SAFE_ARG.test(v) ? v : `'${v.replaceAll("'", `'\\''`)}'`)
const paramValue = (v: unknown) => (typeof v === 'string' ? v : JSON.stringify(v))
/** The exact command that adopts a skill (non-default params only). The page itself never writes. */
export function addCommand(name: string, params: Record<string, unknown> = {}, defaults: Param[] = []): string {
  const flags = Object.entries(params)
    .filter(([k, v]) => JSON.stringify(defaults.find((d) => d.name === k)?.default ?? null) !== JSON.stringify(v))
    .map(([k, v]) => `--param ${shellArg(`${k}=${paramValue(v)}`)}`)
  return ['agentscm skill add', shellArg(name), ...flags].join(' ')
}
export const removeCommand = (name: string) => `agentscm skill remove ${shellArg(name)}`
export const paramText = (params: Record<string, unknown>) => Object.entries(params).map(([k, v]) => `${k}=${v === null ? 'default' : paramValue(v)}`).join(', ')

export function runMatches(run: RunView, query: string): boolean {
  const q = query.trim().toLowerCase()
  if (!q) return true
  return [run.branch, run.pull_request === null ? '' : `#${run.pull_request} ${run.pull_request}`, run.head_sha, run.next_action, runStateLabel(run)].some((v) => v.toLowerCase().includes(q))
}
export const lastEvidence = (run: RunView): Evidence | null =>
  run.gates.flatMap((g) => g.evidence).reduce<Evidence | null>((a, e) => (!a || e.recorded_at > a.recorded_at ? e : a), null)

// ---------- decoding

const bad = (what: string): never => {
  throw new Error(`The hub sent an unexpected ${what} reply.`)
}
const rec = (v: unknown, what: string): Record<string, unknown> => (typeof v === 'object' && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : bad(what))
const str = (v: unknown, what: string) => (typeof v === 'string' ? v : bad(what))
const num = (v: unknown, what: string) => (typeof v === 'number' && Number.isFinite(v) ? v : bad(what))
const list = (v: unknown, what: string): unknown[] => (Array.isArray(v) ? v : bad(what))
const nullable = <T>(v: unknown, f: (x: unknown) => T): T | null => (v === null || v === undefined ? null : f(v))

function evidence(v: unknown): Evidence {
  const o = rec(v, 'evidence')
  return { id: str(o['id'], 'evidence'), kind: str(o['kind'], 'evidence'), head_sha: str(o['head_sha'], 'evidence'), assurance: str(o['assurance'], 'evidence'), summary: str(o['summary'], 'evidence'), url: nullable(o['url'], (x) => str(x, 'evidence')), session: typeof o['session'] === 'string' ? o['session'] : '', agent: typeof o['agent'] === 'string' ? o['agent'] : '', recorded_at: num(o['recorded_at'], 'evidence') }
}
function gate(v: unknown): Gate {
  const o = rec(v, 'gate')
  const state = o['state']
  if (state !== 'done' && state !== 'current' && state !== 'not_reached') bad('gate state')
  return { id: str(o['id'], 'gate'), state: state as GateState, by: str(o['by'], 'gate'), enforce: list(o['enforce'], 'gate').map((x) => str(x, 'gate')), min_assurance: str(o['min_assurance'], 'gate'), assurance: nullable(o['assurance'], (x) => str(x, 'gate')), evidence: list(o['evidence'], 'gate').map(evidence) }
}
export function decodeRun(v: unknown): RunView {
  const o = rec(v, 'run')
  const s = rec(o['skill'], 'run')
  return {
    run_id: str(o['run_id'], 'run'), repo: str(o['repo'], 'run'), branch: str(o['branch'], 'run'), pull_request: nullable(o['pull_request'], (x) => num(x, 'run')), head_sha: str(o['head_sha'], 'run'), state: str(o['state'], 'run'),
    skill: { name: str(s['name'], 'run'), workflow_version: num(s['workflow_version'], 'run'), sha256: str(s['sha256'], 'run') },
    current_gate: nullable(o['current_gate'], (x) => str(x, 'run')), next_action: str(o['next_action'], 'run'), blocks_finish: o['blocks_finish'] === true,
    gates: list(o['gates'], 'run').map(gate), stale_evidence: list(o['stale_evidence'] ?? [], 'run').map(evidence), updated_at: num(o['updated_at'], 'run'),
    params: rec(o['params'] ?? {}, 'run'),
  }
}
export function decodeRuns(v: unknown): { runs: RunView[]; truncated: boolean } {
  const o = rec(v, 'runs')
  return { runs: list(o['runs'], 'runs').map(decodeRun), truncated: o['truncated'] === true }
}
export function decodeLibrary(v: unknown): Library {
  const o = rec(v, 'skills')
  const a = rec(o['adoption'], 'skills')
  const source = a['source']
  if (source !== 'default_branch' && source !== 'reported_by_checkout' && source !== 'none') bad('adoption source')
  const counts: Library['counts'] = {}
  for (const [k, c] of Object.entries(rec(o['counts'] ?? {}, 'skills'))) {
    const r = rec(c, 'skills')
    counts[k] = { open: num(r['open'], 'skills'), at_stop_gate: num(r['at_stop_gate'], 'skills'), completed: num(r['completed'], 'skills') }
  }
  return {
    catalog: list(o['catalog'], 'skills').map((c) => {
      const r = rec(c, 'skills')
      // The contract lists gate ids; an object form {id, enforce} lets the hub add enforcement without a UI change.
      const gates = list(r['gates'], 'skills').map((g) => (typeof g === 'string' ? { id: g, enforce: [] } : { id: str(rec(g, 'skills')['id'], 'skills'), enforce: list(rec(g, 'skills')['enforce'] ?? [], 'skills').map((e) => str(e, 'skills')) }))
      const ap = rec(r['applicability'] ?? {}, 'skills')
      const strs = (x: unknown) => list(x ?? [], 'skills').map((e) => str(e, 'skills'))
      const parameters = list(r['parameters'] ?? [], 'skills').map((x): Param => {
        const q = rec(x, 'skills')
        return { name: str(q['name'], 'skills'), description: typeof q['description'] === 'string' ? q['description'] : '', type: str(q['type'], 'skills'), default: q['default'] ?? null, enum: strs(q['enum']) }
      })
      return { name: str(r['name'], 'skills'), workflow_version: num(r['workflow_version'], 'skills'), sha256: str(r['sha256'], 'skills'), summary: str(r['summary'], 'skills'), gates, first_party: r['first_party'] === true, applicability: { triggers: strs(ap['triggers']), prerequisites: strs(ap['prerequisites']), not_for: strs(ap['not_for']) }, parameters, walkthrough: strs(r['walkthrough']), procedure: typeof r['procedure'] === 'string' ? r['procedure'] : '' }
    }),
    adoption: {
      repo: str(a['repo'], 'skills'), source: source as AdoptionSource, observed_at: nullable(a['observed_at'], (x) => num(x, 'skills')), default_branch: nullable(a['default_branch'], (x) => str(x, 'skills')),
      pins: list(a['pins'] ?? [], 'skills').map((p) => {
        const r = rec(p, 'skills')
        const status = r['status']
        if (status !== 'adopted' && status !== 'unknown_version' && status !== 'unsupported_source' && status !== 'invalid_params') bad('pin status')
        const src = r['source']
        return { name: str(r['name'], 'skills'), workflow_version: num(r['workflow_version'], 'skills'), path: nullable(r['path'], (x) => str(x, 'skills')), sha256: str(r['sha256'], 'skills'), status: status as PinStatus, params: rec(r['params'] ?? {}, 'skills'), walkthrough: list(r['walkthrough'] ?? [], 'skills').map((e) => str(e, 'skills')), source: typeof src === 'object' && src !== null && typeof (src as { kind?: unknown }).kind === 'string' ? (src as { kind: string }).kind : 'first_party' }
      }),
    },
    counts,
  }
}

// ---------- fetching

const MAX_BYTES = 2 * 1024 * 1024
async function getJson(path: string, token: string, signal: AbortSignal): Promise<unknown> {
  const res = await fetch(path, { signal: withTimeout(signal), cache: 'no-store', credentials: 'omit', headers: { Authorization: `Bearer ${token}` } })
  if (!res.ok) throw new Error(res.status === 404 ? 'Not found' : `HTTP ${res.status}`)
  const text = await res.text()
  if (text.length > MAX_BYTES) throw new Error('The hub reply was too large to show.')
  try {
    return JSON.parse(text) as unknown
  } catch {
    throw new Error('The hub reply was not valid JSON.')
  }
}
const base = (repo: string) => `/v1/hub/repos/${encodeURIComponent(repo)}/skills`
export const fetchLibrary = async (repo: string, token: string, signal: AbortSignal) => decodeLibrary(await getJson(`/v1/hub/skills?repo=${encodeURIComponent(repo)}`, token, signal))
export const fetchRuns = async (repo: string, token: string, signal: AbortSignal) => decodeRuns(await getJson(`${base(repo)}/runs?state=all&limit=50`, token, signal))
export const fetchRun = async (repo: string, runId: string, token: string, signal: AbortSignal) => decodeRun(await getJson(`${base(repo)}/runs/${encodeURIComponent(runId)}`, token, signal))
