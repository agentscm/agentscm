// The Inbox's writes: a signed-in person answering for their own agent from the Hub's web pages.
// Every call needs the web sign-in (an HttpOnly cookie, plus its CSRF token for a change); a pasted
// token or a computer's page has none, so the page offers copy prompts there instead.
//
// API for other packages:
//   MyHandoff, HandoffSide       a file handoff the person's own agent is part of, from GET /v1/hub/session/handoffs
//   fetchMyHandoffs(session, repository, signal)    those, newest first
//   replyToRequest(session, body)                   answer a coordination request for the agent that leads the target Work; resolves to the updated request
//   answerHandoff(session, body) / askHandoff(session, body)   hand a file over (or keep it) / ask its holder; resolve to the Hub's outcome
//   operationId()                                    a fresh replay key; reuse it when retrying the same answer
//   sendError(e)                                     a failed send in words a person can act on
import { ApiError } from './api.ts'
import type { BrowserSession } from './devices.ts'
import { refusalFrom } from './refusal.ts'
import { decodeCoordinationRequest, type CoordinationRequest, type CoordinationResponseKind } from './standup.ts'
import { withTimeout } from './timeout.ts'

export interface HandoffSide {
  agent: string
  login: string
  /** The signed-in person's own agent; only then are `token` and `session` present. */
  mine: boolean
  token?: string
  session?: string
}
export type HandoffState = 'pending' | 'granted' | 'declined' | 'lapsed'
export interface MyHandoff {
  id: string
  path: string
  state: HandoffState
  note: string | null
  reply: string | null
  createdAt: number
  decidedAt: number | null
  holderTold: boolean
  requesterTold: boolean
  askedViaHub: boolean
  answeredViaHub: boolean
  /** The answer came from a Slack button, not the agent or the Hub's pages. */
  answeredViaSlack: boolean
  requester: HandoffSide
  holder: HandoffSide
}

const rec = (v: unknown): Record<string, unknown> => (v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : {})
// eslint-disable-next-line no-control-regex
const text = (v: unknown, max = 300): string => (typeof v === 'string' ? v.replace(/[\u0000-\u001f\u007f-\u009f]/g, ' ').trim().slice(0, max) : '')
const textOrNull = (v: unknown, max = 300): string | null => text(v, max) || null
const time = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) && v > 0 ? Math.floor(v) : null)
const STATES = new Set<string>(['pending', 'granted', 'declined', 'lapsed'])

function side(v: unknown): HandoffSide | null {
  const s = rec(v)
  const agent = text(s['agent'], 100)
  const login = text(s['login'], 100)
  if (!agent || !login) return null
  const mine = s['mine'] === true
  const token = text(s['token'], 100)
  const session = text(s['session'], 255)
  return { agent, login, mine, ...(mine && token ? { token } : {}), ...(mine && session ? { session } : {}) }
}

export function decodeHandoffs(raw: unknown): MyHandoff[] {
  const out: MyHandoff[] = []
  const rows = rec(raw)['handoffs']
  for (const row of Array.isArray(rows) ? rows.slice(0, 100) : []) {
    const h = rec(row)
    const id = text(h['id'], 64)
    const path = text(h['path'], 1000)
    const state = typeof h['state'] === 'string' && STATES.has(h['state']) ? (h['state'] as HandoffState) : null
    const createdAt = time(h['created_at'])
    const requester = side(h['requester'])
    const holder = side(h['holder'])
    if (!id || !path || !state || createdAt === null || !requester || !holder) continue
    out.push({
      id,
      path,
      state,
      note: textOrNull(h['note'], 200),
      reply: textOrNull(h['reply'], 200),
      createdAt,
      decidedAt: time(h['decided_at']),
      holderTold: h['holder_told'] === true,
      requesterTold: h['requester_told'] === true,
      askedViaHub: h['asked_via_hub'] === true,
      answeredViaHub: h['answered_via_hub'] === true,
      answeredViaSlack: h['answered_via_slack'] === true,
      requester,
      holder,
    })
  }
  return out
}

