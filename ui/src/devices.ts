// Computers signed in to this Hub, and the admin lists. Two ways in:
// - the local page (`agentscm open`): its local service forwards GET /v1/hub/devices and
//   DELETE /v1/hub/devices/{id} with this computer's credential, behind the page's own grant;
// - the hosted Hub: a browser session (HttpOnly cookie, CSRF on writes) from GET /v1/hub/session.
// Admin routes take only the browser session; a computer credential is never accepted there.
// A read-only repository token can read none of this.
import { ApiError } from './api.ts'
import { safeHubOrigin } from './connectAgent.ts'
import { isLocalCredential } from './frontDoor.ts'
import { clean } from './publications.ts'
import { refusalFrom } from './refusal.ts'
import { withTimeout } from './timeout.ts'

export interface Device {
  id: string
  name: string
  createdAt: number
  lastUsedAt: number | null
  revokedAt: number | null
  /** This very computer (only the local page can know). */
  current: boolean
  repositories: string[]
  owner: { login: string; provider: string } | null
}
export interface AdminRepository { fullName: string; provider: string; host: string; devices: number; agentTokens: number }
export interface SignInRefusal { at: number; count: number; provider: string; login: string; reason: string; via: 'sign_in' | 'device_mint' | 'web_read_token' }
export interface BrowserSession { login: string; /** The forge profile name, when the person has one. */ name?: string; provider: string; admin: boolean; csrf: string }

/** How this page may reach the computer routes, or null when it can't. */
export type DeviceAuth = { kind: 'local'; token: string } | { kind: 'browser'; session: BrowserSession }

const rec = (v: unknown): Record<string, unknown> => (v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : {})
const arr = (v: unknown): unknown[] => (Array.isArray(v) ? v : [])
const text = (v: unknown, max = 100): string | null => clean(v, max).trim() || null
const unix = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) && v > 0 ? Math.floor(v) : null)
const count = (v: unknown): number => (typeof v === 'number' && Number.isFinite(v) && v >= 0 ? Math.floor(v) : 0)
const DEVICE_ID = /^dev_[0-9a-f]{32}$/
const REPO = /^[A-Za-z0-9._-]+\/[A-Za-z0-9._/-]+$/
const LOGIN = /^[A-Za-z0-9._-]{1,100}$/
const PROVIDER = /^(github|gitlab)$/

export function decodeDevice(raw: unknown): Device | null {
  const d = rec(raw)
  const id = typeof d['id'] === 'string' && DEVICE_ID.test(d['id']) ? d['id'] : null
  const createdAt = unix(d['created_at'])
  if (!id || createdAt === null) return null
  const o = rec(d['owner'])
  const login = typeof o['login'] === 'string' && LOGIN.test(o['login']) ? o['login'] : null
  return {
    id,
    name: text(d['name']) ?? 'Unnamed computer',
    createdAt,
    lastUsedAt: unix(d['last_used_at']),
    revokedAt: unix(d['revoked_at']),
    current: d['current'] === true,
    repositories: arr(d['repositories']).filter((r): r is string => typeof r === 'string' && REPO.test(r)).slice(0, 100),
    owner: login ? { login, provider: typeof o['provider'] === 'string' && PROVIDER.test(o['provider']) ? o['provider'] : 'github' } : null,
  }
}

export function decodeAdminRepository(raw: unknown): AdminRepository | null {
  const r = rec(raw)
  const fullName = typeof r['full_name'] === 'string' && REPO.test(r['full_name']) ? r['full_name'] : null
  if (!fullName) return null
  return { fullName, provider: typeof r['provider'] === 'string' && PROVIDER.test(r['provider']) ? r['provider'] : 'github', host: text(r['host'], 200) ?? '', devices: count(r['devices']), agentTokens: count(r['agent_tokens']) }
}

export function decodeRefusal(raw: unknown): SignInRefusal | null {
  const r = rec(raw)
  const at = unix(r['at'])
  const login = typeof r['login'] === 'string' && LOGIN.test(r['login']) ? r['login'] : null
  const reason = typeof r['reason'] === 'string' && /^[a-z_]{1,40}$/.test(r['reason']) ? r['reason'] : null
  if (at === null || !login || !reason) return null
  return { at, count: Math.max(1, count(r['count'])), provider: typeof r['provider'] === 'string' && PROVIDER.test(r['provider']) ? r['provider'] : 'github', login, reason, via: r['via'] === 'device_mint' || r['via'] === 'web_read_token' ? r['via'] : 'sign_in' }
}

