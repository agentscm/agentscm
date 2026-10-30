import { useState, type ReactNode } from 'react'
import { CatchupPanel } from '../Catchup.tsx'
import { NarrativePanel } from '../Narrative.tsx'
import type { Derived, LiveAgent } from '../derive.ts'
import { HideButton, isStale, ShowHidden, useHidden, useHideable } from '../dismiss.tsx'
import { Gloss } from '../Gloss.tsx'
import { needHideKey, needsYou, needUpdatedAt, type Need } from '../needs.ts'
import { blockedStep, contestedStep, handoverStep, overlapSteps, prStep, quietStep, who, type NextStep } from '../nextStep.ts'
import { NextStepView } from '../NextStep.tsx'
import { handoffOf, type Handoff } from '../launchLink.ts'
import { StartTask } from '../StartTask.tsx'
import { workHref, type WorkItem, type WorkOverview } from '../standup.ts'
import { SectionTabs } from '../TopNav.tsx'
import type { HubEvent } from '../types.ts'
import { FINISHED_MEANS_AGENT_SAID } from '../copy.ts'
import { Ago, Box, Empty, Icon, Notice, PageHead, Path, Skeleton, relative } from '../ui.tsx'
import { useNow, type HubData } from '../useHub.ts'
import type { Poll } from '../usePoll.ts'
import { ActivityPage } from './Activity.tsx'
import { InstallCommand } from './Connect.tsx'
import { AgentRow, NowPage } from './Now.tsx'
import { LaneBar, StatePill, lead, plural } from './workParts.tsx'

/** An agent counts as awake while it reported activity in the last five minutes. */
export const AWAKE_SECONDS = 300

/** Moving: the lead says it is working and its claim checked in recently. */
const moving = (i: WorkItem) => i.state === 'active' && i.claim_status === 'active'

/** One sentence that stays on top of every dashboard tab: who is working and what needs you, then why the rest is still. */
function Answer({ question, repo, d, ov, failing, needs }: { question: string; repo: string; d: Derived | null; ov: WorkOverview | null; failing: boolean; needs: Need[] }) {
  const going = ov?.current.filter(moving) ?? []
  const still = ov?.current.filter((i) => !moving(i)) ?? []
  const why = (
    [
      ['blocked', (i: WorkItem) => i.state === 'blocked'],
      ['paused', (i: WorkItem) => i.state === 'paused'],
      ['waiting for another agent', (i: WorkItem) => i.state === 'handover'],
      ['quiet', (i: WorkItem) => i.state === 'active'],
    ] as const
  )
    .map(([what, f]) => [still.filter(f).length, what] as const)
    .filter(([n]) => n > 0)
    .map(([n, what]) => `${n} ${what}`)
  // The head wraps its answer in a <p>, so the loading skeleton sits beside it, not inside.
  if (!d || !ov) {
    return failing ? (
      <PageHead question={question} answer="Waiting for the hub. This page keeps trying and fills in as soon as it answers." live />
    ) : (
      <div className="answer" data-tour="answer">
        <h1 className="page-question">{question}</h1>
        <Skeleton rows={2} />
      </div>
    )
  }
  return (
    <PageHead
      question={question}
      live
      answer={
        <>
          {d.agents.length === 0 && ov.current.length === 0 ? (
            <>Nobody is working in <span className="repo-name">{repo}</span> right now. </>
          ) : ov.current.length === 0 ? (
            <>
              <b>{plural(d.agents.length, 'agent')}</b> {d.agents.length === 1 ? 'is' : 'are'} connected, and no task is claimed yet.{' '}
            </>
          ) : (
            <>
              <b>{plural(ov.current.length, 'unfinished task')}</b>: {going.length ? `${going.length} moving` : 'none moving'}{still.length ? `, ${still.length} stuck` : ''}. {still.length === 0 && <>Nothing is stuck. </>}
            </>
          )}
          {needs.length === 0 ? <>Nothing needs you.</> : <><b>{plural(needs.length, 'thing')}</b> {needs.length === 1 ? 'needs' : 'need'} you.</>}
        </>
      }
      fact={
        ov.current.length > 0 && (
          <>
            {why.length > 0 && <>Stuck: {why.join(', ')}. </>}
            {plural(d.agents.length, 'agent')} {d.agents.length === 1 ? 'is' : 'are'} connected.
          </>
        )
      }
    />
  )
}

