// What needs a person, and where agents work: pure joins over the work overview and live intents,
// shared by the dashboard's answer, its tabs and the rail badges so they always count the same things.
// No DOM, no React.
import { specKey, type Derived } from './derive.ts'
import type { WorkItem, WorkOverview } from './standup.ts'

export type NeedKind = 'blocked' | 'quiet' | 'handover' | 'contested' | 'overlap' | 'pr'

export interface Need {
  key: string
  kind: NeedKind
  tone: 'bad' | 'warn' | 'brand'
  /** The work item it is about, when there is one. */
  item?: WorkItem
  path?: string
  at: number
}

const TONE: Record<NeedKind, Need['tone']> = { blocked: 'bad', contested: 'bad', quiet: 'warn', overlap: 'warn', pr: 'warn', handover: 'brand' }
const ORDER: NeedKind[] = ['blocked', 'contested', 'quiet', 'overlap', 'pr', 'handover']

/**
 * Only things a person should decide: a reported blocker, a task whose lead went quiet but still
 * holds it, a task offered for handover, two agents holding one file, two tasks declaring the same
 * scope, and a pull request touching held files. Worst first, then newest.
 */
export function needsYou(ov: WorkOverview | null, d: Derived | null): Need[] {
  const out: Need[] = []
  for (const i of ov?.current ?? []) {
    if (i.state === 'blocked') out.push({ key: `blocked:${i.id}`, kind: 'blocked', tone: TONE.blocked, item: i, at: i.blocker?.at ?? i.updated_at })
    else if (i.claim_status !== 'active') out.push({ key: `quiet:${i.id}`, kind: 'quiet', tone: TONE.quiet, item: i, at: i.last_checkin_at })
    else if (i.state === 'handover') out.push({ key: `handover:${i.id}`, kind: 'handover', tone: TONE.handover, item: i, at: i.updated_at })
  }
  for (const c of d?.contested ?? []) out.push({ key: `contested:${c.path}`, kind: 'contested', tone: TONE.contested, path: c.path, at: c.refused?.at ?? c.handoff?.at ?? 0 })
  for (const o of ov?.overlaps ?? []) out.push({ key: `overlap:${o.left.id}:${o.right.id}`, kind: 'overlap', tone: TONE.overlap, item: o.left, path: o.reasons[0]?.left_path, at: Math.max(o.left.updated_at, o.right.updated_at) })
  for (const p of d?.prOverlaps ?? []) out.push({ key: `pr:${p.pr.number}`, kind: 'pr', tone: TONE.pr, path: p.with[0]?.files[0], at: 0 })
  return out.sort((a, b) => ORDER.indexOf(a.kind) - ORDER.indexOf(b.kind) || b.at - a.at)
}

/** Two agents holding one file, plus two tasks declaring overlapping scope: the map's crossings. */
export function crossings(ov: WorkOverview | null, d: Derived | null): number {
  return (d?.contested.length ?? 0) + (ov?.overlaps.length ?? 0)
}

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
  crossings: number
}

/** One tile per top-level folder that someone holds or plans to touch, busiest first. */
export function areas(ov: WorkOverview | null, d: Derived | null): Area[] {
  const by = new Map<string, Area>()
  const at = (name: string) => {
    let a = by.get(name)
    if (!a) by.set(name, (a = { name, holders: [], planned: [], crossings: 0 }))
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
  for (const c of d?.contested ?? []) at(areaOf(c.path)).crossings++
  for (const o of ov?.overlaps ?? []) {
    const names = new Set(o.reasons.map((r) => areaOf(r.left_path)))
    for (const name of names) at(name).crossings++
  }
  const weight = (a: Area) => a.crossings * 100 + a.holders.length * 10 + a.planned.length
  return [...by.values()].sort((a, b) => weight(b) - weight(a) || a.name.localeCompare(b.name))
}