async function request(path: string, auth: DeviceAuth | null, signal: AbortSignal | undefined, method = 'GET', body?: unknown): Promise<Response> {
  const headers: Record<string, string> = {}
  if (auth?.kind === 'local') headers['Authorization'] = `Bearer ${auth.token}`
  if (auth?.kind === 'browser' && method !== 'GET') headers['x-csrf-token'] = auth.session.csrf
  if (body !== undefined) headers['Content-Type'] = 'application/json'
  const res = await fetch(path, {
    method,
    signal: method === 'GET' ? withTimeout(signal) : signal,
    cache: 'no-store',
    // The browser session is an HttpOnly cookie; the local page's grant is a header and sends no cookies.
    credentials: auth?.kind === 'browser' || !auth ? 'same-origin' : 'omit',
    headers,
    ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
  })
  if (!res.ok && res.status !== 204) {
    let body: unknown = null
    try {
      body = await res.json()
    } catch {
      /* the status still says enough */
    }
    throw new ApiError(res.status, `HTTP ${res.status}`, refusalFrom(body))
  }
  return res
}

/** The browser's own Hub session, or null when signed out or the Hub has none to offer. */
export async function fetchBrowserSession(signal?: AbortSignal): Promise<BrowserSession | null> {
  let res: Response
  try {
    res = await request('/v1/hub/session', null, signal)
  } catch (e) {
    if (e instanceof ApiError && [401, 403, 404, 405].includes(e.status)) return null
    throw e
  }
  const j = rec(await res.json().catch(() => null))
  const u = rec(j['user'])
  const login = typeof u['login'] === 'string' && LOGIN.test(u['login']) ? u['login'] : null
  const csrf = typeof j['csrf_token'] === 'string' && /^[A-Za-z0-9_-]{16,200}$/.test(j['csrf_token']) ? j['csrf_token'] : null
  const name = typeof u['name'] === 'string' && u['name'].trim() && u['name'].length <= 100 ? u['name'].trim() : undefined
  return login && csrf ? { login, ...(name ? { name } : {}), provider: typeof u['provider'] === 'string' && PROVIDER.test(u['provider']) ? u['provider'] : 'github', admin: j['admin'] === true, csrf } : null
}

/** How this page reaches the computer routes: the local page's grant, else a browser session. */
export async function deviceAuth(token: string, signal?: AbortSignal): Promise<DeviceAuth | null> {
  if (isLocalCredential(token)) return { kind: 'local', token }
  const session = await fetchBrowserSession(signal)
  return session ? { kind: 'browser', session } : null
}

const list = <T>(raw: unknown, key: string, decode: (v: unknown) => T | null, max: number): T[] =>
  arr(rec(raw)[key])
    .slice(0, max)
    .map(decode)
    .filter((v): v is T => v !== null)

/** Your computers, and on the local page the Hub they were read from (one Hub per call). */
export async function fetchDevices(auth: DeviceAuth, signal?: AbortSignal): Promise<{ devices: Device[]; hub: string | null }> {
  const raw: unknown = await (await request('/v1/hub/devices', auth, signal)).json()
  const hub = rec(raw)['hub']
  return { devices: list(raw, 'devices', decodeDevice, 100), hub: typeof hub === 'string' ? safeHubOrigin(hub) : null }
}

/** Signs this browser out of the Hub; its computers stay signed in. */
export async function signOutBrowser(session: BrowserSession): Promise<void> {
  try {
    await request('/v1/hub/session', { kind: 'browser', session }, undefined, 'DELETE')
  } catch (e) {
    // Already signed out is the outcome we wanted.
    if (!(e instanceof ApiError && e.status === 401)) throw e
  }
}

/** `revoked`, or `gone` when the Hub no longer knows it (already removed, or not yours). */
export async function revokeDevice(auth: DeviceAuth, id: string): Promise<'revoked' | 'gone'> {
  if (!DEVICE_ID.test(id)) throw new Error('not a computer id')
  try {
    await request(`/v1/hub/devices/${id}`, auth, undefined, 'DELETE')
    return 'revoked'
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return 'gone'
    throw e
  }
}

