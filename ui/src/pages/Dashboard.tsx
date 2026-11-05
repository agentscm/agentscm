import { recipientLabel, statusLabel, statusProvenance, type ActionableStatus } from '../actionableStatus.ts'
import { useId, useState, type MouseEvent, type ReactNode } from 'react'
import { CatchupPanel } from '../Catchup.tsx'
import { NarrativePanel } from '../Narrative.tsx'
import type { Derived, LiveAgent } from '../derive.ts'
import { HideButton, isStale, ShowHidden, useHidden, useHideable } from '../dismiss.tsx'
import { Gloss } from '../Gloss.tsx'
import { isLiveOverlap, needHideKey, needsYou, legacyNeeds, needUpdatedAt, type Asked, type Need } from '../needs.ts'
import { askedStep, blockedStep, contestedStep, overlapSteps, prStep, who, type NextStep } from '../nextStep.ts'
import { StepAction, StepDetail, useStepActions } from '../NextStep.tsx'
import { named } from '../inboxThreads.ts'
import { handoffOf, type Handoff } from '../launchLink.ts'
import { StartTask } from '../StartTask.tsx'
import { workHref, type WorkItem, type WorkOverview } from '../standup.ts'
import { SectionTabs, useDismiss } from '../TopNav.tsx'
import { Coach } from '../tour/Coach.tsx'
import type { HubEvent } from '../types.ts'
import { FINISHED_MEANS_AGENT_SAID } from '../copy.ts'
import { Ago, Box, Empty, Icon, Notice, PageHead, Path, Reveal, Skeleton, relative } from '../ui.tsx'
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
      ['awaiting handover review', (i: WorkItem) => i.state === 'handover'],
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
              <b>{plural(ov.current.length, 'unfinished task')}</b>: {going.length ? `${going.length} moving` : 'none moving'}{still.length ? `, ${still.length} awaiting review or blocked` : ''}. {still.length === 0 && <>No task is waiting. </>}
            </>
          )}
          {needs.length === 0 ? <>Nothing needs you.</> : <><b>{plural(needs.length, 'thing')}</b> {needs.length === 1 ? 'needs' : 'need'} you.</>}
        </>
      }
      fact={
        ov.current.length > 0 && (
          <>
            {why.length > 0 && <>Reported: {why.join(', ')}. </>}
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

/** The actions that do not fit on the row: a small menu beside the primary one. */
function MoreActions({ items, describedBy }: { items: ReturnType<typeof useStepActions>['more']; describedBy: string }) {
  const [open, setOpen] = useState(false)
  const ref = useDismiss(open, () => setOpen(false))
  if (items.length === 0) return <ActSlot />
  return (
    <div className="pop need-pop" ref={ref}>
      <button type="button" className="need-btn" aria-haspopup="menu" aria-expanded={open} aria-label="More actions" aria-describedby={describedBy} onClick={() => setOpen((o) => !o)}>
        <Icon name="three-dots" />
      </button>
      {open && (
        <div className="pop-menu need-menu" role="menu">
          {items.map((a) => (
            <StepAction key={a.key} a={a} className="pop-item" role="menuitem" onDone={() => setOpen(false)} />
          ))}
        </div>
      )}
    </div>
  )
}

/** An empty menu or chevron slot, so every row's primary pill ends at the same place. */
const ActSlot = () => <span className="need-btn need-slot" aria-hidden="true" />

/** Clicks on the row's own text open it; clicks on a link, button or input inside it do what they say. */
const plainClick = (e: MouseEvent) => !(e.target as HTMLElement).closest('a, button, input, label, .gloss, time')

/**
 * One decision for a person, as one line: who and what, one primary action, a small menu for the rest. Opening it
 * (the chevron, or the row's text) shows the reason, who decides and the box for the person's own words.
 */
/** What a need says, shared by the classic Needs you list and the G layout: icon, title, one-line gist, reason and next step. */
export function needView(n: Need, ov: WorkOverview, d: Derived, me: string | null): { icon: string; title: ReactNode; gist: ReactNode; body: ReactNode; step: NextStep | null } {
  const i = n.item
  let icon = 'exclamation-circle'
  let title: ReactNode = null
  /** One line under the title while closed: the task, when, or where to look. */
  let gist: ReactNode = null
  /** The reason, shown when open. */
  let body: ReactNode = null
  let step: NextStep | null = null
  if (n.kind === 'blocked' && i) {
    icon = 'sign-stop'
    title = <>{leadName(i)} is blocked</>
    gist = <><a className="work-link" href={workHref(i.id)}>{i.title}</a> · reported {i.blocker ? <Ago at={i.blocker.at} /> : 'recently'}</>
    body = <>“{i.blocker?.summary ?? 'No blocker summary reported'}”</>
    step = blockedStep(i)
  } else if (n.kind === 'asked' && i && n.request) {
    const r = n.request
    icon = 'envelope-exclamation'
    title = <>{named(r.requester.login, r.requester.agent, me)} asked for <Path>{n.path ?? 'files'}</Path> you hold, and your agent isn’t running</>
    gist = <><a className="work-link" href={workHref(i.id)}>{i.title}</a> · asked <Ago at={r.created_at} /> · <a href="#inbox">open the Inbox</a></>
    body = <>{i.state === 'paused' ? <>Your agent paused this task</> : i.claim_status === 'stale' ? <>Your agent last checked in <Ago at={i.last_checkin_at} /></> : <>Your agent’s last check-in is unknown</>}, so it can’t answer. The other agent waits until someone does.</>
    step = askedStep(r, i)
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
    gist = <a href="#box-contested">see the contested file</a>
  } else if (n.kind === 'overlap') {
    const o = ov.overlaps.find((x) => `overlap:${x.left.id}:${x.right.id}` === n.key)
    icon = 'sign-intersection-y'
    title = <><Gloss term="overlap">Two tasks plan to touch</Gloss> <Path>{n.path ?? 'the same files'}</Path></>
    body = o ? <><a className="work-link" href={workHref(o.left.id)}>{o.left.title}</a> and <a className="work-link" href={workHref(o.right.id)}>{o.right.title}</a> declared overlapping scope.</> : null
    gist = <a href="#inbox">open the Inbox</a>
    if (o) step = overlapSteps(o)
  } else if (n.kind === 'pr') {
    const p = d.prOverlaps.find((x) => `pr:${x.pr.number}` === n.key)
    icon = 'git'
    title = <>PR #{p?.pr.number} touches files an agent holds</>
    body = p ? <>“{p.pr.title}” overlaps {p.with.map((w) => who(w.agent.owner, w.agent.name)).join(', ')}.</> : null
    if (p) step = prStep(p)
    gist = <a href="#box-prs">see the pull request</a>
  }
  return { icon, title, gist, body, step }
}

function NeedRow({ n, ov, d, me, handoff, hidden }: { n: Need; ov: WorkOverview; d: Derived; me: string | null; handoff: Handoff; hidden: boolean }) {
  const now = useNow() / 1000
  const [open, setOpen] = useState(false)
  const id = useId()
  const { icon, title, gist, body, step } = needView(n, ov, d, me)
  const acts = useStepActions(step, { viewer: handoff.me, repo: handoff.repo, originUrl: handoff.originUrl })
  const canHide = !hidden && isStale(needUpdatedAt(n), now)
  return (
    <li className={`attn-item need tone-${n.tone}${open ? ' is-open' : ''}${hidden ? ' is-hidden' : ''}`}>
      <span className="attn-icon" aria-hidden="true">
        <Icon name={icon} />
      </span>
      <div className="need-main" onClick={(e) => plainClick(e) && setOpen((o) => !o)}>
        <div className="attn-title" id={`${id}-t`}>{title}</div>
        {gist && <div className="need-gist">{gist}</div>}
      </div>
      <div className="need-acts">
        {hidden && <HideButton k={needHideKey(n)} hidden what="this item" />}
        {acts.primary && <StepAction a={acts.primary} className="btn btn-sm btn-outline-secondary need-primary" />}
        <MoreActions items={acts.more} describedBy={`${id}-t`} />
        <button type="button" className="need-btn need-chev" aria-expanded={open} aria-controls={id} aria-describedby={`${id}-t`} onClick={() => setOpen((o) => !o)}>
          <Icon name="chevron-down" />
          <span className="visually-hidden">{open ? 'Hide details' : 'Show details'}</span>
        </button>
      </div>
      <Reveal open={open} id={id}>
        <div className="need-more">
          {body && <p>{body}</p>}
          {step && <StepDetail step={step} blanks={acts.blanks} words={acts.words} setWord={acts.setWord} />}
          {canHide && (
            <div className="attn-status">
              <HideButton k={needHideKey(n)} hidden={false} what="this item" />
            </div>
          )}
        </div>
      </Reveal>
    </li>
  )
}

/** A need the agents are still settling between themselves: what happened, in one sentence. */
function SortingRow({ n, d }: { n: Need; d: Derived }) {
  let icon = 'arrow-left-right'
  let text: ReactNode = null
  if (n.kind === 'contested') {
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

/**
 * Overlaps are settled in the Inbox, so Needs you shows them as one line that leads there instead of repeating each.
 * Overlaps hidden in this browser (before they were folded) fold into their own dimmed row, shown only with the hidden ones.
 */
function TalkRow({ overlaps, hidden }: { overlaps: Need[]; hidden: boolean }) {
  const n = overlaps.length
  const first = overlaps.find((o) => o.path)?.path
  return (
    <li className={`attn-item need tone-warn${hidden ? ' is-hidden' : ''}`}>
      <span className="attn-icon" aria-hidden="true">
        <Icon name="sign-intersection-y" />
      </span>
      <div className="need-main">
        <div className="attn-title">
          <a className="work-link" href="#inbox">{n === 1 ? '1 overlap' : `${n} overlaps`} to talk through</a>
        </div>
        <div className="need-gist">
          {n === 1 ? 'Two tasks plan' : 'Tasks plan'} to touch the <Gloss term="overlap">same files</Gloss>
          {first && <>, starting with <Path>{first}</Path></>}
        </div>
      </div>
      <div className="need-acts">
        {hidden && <HideButton k={overlaps.map(needHideKey)} hidden what={n === 1 ? 'this overlap' : 'these overlaps'} />}
        <a className="btn btn-sm btn-outline-secondary need-primary" href="#inbox">
          Open the Inbox
        </a>
        <ActSlot />
        <ActSlot />
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
  const overlaps = needs.filter((n) => n.kind === 'overlap' && !isHidden(n))
  const hiddenOverlaps = needs.filter((n) => n.kind === 'overlap' && isHidden(n))
  // One row per need, except the overlaps, which share one row at the place the first would sit (hidden ones, a second).
  const rows = needs.filter((n) => n.kind !== 'overlap' || n === overlaps[0] || n === hiddenOverlaps[0])
  return (
    <Box
      id="box-needs"
      tour="needs"
      icon="bell"
      title="Needs you"
      count={needs.length > 0 ? needs.length : undefined}
      tone={you.some((n) => n.tone === 'bad' && !isHidden(n)) ? 'bad' : 'brand'}
      explain="Things a person can act on now. Agent-to-agent requests sit under “Agents are sorting it out” and join this list only when the hub records a change, such as a file request lapsing or the same edit being refused again."
    >
      {needs.length === 0 ? (
        <div className="all-clear">
          <Icon name="check2-circle" />
          <div>
            <b>All clear</b>
            <span>A blocked task, an overlap between live tasks or a contested file would show up here.</span>
          </div>
        </div>
      ) : (
        <>
          <ul className="attn">
            {(expanded ? rows : rows.slice(0, MAX_NEEDS)).map((n) =>
              n.kind === 'overlap' ? (
                isHidden(n) ? <TalkRow key="overlaps-hidden" overlaps={hiddenOverlaps} hidden /> : <TalkRow key="overlaps" overlaps={overlaps} hidden={false} />
              ) : <NeedRow key={n.key} n={n} ov={ov} d={d} me={me} handoff={handoff} hidden={isHidden(n)} />,
            )}
          </ul>
          {rows.length > MAX_NEEDS && !expanded && (
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
                      {i.branch && <span className="lane-on">on <code className="lane-branch">{i.branch}</code></span>}
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
export function dashboardStatus(ov: WorkOverview | null, d: Derived | null, events: HubEvent[] | null, asOfMs: number | null, hidden: ReadonlySet<string> = new Set(), asked?: Asked, shared?: ActionableStatus | null) {
  const needs = legacyNeeds(ov, d, asked).filter((n) => n.who === 'you' && !hidden.has(needHideKey(n)))
  const contested = d?.contested.length ?? 0
  const overlaps = ov?.overlaps.filter(isLiveOverlap).length ?? 0
  const latest = events?.[0]
  const up = d && asOfMs ? d.agents.filter((a) => asOfMs / 1000 - a.lastActive < AWAKE_SECONDS).length : 0
  const personal = shared === undefined ? needs : needsYou(ov, d, asked, shared).filter((n) => n.who === 'you' && !hidden.has(needHideKey(n)))
  return {
    needs: personal,
    home: shared !== undefined ? (shared ? { text: personal.length ? 'Waiting on you' : shared.complete ? 'Nothing needs you' : 'No action observed · partial coverage', tone: personal.length ? 'bad' as const : shared.complete ? 'ok' as const : 'warn' as const } : { text: 'Shared status unavailable', tone: 'warn' as const }) : d && ov ? (needs.length ? { text: 'Waiting on you', tone: 'bad' as const } : { text: 'Nothing needs you', tone: 'ok' as const }) : undefined,
    now: d ? (contested || overlaps ? { text: `${plural(contested, 'contested file')} · ${plural(overlaps, 'overlap')}`, tone: 'bad' as const } : { text: 'No contested files', tone: 'muted' as const }) : undefined,
    activity: latest && asOfMs ? { text: `Last event ${relative(latest.at, asOfMs)}`, tone: 'brand' as const } : events ? { text: 'Quiet so far', tone: 'muted' as const } : undefined,
    team: d ? (d.agents.length ? { text: `${up} of ${d.agents.length} awake`, tone: up ? ('ok' as const) : ('warn' as const) } : { text: 'Nobody connected', tone: 'muted' as const }) : undefined,
  }
}

/** `answer` replaces the shared headline: the G layout gives each tab its own sentence. */
export function DashboardPage({ section, question, token, repo, data, d, overview, asked, answer, status }: { section: DashTab; question: string; token: string; repo: string; data: HubData; d: Derived | null; overview: Poll<WorkOverview>; asked?: Asked; answer?: ReactNode; status?: Poll<ActionableStatus> }) {
  const ov = overview.kind === 'ready' ? overview.value : null
  const hidden = useHidden()
  const all = legacyNeeds(ov, d, asked)
  const needs = all.filter((n) => n.who === 'you' && !hidden.has(needHideKey(n)))
  const needsCount = status === undefined ? needs.length : status.kind === 'ready' ? status.value.counts.needs_you : 0
  const handoff = handoffOf(data.whoami)
  const start = <StartTask token={token} me={handoff.me} repo={handoff.repo} originUrl={handoff.originUrl} />
  return (
    <div className="page">
      {status !== undefined ? <SharedAnswer question={question} status={status} /> : answer ? <PageHead question={question} answer={answer} live /> : <Answer question={question} repo={repo} d={d} ov={ov} failing={data.failing} needs={needs} />}
      {section === 'home' && ov && d && <Coach id="answer" side="below">This sentence is the page in brief: how many tasks are unfinished, their reported states and named recipients, and how many need you.</Coach>}
      <SectionTabs label="Dashboard views" />
      {overview.kind === 'error' && (
        <Notice tone="bad" title="Could not load the work overview" action={<button className="btn btn-sm btn-outline-secondary" onClick={overview.retry}>Retry</button>}>
          {overview.message}
        </Notice>
      )}
      {ov && overview.kind === 'ready' && overview.refreshError && <Notice title="Showing the last overview that loaded">{overview.refreshError}</Notice>}
      {section === 'home' && (!ov || !d ? <Skeleton rows={6} /> : (
        <>
          {/* What waits on a person comes first; starting a task is one quiet line until asked for. With nothing waiting, it leads. */}
          {needsCount === 0 && start}
          <Coach id="needs" side="above">These wait on a person. Open a row for the reason and who decides; its button opens or copies the prompt for the agent.</Coach>
          {status === undefined ? <NeedsYou needs={all} ov={ov} d={d} me={handoff.me} handoff={handoff} /> : <SharedNeeds status={status} legacy={{ ov, d, me: handoff.me, handoff }} />}
          <NarrativePanel token={token} compact />
          {needsCount > 0 && start}
          <Coach id="track" side="above">Every unfinished task, left to right: claimed, now, next, finished. Open a task for its full story.</Coach>
          <Track ov={ov} />
        </>
      ))}
      {section === 'now' && <NowPage data={data} d={d} overview={overview} token={token} />}
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


export function SharedAnswer({ question, status }: { question: string; status: Poll<ActionableStatus> }) {
  const hidden = useHidden()
  if (status.kind !== 'ready') return <PageHead question={question} answer={status.kind === 'loading' ? 'Checking shared status…' : 'Shared status is unavailable. Retry to check what needs you.'} live />
  const s = status.value, personal = needsYou(null, null, undefined, status.value).filter((n) => !hidden.has(needHideKey(n))).length
  const parts = [['active', s.counts.active], ['awaiting a named party', s.counts.awaiting_party], ['blocked', s.counts.blocked], ['needing reassessment', s.counts.reassessment_needed], ['retired', s.counts.retired]] as const
  const states = parts.filter(([, n]) => n > 0).map(([label, n]) => `${n} ${label}`).join(', ')
  return <PageHead question={question} live answer={<>{status.refreshError && 'Last known: '}{s.work.length ? `${s.work.length} tasks: ${states}. ` : 'No Work observed. '}{personal ? <b>{personal} {personal === 1 ? 'action needs' : 'actions need'} you.</b> : s.complete ? 'Nothing needs you.' : 'No visible personal action observed.'}</>} fact={<>{status.refreshError ? 'Showing the last status that loaded. ' : ''}{!s.complete && 'Coverage is incomplete; missing signals remain unknown. '}Read <Ago at={s.observed_at} />.</>} />
}

/** Where a shared action is handled: its page, button text and icon. Others go to the Inbox. */
const ACTION_LINK: Record<string, [string, string, string]> = {
  pr_overlap: ['#now', 'Open current work', 'envelope-exclamation'],
  file_conflict: ['#now', 'Open current work', 'envelope-exclamation'],
  failed_check: ['#landed', 'Open changes', 'x-circle'],
  skill_gate: ['#skills', 'Open Skills', 'sign-stop'],
  agent_setup: ['#setup', 'Open Agents', 'plug'],
}

export function SharedNeeds({ status, legacy }: { status: Poll<ActionableStatus>; legacy?: { ov: WorkOverview; d: Derived; me: string | null; handoff: Handoff } }) {
  const shared = status.kind === 'ready' ? status.value : null
  const all = needsYou(legacy?.ov ?? null, legacy?.d ?? null, undefined, shared).filter((n) => n.who === 'you')
  const { rows, count, show, setShow, isHidden } = useHideable(all, needHideKey)
  if (status.kind === 'loading') return <Box id="box-needs" icon="bell" title="Needs you"><Skeleton rows={3} /></Box>
  if (status.kind === 'error') return <Box id="box-needs" icon="bell" title="Needs you"><Notice title="Shared status could not load" action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={status.retry}>Retry</button>}>{status.message}</Notice></Box>
  const s = status.value, keys = new Set(rows.map((n) => n.key)), work = s.work.filter((w) => w.needs_you && keys.has(w.state === 'blocked' ? `blocked:${w.work_id}` : `status:${w.work_id}`)), actions = s.actions.filter((a) => a.needs_you && keys.has(a.id))
  return <Box id="box-needs" icon="bell" title="Needs you" count={rows.length || undefined} tone={work.some((w) => w.state === 'blocked') ? 'bad' : 'brand'} explain="Actions addressed to you by the shared status projection. Reports and observed evidence remain separate.">
    {status.refreshError && <Notice title="Showing the last status that loaded">{status.refreshError}</Notice>}
    {s.work.some((w) => w.limits.includes('provider_coverage_unavailable')) && <Notice title="Provider evidence is unavailable">The Hub could not observe all provider facts. Reports and older evidence do not establish clear status.</Notice>}
    {work.length === 0 && actions.length === 0 && <Empty icon="check2-circle" title={s.complete ? 'All clear' : 'No personal action observed'}>{!s.complete && 'Coverage is incomplete. Open the evidence below before treating this as clear.'}</Empty>}
    <ul className="attn">{work.map((w) => { const n = rows.find((n) => n.key === (w.state === 'blocked' ? `blocked:${w.work_id}` : `status:${w.work_id}`))!; return legacy && w.state === 'blocked' && n.item ? <NeedRow key={w.work_id} n={n} ov={legacy.ov} d={legacy.d} me={legacy.me} handoff={legacy.handoff} hidden={isHidden(n)} /> : <li className={`attn-item tone-${w.state === 'blocked' ? 'bad' : 'warn'}`} key={w.work_id}><span className="attn-icon" aria-hidden="true"><Icon name={w.state === 'blocked' ? 'sign-stop' : 'arrow-left-right'} /></span><div className="attn-main text-break"><a className="work-link" href={workHref(w.work_id)}>{w.title}</a><p>{statusLabel(w.state)} · {recipientLabel(w.recipient)}</p><p>{w.action}</p><details className="disclosure"><summary>Evidence and reported next step</summary><p>{statusProvenance(w)}</p><p>Reservation {w.operational.reservation.status}. Outcome {w.operational.outcome.value} ({w.operational.outcome.provenance}).</p>{w.reported_next_action && <p>Reported: {w.reported_next_action}</p>}{w.limits.length > 0 && <p>Coverage: {w.limits.join(', ')}</p>}</details><a className="btn btn-sm btn-outline-secondary" href={workHref(w.work_id)}>Open work</a></div></li>})}{actions.map((a) => { const [href, label, icon] = ACTION_LINK[a.kind] ?? ['#inbox', 'Open Inbox', 'envelope-exclamation']; return <li className="attn-item tone-warn" key={a.id}><span className="attn-icon" aria-hidden="true"><Icon name={icon} /></span><div className="attn-main text-break"><p>{a.action}</p><p>{a.recipients.map(recipientLabel).join(', ')}</p><a className="btn btn-sm btn-outline-secondary" href={href}>{label}</a></div></li> })}</ul>
    <ShowHidden count={count} show={show} onShow={setShow} />
    <details className="disclosure text-break"><summary>All shared status evidence · {s.counts.held_reservations} held reservations</summary><ul>{s.work.map((w) => <li key={w.work_id}><a href={workHref(w.work_id)}>{w.title}</a> · {statusLabel(w.state)} · {recipientLabel(w.recipient)} · reservation {w.operational.reservation.status}<p>{statusProvenance(w)}</p></li>)}</ul><p>{s.limits.join(', ') || 'No collection limits reported.'}</p><p>Read as of {new Date(s.observed_at * 1000).toLocaleString()}; provider check times appear above.</p><p>Evidence revision <code className="text-break">{s.digest}</code>. Reads are not one atomic snapshot.</p></details>
  </Box>
}
