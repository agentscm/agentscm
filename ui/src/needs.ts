// What needs a person, and where agents work: pure joins over the work overview and live intents,
// shared by the dashboard's answer, its tabs and the rail badges so they always count the same things.
// No DOM, no React.
import { specKey, type Contested, type Derived } from './derive.ts'
import { currentResponse, pending } from './requests.ts'
import type { CoordinationRequest, WorkItem, WorkOverview } from './standup.ts'

export type NeedKind = 'blocked' | 'asked' | 'contested' | 'overlap' | 'pr'

export interface Need {
  key: string
  kind: NeedKind
  tone: 'bad' | 'warn' | 'brand'
  /** The work item it is about, when there is one. */
  item?: WorkItem
  path?: string
  /** For `asked`: the request waiting on the viewer's agent. */
  request?: CoordinationRequest
  at: number
  /** `you`: a person can act on it now. `agents`: the agents are still sorting it out, so no count includes it. */
  who: 'you' | 'agents'
}

const TONE: Record<NeedKind, Need['tone']> = { blocked: 'bad', asked: 'bad', contested: 'bad', overlap: 'warn', pr: 'warn' }
const ORDER: NeedKind[] = ['blocked', 'asked', 'contested', 'overlap', 'pr']

/** An overlap is live when both tasks are still being worked: an active claim, and a state of active or blocked. */
const liveSide = (i: WorkItem) => i.claim_status === 'active' && (i.state === 'active' || i.state === 'blocked')
export const isLiveOverlap = (o: { left: WorkItem; right: WorkItem }): boolean => liveSide(o.left) && liveSide(o.right)

/** A handoff still waiting for its answer is the agents' to settle; so is a settled or refused-only file one agent holds. */
export function contestedParty(c: Contested): Need['who'] {
  const pending = !!c.handoff && !c.handoff.answer && !c.handoff.lapsed
  return c.repeated || (c.holders.length >= 2 && !pending) ? 'you' : 'agents'
}

/** Open requests from other agents, and the viewer's login, so a request their idle agent cannot answer reaches them. */
export interface Asked {
  requests: CoordinationRequest[]
  me: string | null
}

/** True when a task's lead will not answer anything soon: it paused, or stopped checking in. */
const idle = (i: WorkItem) => i.state === 'paused' || i.claim_status !== 'active'

/**
 * Only things a person should decide: a reported blocker, another agent's request to the viewer's
 * own idle task, two agents holding one file, two live tasks declaring the same scope, and a pull
 * request touching held files. Worst first, then newest. Paused, handed-over and stale-claim tasks
 * are not here unless another agent is waiting on them: they stay in All work, still claimed. An
 * open agent-to-agent file request is marked `who: 'agents'`: listed, never counted.
 */
export function needsYou(ov: WorkOverview | null, d: Derived | null, asked?: Asked): Need[] {
  const out: Need[] = []
  const works = new Map((ov?.current ?? []).map((i) => [i.id, i]))
  for (const r of asked?.requests ?? []) {
    if (!pending(r) || asked?.me == null) continue
    for (const t of r.targets) {
      const i = works.get(t.work_id)
      if (t.lead.login !== asked.me || !i || !idle(i) || currentResponse(r, t)) continue
      out.push({ key: `asked:${r.id}:${t.work_id}`, kind: 'asked', tone: TONE.asked, item: i, path: t.reasons[0]?.your_path, request: r, at: r.created_at, who: 'you' })
    }
  }
  for (const i of ov?.current ?? []) {
    if (i.state === 'blocked') out.push({ key: `blocked:${i.id}`, kind: 'blocked', tone: TONE.blocked, item: i, at: i.blocker?.at ?? i.updated_at, who: 'you' })
  }
  for (const c of d?.contested ?? []) out.push({ key: `contested:${c.path}`, kind: 'contested', tone: TONE.contested, path: c.path, at: c.refused?.at ?? c.handoff?.at ?? 0, who: contestedParty(c) })
  for (const o of (ov?.overlaps ?? []).filter(isLiveOverlap)) out.push({ key: `overlap:${o.left.id}:${o.right.id}`, kind: 'overlap', tone: TONE.overlap, item: o.left, path: o.reasons[0]?.left_path, at: Math.max(o.left.updated_at, o.right.updated_at), who: 'you' })
  for (const p of d?.prOverlaps ?? []) out.push({ key: `pr:${p.pr.number}`, kind: 'pr', tone: TONE.pr, path: p.with[0]?.files[0], at: 0, who: 'you' })
  return out.sort((a, b) => ORDER.indexOf(a.kind) - ORDER.indexOf(b.kind) || b.at - a.at)
}

