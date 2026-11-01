// The sentence that opens each dashboard tab inside the G layout. The classic Dashboard sums up every task above all four
// tabs; G has its own Now page, so the other three answer their own question. Undefined until the data has loaded, so the
// page keeps the Dashboard's skeleton or waiting line.
import type { ReactNode } from 'react'
import type { Derived } from '../derive.ts'
import { AWAKE_SECONDS, type DashTab } from '../pages/Dashboard.tsx'
import { plural } from '../pages/workParts.tsx'
import type { HubEvent } from '../types.ts'
import { Ago } from '../ui.tsx'

/** `asOfMs` is when the data was read (the last successful poll), as for `dashboardStatus`. */
export function gAnswer(section: DashTab, d: Derived | null, events: HubEvent[] | null, asOfMs: number | null): ReactNode {
  if (section === 'activity') {
    const latest = events?.[0]
    return events && (latest ? <>The latest event was <Ago at={latest.at} />.</> : <>Nothing has been recorded here yet.</>)
  }
  if (!d) return undefined
  if (section === 'team') {
    if (d.agents.length === 0) return <>No agent is connected right now.</>
    const up = asOfMs ? d.agents.filter((a) => asOfMs / 1000 - a.lastActive < AWAKE_SECONDS).length : 0
    return <><b>{up} of {plural(d.agents.length, 'agent')}</b> awake, across {plural(d.groups.length, 'person', 'people')}.</>
  }
  if (section === 'now') {
    if (d.filesHeld === 0) return <>No agent holds a file right now.</>
    const holders = d.agents.filter((a) => a.holdings.length > 0).length
    return <><b>{plural(d.filesHeld, 'file')}</b> held by {plural(holders, 'agent')}, {d.contested.length ? <><b>{d.contested.length}</b> contested</> : 'none contested'}.</>
  }
  return undefined
}
