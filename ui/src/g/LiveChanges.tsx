// G · Work over live data: every unfinished change by lifecycle step, and one change with its status, the six
// lifecycle steps and how each is known, the evidence behind them, and the Hub's full record one click away.
import { useState } from 'react'
import { Ago, Avatar, Box, Icon, Notice, PageHead, Pill, Reveal, Skeleton } from '../ui.tsx'
import { usePoll, type Poll } from '../usePoll.ts'
import { fetchStandupWork, fetchCoordination, workHref, type WorkItem } from '../standup.ts'
import { fetchChangeRecord } from '../changeRecord.ts'
import { httpsOnly, listPublications } from '../publications.ts'
import { fetchWork } from '../api.ts'
import { lifecycle } from '../lifecycle.ts'
import { fetchRuns } from '../skills.ts'
import { PROOF } from '../proof.ts'
import { lead, plural } from '../pages/workParts.tsx'
import { blockedStep, handoverStep, quietStep, who } from '../nextStep.ts'
import { scrollToAnchor } from '../LifecycleStrip.tsx'
import { StepAction, useStepActions } from '../NextStep.tsx'
import { StartTask } from '../StartTask.tsx'
import { WorkDetail } from '../pages/WorkDetail.tsx'
import type { LiveProps } from './GLive.tsx'
import { LiveLane, LiveTrack, useRuns } from './LiveNow.tsx'
import { STEPS, dotTone, prFor, stateText, stepOf, toneOf } from './liveModel.ts'

const FILTERS = [
  { id: 'all', label: 'Every change', test: () => true },
  { id: 'people', label: 'Waiting on people', test: (w: WorkItem) => w.state === 'blocked' || w.state === 'handover' || w.claim_status !== 'active' },
  { id: 'moving', label: 'Moving', test: (w: WorkItem) => w.state === 'active' && w.claim_status === 'active' },
  { id: 'paused', label: 'Paused', test: (w: WorkItem) => w.state === 'paused' },
] as const

export function LiveChanges({ live }: { live: LiveProps }) {
  const ov = live.overview.kind === 'ready' ? live.overview.value : null
  const runs = useRuns(live)
  const [f, setF] = useState<(typeof FILTERS)[number]['id']>('all')
  const all = ov?.current ?? []
  const filter = FILTERS.find((x) => x.id === f) ?? FILTERS[0]
  const waiting = all.filter(FILTERS[1].test).length
  const finished = (ov?.recent ?? []).filter((o) => o.kind === 'completion')
  return (
    <>
      <PageHead question="Where is each change?" answer={ov ? <>{plural(all.length, 'unfinished change')}.<br /><span className="g-quiet">{plural(waiting, 'waits', 'wait')} on people; the rest {all.length - waiting === 1 ? 'is' : 'are'} moving.</span></> : <>Reading the Hub…</>} />
      {ov && !ov.current_complete && <Notice title="This snapshot is incomplete">The overview limit was reached. More work exists outside it.</Notice>}
      <div className="g-filters" role="group" aria-label="Show">
        {FILTERS.map((x) => <button key={x.id} type="button" aria-pressed={f === x.id} className={f === x.id ? 'on' : ''} onClick={() => setF(x.id)}>{x.label}<span>{all.filter(x.test).length}</span></button>)}
      </div>
      <Box icon="diagram-3" title="By lifecycle step" count={all.filter(filter.test).length}>
        {ov ? <LiveTrack items={all.filter(filter.test)} ov={ov} live={live} runs={runs} /> : <Skeleton rows={3} />}
      </Box>
      <div className="g-two">
        <Box icon="flag" title="Recently reported finished" count={finished.length} explain="Reported finished by the agent. That does not confirm review, verification or merge.">
          {finished.length === 0 && <p className="small muted mb-0">Nothing reported finished in this window.</p>}
          <ul className="g-links">{finished.slice(0, 6).map((o) => <li key={o.seq}><Icon name="flag" /><span><a href={workHref(o.item.id)} className="px-link-quiet"><b>{o.item.title}</b></a><small>{o.item.owner_login} · <Ago at={o.at} /></small></span></li>)}</ul>
          <a href="#work/finished" className="px-link">Finished and history <Icon name="arrow-right" /></a>
        </Box>
        <StartTask token={live.token} me={live.handoff.me} repo={live.handoff.repo} originUrl={live.handoff.originUrl} />
      </div>
    </>
  )
}