/* ---------- Now: what needs you, then the track ---------- */

function leadName(i: WorkItem) {
  return who(lead(i).login, lead(i).agent)
}
const person = (p: { owner: string; name: string }) => ({ login: p.owner, agent: p.name })

function NeedRow({ n, ov, d, me, handoff, hidden }: { n: Need; ov: WorkOverview; d: Derived; me: string | null; handoff: Handoff; hidden: boolean }) {
  const now = useNow() / 1000
  const i = n.item
  let icon = 'exclamation-circle'
  let title: ReactNode = null
  let body: ReactNode = null
  let then: ReactNode = null
  let step: NextStep | null = null
  if (n.kind === 'blocked' && i) {
    icon = 'sign-stop'
    title = <>{leadName(i)} is blocked</>
    body = <>“{i.blocker?.summary ?? 'No blocker summary reported'}” on <a className="work-link" href={workHref(i.id)}>{i.title}</a>.</>
    then = <>reported {i.blocker ? <Ago at={i.blocker.at} /> : 'recently'}</>
    step = blockedStep(i)
  } else if (n.kind === 'quiet' && i) {
    icon = 'hourglass-split'
    title = <>{leadName(i)} has gone quiet</>
    body = <>{i.claim_status === 'stale' ? <>Last check-in <Ago at={i.last_checkin_at} />.</> : <>Its last check-in is unknown.</>} <a className="work-link" href={workHref(i.id)}>{i.title}</a> is still assigned, so it isn’t free to take.</>
    step = quietStep(i, me)
  } else if (n.kind === 'handover' && i) {
    icon = 'arrow-left-right'
    title = <><a className="work-link" href={workHref(i.id)}>{i.title}</a> is ready for another agent</>
    body = i.next_action ?? i.current?.summary ?? `${leadName(i)} reported it ready to hand over.`
    then = <>offered <Ago at={i.updated_at} /></>
    step = handoverStep(i)
  } else if (n.kind === 'contested') {
    const c = d.contested.find((x) => x.path === n.path)
    icon = 'exclamation-diamond'
    const holders = c?.holders ?? []
    const names = [...new Set(holders.map((h) => who(h.owner, h.name)))]
    const wanter = c?.refused ?? c?.handoff?.from
    const wants = wanter ? who(wanter.owner, wanter.name) : 'another agent (not named by the hub)'
    const hold = <Gloss term="hold">{names.length >= 2 ? 'hold' : 'holds'}</Gloss>
    title = names.length >= 2 ? <>{names.join(' and ')} both {hold} <Path>{n.path ?? ''}</Path></> : <>{names[0] ?? 'An agent'} {hold} <Path>{n.path ?? ''}</Path>, and {wants} wants it</>
    const contested = <Gloss term="contested">contested</Gloss>
    body = c?.refused ? <>The file is {contested}: the hub refused {wants} {c.repeated ? 'once, then let a repeated edit through' : 'its first edit'}.</> : <>The file is {contested}: both agents can still edit it after one refusal.</>
    // Ask the holder that is not the one wanting it; with two holders and nobody named, either can ask.
    const asker = wanter ?? holders[1]
    const holder = holders.find((h) => !asker || h.owner !== asker.owner || h.name !== asker.name) ?? c?.handoff?.to
    if (c && asker && holder) step = contestedStep(c, person(asker), person(holder))
    then = <>{!step && <>Agree who keeps it · </>}<a href="#box-contested">see the contested file</a></>
  } else if (n.kind === 'overlap') {
    const o = ov.overlaps.find((x) => `overlap:${x.left.id}:${x.right.id}` === n.key)
    icon = 'sign-intersection-y'
    title = <><Gloss term="overlap">Two tasks plan to touch</Gloss> <Path>{n.path ?? 'the same files'}</Path></>
    body = o ? <><a className="work-link" href={workHref(o.left.id)}>{o.left.title}</a> and <a className="work-link" href={workHref(o.right.id)}>{o.right.title}</a> declared overlapping scope.</> : null
    then = <a href="#inbox">open the Inbox</a>
    if (o) step = overlapSteps(o)
  } else if (n.kind === 'pr') {
    const p = d.prOverlaps.find((x) => `pr:${x.pr.number}` === n.key)
    icon = 'git'
    title = <>PR #{p?.pr.number} touches files an agent holds</>
    body = p ? <>“{p.pr.title}” overlaps {p.with.map((w) => who(w.agent.owner, w.agent.name)).join(', ')}.</> : null
    if (p) step = prStep(p)
    then = <>{!step && <>Check before merging · </>}<a href="#box-prs">see the pull request</a></>
  }
  return (
    <li className={`attn-item tone-${n.tone}${hidden ? ' is-hidden' : ''}`}>
      <span className="attn-icon" aria-hidden="true">
        <Icon name={icon} />
      </span>
      <div className="attn-main">
        <div className="attn-title">{title}</div>
        {step && n.kind === 'contested' && <NextStepView step={step} viewer={handoff.me} repo={handoff.repo} originUrl={handoff.originUrl} />}
        {body && <p>{body}</p>}
        <div className="attn-status">
          {then}
          {(hidden || isStale(needUpdatedAt(n), now)) && <HideButton k={needHideKey(n)} hidden={hidden} what="this item" />}
        </div>
        {step && n.kind !== 'contested' && <NextStepView step={step} viewer={handoff.me} repo={handoff.repo} originUrl={handoff.originUrl} />}
      </div>
    </li>
  )
}

