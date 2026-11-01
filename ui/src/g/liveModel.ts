// Where each unfinished change stands, from what the Hub can see without opening it: an open pull request for its
// branch, a skill gate run on its branch, or a declared-scope overlap. Same six steps as the change page's lifecycle.
import { STATE } from '../pages/workParts.tsx'
import type { RunView } from '../skills.ts'
import type { WorkItem, WorkOverview } from '../standup.ts'
import type { PullRequest } from '../types.ts'

export const STEPS = [
  { key: 'claimed', label: 'Claimed', icon: 'person-check' },
  { key: 'working', label: 'Working', icon: 'play-circle' },
  { key: 'coordination', label: 'Coordination', short: 'Coordinate', icon: 'people' },
  { key: 'checks', label: 'Checks', icon: 'clipboard2-check' },
  { key: 'merge', label: 'Merge', icon: 'sign-merge-right' },
  { key: 'finished', label: 'Finished', icon: 'flag' },
] as const

const CHECK_GATES = new Set(['tests', 'review', 'fix', 'ci'])
const MERGE_GATES = new Set(['approval', 'merge', 'post_merge'])

/** The furthest step the Hub has a reason to place this change at. */
export function stepOf(item: WorkItem, pulls: PullRequest[], runs: RunView[], ov: WorkOverview | null): number {
  if (item.state === 'finished') return 5
  const branch = item.branch
  if (branch && pulls.some((p) => p.head_ref === branch && p.state === 'open')) return 4
  const run = branch ? runs.find((r) => r.branch === branch) : undefined
  if (run?.current_gate && MERGE_GATES.has(run.current_gate)) return 4
  if (run?.current_gate && CHECK_GATES.has(run.current_gate)) return 3
  if (ov?.overlaps.some((o) => o.left.id === item.id || o.right.id === item.id)) return 2
  return 1
}

export type LaneTone = 'brand' | 'bad' | 'warn' | 'ok' | 'handover'

/** Blocked is trouble; quiet, stale or paused is waiting; a handover is offered; everything else is moving. */
export function toneOf(item: WorkItem): LaneTone {
  if (item.state === 'blocked') return 'bad'
  if (item.state === 'finished') return 'ok'
  if (item.claim_status !== 'active' || item.state === 'paused') return 'warn'
  if (item.state === 'handover') return 'handover'
  return 'brand'
}

export function stateText(item: WorkItem): string {
  if (item.state !== 'finished' && item.claim_status !== 'active') return item.claim_status === 'stale' ? 'Quiet · still assigned' : 'Check-in unknown · still assigned'
  return STATE[item.state].label
}

export const dotTone = (t: LaneTone): 'brand' | 'bad' | 'warn' | 'ok' => (t === 'handover' ? 'brand' : t)

/** Open pull requests keyed by branch, for "PR #n" labels. */
export const prFor = (item: WorkItem, pulls: PullRequest[]) => (item.branch ? pulls.find((p) => p.head_ref === item.branch && p.state === 'open') : undefined)
