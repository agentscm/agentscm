import { useState, type ReactNode } from 'react'
import { CatchupPanel } from '../Catchup.tsx'
import { NarrativePanel } from '../Narrative.tsx'
import type { Derived, LiveAgent } from '../derive.ts'
import { crossings, needsYou, type Need } from '../needs.ts'
import { workHref, type WorkItem, type WorkOverview } from '../standup.ts'
import { SectionTabs } from '../TopNav.tsx'
import type { HubEvent } from '../types.ts'
import { agentLabel, Ago, Box, Empty, Icon, Notice, Path, Skeleton, relative } from '../ui.tsx'
import { useNow, type HubData } from '../useHub.ts'
import type { Poll } from '../usePoll.ts'
import { ActivityPage } from './Activity.tsx'
import { AgentRow, NowPage } from './Now.tsx'
import { LaneBar, StatePill, lead } from './Work.tsx'

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`
/** An agent counts as awake while it reported activity in the last five minutes. */
export const AWAKE_SECONDS = 300

/** A count in the answer that brings its evidence box into view and rings it; elsewhere a plain link to its tab. */
function Jump({ to, href, children, tone = 'brand' }: { to: string; href: string; children: ReactNode; tone?: string }) {
  return (
    <a
      href={href}
      className={`jump tone-${tone}`}
      onClick={(e) => {
        const el = document.getElementById(to)
        if (!el) return
        e.preventDefault()
        el.scrollIntoView({ behavior: window.matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth', block: 'start' })
        el.classList.remove('ring')
        void el.offsetWidth
        el.classList.add('ring')
      }}
    >
      {children}
    </a>
  )
}

/** One sentence that stays on top of every dashboard tab: who is working, what needs you, where things cross. */
function Answer({ question, repo, d, ov, failing, needs }: { question: string; repo: string; d: Derived | null; ov: WorkOverview | null; failing: boolean; needs: Need[] }) {
  const x = crossings(ov, d)
  return (
    <div className="answer" data-tour="answer">
      <h1 className="page-question">{question}</h1>
      {!d || !ov ? (
        failing ? <p className="answer-text">Waiting for the hub. This page keeps trying and fills in as soon as it answers.</p> : <Skeleton rows={2} />
      ) : (
        <p className="answer-text" aria-live="polite">
          {d.agents.length === 0 && ov.current.length === 0 ? (
            <>Nobody is working in <span className="repo-name">{repo}</span> right now. </>
          ) : ov.current.length === 0 ? (
            <>
              <b>{plural(d.agents.length, 'agent')}</b> {d.agents.length === 1 ? 'is' : 'are'} connected, and no task is claimed yet.{' '}
            </>
          ) : (
            <>
              <b>{plural(d.agents.length, 'agent')}</b> {d.agents.length === 1 ? 'is' : 'are'} working on <b>{plural(ov.current.length, 'task')}</b>.{' '}
            </>
          )}
          {needs.length > 0 ? (
            <Jump to="box-needs" href="#home" tone="bad">
              {needs.length === 1 ? '1 thing needs you' : `${needs.length} things need you`}.
            </Jump>
          ) : (
            <>Nothing needs you.</>
          )}{' '}
          {x > 0 && (
            <Jump to="box-map" href="#now" tone="bad">
              {plural(x, 'crossing')}.
            </Jump>
          )}
        </p>
      )}
    </div>
  )
}

/* ---------- Now: what needs you, then the track ---------- */

function who(i: WorkItem) {
  const l = lead(i)
  return `${l.login}’s ${agentLabel(l.agent)}`
}

function NeedRow({ n, ov, d }: { n: Need; ov: WorkOverview; d: Derived }) {
  const i = n.item
  let icon = 'exclamation-circle'
  let title: ReactNode = null
  let body: ReactNode = null
  let then: ReactNode = null
  if (n.kind === 'blocked' && i) {
    icon = 'sign-stop'
    title = <>{who(i)} is blocked</>
    body = <>“{i.blocker?.summary ?? 'No blocker summary reported'}” on <a className="work-link" href={workHref(i.id)}>{i.title}</a>.</>
    then = <>Unblock it or reassign the task · reported {i.blocker ? <Ago at={i.blocker.at} /> : 'recently'}</>
  } else if (n.kind === 'quiet' && i) {
    icon = 'hourglass-split'
    title = <>{who(i)} has gone quiet</>
    body = <>{i.claim_status === 'stale' ? <>No check-in since {new Date(i.last_checkin_at * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}.</> : <>Its last check-in is unknown.</>} <a className="work-link" href={workHref(i.id)}>{i.title}</a> is still assigned, so it isn’t free to take.</>
    then = <>Ask {lead(i).login} before picking it up</>
  } else if (n.kind === 'handover' && i) {
    icon = 'arrow-left-right'
    title = <><a className="work-link" href={workHref(i.id)}>{i.title}</a> is ready for another agent</>
    body = i.next_action ?? i.current?.summary ?? `${who(i)} reported it ready to hand over.`
    then = <>Pick an agent to take it over · <Ago at={i.updated_at} /></>
  } else if (n.kind === 'contested') {
    const c = d.contested.find((x) => x.path === n.path)
    icon = 'exclamation-diamond'
    const names = [...new Set(c?.holders.map((h) => `${h.owner}’s ${agentLabel(h.name)}`) ?? [])]
    const wants = c?.refused ? `${c.refused.owner}’s ${agentLabel(c.refused.name)}` : 'another agent'
    title = names.length >= 2 ? <>{names.join(' and ')} both hold <Path>{n.path ?? ''}</Path></> : <>{names[0] ?? 'An agent'} holds <Path>{n.path ?? ''}</Path>, and {wants} wants it</>
    body = c?.refused ? <>The hub refused {c.refused.owner}’s {agentLabel(c.refused.name)} {c.repeated ? 'once, then let a repeated edit through' : 'its first edit'}.</> : <>Both agents can still edit it after one refusal.</>
    then = <>Agree who keeps it · <a href="#box-contested">see the contested file</a></>
  } else if (n.kind === 'overlap') {
    const o = ov.overlaps.find((x) => `overlap:${x.left.id}:${x.right.id}` === n.key)
    icon = 'sign-intersection-y'
    title = <>Two tasks plan to touch <Path>{n.path ?? 'the same files'}</Path></>
    body = o ? <><a className="work-link" href={workHref(o.left.id)}>{o.left.title}</a> and <a className="work-link" href={workHref(o.right.id)}>{o.right.title}</a> declared overlapping scope.</> : null
    then = <>A signal to talk, not proof of a conflict · <a href="#inbox">open the Inbox</a></>
  } else if (n.kind === 'pr') {
    const p = d.prOverlaps.find((x) => `pr:${x.pr.number}` === n.key)
    icon = 'git'
    title = <>PR #{p?.pr.number} touches files an agent holds</>
    body = p ? <>“{p.pr.title}” overlaps {p.with.map((w) => `${w.agent.owner}’s ${agentLabel(w.agent.name)}`).join(', ')}.</> : null
    then = <>Check before merging · <a href="#box-prs">see the pull request</a></>
  }
  return (
    <li className={`attn-item tone-${n.tone}`}>
      <span className="attn-icon" aria-hidden="true">
        <Icon name={icon} />
      </span>
      <div className="attn-main">
        <div className="attn-title">{title}</div>
        {body && <p>{body}</p>}
        <div className="attn-status">{then}</div>
      </div>
    </li>
  )
}

const MAX_NEEDS = 6

function NeedsYou({ needs, ov, d }: { needs: Need[]; ov: WorkOverview; d: Derived }) {
  return (
    <Box id="box-needs" tour="needs" icon="bell" title="Needs you" count={needs.length} tone={needs.some((n) => n.tone === 'bad') ? 'bad' : 'brand'} explain="Only things a person should decide: a blocker, a task whose agent went quiet, a handover, or two agents or tasks crossing. Reading this page answers none of them.">
      {needs.length === 0 ? (
        <div className="all-clear">
          <Icon name="check2-circle" />
          <div>
            <b>Nothing needs you</b>
            <span>No blocker, no quiet claim, no handover waiting and no crossing.</span>
          </div>
        </div>
      ) : (
        <>
          <ul className="attn">
            {needs.slice(0, MAX_NEEDS).map((n) => (
              <NeedRow key={n.key} n={n} ov={ov} d={d} />
            ))}
          </ul>
          {needs.length > MAX_NEEDS && (
            <p className="small muted mb-0">
              {plural(needs.length - MAX_NEEDS, 'more')} in the <a href="#inbox">Inbox</a> and on <a href="#now">Where</a>.
            </p>
          )}
        </>
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
    <Box id="box-track" tour="work-lanes" icon="signpost-split" title="The track" count={ov.current.length} explain="Every unfinished task, left to right. The wide part is where it is now, with the face of the agent that holds it; then its next step. Reports are the agents’ own words: they don’t prove review or landing.">
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
              const l = lead(i)
              return (
                <li key={i.id} className="rl-row">
                  <div className="rl-task">
                    <a className="work-link lane-title" href={workHref(i.id)}>
                      {i.title}
                    </a>
                    <div className="lane-meta">
                      <StatePill state={i.state} />
                      {i.branch && <code className="lane-branch">{i.branch}</code>}
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
                        {agentLabel(l.agent)}: “{i.current.summary}”
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
    ['Edits repeated and let through', tally((k) => k === 'edit_repeated')],
    ['Handoffs asked', tally((k) => k === 'handoff_request')],
    ['Warnings sent to agents', tally((k) => k === 'intent_notice' || k === 'pr_notice' || k === 'task_overlap')],
  ]
  return (
    <div className="today-grid">
      <Box id="box-agents" tour="now-agents" icon="people" title="The team right now" count={d.agents.length} explain="Live agents grouped by the person who owns them. A green dot means it reported activity in the last five minutes; amber means it is quieter but still holds its files.">
        {d.groups.length === 0 ? (
          <Empty icon="moon-stars" title="No agents active">
            When an agent connected through a hook or MCP starts editing, it appears here with the files it holds.
          </Empty>
        ) : (
          d.groups.map((g) => (
            <div key={g.owner} className="crew-person">
              <h3 className="crew-person-head">
                {g.owner}{' '}
                <span>
                  · {plural(g.agents.length, 'agent')}
                  {tasksBy.get(g.owner) ? ` · leads ${plural(tasksBy.get(g.owner)!, 'task')}` : ''}
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
      <Box icon="bar-chart" title="Lately" count={events ? `last ${events.length} events` : undefined} explain="Counts over the hub’s most recent events, the same window the log shows. Older events are on Catch up › The full log.">
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
export function dashboardStatus(ov: WorkOverview | null, d: Derived | null, events: HubEvent[] | null, asOfMs: number | null) {
  const needs = needsYou(ov, d)
  const x = crossings(ov, d)
  const latest = events?.[0]
  const up = d && asOfMs ? d.agents.filter((a) => asOfMs / 1000 - a.lastActive < AWAKE_SECONDS).length : 0
  return {
    needs,
    home: d && ov ? (needs.length ? { text: `${needs.length} need${needs.length === 1 ? 's' : ''} you`, tone: 'bad' as const } : { text: 'Nothing needs you', tone: 'ok' as const }) : undefined,
    now: d ? (x ? { text: plural(x, 'crossing'), tone: 'bad' as const } : { text: 'No crossings', tone: 'muted' as const }) : undefined,
    activity: latest && asOfMs ? { text: `Last event ${relative(latest.at, asOfMs)}`, tone: 'brand' as const } : events ? { text: 'Quiet so far', tone: 'muted' as const } : undefined,
    team: d ? (d.agents.length ? { text: `${up} of ${d.agents.length} awake`, tone: up ? ('ok' as const) : ('warn' as const) } : { text: 'Nobody connected', tone: 'muted' as const }) : undefined,
  }
}

export function DashboardPage({ section, question, token, repo, data, d, overview }: { section: DashTab; question: string; token: string; repo: string; data: HubData; d: Derived | null; overview: Poll<WorkOverview> }) {
  const ov = overview.kind === 'ready' ? overview.value : null
  const needs = needsYou(ov, d)
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
          <NeedsYou needs={needs} ov={ov} d={d} />
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
