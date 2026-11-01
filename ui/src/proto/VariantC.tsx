// C · Quiet: one column of decisions for your role, each shown the way it also reaches Slack and your agent.
// Work in motion stays folded; everything else is a sheet behind ⌘K. The Hub as the last place you need.
import { useState } from 'react'
import { Icon } from '../ui.tsx'
import { WORK, needsFor, work, type Need, type Role } from './data.ts'
import { Go, InboxView, NeedRow, RepositoryView, SetupSummary, SetupView, ValueStrip, ValueView, WorkDetail, WorkRow, WorkflowsView, useProto } from './kit.tsx'
import { Sheet, TopBar, openPalette } from './shell.tsx'

const GREETING: Record<Role, (n: number) => string> = {
  author: (n) => n ? `${n === 1 ? 'One thing needs' : `${n} things need`} you. Your agents have the rest.` : 'Nothing needs you. Your agents have it.',
  reviewer: (n) => n ? `${n === 1 ? 'One change is' : `${n} changes are`} ready for your decision.` : 'No decisions waiting.',
  lead: (n) => n ? `${n} questions about ownership and setup.` : 'Ownership is settled.',
}

export function VariantC() {
  const { route } = useProto()
  const [page = '', id] = route.parts
  return (
    <div className="app-shell px-norail px-shell-c">
      <TopBar minimal />
      <div className="content">
        <main className="main px-main-quiet" id="main" tabIndex={-1}>
          {page === '' ? <Quiet /> : (
            <Sheet wide={page !== 'inbox'}>
              <Go to="" className="px-link px-back"><Icon name="arrow-left" /> Now</Go>
              {page === 'work' && (id ? <WorkDetail id={id} /> : <div className="px-rows">{WORK.map((w) => <WorkRow key={w.id} w={w} />)}</div>)}
              {page === 'workflows' && <WorkflowsView flowId={id} />}
              {page === 'repository' && <RepositoryView />}
              {page === 'setup' && <SetupView />}
              {page === 'value' && <ValueView />}
              {page === 'inbox' && <InboxView />}
            </Sheet>
          )}
        </main>
      </div>
    </div>
  )
}

function Quiet() {
  const { role, resolved } = useProto()
  const mine = needsFor(role)
  const open = mine.filter((n) => !resolved.has(n.id))
  const [moving, setMoving] = useState(false)
  return (
    <div className="px-quiet">
      <p className="px-quiet-hello">{GREETING[role](open.length)}</p>
      <div className="px-needs">{mine.map((n) => <Decision key={n.id} n={n} />)}</div>
      <button type="button" className="px-fold" aria-expanded={moving} onClick={() => setMoving(!moving)}>
        <Icon name={moving ? 'chevron-down' : 'chevron-right'} /> Moving without you <span className="px-count">{WORK.length}</span>
      </button>
      {moving && <div className="px-rows">{WORK.map((w) => <WorkRow key={w.id} w={w} dense />)}</div>}
      <ValueStrip />
      <SetupSummary />
      <p className="px-hint"><button type="button" className="px-link" onClick={openPalette}><kbd>⌘ K</kbd> Work, workflows, the repository and setup</button></p>
    </div>
  )
}

/** One decision, and the same decision as it reaches Slack and the agent. Answering anywhere answers it everywhere. */
function Decision({ n }: { n: Need }) {
  const [where, setWhere] = useState<'hub' | 'slack' | 'agent'>('hub')
  const w = n.work[0] ? work(n.work[0]) : undefined
  return (
    <div className="px-decision">
      {where === 'hub' && <NeedRow n={n} card />}
      {where === 'slack' && <SlackPreview n={n} />}
      {where === 'agent' && <AgentPreview n={n} title={w?.title} />}
      <div className="px-surfaces" role="group" aria-label="Where this decision appears">
        <span className="small muted">Also reaches</span>
        {(['hub', 'slack', 'agent'] as const).map((s) => <button key={s} type="button" className={where === s ? 'on' : ''} onClick={() => setWhere(s)}><Icon name={s === 'hub' ? 'window' : s === 'slack' ? 'slack' : 'terminal'} /> {s === 'hub' ? 'Hub' : s === 'slack' ? 'Slack' : 'Your agent'}</button>)}
      </div>
    </div>
  )
}

function SlackPreview({ n }: { n: Need }) {
  const { resolved, resolve } = useProto()
  const done = resolved.has(n.id)
  return (
    <div className="px-slack">
      <div className="px-slack-head"><span className="px-slack-app">A</span><strong>AgentSCM</strong><span className="px-slack-tag">APP</span><span className="small muted">direct message</span></div>
      <p><strong>{n.title}</strong><br />{n.reason}</p>
      {done ? <p className="px-ok-text small mb-0"><Icon name="check2" /> Answered. The Hub and the agent show the same answer.</p>
        : <div className="px-slack-actions"><button type="button" onClick={() => resolve(n.id)}>{n.action}</button><button type="button">Open in the Hub</button></div>}
    </div>
  )
}

function AgentPreview({ n, title }: { n: Need; title?: string }) {
  const { resolved } = useProto()
  return (
    <div className="px-agent">
      <div className="px-agent-head"><Icon name="terminal" /> claude-code · inline view</div>
      <div className="px-agent-card">
        <div className="small muted">AgentSCM · {title ?? 'this change'}</div>
        <strong>{resolved.has(n.id) ? 'Answered by a person' : n.title}</strong>
        <p className="small mb-1">{resolved.has(n.id) ? 'Continue with the answer you were given.' : n.reason}</p>
        <code className="small">agentscm_coordination_inbox → 1 open</code>
      </div>
    </div>
  )
}
