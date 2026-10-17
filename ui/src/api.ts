// The page only ever issues GETs. Responses are coerced defensively because
// every string in them is teammate-supplied.
import type { Activity, Health, HubEvent, Intent, Session, Whoami, Work } from './types.ts'

export const AGENT_TOKEN_MESSAGE = 'This is an agent token, which can change coordination. The status page needs a read-only token.'

export class ApiError extends Error {
  status: number
  constructor(status: number, message: string) {
    super(message)
    this.status = status
  }
}

async function getJson(path: string, signal: AbortSignal, token?: string): Promise<unknown> {
  const res = await fetch(path, {
    signal,
    cache: 'no-store',
    credentials: 'omit',
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  })
  if (!res.ok) throw new ApiError(res.status, `HTTP ${res.status}`)
  return res.json()
}

const rec = (v: unknown): Record<string, unknown> => (v && typeof v === 'object' ? (v as Record<string, unknown>) : {})
const list = (v: unknown): Record<string, unknown>[] => (Array.isArray(v) ? v.map(rec) : [])
const strs = (v: unknown): string[] => (Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [])
const str = (v: unknown, d = ''): string => (typeof v === 'string' ? v : d)
const strOrNull = (v: unknown): string | null => (typeof v === 'string' && v !== '' ? v : null)
const num = (v: unknown, d = 0): number => (typeof v === 'number' && Number.isFinite(v) ? v : d)
const agentRef = (v: unknown) => ({ token: str(rec(v)['token']), name: str(rec(v)['name'], 'agent') })
const ownerRef = (v: unknown) => ({ login: str(rec(v)['login'], 'unknown') })

/** Handoff events carry teammate text (note, names): keep only the known fields, as strings. */
const handoffParty = (v: unknown) => ({ agent: str(rec(v)['agent'], 'agent'), login: str(rec(v)['login'], 'unknown') })
function eventData(kind: string, v: unknown): Record<string, unknown> {
  const d = rec(v)
  if (kind === 'handoff_request') return { request: str(d['request']), paths: strs(d['paths']), note: strOrNull(d['note']), to: handoffParty(d['to']) }
  if (kind === 'handoff_grant') return { request: str(d['request']), paths: strs(d['paths']), from: handoffParty(d['from']) }
  if (kind === 'handoff_decline') return { request: str(d['request']), paths: strs(d['paths']), from: handoffParty(d['from']), reason: strOrNull(d['reason']) }
  return d
}

export async function fetchWhoami(token: string, signal: AbortSignal): Promise<Whoami> {
  const j = rec(await getJson('/v1/hub/whoami', signal, token))
  const t = rec(j['token'])
  const o = rec(j['owner'])
  return {
    token: { id: str(t['id']), name: str(t['name'], 'token'), repository: str(t['repository']), read_only: t['read_only'] === true, expires_at: typeof t['expires_at'] === 'number' ? t['expires_at'] : null },
    owner: { provider: str(o['provider'], 'github'), host: str(o['host']), login: str(o['login'], 'unknown') },
  }
}

const repoPath = (repo: string) => `/v1/hub/repos/${encodeURIComponent(repo)}`

export async function fetchWork(repo: string, token: string, signal: AbortSignal): Promise<Work> {
  const j = rec(await getJson(`${repoPath(repo)}/work`, signal, token))
  return {
    intents: list(j['intents']).map(
      (i): Intent => ({
        id: str(i['id']),
        task: strOrNull(i['task']),
        branch: strOrNull(i['branch']),
        base: strOrNull(i['base']),
        machine: strOrNull(i['machine']),
        paths: strs(i['paths']),
        via: strs(i['via']),
        expires_at: num(i['expires_at']),
        created_at: num(i['created_at']),
        updated_at: num(i['updated_at']),
        agent: agentRef(i['agent']),
        owner: ownerRef(i['owner']),
      }),
    ),
    intents_truncated: j['intents_truncated'] === true,
    connected: j['connected'] === true,
    pull_requests: list(j['pull_requests']).map((p) => ({
      number: num(p['number']),
      title: str(p['title']),
      state: str(p['state']),
      draft: p['draft'] === true,
      head_ref: str(p['head_ref']),
      base_ref: str(p['base_ref']),
      author: { login: str(rec(p['author'])['login']) },
      html_url: strOrNull(p['html_url']),
      files: strs(p['files']),
      files_truncated: p['files_truncated'] === true,
    })),
  }
}

export async function fetchActivity(repo: string, token: string, signal: AbortSignal): Promise<Activity> {
  const j = rec(await getJson(`${repoPath(repo)}/activity?limit=50`, signal, token))
  return {
    sessions: list(j['sessions']).map(
      (s): Session => ({
        key: str(s['key']),
        agent: agentRef(s['agent']),
        owner: ownerRef(s['owner']),
        intent: strOrNull(s['intent']),
        task: strOrNull(s['task']),
        via: strs(s['via']),
        created_at: num(s['created_at']),
        updated_at: num(s['updated_at']),
      }),
    ),
    events: list(j['events']).map(
      (e): HubEvent => ({
        seq: num(e['seq']),
        id: str(e['id'], String(e['seq'])),
        at: num(e['at']),
        kind: str(e['kind']),
        agent: agentRef(e['agent']),
        owner: ownerRef(e['owner']),
        data: eventData(str(e['kind']), e['data']),
      }),
    ),
  }
}

/** 503 `{status:"unavailable"}` is a real answer; transport errors are not. */
export async function fetchHealth(signal: AbortSignal): Promise<Health> {
  try {
    const j = rec(await getJson('/v1/hub/health', signal))
    const c = rec(j['coordination'])
    return {
      status: j['status'] === 'ok' ? 'ok' : 'unavailable',
      storage: str(j['storage']),
      ...(j['demo'] === true ? { demo: true as const } : {}),
      coordination: {
        denied: num(c['denied']),
        repeated: num(c['repeated']),
        rate_limited: num(c['rate_limited']),
        fail_open: num(c['fail_open']),
        hook_p95_ms: num(c['hook_p95_ms']),
      },
    }
  } catch (e) {
    if (e instanceof ApiError && e.status === 503) return { status: 'unavailable' }
    throw e
  }
}