async function call(path: string, session: BrowserSession, method: 'GET' | 'POST', body?: unknown, signal?: AbortSignal): Promise<unknown> {
  const res = await fetch(path, {
    method,
    signal: withTimeout(signal),
    cache: 'no-store',
    credentials: 'same-origin',
    headers: { ...(method === 'POST' ? { 'x-csrf-token': session.csrf } : {}), ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}) },
    ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
  })
  if (!res.ok) {
    let parsed: unknown = null
    try {
      parsed = await res.json()
    } catch {
      /* the status still says enough */
    }
    throw new ApiError(res.status, `HTTP ${res.status}`, refusalFrom(parsed))
  }
  return res.json()
}

export async function fetchMyHandoffs(session: BrowserSession, repository: string, signal?: AbortSignal): Promise<MyHandoff[]> {
  return decodeHandoffs(await call(`/v1/hub/session/handoffs?repository=${encodeURIComponent(repository)}`, session, 'GET', undefined, signal))
}

export interface ReplyBody {
  /** The agent token that leads the target Work (from the request's target lead). */
  token: string
  request_id: string
  operation_id: string
  target_work_id: string
  session: string
  expected_updated_seq: number
  kind: CoordinationResponseKind
  note?: string
}
export async function replyToRequest(session: BrowserSession, body: ReplyBody): Promise<CoordinationRequest> {
  return decodeCoordinationRequest(await call('/v1/hub/session/coordination/reply', session, 'POST', body))
}

export type AnswerOutcome = 'granted' | 'declined' | 'no_open_request' | 'not_held'
export type AskOutcome = 'requested' | 'already_asked' | 'not_held' | 'yours' | 'too_many'
const ANSWER = new Set<string>(['granted', 'declined', 'no_open_request', 'not_held'])
const ASK = new Set<string>(['requested', 'already_asked', 'not_held', 'yours', 'too_many'])

export async function answerHandoff(session: BrowserSession, body: { token: string; session?: string; path: string; accept: boolean; reason?: string }): Promise<AnswerOutcome> {
  const o = rec(await call('/v1/hub/session/handoff/answer', session, 'POST', body))['outcome']
  if (typeof o !== 'string' || !ANSWER.has(o)) throw new ApiError(502, 'The Hub sent an unreadable answer')
  return o as AnswerOutcome
}

export async function askHandoff(session: BrowserSession, body: { token: string; session?: string; path: string; note?: string }): Promise<AskOutcome> {
  const o = rec(await call('/v1/hub/session/handoff/ask', session, 'POST', body))['outcome']
  if (typeof o !== 'string' || !ASK.has(o)) throw new ApiError(502, 'The Hub sent an unreadable answer')
  return o as AskOutcome
}

/** A key for one answer: sent again with the same key, the Hub replays the first instead of answering twice. */
export function operationId(): string {
  const c = globalThis.crypto
  return `web-${typeof c?.randomUUID === 'function' ? c.randomUUID() : `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`}`
}

/** What the Hub said no to a handoff ask or answer that went through, in a sentence. */
export const OUTCOME_SENTENCE: Record<AnswerOutcome | AskOutcome, string> = {
  granted: 'Handed over.',
  declined: 'Kept for now.',
  no_open_request: 'There is no open request any more: it was answered or lapsed.',
  not_held: 'That agent no longer holds the file, so there is nothing to hand over.',
  requested: 'Asked.',
  already_asked: 'Already asked; the other agent has not answered yet.',
  yours: 'Your own agent already holds that file.',
  too_many: 'Too many requests are open for that file or session; wait for one to be answered.',
}

/** A failed send in words a person can act on; the Hub's own sentence when it wrote one. */
export function sendError(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.refusal?.message) return e.refusal.message
    if (e.status === 401) return 'Your Hub sign-in has ended. Sign in again, then send it again.'
    if (e.status === 503) return 'The Hub is busy right now. Nothing was sent; try again in a moment.'
    if (e.status === 403) return 'The Hub did not accept this: it is not your agent, or it is no longer connected.'
    return 'The Hub did not accept that answer. Nothing was sent.'
  }
  return e instanceof Error && e.name === 'TimeoutError' ? 'The Hub did not answer in time. It may or may not have gone through; refresh to see.' : 'Could not reach the Hub. Nothing was sent.'
}
