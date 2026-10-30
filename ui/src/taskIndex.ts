// Look up a task by branch or work ID, from the work overview the hub already serves.
//
// API for other packages:
//   useTaskIndex(token)   { byBranch, byId } maps over current and recent tasks, or null while loading or on error
//   indexTasks(ov)        the same maps, from an overview you already hold
import { useMemo } from 'react'
import { fetchOverview, type WorkItem, type WorkOverview } from './standup.ts'
import { usePoll } from './usePoll.ts'

export interface TaskIndex {
  byBranch: Map<string, WorkItem>
  byId: Map<string, WorkItem>
}

/** Current tasks first, so a branch reused by a finished task maps to the live one. Branchless tasks are only in `byId`. */
export function indexTasks(ov: WorkOverview): TaskIndex {
  const byBranch = new Map<string, WorkItem>()
  const byId = new Map<string, WorkItem>()
  for (const item of [...ov.current, ...ov.recent.map((o) => o.item)]) {
    if (!byId.has(item.id)) byId.set(item.id, item)
    if (item.branch && !byBranch.has(item.branch)) byBranch.set(item.branch, item)
  }
  return { byBranch, byId }
}

/** Tasks by branch and ID, refreshed every minute. Null while loading or on error: callers render nothing extra. */
export function useTaskIndex(token: string): TaskIndex | null {
  const load = usePoll((s) => fetchOverview(token, s), 60_000, `tasks|${token}`)
  const ov = load.kind === 'ready' ? load.value : null
  return useMemo(() => (ov ? indexTasks(ov) : null), [ov])
}
