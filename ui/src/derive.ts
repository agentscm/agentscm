// Pure joins and overlap rules. Mirrors the hub: no DOM, no React, runnable
// under `node` type stripping (see scripts/check-derive.mjs).
import type { Activity, HubEvent, Intent, PullRequest, Work } from './types.ts'

/** A path spec is an exact path or a directory `dir/**`; its key drops a trailing `**`. */
export const specKey = (spec: string): string => (spec.endsWith('**') ? spec.slice(0, -2) : spec)

export function overlaps(a: string, b: string): boolean {
  const ka = specKey(a)
  const kb = specKey(b)
  return ka === kb || (ka.endsWith('/') && kb.startsWith(ka)) || (kb.endsWith('/') && ka.startsWith(kb))
}

export interface Holding {
  spec: string
  contested: boolean
}

export interface LiveAgent {
  id: string // the session's key, or `intent:<id>` for an intent no session points at
  owner: string
  token: string
  name: string
  via: string[]
  task: string | null
  machine: string | null
  branch: string | null
  holdings: Holding[]
  lastActive: number
  /** The intent this row holds files through, and its via: the hub's same-agent rule reads the intent's. */
  intent: { id: string; via: string[] } | null
}

export interface OwnerGroup {
  owner: string
  agents: LiveAgent[]
}

/** The newest handoff request touching a file, and the holder's answer once one with the same request id exists. */
export interface Handoff {
  request: string
  at: number
  from: { owner: string; name: string } // the requester
  to: { owner: string; name: string } // the holder
  note: string | null
  answer?: { kind: 'granted' | 'declined'; at: number; reason?: string }
  /** unanswered after the hub's 30-minute cap, so it can no longer be granted */
  lapsed?: true
}

const HANDOFF_LAPSE_SECONDS = 1800

export interface Contested {
  path: string
  holders: { id: string; owner: string; name: string }[]
  handoff?: Handoff
  refused?: { at: number; owner: string; name: string }
  repeated?: { at: number; owner: string; name: string }
}

export interface PrOverlap {
  pr: PullRequest
  with: { agent: LiveAgent; files: string[] }[]
}

export interface Derived {
  agents: LiveAgent[]
  groups: OwnerGroup[]
  filesHeld: number
  contested: Contested[]
  prOverlaps: PrOverlap[]
  otherOpenPrs: number
  intentById: Map<string, Intent>
}

/** The hub fills `task unknown` when an agent announced none: that is an absence, not a task to show. */
export const announced = (task: string | null | undefined): string | null => (task && task !== 'task unknown' ? task : null)

const mcpOnly = (via: string[]): boolean => via.length === 1 && via[0] === 'mcp'

/**
 * Mirrors the hub's `same_agent`: two intents of ONE token are the same agent only when one of them is
 * MCP-only (via exactly ["mcp"]). Two hook sessions of one token are different agents and refuse each other.
 */
export function sameAgent(a: LiveAgent, b: LiveAgent): boolean {
  return a.token === b.token && !!a.intent && !!b.intent && (mcpOnly(a.intent.via) || mcpOnly(b.intent.via))
}

