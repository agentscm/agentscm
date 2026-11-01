import { useEffect, useState, type ReactNode } from 'react'
import { SectionTabs } from '../TopNav.tsx'
import {
  fetchBriefing,
  hasMoreBriefing,
  workIdFromHash,
  workMatchesSearch,
  type BriefingQuery,
  type StandupBriefing,
  type BriefingEvent,
  type WorkItem,
  type WorkOverview,
  type WorkState,
} from '../standup.ts'
import { agentLabel, Ago, Avatar, Empty, Explain, Icon, Notice, PageHead, Skeleton, Tip, localDate } from '../ui.tsx'
import { FINISHED_MEANS_AGENT_SAID, SEARCH_THIS_PAGE_ONLY } from '../copy.ts'
import { who } from '../nextStep.ts'
import { usePoll, type Poll } from '../usePoll.ts'
import { hashPath, Pager } from '../Pager.tsx'
import { WorkDetail } from './WorkDetail.tsx'
import type { Handoff } from '../launchLink.ts'
import { ProofBadge } from '../Proof.tsx'
import { lead, plural, Search, Source, STATE, StatePill, ClaimPill, WorkLink } from './workParts.tsx'

// Re-exported so importers of this module keep working after the split.
export { InboxPage } from './Inbox.tsx'
export { STATE, lead, StatePill, ClaimPill, LaneBar } from './workParts.tsx'

const LANES: WorkState[] = ['active', 'blocked', 'handover', 'paused']

const EVENT_LABEL: Record<string, string> = { work_started: 'Started work', work_joined: 'Joined work', work_takeover: 'Took over the claim', work_checkpoint: 'Reported a checkpoint', work_pr_link: 'Relationship changed' }
const EVENT_ICON: Record<string, string> = { work_started: 'play-fill', work_joined: 'person-plus', work_takeover: 'arrow-left-right', work_checkpoint: 'flag', work_pr_link: 'git' }

const spanText = (seconds: number) => (seconds <= 48 * 3600 ? plural(Math.max(1, Math.round(seconds / 3600)), 'hour') : plural(Math.round(seconds / 86400), 'day'))

function Action({ item }: { item: WorkItem }) {
  if (item.state === 'finished') return item.current ? <div className="action"><span className="action-label">Latest report</span>{item.current.summary}</div> : null
  if (item.blocker) return <div className="action is-blocked"><span className="action-label"><Icon name="sign-stop" /> Blocker</span>{item.blocker.summary}</div>
  if (item.next_action) return <div className="action"><span className="action-label"><Icon name="arrow-right-circle" /> Next</span>{item.next_action}</div>
  return <div className="muted small">No next action reported</div>
}

function WorkCard({ item }: { item: WorkItem }) {
  const l = lead(item)
  const helpers = item.participants.filter((p) => p.role !== 'lead')
  return (
    <article className={`work-card is-${item.state}`}>
      <h3 className="work-title">
        <WorkLink id={item.id}>{item.title}</WorkLink>
      </h3>
      <div className="work-owner">
        <Avatar name={l.agent} size={28} />
        <span>
          {l.login}’s <strong title={l.agent}>{agentLabel(l.agent)}</strong>
        </span>
      </div>
      <div className="pills">
        {/* A finished card says so once: the proof badge carries the caveat, the foot carries the time. */}
        {item.state === 'finished' ? <ProofBadge level="reported" tone="muted" /> : <StatePill state={item.state} />}
        {/* In progress, a live claim is the normal case; elsewhere "Assigned" says the lead still holds it. */}
        {(item.state !== 'active' || item.claim_status !== 'active') && <ClaimPill item={item} />}
      </div>
      <Action item={item} />
      {helpers.length > 0 && (
        <div className="small muted">
          <Icon name="people" /> {helpers.slice(0, 2).map((p) => `${agentLabel(p.agent)} (${p.role})`).join(', ')}
          {helpers.length > 2 && <> · <WorkLink id={item.id}>+{helpers.length - 2} more</WorkLink></>}
        </div>
      )}
      <div className="work-foot small muted">
        <Icon name="clock" /> Checked in <Ago at={item.last_checkin_at} />
        <span className="ms-auto">
          {item.source.kind !== 'native' && <Source item={item} />}
        </span>
      </div>
    </article>
  )
}