/** A need the agents are still settling between themselves: what happened, in one sentence. */
function SortingRow({ n, d }: { n: Need; d: Derived }) {
  const i = n.item
  let icon = 'arrow-left-right'
  let text: ReactNode = null
  if (n.kind === 'quiet' && i) {
    icon = 'pause-circle'
    text = <>{leadName(i)} paused “<a className="work-link" href={workHref(i.id)}>{i.title}</a>”: “{i.next_action ?? i.current?.summary ?? 'no reason given'}”. Still assigned.</>
  } else if (n.kind === 'contested') {
    const c = d.contested.find((x) => x.path === n.path)
    const h = c?.handoff
    const path = <Path>{n.path ?? ''}</Path>
    if (h) {
      const from = who(h.from.owner, h.from.name)
      const to = who(h.to.owner, h.to.name)
      if (h.answer?.kind === 'granted') text = <>{to} handed {path} to {from} <Ago at={h.answer.at} />.</>
      else if (h.answer?.kind === 'declined') text = h.answer.reason ? <>{to} kept {path}: “{h.answer.reason}”.</> : <>{to} kept {path}.</>
      else if (h.lapsed) text = <>{from} asked {to} for {path}, and nobody answered within 30 minutes.</>
      else text = <>{from} asked {to} for {path} <Ago at={h.at} />. Waiting for an answer; unanswered requests lapse after 30 minutes.</>
    } else if (c?.refused) {
      icon = 'exclamation-diamond'
      text = <>The hub refused {who(c.refused.owner, c.refused.name)}’s first edit to {path} and told it who holds the file.</>
    } else text = <>Agents are settling who keeps {path}.</>
  }
  return (
    <li className="attn-item">
      <span className="attn-icon" aria-hidden="true">
        <Icon name={icon} />
      </span>
      <div className="attn-main">
        <p>{text}</p>
      </div>
    </li>
  )
}

const MAX_NEEDS = 6

