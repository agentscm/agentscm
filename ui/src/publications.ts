// Hub publications (#62): where a published change got to, and what CI and deployments did after it
// landed. Only GETs. Every string comes from the hub or the forge, so it is decoded defensively.
import { ApiError } from './api.ts'
import type { Tone } from './ui.tsx'

export type BranchState = 'pending' | 'created' | 'rejected' | 'unknown'
export type PrState = 'pending' | 'opened' | 'merged' | 'unknown'
export type ExternalState = 'observed' | 'incomplete' | 'unobserved'
export type Outcome = 'pending' | 'running' | 'succeeded' | 'failed' | 'cancelled' | 'superseded' | 'unknown'

export interface ExternalRun {
  runId: number
  attempt: number
  /** CI: the workflow name. Deployments: the deployment's own name. */
  name: string
  /** Deployments only. */
  environment: string | null
  branch: string | null
  outcome: Outcome
  updatedAt: number
  url: string | null
}

export interface External {
  state: ExternalState
  note: string
  mergeCommit: string
  reconciledAt: number | null
  ci: ExternalRun[]
  deployments: ExternalRun[]
}

export interface Publication {
  id: string
  repository: string
  revision: string
  commit: string
  branch: string
  base: string
  nativeLanded: boolean
  branchState: BranchState
  branchMessage: string | null
  pr: { state: PrState; number: number | null; url: string | null; mergeCommit: string | null }
  error: string | null
  historical: boolean
  external: External | null
  /** List items only (unix seconds); `updatedAt` is the staleness hint for merge state. */
  createdAt: number | null
  updatedAt: number | null
}

export const PUBLICATION_ID = /^pub_[0-9a-f]{32}$/
export const validPublicationId = (id: string) => PUBLICATION_ID.test(id)

const OUTCOMES: readonly Outcome[] = ['pending', 'running', 'succeeded', 'failed', 'cancelled', 'superseded', 'unknown']
const rec = (v: unknown): Record<string, unknown> => (v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : {})
const num = (v: unknown, d = 0) => (typeof v === 'number' && Number.isFinite(v) ? v : d)
const numOrNull = (v: unknown) => (typeof v === 'number' && Number.isFinite(v) ? v : null)
// Provider text is display data only: drop control characters and keep it short, as the hub does.
// eslint-disable-next-line no-control-regex
export const clean = (v: unknown, max = 200): string => (typeof v === 'string' ? v.replace(/[\u0000-\u001f\u007f-\u009f]/g, '').slice(0, max) : '')
const cleanOrNull = (v: unknown) => (typeof v === 'string' && clean(v) !== '' ? clean(v) : null)
/** Only https links become clickable; anything else (javascript:, http:) is shown as text. */
export const httpsOnly = (v: unknown): string | null => (typeof v === 'string' && /^https:\/\/[^\s]+$/.test(v) ? v : null)
const pick = <T extends string, F extends string>(v: unknown, allowed: readonly T[], fallback: F): T | F => (typeof v === 'string' && (allowed as readonly string[]).includes(v) ? (v as T) : fallback)

function decodeRun(v: unknown, deployment: boolean): ExternalRun {
  const r = rec(v)
  return {
    runId: num(r['run_id']),
    attempt: num(r['attempt'], 1),
    name: clean(r['name']),
    environment: deployment ? cleanOrNull(r['environment']) : null,
    branch: deployment ? null : cleanOrNull(r['branch']),
    outcome: pick(r['outcome'], OUTCOMES, 'unknown'),
    updatedAt: num(r['provider_updated_at']),
    url: httpsOnly(r['html_url']),
  }
}

function decodeExternal(v: unknown): External | null {
  if (!v || typeof v !== 'object') return null
  const e = rec(v)
  const list = (x: unknown, deployment: boolean) => (Array.isArray(x) ? x.slice(0, 200).map((r) => decodeRun(r, deployment)) : [])
  return {
    // Anything the UI does not recognise is treated as "not checked", never as observed.
    state: pick(e['state'], ['observed', 'incomplete', 'unobserved'] as const, 'unobserved'),
    note: clean(e['note']) || 'Observed after landing; not evidence for the landing decision.',
    mergeCommit: clean(e['merge_commit'], 64),
    reconciledAt: numOrNull(e['reconciled_at']),
    ci: list(e['ci'], false),
    deployments: list(e['deployments'], true),
  }
}

export function decodePublication(v: unknown): Publication {
  const p = rec(v)
  const states = rec(p['states'])
  const branch = rec(states['branch'])
  const pr = rec(states['pull_request'])
  const id = clean(p['id'], 64)
  if (!validPublicationId(id)) throw new Error('The hub returned a publication without a valid id')
  return {
    id,
    repository: clean(p['repository']),
    revision: clean(p['revision'], 128),
    commit: clean(p['commit'], 64),
    branch: clean(p['branch']),
    base: clean(p['base']),
    nativeLanded: states['native_landed'] === true,
    branchState: pick(branch['state'], ['pending', 'created', 'rejected'] as const, 'unknown'),
    branchMessage: cleanOrNull(branch['message']),
    pr: {
      state: pick(pr['state'], ['pending', 'opened', 'merged'] as const, 'unknown'),
      number: numOrNull(pr['number']),
      url: httpsOnly(pr['html_url']),
      mergeCommit: cleanOrNull(pr['merge_commit']),
    },
    error: cleanOrNull(p['error']),
    historical: p['historical'] === true,
    external: decodeExternal(p['external']),
    createdAt: numOrNull(p['created_at']),
    updatedAt: numOrNull(p['updated_at']),
  }
}

