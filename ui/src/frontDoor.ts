// This computer's Hub page. `agentscm open` (or Open Hub in the menu bar app) serves this same page
// from the local service at http://127.0.0.1:…/ui/ and opens it with `#pair=<code>&repo=<owner/name>`.
// The page removes the fragment from history, redeems the single-use code with its own service, and
// keeps `local:<grant>:<repo>` where the Hub sign-in normally lives. Every request then carries it:
// the service serves this computer's files and saved conversations itself and fetches team data from
// the repository's Hub with a read-only token that never reaches the browser.
import { clean } from './publications.ts'
import { clearToken, loadToken, saveToken } from './storage.ts'

const PREFIX = 'local:'
const OUTCOME_KEY = 'agentscm.hub.connectOutcome'
const hex = (v: unknown, n: number): v is string => typeof v === 'string' && v.length === n && /^[0-9a-f]+$/.test(v)
const REPO = /^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/
/** `owner/name`, optionally `@<hub URL>` when the same repository is connected to more than one Hub. */
const SELECTOR = /^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+(@https?:\/\/[^\s@]+)?$/

export const isLocalCredential = (token: string | null): token is string => typeof token === 'string' && token.startsWith(PREFIX)

/** The selection (`owner/name[@hub]`) of a local credential. */
export function localSelection(token: string): string | null {
  const selection = token.slice(PREFIX.length).split(':').slice(1).join(':')
  return SELECTOR.test(selection) ? selection : null
}

/** The repository of a local credential. */
export function localRepository(token: string): string | null {
  return localSelection(token)?.split('@')[0] ?? null
}

/** Set when this page is served by this computer's local service, decided before the first render. */
let frontDoor = false
export const onFrontDoor = () => frontDoor

/** What the last `#pair` link did, shown once. */
export type ConnectOutcome = 'connected' | 'used'

export function takeConnectOutcome(): ConnectOutcome | null {
  try {
    const v = window.sessionStorage.getItem(OUTCOME_KEY)
    window.sessionStorage.removeItem(OUTCOME_KEY)
    return v === 'connected' || v === 'used' ? v : null
  } catch {
    return null
  }
}

function recordOutcome(v: ConnectOutcome): void {
  try {
    window.sessionStorage.setItem(OUTCOME_KEY, v)
  } catch {
    /* shown only when storage works */
  }
}

async function json(path: string, init: RequestInit = {}): Promise<{ status: number; body: unknown }> {
  const res = await fetch(path, { cache: 'no-store', credentials: 'omit', redirect: 'error', ...init })
  return { status: res.status, body: await res.json().catch(() => null) }
}
const rec = (v: unknown): Record<string, unknown> => (v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : {})

/**
 * Before the first render: learn whether this is the local page, and redeem a `#pair` link. The
 * fragment leaves the address bar and history before the code is sent.
 */
export async function adoptFrontDoorLink(): Promise<boolean> {
  const fragment = new URLSearchParams(window.location.hash.slice(1))
  const code = fragment.get('pair')
  const repo = fragment.get('repo') ?? ''
  if (fragment.has('pair')) history.replaceState(null, '', `${window.location.pathname}${window.location.search}#explorer`)
  try {
    const identity = await json('/v1/local/identity')
    frontDoor = identity.status === 200 && hex(rec(identity.body)['instance'], 32)
  } catch {
    frontDoor = false
  }
  if (!fragment.has('pair') || !frontDoor) return false
  if (!hex(code, 64) || !SELECTOR.test(repo)) {
    recordOutcome('used')
    return false
  }
  try {
    const reply = await json('/v1/local/pairings/redeem', { method: 'POST', headers: { Authorization: `Bearer ${code}` } })
    const grant = rec(reply.body)['grant']
    if (reply.status !== 200 || !hex(grant, 64)) {
      recordOutcome('used')
      return false
    }
    saveToken(`${PREFIX}${grant}:${repo}`, true)
    recordOutcome('connected')
    return true
  } catch {
    recordOutcome('used')
    return false
  }
}

export interface Identity {
  computer: string
  /** Null when the selected repository is no longer connected on this computer. */
  repository: string | null
  hub: { url: string; signed_in: boolean } | null
  expires_at: number | null
}

export async function fetchIdentity(token: string, signal: AbortSignal): Promise<Identity> {
  const res = await fetch('/v1/local/identity', { signal, cache: 'no-store', credentials: 'omit', headers: { Authorization: `Bearer ${token}` } })
  if (!res.ok) throw new Error(res.status === 401 ? 'not_connected' : `HTTP ${res.status}`)
  const body = rec(await res.json())
  const hub = body['hub'] === null ? null : rec(body['hub'])
  return {
    computer: clean(body['computer'], 63) || 'This computer',
    repository: typeof body['repository'] === 'string' ? clean(body['repository']) : null,
    hub: hub && { url: clean(hub['url'], 300), signed_in: hub['signed_in'] === true },
    expires_at: typeof body['expires_at'] === 'number' ? body['expires_at'] : null,
  }
}

export interface ConnectedRepository {
  repository: string
  hub: string
  folder: string
}

export async function fetchRepositories(token: string, signal: AbortSignal): Promise<ConnectedRepository[]> {
  const res = await fetch('/v1/local/repositories', { signal, cache: 'no-store', credentials: 'omit', headers: { Authorization: `Bearer ${token}` } })
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  const list = rec(await res.json())['repositories']
  return (Array.isArray(list) ? list : []).slice(0, 200).flatMap((item) => {
    const r = rec(item)
    const repository = clean(r['repository'])
    return REPO.test(repository) ? [{ repository, hub: clean(r['hub'], 300), folder: clean(r['folder'], 200) }] : []
  })
}

/** Show another connected repository (`owner/name[@hub]`) with the same connection. */
export function switchRepository(token: string, repository: string): string | null {
  if (!isLocalCredential(token) || !SELECTOR.test(repository)) return null
  const grant = token.slice(PREFIX.length).split(':')[0]!
  const next = `${PREFIX}${grant}:${repository}`
  saveToken(next, true)
  return next
}

/** Disconnect this browser: the service forgets the grant and the page forgets the credential. */
export async function disconnectComputer(): Promise<void> {
  const token = loadToken()
  if (isLocalCredential(token)) {
    try {
      await fetch('/v1/local/session', { method: 'DELETE', cache: 'no-store', credentials: 'omit', headers: { Authorization: `Bearer ${token}` } })
    } catch {
      /* the grant ends with the service anyway */
    }
  }
  clearToken()
}