function Board({ items, showFinished = false }: { items: WorkItem[]; showFinished?: boolean }) {
  const lanes = showFinished ? [...LANES, 'finished' as const] : LANES
  const [mobileLane, setMobileLane] = useState<WorkState>(() => lanes.find((s) => items.some((i) => i.state === s)) ?? 'active')
  return (
    <>
      <div className="lane-picker" role="group" aria-label="Choose a lane">
        {lanes.map((s) => (
          <button key={s} type="button" className={`btn btn-sm ${mobileLane === s ? 'btn-secondary' : 'btn-outline-secondary'}`} aria-pressed={mobileLane === s} onClick={() => setMobileLane(s)}>
            {STATE[s].label} · {items.filter((i) => i.state === s).length}
          </button>
        ))}
      </div>
      <div data-tour="work-lanes" className={`board lanes-${lanes.length}`}>
        {lanes.map((s) => {
          const inLane = items.filter((i) => i.state === s)
          return (
            <section key={s} className={`lane tone-${STATE[s].tone}${mobileLane === s ? ' is-picked' : ''}`} aria-label={STATE[s].label}>
              <header className="lane-head">
                <Icon name={STATE[s].icon} />
                <h2 className="lane-title">{STATE[s].label}</h2>
                <span className="lane-count">{inLane.length}</span>
                <Explain text={STATE[s].tip} />
              </header>
              <div className="lane-body">
                {inLane.length === 0 ? <p className="lane-empty">Nothing {STATE[s].label.toLowerCase()}</p> : inLane.map((i) => <WorkCard key={i.id} item={i} />)}
              </div>
            </section>
          )
        })}
      </div>
    </>
  )
}

/* ---------- Recently finished ---------- */

function Finished({ overview, search }: { overview: WorkOverview; search: string }) {
  const all = overview.recent.filter((o) => o.kind === 'completion')
  const visible = all.filter((o) => workMatchesSearch(o.item, search)).sort((a, b) => b.at - a.at || b.seq - a.seq)
  return (
    <div className="stack">
      {!overview.recent_complete && <Notice title="This window is incomplete">{overview.history_retention_gap ? 'Some outcomes in this window were compacted from retained history.' : 'The overview limit was reached; more completions exist.'}</Notice>}
      {visible.length === 0 ? (
        <Empty icon="flag" title={all.length ? 'No finished task matches your search' : 'Nothing reported finished in this window'}>
          {!all.length && 'When an agent reports a task finished, it shows here.'}
        </Empty>
      ) : (
        // The same row as All repositories' "Done lately": title and time, who, then the agent's last report.
        <>
          <h2 className="visually-hidden">Reported finished</h2>
          <ul className="rp-done" aria-label="Reported finished">
            {visible.map((o) => {
              const l = lead(o.item)
              return (
                <li key={o.seq}>
                  <WorkLink id={o.item.id}>{o.item.title}</WorkLink>
                  <span className="rp-done-at"><Ago at={o.at} /></span>
                  <span className="rp-done-who">{who(l.login, l.agent)} · <ProofBadge level="reported" tone="muted" /></span>
                  {o.item.current && <span className="rp-done-say">{o.item.current.summary}</span>}
                </li>
              )
            })}
          </ul>
        </>
      )}
    </div>
  )
}

/* ---------- Change history ---------- */

/** One page of changes, newest first. The Hub pages forward by sequence, oldest first, and a sequence is not a time. */
export const newestFirst = (events: readonly BriefingEvent[]): BriefingEvent[] => [...events].sort((a, b) => b.at - a.at || b.seq - a.seq)