/** One row per live session (joined to its intent by id), plus one per intent no session points at. */
export function liveAgents(work: Work, activity: Activity): LiveAgent[] {
  const intentById = new Map(work.intents.map((i) => [i.id, i]))
  const rows: LiveAgent[] = []
  const used = new Set<string>()
  const holdings = (i: Intent): Holding[] => [...new Set(i.paths)].map((spec) => ({ spec, contested: false }))
  for (const s of activity.sessions) {
    const i = s.intent ? intentById.get(s.intent) : undefined
    if (i) used.add(i.id)
    rows.push({
      id: s.key || `session:${s.agent.token}:${s.created_at}:${s.intent ?? ''}`,
      owner: s.owner.login,
      token: s.agent.token,
      name: s.agent.name,
      via: [...new Set([...s.via, ...(i?.via ?? [])])],
      task: announced(s.task) ?? announced(i?.task) ?? null,
      machine: i?.machine ?? null,
      branch: i?.branch ?? null,
      holdings: i ? holdings(i) : [],
      lastActive: Math.max(s.updated_at, i?.updated_at ?? 0),
      intent: i ? { id: i.id, via: i.via } : null,
    })
  }
  for (const i of work.intents) {
    if (used.has(i.id)) continue
    rows.push({
      id: `intent:${i.id}`,
      owner: i.owner.login,
      token: i.agent.token,
      name: i.agent.name,
      via: i.via,
      task: announced(i.task),
      machine: i.machine,
      branch: i.branch,
      holdings: holdings(i),
      lastActive: i.updated_at,
      intent: { id: i.id, via: i.via },
    })
  }
  // Contested = a held spec that overlaps a spec held by a different agent.
  for (const a of rows) {
    for (const h of a.holdings) {
      h.contested = rows.some((b) => b !== a && !sameAgent(a, b) && b.holdings.some((o) => overlaps(h.spec, o.spec)))
    }
  }
  return rows.sort((x, y) => x.owner.localeCompare(y.owner) || x.name.localeCompare(y.name) || x.id.localeCompare(y.id))
}

/** The more specific of two overlapping specs (longer key wins; ties keep the first). */
const moreSpecific = (a: string, b: string): string => (specKey(b).length > specKey(a).length ? b : a)

export function contestedFiles(agents: LiveAgent[], events: HubEvent[], now = Date.now() / 1000): Contested[] {
  const found = new Map<string, string>() // key -> spec
  for (let i = 0; i < agents.length; i++) {
    for (let j = i + 1; j < agents.length; j++) {
      const a = agents[i]!
      const b = agents[j]!
      if (sameAgent(a, b)) continue
      for (const x of a.holdings) {
        for (const y of b.holdings) {
          if (!overlaps(x.spec, y.spec)) continue
          const spec = moreSpecific(x.spec, y.spec)
          found.set(specKey(spec), spec)
        }
      }
    }
  }
  const sorted = [...found.values()].sort((p, q) => p.localeCompare(q))
  // Files with a refusal or a handoff in the recent events appear too, after the held ones, even when
  // only one agent holds them now (after a refusal the requester holds nothing).
  const storyOnly = new Map<string, string>()
  for (const e of events) {
    if (e.kind !== 'edit_denied' && !e.kind.startsWith('handoff_')) continue
    for (const p of pathsOf(e)) if (![...found.values(), ...storyOnly.values()].some((q) => overlaps(p, q))) storyOnly.set(specKey(p), p)
  }
  const paths = [...sorted, ...[...storyOnly.values()].sort((p, q) => p.localeCompare(q))]
  return paths.map((path) => {
    const holders = new Map<string, { id: string; owner: string; name: string }>()
    for (const a of agents) {
      if (a.holdings.some((h) => overlaps(h.spec, path))) holders.set(`${a.owner}\u0000${a.token}`, { id: `${a.owner}\u0000${a.token}`, owner: a.owner, name: a.name })
    }
    const who = (e: HubEvent) => ({ at: e.at, owner: e.owner.login, name: e.agent.name })
    const touches = (e: HubEvent) => pathsOf(e).some((p) => overlaps(p, path))
    // events are newest first
    const refusedEvent = events.find((e) => e.kind === 'edit_denied' && touches(e))
    const repeatedEvent = refusedEvent
      ? events.find((e) => e.kind === 'edit_repeated' && e.agent.token === refusedEvent.agent.token && e.at >= refusedEvent.at && touches(e))
      : undefined
    const requestEvent = events.find((e) => e.kind === 'handoff_request' && touches(e))
    const requestId = requestEvent ? str(requestEvent.data['request']) : ''
    // an empty id never matches, so an old answer cannot settle a request it does not name
    const answerEvent = requestId ? events.find((e) => (e.kind === 'handoff_grant' || e.kind === 'handoff_decline') && e.data['request'] === requestId) : undefined
    const party = (v: unknown) => ({ owner: str(rec(v)['login']), name: str(rec(v)['agent']) })
    const handoff: Handoff | undefined = requestEvent && {
      request: requestId,
      at: requestEvent.at,
      from: { owner: requestEvent.owner.login, name: requestEvent.agent.name },
      to: party(requestEvent.data['to']),
      note: typeof requestEvent.data['note'] === 'string' && requestEvent.data['note'] ? requestEvent.data['note'] : null,
      ...(answerEvent
        ? {
            answer: {
              kind: answerEvent.kind === 'handoff_grant' ? ('granted' as const) : ('declined' as const),
              at: answerEvent.at,
              ...(str(answerEvent.data['reason']) ? { reason: str(answerEvent.data['reason']) } : {}),
            },
          }
        : {}),
      ...(!answerEvent && now - requestEvent.at > HANDOFF_LAPSE_SECONDS ? { lapsed: true as const } : {}),
    }
    return {
      path,
      holders: [...holders.values()],
      ...(handoff ? { handoff } : {}),
      ...(refusedEvent ? { refused: who(refusedEvent) } : {}),
      ...(repeatedEvent ? { repeated: who(repeatedEvent) } : {}),
    }
  })
}