/** `needs` includes the ones hidden in this browser; they stay out of the list until asked for. */
function NeedsYou({ needs: all, ov, d, me, handoff }: { needs: Need[]; ov: WorkOverview; d: Derived; me: string | null; handoff: Handoff }) {
  const you = all.filter((n) => n.who === 'you')
  const sorting = all.filter((n) => n.who === 'agents')
  const { rows: needs, count: hidden, show, setShow, isHidden } = useHideable(you, needHideKey)
  const [expanded, setExpanded] = useState(false)
  return (
    <Box
      id="box-needs"
      tour="needs"
      icon="bell"
      title="Needs you"
      tone={you.some((n) => n.tone === 'bad' && !isHidden(n)) ? 'bad' : 'brand'}
      explain="Things a person can act on now. Agent-to-agent requests sit under “Agents are sorting it out” and join this list only when the hub records a change, such as a file request lapsing or the same edit being refused again."
    >
      {needs.length === 0 ? (
        <div className="all-clear">
          <Icon name="check2-circle" />
          <div>
            <b>All clear</b>
            <span>A blocked task, quiet claim, handover or contested file would show up here.</span>
          </div>
        </div>
      ) : (
        <>
          <ul className="attn">
            {(expanded ? needs : needs.slice(0, MAX_NEEDS)).map((n) => (
              <NeedRow key={n.key} n={n} ov={ov} d={d} me={me} handoff={handoff} hidden={isHidden(n)} />
            ))}
          </ul>
          {needs.length > MAX_NEEDS && !expanded && (
            <button type="button" className="show-hidden" onClick={() => setExpanded(true)}>
              <Icon name="chevron-down" /> Show all {needs.length}
            </button>
          )}
        </>
      )}
      <ShowHidden count={hidden} show={show} onShow={setShow} />
      {sorting.length > 0 && (
        <details className="disclosure">
          <summary>Agents are sorting it out · {sorting.length}</summary>
          <p className="small muted">No decision needed from you yet. They move up only if something changes.</p>
          <ul className="attn">
            {sorting.map((n) => (
              <SortingRow key={n.key} n={n} d={d} />
            ))}
          </ul>
        </details>
      )}
    </Box>
  )
}

const signatures = (ov: WorkOverview) => new Map(ov.current.map((i) => [i.id, `${i.state}|${i.claim_status}|${i.lead_session}`]))

function Track({ ov }: { ov: WorkOverview }) {
  // The baton ride: a task whose state changed since the last refresh sweeps its current segment in.
  // Compared once per new overview (React's "adjust state while rendering" pattern), never on a re-render.
  const [seen, setSeen] = useState<{ ov: WorkOverview; sigs: Map<string, string>; changed: Set<string> }>(() => ({ ov, sigs: signatures(ov), changed: new Set() }))
  if (seen.ov !== ov) {
    const sigs = signatures(ov)
    setSeen({ ov, sigs, changed: new Set([...sigs].filter(([id, sig]) => seen.sigs.has(id) && seen.sigs.get(id) !== sig).map(([id]) => id)) })
  }
  const changed = seen.changed
  return (
    <Box id="box-track" tour="work-lanes" icon="signpost-split" title="The track" explain={`Every unfinished task, left to right: claimed, where it is now, next step, finished. ${FINISHED_MEANS_AGENT_SAID}`}>
      {!ov.current_complete && <Notice title="This snapshot is incomplete">The overview limit was reached. More work exists outside it.</Notice>}
      {ov.current.length === 0 ? (
        <Empty icon="signpost" title="No unfinished work">
          When an agent starts a task, it appears here as a lane.
        </Empty>
      ) : (
        <>
          <div className="rl-head" aria-hidden="true">
            <span />
            <div className="rl-cols lb-fit">
              <b>Claimed</b>
              <b>Now</b>
              <b>Next</b>
              <b>Finished</b>
            </div>
          </div>
          <ol className="rl">
            {ov.current.map((i) => {
              return (
                <li key={i.id} className="rl-row">
                  <div className="rl-task">
                    <a className="work-link lane-title" href={workHref(i.id)}>
                      {i.title}
                    </a>
                    <div className="lane-meta">
                      <StatePill state={i.state} />
                      <span className="muted">{leadName(i)}</span>
                      {i.branch && <>on <code className="lane-branch">{i.branch}</code></>}
                    </div>
                    <div className={`start-check tone-${i.claim_status === 'active' ? 'ok' : 'warn'}`}>
                      <i aria-hidden="true" />
                      {i.claim_status === 'active' ? <>Checked in <Ago at={i.last_checkin_at} /></> : i.claim_status === 'stale' ? <>No check-in since <Ago at={i.last_checkin_at} /> · still assigned</> : 'Check-in unknown · still assigned'}
                    </div>
                    {i.next_action && (
                      <p className="rl-next">
                        <Icon name="arrow-right-short" /> Next: {i.next_action}
                      </p>
                    )}
                  </div>
                  <LaneBar item={i} fresh={changed.has(i.id)} />
                  {i.current && (
                    <p className="rl-said">
                      <Icon name="chat-left-quote" />
                      <span>
                        “{i.current.summary}”
                      </span>
                      <small>
                        <Ago at={i.current.at} />
                      </small>
                    </p>
                  )}
                </li>
              )
            })}
          </ol>
        </>
      )}
    </Box>
  )
}