/** What the Inbox badge counts: overlaps to talk about, and open requests to the viewer's agents that they have not answered. */
export function inboxBadge(ov: WorkOverview | null, asked?: Asked): { n: number; tip: string } {
  const overlaps = ov?.overlaps.filter(isLiveOverlap).length ?? 0
  const me = asked?.me
  const waiting = me == null ? 0 : (asked?.requests ?? []).filter((r) => pending(r) && r.targets.some((t) => t.lead.login === me && !currentResponse(r, t))).length
  const parts = [overlaps ? `${overlaps} overlap${overlaps === 1 ? '' : 's'} to talk about` : '', waiting ? `${waiting} request${waiting === 1 ? '' : 's'} waiting on you` : '']
  return { n: overlaps + waiting, tip: parts.filter(Boolean).join(' · ') }
}

/** When a need last changed: its own time or its task's last update, whichever is newer. */
export const needUpdatedAt = (n: Need): number => Math.max(n.at, n.item?.updated_at ?? 0)
/** The browser-only hide key: a need that changes gets a new key, so it shows again. */
export const needHideKey = (n: Need): string => `${n.key}@${needUpdatedAt(n)}`

/** The top-level folder a path or `dir/**` spec lives in; files at the root share one area. */
export function areaOf(spec: string): string {
  const key = specKey(spec).replace(/^\.?\/+/, '')
  const slash = key.indexOf('/')
  return slash < 0 ? '' : key.slice(0, slash)
}

export interface Area {
  /** Folder name, or '' for files at the repository root. */
  name: string
  /** Live agents holding files here, by session key. */
  holders: { id: string; name: string; owner: string; files: string[] }[]
  /** Unfinished tasks that declared scope here. */
  planned: { item: WorkItem; paths: string[] }[]
  /** Contested files here (two agents holding one path) and overlaps (two tasks planning the same one). */
  contested: number
  overlaps: number
}

/** One tile per top-level folder that someone holds or plans to touch, busiest first. */
export function areas(ov: WorkOverview | null, d: Derived | null): Area[] {
  const by = new Map<string, Area>()
  const at = (name: string) => {
    let a = by.get(name)
    if (!a) by.set(name, (a = { name, holders: [], planned: [], contested: 0, overlaps: 0 }))
    return a
  }
  for (const agent of d?.agents ?? []) {
    const files = new Map<string, string[]>()
    for (const h of agent.holdings) files.set(areaOf(h.spec), [...(files.get(areaOf(h.spec)) ?? []), h.spec])
    for (const [name, f] of files) at(name).holders.push({ id: agent.id, name: agent.name, owner: agent.owner, files: f })
  }
  for (const item of ov?.current ?? []) {
    const paths = new Map<string, string[]>()
    for (const s of item.scope) paths.set(areaOf(s), [...(paths.get(areaOf(s)) ?? []), s])
    for (const [name, p] of paths) at(name).planned.push({ item, paths: p })
  }
  for (const c of d?.contested ?? []) at(areaOf(c.path)).contested++
  for (const o of (ov?.overlaps ?? []).filter(isLiveOverlap)) {
    const names = new Set(o.reasons.map((r) => areaOf(r.left_path)))
    for (const name of names) at(name).overlaps++
  }
  const weight = (a: Area) => (a.contested + a.overlaps) * 100 + a.holders.length * 10 + a.planned.length
  return [...by.values()].sort((a, b) => weight(b) - weight(a) || a.name.localeCompare(b.name))
}