/** The newest story across contested files: a refusal (or its repeat) or a handoff (or its answer). A tie goes to the handoff (it follows the refusal); otherwise to the first file by path. */
export function notable(contested: Contested[]): { kind: 'refusal' | 'handoff'; c: Contested; at: number } | null {
  let best: { kind: 'refusal' | 'handoff'; c: Contested; at: number } | null = null
  for (const c of contested) {
    if (c.refused) {
      const at = c.repeated?.at ?? c.refused.at
      if (!best || at > best.at) best = { kind: 'refusal', c, at }
    }
    if (c.handoff) {
      const at = c.handoff.answer?.at ?? c.handoff.at
      // `at` is whole seconds; a handoff in the same second as a refusal came after it.
      if (!best || at > best.at || (at === best.at && best.kind === 'refusal')) best = { kind: 'handoff', c, at }
    }
  }
  return best
}

const str = (v: unknown): string => (typeof v === 'string' ? v : '')
const rec = (v: unknown): Record<string, unknown> => (v && typeof v === 'object' ? (v as Record<string, unknown>) : {})

export function pathsOf(e: HubEvent): string[] {
  const p = e.data['paths']
  return Array.isArray(p) ? p.filter((x): x is string => typeof x === 'string') : []
}

export function prOverlaps(prs: PullRequest[], agents: LiveAgent[]): { overlaps: PrOverlap[]; others: number } {
  const open = prs.filter((p) => p.state === 'open')
  const out: PrOverlap[] = []
  for (const pr of open) {
    const hits: PrOverlap['with'] = []
    for (const agent of agents) {
      const files = pr.files.filter((f) => agent.holdings.some((h) => overlaps(f, h.spec)))
      if (files.length) hits.push({ agent, files })
    }
    if (hits.length) out.push({ pr, with: hits })
  }
  return { overlaps: out, others: open.length - out.length }
}

export function derive(work: Work, activity: Activity, now = Date.now() / 1000): Derived {
  const agents = liveAgents(work, activity)
  const owners = new Map<string, LiveAgent[]>()
  for (const a of agents) owners.set(a.owner, [...(owners.get(a.owner) ?? []), a])
  const groups = [...owners].map(([owner, list]) => ({ owner, agents: list }))
  const pr = prOverlaps(work.pull_requests, agents)
  return {
    agents,
    groups,
    filesHeld: new Set(agents.flatMap((a) => a.holdings.map((h) => specKey(h.spec)))).size,
    contested: contestedFiles(agents, activity.events, now),
    prOverlaps: pr.overlaps,
    otherOpenPrs: pr.others,
    intentById: new Map(work.intents.map((i) => [i.id, i])),
  }
}
