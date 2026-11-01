// B · Process canvas: the lifecycle graph is home. Each stage shows how many changes sit there and whether people
// are needed; selecting a stage lists its changes, and a change opens in a drawer over the canvas.
import { Box, Icon, PageHead } from '../ui.tsx'
import { NEEDS, STAGES, WORK, needsFor, type StageId } from './data.ts'
import { Go, Graph, InboxView, NeedRow, RepositoryView, SetupSummary, SetupView, ValueStrip, ValueView, WorkDetail, WorkRow, WorkflowsView, useProto } from './kit.tsx'
import { TopBar } from './shell.tsx'

const TABS = [
  { to: '', key: '', label: 'Lifecycle', icon: 'diagram-3' },
  { to: 'workflows', key: 'workflows', label: 'Workflows', icon: 'sliders' },
  { to: 'repository', key: 'repository', label: 'Repository', icon: 'folder2-open' },
  { to: 'setup', key: 'setup', label: 'Setup', icon: 'plug' },
]

export function VariantB() {
  const { route } = useProto()
  const [page = '', id] = route.parts
  const canvas = page === '' || page === 'stage' || page === 'work'
  return (
    <div className="app-shell px-norail px-shell-b">
      <TopBar />
      <div className="content">
        <main className="main px-main-wide" id="main" tabIndex={-1}>
          <nav className="px-tabs" aria-label="Sections">
            {TABS.map((t) => <Go key={t.key} to={t.to} className={(canvas ? '' : page) === t.key ? 'on' : ''}><Icon name={t.icon} /> {t.label}</Go>)}
          </nav>
          {canvas && <Lifecycle stage={page === 'stage' ? (id as StageId) : page === 'work' ? WORK.find((w) => w.id === id)?.stage : undefined} />}
          {page === 'workflows' && <WorkflowsView flowId={id} />}
          {page === 'repository' && <RepositoryView />}
          {page === 'setup' && <SetupView />}
          {page === 'value' && <ValueView />}
          {page === 'inbox' && <InboxView />}
        </main>
      </div>
      {page === 'work' && id && <Drawer id={id} />}
    </div>
  )
}

function Lifecycle({ stage }: { stage?: StageId }) {
  const { role, resolved, go } = useProto()
  const mine = needsFor(role).filter((n) => !resolved.has(n.id))
  // A stage glows when a person is needed there, not when it is merely busy.
  const waiting = new Set(NEEDS.filter((n) => !resolved.has(n.id)).flatMap((n) => n.work).map((w) => WORK.find((x) => x.id === w)?.stage))
  const sel = stage ?? (STAGES.find((s) => waiting.has(s.id))?.id ?? 'review')
  const meta = STAGES.find((s) => s.id === sel)
  const here = WORK.filter((w) => w.stage === sel)
  return (
    <>
      <PageHead question="Where is every change?" answer={`${WORK.length} changes on their way. ${mine.length ? `${mine.length} need${mine.length === 1 ? 's' : ''} you.` : 'Nothing needs you.'}`} />
      {mine.length > 0 && <div className="px-ribbon">{mine.map((n) => <NeedRow key={n.id} n={n} card />)}</div>}
      <section className="px-canvas" aria-label="Lifecycle">
        <div className="px-canvas-head">
          <span><Icon name="diagram-3" /> Get changes ready for review</span>
          <Go to="workflows/f1" className="px-link">Edit this process</Go>
        </div>
        <Graph
          big
          nodes={STAGES.map((s) => {
            const n = WORK.filter((w) => w.stage === s.id).length
            return { id: s.id, title: s.label, sub: s.plain, icon: s.icon, badge: String(n), tone: waiting.has(s.id) ? 'warn' : n ? 'brand' : 'muted' }
          })}
          loop={{ from: 'review', to: 'building', label: 'Findings go back to the author' }}
          selected={sel}
          onNode={(sid) => go(`stage/${sid}`)}
        />
      </section>
      <div className="px-tower">
        <Box icon={meta?.icon ?? 'circle'} title={meta?.label ?? ''} count={here.length} intro={meta?.plain}>
          <div className="px-rows">{here.map((w) => <WorkRow key={w.id} w={w} />)}</div>
          {here.length === 0 && <p className="small muted mb-0">No change is at this stage.</p>}
        </Box>
        <div className="px-col">
          <ValueStrip />
          <SetupSummary />
        </div>
      </div>
    </>
  )
}

function Drawer({ id }: { id: string }) {
  const w = WORK.find((x) => x.id === id)
  return (
    <div className="px-drawer-scrim">
      <aside className="px-drawer" aria-label="Change">
        <Go to={w ? `stage/${w.stage}` : ''} className="px-link px-drawer-close"><Icon name="x-lg" /> Back to the lifecycle</Go>
        <WorkDetail id={id} />
      </aside>
    </div>
  )
}