function changesAnswer(events: readonly BriefingEvent[]) {
  if (events.length === 0) return 'No changes on this page.'
  return <><b>{plural(events.length, 'change')}</b> on this page, the latest <Ago at={Math.max(...events.map((e) => e.at))} />.</>
}

function HistoryHead({ answer }: { answer: ReactNode }) {
  return (
    <>
      <PageHead question="What changed in Work, and when?" answer={answer} fact="Starts, joins, takeovers and checkpoints, newest first. Next goes forward in time." />
      <SectionTabs />
    </>
  )
}

function History({ token }: { token: string }) {
  const [queries, setQueries] = useState<BriefingQuery[]>([{ limit: 10 }])
  const [page, setPage] = useState(0)
  const [view, setView] = useState<'changes' | 'current'>('changes')
  const [search, setSearch] = useState('')
  const [filter, setFilter] = useState<'all' | WorkState>('all')
  const query = queries[page]!
  const load = usePoll((signal) => fetchBriefing(token, signal, query), 0, JSON.stringify(query))
  if (load.kind === 'loading') return <><HistoryHead answer="Checking what changed…" /><Skeleton rows={6} /></>
  if (load.kind === 'error')
    return (
      <>
        <HistoryHead answer="The change history could not be loaded." />
        <Notice tone="bad" title="Could not load the change history" action={<button className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>}>
          {load.message}
        </Notice>
      </>
    )
  const b: StandupBriefing = load.value
  const items = b.items.filter((i) => workMatchesSearch(i, search))
  const events = newestFirst(b.events).filter((e) => (filter === 'all' || e.item.state === filter) && workMatchesSearch(e.item, search))
  const canNext = hasMoreBriefing(b, view)
  return (
    <>
    <HistoryHead answer={changesAnswer(b.events)} />
    <div className="stack">
      {b.retention_gap && <Notice title="Some history is no longer retained">The current work snapshot is complete, but this change history has a gap.</Notice>}
      <div className="toolbar">
        <div className="lens" role="tablist" aria-label="History view">
          <button type="button" role="tab" aria-selected={view === 'changes'} className={`lens-seg${view === 'changes' ? ' on' : ''}`} onClick={() => setView('changes')}>
            <Icon name="list-ul" /> Recent changes
          </button>
          <button type="button" role="tab" aria-selected={view === 'current'} className={`lens-seg${view === 'current' ? ' on' : ''}`} onClick={() => setView('current')}>
            <Icon name="kanban" /> Work on this page
          </button>
        </div>
        <Search value={search} onChange={setSearch} placeholder="Search this page" />
        {view === 'changes' && (
          <select className="form-select form-select-sm w-auto" aria-label="Filter by status" value={filter} onChange={(e) => setFilter(e.target.value as 'all' | WorkState)}>
            <option value="all">All statuses</option>
            {(Object.keys(STATE) as WorkState[]).map((s) => (
              <option key={s} value={s}>
                {STATE[s].label}
              </option>
            ))}
          </select>
        )}
      </div>
      {search.trim() && <div className="small muted" aria-live="polite">{SEARCH_THIS_PAGE_ONLY}</div>}
      {view === 'current' ? (
        <Board items={items} showFinished={b.items.some((i) => i.state === 'finished')} />
      ) : events.length === 0 ? (
        <Empty icon="list" title="No changes on this page">
          {!search.trim() && filter === 'all' && 'Changes appear here when an agent starts, joins or checks in on a task.'}
        </Empty>
      ) : (
        <ol className="change-feed box">
          {events.map((e) => {
            const l = lead(e.item)
            return (
              <li key={e.seq} className="change-row">
                <span className={`change-mark is-${e.kind}`} aria-hidden="true"><Icon name={EVENT_ICON[e.kind] ?? 'dot'} /></span>
                <div className="change-main">
                  <WorkLink id={e.work_id}>{e.item.title}</WorkLink>
                  <div className="change-meta">
                    <span>{EVENT_LABEL[e.kind] ?? e.kind}</span>
                    <span className="change-who"><Avatar name={l.agent} size={18} /> <span title={l.agent}>{who(l.login, l.agent)}</span></span>
                    <StatePill state={e.item.state} />
                  </div>
                  {e.item.current && e.kind === 'work_checkpoint' && <p className="change-quote">“{e.item.current.summary}”</p>}
                </div>
                <span className="change-when"><Ago at={e.at} /></span>
              </li>
            )
          })}
        </ol>
      )}
      <Pager
        newerLabel="Previous"
        olderLabel="Next"
        status={`Page ${page + 1}`}
        onNewer={page > 0 ? () => setPage((p) => p - 1) : undefined}
        onOlder={!canNext ? undefined : () => {
          const next: BriefingQuery = { after: b.next_after, ...(b.next_work_after ? { workAfter: b.next_work_after } : {}), workDone: b.next_work_done, through: b.through, limit: query.limit }
          setQueries((all) => [...all.slice(0, page + 1), next])
          setPage((p) => p + 1)
        }}
      />
    </div>
    </>
  )
}