const repoPath = (repo: string) => `/v1/hub/repos/${encodeURIComponent(repo)}/publications`

async function get(path: string, token: string, signal: AbortSignal): Promise<unknown> {
  const res = await fetch(path, { signal, cache: 'no-store', credentials: 'omit', headers: { Authorization: `Bearer ${token}` } })
  if (!res.ok) {
    const body = res.status === 403 ? rec(await res.json().catch(() => null)) : {}
    throw new ApiError(res.status, body['error'] === 'read_only' ? 'read_only' : `HTTP ${res.status}`)
  }
  return res.json()
}

/**
 * Hubs before the read-only publication routes refuse a status-page token (403 `read_only`) or have no
 * GET list (404/405). All three mean "this hub can't show landed changes to this page yet".
 */
const unsupported = (e: unknown) => e instanceof ApiError && (e.status === 404 || e.status === 405 || (e.status === 403 && e.message === 'read_only'))

export type PublicationRead = { supported: true; publication: Publication } | { supported: false }

export async function fetchPublication(repo: string, id: string, token: string, signal: AbortSignal): Promise<PublicationRead> {
  if (!validPublicationId(id)) throw new ApiError(400, 'Not a publication id')
  try {
    return { supported: true, publication: decodePublication(await get(`${repoPath(repo)}/${encodeURIComponent(id)}`, token, signal)) }
  } catch (e) {
    // A 404 on one id means that change does not exist here, not that the route is missing.
    if (e instanceof ApiError && e.status === 403 && e.message === 'read_only') return { supported: false }
    throw e
  }
}

export type PublicationList =
  | { supported: true; publications: Publication[]; nextCursor: string | null; complete: boolean }
  | { supported: false }

/**
 * Newest first, `{publications, next_cursor, complete}`. Items have no `external` or `forge_check` (fetch the
 * show reply for those) and add `created_at`/`updated_at`. `next_cursor` is the last item's id.
 */
export async function listPublications(repo: string, token: string, signal: AbortSignal, before?: string): Promise<PublicationList> {
  const q = new URLSearchParams({ limit: '25' })
  if (before) q.set('before', before)
  try {
    const body = rec(await get(`${repoPath(repo)}?${q}`, token, signal))
    const items = Array.isArray(body['publications']) ? body['publications'] : []
    return {
      supported: true,
      // One bad entry must not hide the rest.
      publications: items.slice(0, 100).flatMap((p) => {
        try {
          return [decodePublication(p)]
        } catch {
          return []
        }
      }),
      nextCursor: cleanOrNull(body['next_cursor']),
      complete: body['complete'] === true,
    }
  } catch (e) {
    if (unsupported(e)) return { supported: false }
    throw e
  }
}

// ---------- The five-step track

export interface Step {
  key: 'branch' | 'pr' | 'merged' | 'ci' | 'deploys'
  label: string
  icon: string
  tone: Tone
  word: string
  detail?: string
}