/* ---------- Team: who is awake, the crew, and what the recent events add up to ---------- */

function awake(a: LiveAgent, nowS: number) {
  return nowS - a.lastActive < AWAKE_SECONDS
}

function Team({ d, ov, events }: { d: Derived; ov: WorkOverview | null; events: HubEvent[] | null }) {
  const now = useNow() / 1000
  const tasksBy = new Map<string, number>()
  for (const i of ov?.current ?? []) tasksBy.set(lead(i).login, (tasksBy.get(lead(i).login) ?? 0) + 1)
  const tally = (match: (k: string) => boolean) => events?.filter((e) => match(e.kind)).length ?? 0
  const tallies: [string, number][] = [
    ['Sessions started', tally((k) => k === 'session_started' || k === 'session_connected')],
    ['Work announced', tally((k) => k === 'intent_announced')],
    ['Edits refused', tally((k) => k === 'edit_denied')],
    ['Edits that went ahead after a warning', tally((k) => k === 'edit_repeated')],
    ['Handoffs asked', tally((k) => k === 'handoff_request')],
    ['Warnings sent to agents', tally((k) => k === 'intent_notice' || k === 'pr_notice' || k === 'task_overlap')],
  ]
  return (
    <div className="today-grid">
      <Box id="box-agents" tour="now-agents" icon="people" title="The team right now" explain="Live agents grouped by owner. Green means active in the last five minutes; amber means quieter but still holding files.">
        {d.groups.length === 0 ? (
          events?.length !== 0 ? (
            <Empty icon="moon-stars" title="No agents active right now">
              {events && <>Last agent activity <Ago at={Math.max(...events.map((e) => e.at))} />. </>}Agents appear here with the files they hold as soon as they start editing.
            </Empty>
          ) : (
            <>
              <Empty icon="moon-stars" title="No agent has connected yet" />
              <InstallCommand />
            </>
          )
        ) : (
          d.groups.map((g) => (
            <div key={g.owner} className="crew-person">
              <h3 className="crew-person-head">
                {g.owner}{' '}
                <span>
                  · {plural(g.agents.length, 'agent')}
                  {tasksBy.get(g.owner) ? `, leading ${plural(tasksBy.get(g.owner)!, 'task')}` : ''}
                </span>
              </h3>
              <ul className="agent-list">
                {g.agents.map((a) => (
                  <AgentRow key={a.id} a={a} awake={awake(a, now)} />
                ))}
              </ul>
            </div>
          ))
        )}
      </Box>
      <Box icon="bar-chart" title="Lately" count={events ? `last ${events.length} events` : undefined} explain="Counts over the hub’s most recent events. Older ones are on Catch up.">
        {!events ? (
          <Skeleton rows={4} />
        ) : (
          <ul className="tally">
            {tallies.map(([label, n]) => (
              <li key={label} className={n === 0 ? 'is-zero' : undefined}>
                <span>{label}</span>
                <b>{n}</b>
              </li>
            ))}
          </ul>
        )}
      </Box>
    </div>
  )
}