export async function fetchAdmin(auth: DeviceAuth & { kind: 'browser' }, signal?: AbortSignal) {
  const [repos, refusals, devices] = await Promise.all([
    request('/v1/hub/admin/repositories', auth, signal).then((r) => r.json()),
    request('/v1/hub/admin/sign-in-refusals', auth, signal).then((r) => r.json()),
    request('/v1/hub/admin/devices', auth, signal).then((r) => r.json()),
  ])
  return { repositories: list(repos, 'repositories', decodeAdminRepository, 500), refusals: list(refusals, 'refusals', decodeRefusal, 200), devices: list(devices, 'devices', decodeDevice, 500) }
}

/** A read-only token the Hub hands its own pages for one repository. Kept in memory only, never stored. */
export interface WebReadToken { repository: string; token: string; expiresAt: number }

/** Asks the Hub for a 30-minute read-only token for `repository` on this browser's sign-in. Each call stores a new token on the Hub. */
export async function mintWebReadToken(session: BrowserSession, repository: string, signal?: AbortSignal): Promise<WebReadToken> {
  if (!REPO.test(repository)) throw new Error('not a repository name')
  const raw = rec(await (await request('/v1/hub/session/read-token', { kind: 'browser', session }, signal, 'POST', { repository })).json())
  const token = typeof raw['token'] === 'string' && /^[A-Za-z0-9_.:-]{16,512}$/.test(raw['token']) ? raw['token'] : null
  const expiresAt = unix(raw['expires_at'])
  if (!token || expiresAt === null) throw new ApiError(502, 'The Hub sent an unreadable read token')
  return { repository, token, expiresAt }
}

/** One connected repository the person can read now, as GET /v1/hub/session/repositories counts it. */
export interface RepositoryRow {
  repository: string
  provider: string
  unfinished: { total: number; moving: number; stuck: number }
  needsYou: number
  agents: number
  lastActivityAt: number | null
  /** False when the Hub cut a list short, so the counts are at least these. */
  complete: boolean
}
export interface RepositoryList { rows: RepositoryRow[]; unchecked: number; complete: boolean }

function decodeRepositoryRow(raw: unknown): RepositoryRow | null {
  const r = rec(raw)
  const repository = typeof r['repository'] === 'string' && REPO.test(r['repository']) ? r['repository'] : null
  if (!repository) return null
  const u = rec(r['unfinished'])
  return {
    repository,
    provider: typeof r['provider'] === 'string' && PROVIDER.test(r['provider']) ? r['provider'] : 'github',
    unfinished: { total: count(u['total']), moving: count(u['moving']), stuck: count(u['stuck']) },
    needsYou: count(r['needs_you']),
    agents: count(r['agents']),
    lastActivityAt: unix(r['last_activity_at']),
    complete: r['complete'] === true,
  }
}

/** Every connected repository this web sign-in can read now, worst first, with counts only. Each is role-checked by the Hub, so this is one forge request per repository. */
export async function fetchRepositoryList(session: BrowserSession, signal?: AbortSignal): Promise<RepositoryList> {
  const j = rec(await (await request('/v1/hub/session/repositories', { kind: 'browser', session }, signal)).json())
  return { rows: list(j, 'repositories', decodeRepositoryRow, 200), unchecked: count(j['unchecked']), complete: j['complete'] === true }
}

/**
 * Repositories this browser's sign-in can offer, from what it may already read: the person's live
 * agent tokens and their computers, and for a Hub admin every connected repository. A source that
 * fails is left out rather than failing the list.
 */
export async function sessionRepositories(session: BrowserSession, signal?: AbortSignal): Promise<string[]> {
  const auth: DeviceAuth = { kind: 'browser', session }
  const now = Date.now() / 1000
  const tokens = request('/v1/hub/tokens', auth, signal)
    .then((r) => r.json())
    .then((j) =>
      arr(rec(j)['tokens'])
        .map(rec)
        .filter((t) => t['revoked'] !== true && (unix(t['expires_at']) ?? 0) > now)
        .map((t) => t['repository']),
    )
  const devices = fetchDevices(auth, signal).then(({ devices }) => devices.filter((d) => !d.revokedAt).flatMap((d) => d.repositories))
  const admin = session.admin ? request('/v1/hub/admin/repositories', auth, signal).then(async (r) => list(await r.json(), 'repositories', decodeAdminRepository, 500).map((a) => a.fullName)) : Promise.resolve([])
  const found = await Promise.allSettled([tokens, devices, admin])
  const names = found.flatMap((f) => (f.status === 'fulfilled' ? f.value : [])).filter((n): n is string => typeof n === 'string' && REPO.test(n))
  return [...new Set(names)].sort((a, b) => a.localeCompare(b)).slice(0, 200)
}