const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? '' : 's'}`

/** CI or deployments after landing, in one honest word: a missing or partial read is never success. */
export function outcomeStep(key: 'ci' | 'deploys', p: Publication): Step {
  const label = key === 'ci' ? 'CI' : 'Deploys'
  const icon = key === 'ci' ? 'gear-wide-connected' : 'cloud-upload'
  if (p.pr.state !== 'merged') return { key, label, icon, tone: 'muted', word: 'After merge', detail: 'Checked only once the change has merged.' }
  const x = p.external
  if (!x) return { key, label, icon, tone: 'muted', word: 'Not tracked', detail: 'This hub is not watching CI and deployments for this repository.' }
  if (x.state === 'unobserved') return { key, label, icon, tone: 'muted', word: 'Not checked yet', detail: 'The hub has not read this merge yet. An empty list does not mean success.' }
  const runs = key === 'ci' ? x.ci : x.deployments
  const failed = runs.filter((r) => r.outcome === 'failed').length
  const active = runs.filter((r) => r.outcome === 'running' || r.outcome === 'pending').length
  const odd = runs.filter((r) => r.outcome === 'cancelled' || r.outcome === 'superseded' || r.outcome === 'unknown').length
  const partly = x.state === 'incomplete'
  if (failed > 0) return { key, label, icon, tone: 'bad', word: 'Failed', detail: `${plural(failed, key === 'ci' ? 'run' : 'deployment')} failed${partly ? '; the hub could not read everything' : ''}.` }
  if (active > 0) return { key, label, icon, tone: 'brand', word: key === 'ci' ? 'Running' : 'Deploying', detail: `${plural(active, key === 'ci' ? 'run' : 'deployment')} still going.` }
  if (partly) return { key, label, icon, tone: 'warn', word: 'Partly checked', detail: 'The hub could not read everything, so there may be more.' }
  if (runs.length === 0) return { key, label, icon, tone: 'muted', word: 'None found', detail: key === 'ci' ? 'GitHub showed no CI runs for this merge.' : 'GitHub showed no deployments for this merge.' }
  if (odd > 0) return { key, label, icon, tone: 'warn', word: 'Mixed', detail: `${plural(odd, 'result')} cancelled, replaced or unclear.` }
  return { key, label, icon, tone: 'ok', word: key === 'ci' ? 'Passed' : 'Deployed', detail: `${plural(runs.length, key === 'ci' ? 'run' : 'deployment')}, all succeeded.` }
}

/** List rows: CI and Deploys stay "Checking…" until that change's own show reply has loaded. */
export function listTrack(p: Publication, detail: Publication | 'failed' | undefined): Step[] {
  if (p.pr.state !== 'merged') return track(p)
  if (detail && detail !== 'failed') return track(detail)
  const [branch, pr, merged] = track(p) as [Step, Step, Step]
  const pending = (key: 'ci' | 'deploys'): Step => ({
    key,
    label: key === 'ci' ? 'CI' : 'Deploys',
    icon: key === 'ci' ? 'gear-wide-connected' : 'cloud-upload',
    tone: 'muted',
    word: detail === 'failed' ? 'Couldn’t check' : 'Checking…',
    detail: detail === 'failed' ? 'This change’s details did not load. Open it to try again.' : 'Loading this change’s CI and deployments.',
  })
  return [branch, pr, merged, pending('ci'), pending('deploys')]
}

export function track(p: Publication): Step[] {
  const rejected = p.branchState === 'rejected'
  const branch: Step =
    p.branchState === 'created'
      ? { key: 'branch', label: 'Branch', icon: 'git', tone: 'ok', word: 'Created' }
      : rejected
        ? { key: 'branch', label: 'Branch', icon: 'git', tone: 'bad', word: 'Rejected', detail: p.branchMessage ?? 'GitHub refused the branch.' }
        : { key: 'branch', label: 'Branch', icon: 'git', tone: 'muted', word: p.branchState === 'pending' ? 'Pending' : 'Unknown' }
  const prWord = p.pr.number !== null ? `#${p.pr.number}` : ''
  const pr: Step =
    rejected
      ? { key: 'pr', label: 'Pull request', icon: 'arrow-left-right', tone: 'muted', word: 'Not started' }
      : p.pr.state === 'opened' || p.pr.state === 'merged'
        ? { key: 'pr', label: 'Pull request', icon: 'arrow-left-right', tone: 'ok', word: `Opened ${prWord}`.trim() }
        : { key: 'pr', label: 'Pull request', icon: 'arrow-left-right', tone: 'muted', word: p.pr.state === 'pending' ? 'Pending' : 'Unknown' }
  const merged: Step =
    p.pr.state === 'merged'
      ? { key: 'merged', label: 'Merged', icon: 'sign-merge-right', tone: 'ok', word: 'Merged' }
      : { key: 'merged', label: 'Merged', icon: 'sign-merge-right', tone: 'muted', word: rejected ? 'Not started' : 'Not yet' }
  return [branch, pr, merged, outcomeStep('ci', p), outcomeStep('deploys', p)]
}

/** How far a change got, in one phrase, for list rows and the answer sentence. */
export function headline(p: Publication): { tone: Tone; text: string } {
  const steps = track(p)
  if (p.branchState === 'rejected') return { tone: 'bad', text: 'Branch rejected' }
  if (p.pr.state !== 'merged') return { tone: 'muted', text: p.pr.state === 'opened' ? 'Waiting to merge' : 'Getting ready' }
  const bad = steps.find((s) => s.tone === 'bad')
  if (bad) return { tone: 'bad', text: `Landed, ${bad.label === 'CI' ? 'CI' : 'a deploy'} failed after` }
  if (steps.some((s) => s.tone === 'brand')) return { tone: 'brand', text: 'Landed, checks still running' }
  if (steps.some((s) => s.tone === 'warn')) return { tone: 'warn', text: 'Landed, only partly checked' }
  if (steps.slice(3).every((s) => s.tone === 'ok')) return { tone: 'ok', text: 'Landed and passed' }
  return { tone: 'muted', text: 'Landed' }
}

// ---------- Recently opened ids (this browser only)

const RECENT_KEY = 'agentscm.hub.recentPublications'
export function loadRecent(): string[] {
  try {
    const v: unknown = JSON.parse(window.localStorage.getItem(RECENT_KEY) ?? '[]')
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string' && validPublicationId(x)).slice(0, 8) : []
  } catch {
    return []
  }
}
export function rememberRecent(id: string): string[] {
  const next = [id, ...loadRecent().filter((x) => x !== id)].slice(0, 8)
  try {
    window.localStorage.setItem(RECENT_KEY, JSON.stringify(next))
  } catch {
    /* not remembered in this browser */
  }
  return next
}