/* ---------- Pages ---------- */

type Tab = 'working' | 'finished' | 'coordination' | 'history'
const TABS: readonly Tab[] = ['working', 'finished', 'coordination', 'history']
/** `#work/<tab>` (work ids are `wrk_…`, so the two never collide). Working and coordination now live on the Dashboard and in the Inbox. */
export function workTabFromHash(hash = window.location.hash): Tab {
  const t = hashPath(hash).slice(1).split('/')[1] as Tab
  return TABS.includes(t) ? t : 'working'
}
export const workTabHref = (t: Tab) => (t === 'working' ? '#work' : `#work/${t}`)

function useWorkRoute() {
  const [route, setRoute] = useState(() => ({ id: workIdFromHash(window.location.hash), tab: workTabFromHash() }))
  useEffect(() => {
    const on = () => setRoute({ id: workIdFromHash(window.location.hash), tab: workTabFromHash() })
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  return route
}

/** History › Finished and Changes, and the page of one task. */
export function WorkPage({ token, overview, handoff }: { token: string; overview: Poll<WorkOverview>; handoff?: Handoff }) {
  const { id, tab } = useWorkRoute()
  const [search, setSearch] = useState('')
  if (id) return <WorkDetail id={id} token={token} overview={overview} handoff={handoff} />
  const ov = overview.kind === 'ready' ? overview.value : null
  const finished = ov?.recent.filter((o) => o.kind === 'completion').length ?? 0
  const history = tab === 'history'
  return (
    <div className="page history-page">
      {!history && (
        <PageHead
          question="What did agents report finished?"
          answer={
            ov ? (
              finished === 0 ? 'Nothing was reported finished recently.' : (
                <>
                  <b>{plural(finished, 'task')}</b> reported finished in the{' '}
                  <Tip text={`${localDate(ov.recent_since)} to ${localDate(ov.until)}`} focusable>
                    <span>last {spanText(ov.until - ov.recent_since)}</span>
                  </Tip>
                  .
                </>
              )
            ) : overview.kind === 'error' ? 'The finished count could not be loaded.' : 'Checking what was reported finished…'
          }
          fact={FINISHED_MEANS_AGENT_SAID}
        />
      )}
      {!history && <SectionTabs />}
      {!history && overview.kind === 'error' && (
        <Notice tone="bad" title="Could not load the work overview" action={<button className="btn btn-sm btn-outline-secondary" onClick={overview.retry}>Retry</button>}>
          {overview.message}
        </Notice>
      )}
      {!history && !ov && overview.kind === 'loading' && <Skeleton rows={6} />}
      {!history && ov && (
        <>
          <Search value={search} onChange={setSearch} placeholder="Search agents, tasks, or paths" />
          <Finished overview={ov} search={search} />
        </>
      )}
      {history && <History token={token} />}
    </div>
  )
}