/* ---------- the page ---------- */

export type DashTab = 'home' | 'now' | 'activity' | 'team'

/** The live status under each dashboard tab and the rail badge, from the same joins as the page. */
/** `asOfMs` is when the data was read (the last successful poll); null before the first one. */
/** `hidden` holds the needs hidden in this browser, which no count includes. */
export function dashboardStatus(ov: WorkOverview | null, d: Derived | null, events: HubEvent[] | null, asOfMs: number | null, hidden: ReadonlySet<string> = new Set()) {
  const needs = needsYou(ov, d).filter((n) => n.who === 'you' && !hidden.has(needHideKey(n)))
  const contested = d?.contested.length ?? 0
  const overlaps = ov?.overlaps.length ?? 0
  const latest = events?.[0]
  const up = d && asOfMs ? d.agents.filter((a) => asOfMs / 1000 - a.lastActive < AWAKE_SECONDS).length : 0
  return {
    needs,
    home: d && ov ? (needs.length ? { text: 'Waiting on you', tone: 'bad' as const } : { text: 'Nothing needs you', tone: 'ok' as const }) : undefined,
    now: d ? (contested || overlaps ? { text: `${plural(contested, 'contested file')} · ${plural(overlaps, 'overlap')}`, tone: 'bad' as const } : { text: 'No contested files', tone: 'muted' as const }) : undefined,
    activity: latest && asOfMs ? { text: `Last event ${relative(latest.at, asOfMs)}`, tone: 'brand' as const } : events ? { text: 'Quiet so far', tone: 'muted' as const } : undefined,
    team: d ? (d.agents.length ? { text: `${up} of ${d.agents.length} awake`, tone: up ? ('ok' as const) : ('warn' as const) } : { text: 'Nobody connected', tone: 'muted' as const }) : undefined,
  }
}

export function DashboardPage({ section, question, token, repo, data, d, overview }: { section: DashTab; question: string; token: string; repo: string; data: HubData; d: Derived | null; overview: Poll<WorkOverview> }) {
  const ov = overview.kind === 'ready' ? overview.value : null
  const hidden = useHidden()
  const all = needsYou(ov, d)
  const needs = all.filter((n) => n.who === 'you' && !hidden.has(needHideKey(n)))
  const handoff = handoffOf(data.whoami)
  return (
    <div className="page">
      <Answer question={question} repo={repo} d={d} ov={ov} failing={data.failing} needs={needs} />
      <SectionTabs label="Dashboard views" />
      {overview.kind === 'error' && (
        <Notice tone="bad" title="Could not load the work overview" action={<button className="btn btn-sm btn-outline-secondary" onClick={overview.retry}>Retry</button>}>
          {overview.message}
        </Notice>
      )}
      {ov && overview.kind === 'ready' && overview.refreshError && <Notice title="Showing the last overview that loaded">{overview.refreshError}</Notice>}
      {section === 'home' && (!ov || !d ? <Skeleton rows={6} /> : (
        <>
          <StartTask token={token} me={handoff.me} repo={handoff.repo} originUrl={handoff.originUrl} />
          <NeedsYou needs={all} ov={ov} d={d} me={handoff.me} handoff={handoff} />
          <Track ov={ov} />
        </>
      ))}
      {section === 'now' && <NowPage data={data} d={d} overview={overview} />}
      {section === 'activity' && (
        <>
          <NarrativePanel token={token} />
          <CatchupPanel token={token} />
          <ActivityPage d={d} events={data.activity?.events ?? null} fresh={data.fresh} repo={data.whoami?.token.repository ?? null} token={token} />
        </>
      )}
      {section === 'team' && (!d ? <Skeleton rows={6} /> : <Team d={d} ov={ov} events={data.activity?.events ?? null} />)}
    </div>
  )
}