/** Turns a poll into a lifecycle input: null while loading, 'failed' on error, otherwise the picked value. */
function input<T, V>(p: Poll<T>, pick: (v: T) => V): V | null | 'failed' {
  return p.kind === 'loading' ? null : p.kind === 'error' ? 'failed' : pick(p.value)
}

export function LiveChange({ id, live }: { id: string; live: LiveProps }) {
  const item = usePoll((s) => fetchStandupWork(id, live.token, s), 15_000, `g-work:${id}`)
  if (item.kind === 'loading') return <><a href="#changes" className="px-link g-back"><Icon name="arrow-left" /> All changes</a><Skeleton rows={3} /></>
  if (item.kind === 'error') return <><a href="#changes" className="px-link g-back"><Icon name="arrow-left" /> All changes</a><Notice tone="bad" title="Could not load this change" action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={item.retry}>Retry</button>}>{item.message}</Notice></>
  return <Change item={item.value} live={live} />
}

function Change({ item, live }: { item: WorkItem; live: LiveProps }) {
  const ov = live.overview.kind === 'ready' ? live.overview.value : null
  const branch = item.branch
  const record = usePoll((s) => fetchChangeRecord(item.repository, item.id, live.token, s), 0, `g-rec:${item.id}`)
  const requests = usePoll((s) => fetchCoordination(live.token, s, { limit: 50, activeOnly: true }), 0, `g-req:${item.id}`)
  const runs = usePoll((s) => fetchRuns(item.repository, live.token, s), 0, `g-runs:${item.id}`)
  const pubs = usePoll((s) => (branch ? listPublications(item.repository, live.token, s, undefined, { q: branch, limit: 10 }) : Promise.resolve(null)), 0, `g-pubs:${item.id}`)
  const work = usePoll((s) => fetchWork(item.repository, live.token, s), 0, `g-workpr:${item.id}`)
  const [full, setFull] = useState(false)
  const steps = lifecycle({
    item, overview: ov,
    requests: input(requests, (v) => v.requests),
    skillRuns: input(runs, (v) => v.runs),
    changeRecord: record.kind === 'error' && record.status === 404 ? 'none' : input(record, (v) => v.summary),
    publications: input(pubs, (v) => (v === null ? [] : v.supported ? v.publications : 'failed')),
    pulls: input(work, (v) => v.pull_requests),
  })
  const pulls = work.kind === 'ready' ? work.value.pull_requests : []
  const runList = runs.kind === 'ready' ? runs.value.runs : []
  const at = stepOf(item, pulls, runList, ov)
  const tone = toneOf(item)
  const l = lead(item)
  // Same sources and order as the classic record, so both name the same pull request.
  const pub = pubs.kind === 'ready' && pubs.value?.supported ? pubs.value.publications.find((x) => x.branch === branch) : undefined
  const open = prFor(item, pulls)
  const prNumber = pub?.pr.number ?? open?.number ?? runList.find((x) => x.branch === branch)?.pull_request ?? null
  const prUrl = pub?.pr.url ?? httpsOnly(open?.html_url)
  const step = item.state === 'blocked' ? blockedStep(item) : item.state === 'handover' ? handoverStep(item) : item.claim_status !== 'active' ? quietStep(item, live.handoff.me) : null
  const acts = useStepActions(step, { viewer: live.handoff.me, repo: live.handoff.repo, originUrl: live.handoff.originUrl })
  const why = item.blocker?.summary ?? item.current?.summary ?? 'No progress report yet.'
  return (
    <article className="g-work">
      <a href="#changes" className="px-link g-back"><Icon name="arrow-left" /> All changes</a>
      <header className="g-work-hero" style={{ viewTransitionName: `g-w-${item.id}` }}>
        <div>
          <h1 className="g-work-title">{item.title}</h1>
          <p className="g-work-context"><Avatar name={l.agent} size={22} /> {who(l.login, l.agent)}{branch ? <> · <code>{branch}</code></> : null}{prNumber !== null ? <> · {prUrl ? <a href={prUrl} target="_blank" rel="noreferrer noopener">PR #{prNumber}</a> : `PR #${prNumber}`}</> : null} · checked in <Ago at={item.last_checkin_at} /></p>
        </div>
      </header>

      <section className={`g-status tone-${dotTone(tone)}`} aria-live="polite">
        <span className={`g-ring big tone-${dotTone(tone)}`}><Icon name={STEPS[at]?.icon ?? 'circle'} /></span>
        <div className="g-status-text"><b>{stateText(item)}</b><p>{why}</p>{item.next_action && <p className="small mb-0"><Icon name="arrow-right-short" /> Next: {item.next_action}</p>}</div>
        {acts.primary && <div className="g-acts"><StepAction a={acts.primary} className="btn btn-primary g-cta" /></div>}
      </section>

      <LiveLane at={at} tone={tone} holder={l.agent} label={`${item.title}: ${STEPS[at]?.label}`} note={stateText(item)} />

      <div className="g-work-grid">
        <div className="g-col">
          <Box icon="patch-check" title="Lifecycle, and how each step is known" explain="Each step names the system that records it and its proof rung: reported by an agent, recorded by a collector, observed by the Hub or forge, or verified.">
            <ol className="g-life">
              {steps.map((s, i) => {
                const p = s.proof ? PROOF[s.proof] : null
                return (
                  <li key={s.key} className={i < at ? 'past' : i === at ? 'now' : ''}>
                    <span className={`g-ring small tone-${s.tone}`}><Icon name={s.icon} /></span>
                    <span className="g-life-main"><b>{s.label}</b><span className={`tone-${s.tone}-text`}>{s.word}</span>{s.detail && <small>{s.detail}</small>}</span>
                    <span className="g-life-proof">{p && <Pill tone={p.tone} icon={p.icon} tip={s.proofTip ?? p.tip}>{p.label}</Pill>}{s.href && <a href={s.href} className="px-link" target={s.href.startsWith('http') ? '_blank' : undefined} rel="noreferrer noopener" onClick={(e) => scrollToAnchor(e, s.href!)}>Evidence</a>}</span>
                  </li>
                )
              })}
            </ol>
          </Box>
          <Box icon="list-check" title="What it touches" count={item.scope.length}>
            {item.scope.length === 0 ? <p className="small muted mb-0">No scope declared. Overlap detection only sees declared paths.</p> : <div className="g-files">{item.scope.map((p) => <code key={p}>{p}</code>)}</div>}
          </Box>
        </div>
        <aside className="g-col">
          <Box icon="people" title="People and agents" count={item.participants.length}>
            <ul className="g-people compact">
              {item.participants.map((p) => <li key={`${p.login}${p.agent}${p.session}`}><Avatar name={p.agent} size={30} /><span className="g-person"><b>{p.login}</b><span className="small muted">{p.agent} · {p.role}</span></span></li>)}
            </ul>
          </Box>
          <Box id="agent-reports" icon="clock-history" title="Latest reports">
            <ol className="g-timeline">
              {item.blocker && <li className="fresh"><span className="g-tl-dot tone-bad" aria-hidden="true" /><span className="g-tl-text">Blocked: {item.blocker.summary}</span><span className="g-tl-at"><Ago at={item.blocker.at} /></span></li>}
              {item.current && <li><span className="g-tl-dot" aria-hidden="true" /><span className="g-tl-text">{item.current.summary}</span><span className="g-tl-at">{item.current.kind} · <Ago at={item.current.at} /></span></li>}
              <li><span className="g-tl-dot tone-ok" aria-hidden="true" /><span className="g-tl-text">Claimed by {who(l.login, l.agent)}</span><span className="g-tl-at"><Ago at={item.created_at} /></span></li>
            </ol>
          </Box>
          {acts.more.length > 0 && <Box icon="terminal" title="Other ways to act"><div className="g-acts">{acts.more.map((a) => <StepAction key={a.key} a={a} className="btn btn-sm btn-outline-secondary" />)}</div></Box>}
          {item.depends_on.length + item.related_work.length > 0 && <Box icon="link-45deg" title="Linked work">
            <ul className="g-links">{[...item.depends_on.map((x) => ['Depends on', x] as const), ...item.related_work.map((x) => ['Related', x] as const)].map(([k, x]) => <li key={`${k}${x}`}><Icon name="link-45deg" /><span><a href={workHref(x)} className="px-link-quiet"><b>{x}</b></a><small>{k}</small></span></li>)}</ul>
          </Box>}
        </aside>
      </div>
      <section className="g-full">
        <button type="button" className="g-full-toggle" aria-expanded={full} onClick={() => setFull(!full)}><Icon name={full ? 'chevron-up' : 'chevron-down'} /> Everything the Hub recorded: change record, PR context, requests and checkpoints</button>
        <Reveal open={full}>{full && <WorkDetail id={item.id} token={live.token} overview={live.overview} handoff={live.handoff} />}</Reveal>
      </section>
    </article>
  )
}
