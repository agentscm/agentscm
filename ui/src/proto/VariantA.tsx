// A · Control tower: the rail carries Now · Work · Workflows · Repository; Setup is a utility. Now is calm:
// decisions for your role, then work in motion, then what changed, with contributions underneath.
import type { ReactNode } from 'react'
import { PageHead, Box, Icon } from '../ui.tsx'
import { WORK, needsFor, type Role } from './data.ts'
import { ChangedList, Go, InboxView, NeedRow, RepositoryView, SetupSummary, SetupView, ValueStrip, ValueView, WorkBoard, WorkDetail, WorkRow, WorkflowsView, useProto } from './kit.tsx'
import { TopBar } from './shell.tsx'

const NAV = [
  { to: '', key: '', icon: 'bell', label: 'Now', q: 'What needs me?' },
  { to: 'work', key: 'work', icon: 'kanban', label: 'Work', q: 'Where is each change?' },
  { to: 'workflows', key: 'workflows', icon: 'diagram-3', label: 'Workflows', q: 'What process applies?' },
  { to: 'repository', key: 'repository', icon: 'folder2-open', label: 'Repository', q: 'What do we know about this code?' },
]

export const ANSWER: Record<Role, (n: number, moving: number) => string> = {
  author: (n, m) => n ? `${n === 1 ? 'One thing needs' : `${n} things need`} you. ${m} changes are moving without you.` : `Nothing needs you. ${m} changes are moving.`,
  reviewer: (n, m) => n ? `${n === 1 ? 'One change is' : `${n} changes are`} ready for your decision. ${m} more are moving.` : `No decisions waiting. ${m} changes are moving.`,
  lead: (n, m) => n ? `${n} ownership and setup questions are open. ${m} changes are moving.` : `Ownership is settled. ${m} changes are moving.`,
}

export const VariantA = () => <RailShell now={<Now />} />

/** A's frame: top bar, the four-item rail with Setup as a utility, and the sheet. The A+B options swap only `now`. */
export function RailShell({ now }: { now: ReactNode }) {
  const { route, role, resolved } = useProto()
  const [page = '', id] = route.parts
  const open = needsFor(role).filter((n) => !resolved.has(n.id))
  return (
    <div className="app-shell px-shell-a">
      <TopBar />
      <aside className="side" aria-label="Sections">
        <nav className="side-nav">
          <div className="side-group">
            {NAV.map((n) => (
              <Go key={n.key} to={n.to} className={`side-link${page === n.key ? ' active' : ''}`}>
                <span className="side-icon"><Icon name={n.icon} /></span>
                <span className="side-text"><span className="side-label">{n.label}</span><span className="side-q">{n.q}</span></span>
                {n.key === '' && open.length > 0 && <span className="side-badge tone-warn">{open.length}</span>}
              </Go>
            ))}
          </div>
        </nav>
        <div className="side-foot px-side-foot">
          <Go to="setup" className={`side-link${page === 'setup' ? ' active' : ''}`}><span className="side-icon"><Icon name="plug" /></span><span className="side-text"><span className="side-label">Setup</span></span></Go>
        </div>
      </aside>
      <div className="content">
        <main className="main" id="main" tabIndex={-1}>
          {page === '' && now}
          {page === 'work' && (id ? <WorkDetail id={id} /> : <WorkPage />)}
          {page === 'workflows' && <><PageHead question="What process applies here?" answer={id ? 'One workflow, in plain words first.' : 'Your repository already has a process. Here it is.'} /><WorkflowsView flowId={id} /></>}
          {page === 'repository' && <><PageHead question="What do we know about this code?" answer="Six areas. Pick a lens to see what is true about each." /><RepositoryView /></>}
          {page === 'setup' && <><PageHead question="Is everything connected?" answer="Everything you rely on is connected. One agent needs a fix." /><SetupView /></>}
          {page === 'value' && <><PageHead question="What did AgentSCM contribute?" answer="Four contributions this week, each with its record." /><ValueView /></>}
          {page === 'inbox' && <><PageHead question="What conversations are open?" answer="Every open decision, for every role. Answer once; it’s answered everywhere." /><InboxView /></>}
        </main>
      </div>
    </div>
  )
}

function Now() {
  const { role, resolved } = useProto()
  const mine = needsFor(role)
  const open = mine.filter((n) => !resolved.has(n.id))
  const moving = WORK.filter((w) => w.tone === 'brand' || w.stage === 'checks')
  return (
    <>
      <PageHead question="What needs me?" answer={ANSWER[role](open.length, moving.length)} />
      <div className="px-tower">
        <div className="px-col">
          <Box icon="bell" title="Needs you" count={open.length} tone={open.length ? 'warn' : 'ok'} explain="Only items a person can act on now, ordered by risk, what they block and how long they have waited.">
            {mine.length === 0 && <p className="small muted mb-0">Nothing for your role. Switch roles in the bar below to see what others see.</p>}
            <div className="px-needs">{mine.map((n) => <NeedRow key={n.id} n={n} />)}</div>
          </Box>
          <Box icon="clock-history" title="What changed" count={3}><ChangedList /></Box>
        </div>
        <div className="px-col">
          <Box icon="activity" title="In motion" count={WORK.length} actions={<Go to="work" className="px-link">Board</Go>} explain="Every change, with its owner, actual stage and freshness. Agents fixing a known failure stay here until they need a decision.">
            <div className="px-rows">{WORK.map((w) => <WorkRow key={w.id} w={w} />)}</div>
          </Box>
          <ValueStrip />
        </div>
      </div>
      <SetupSummary />
    </>
  )
}

function WorkPage() {
  return (
    <>
      <PageHead question="Where is each change?" answer="Seven changes. Two are waiting on people; the rest are moving." />
      <WorkBoard />
    </>
  )
}
