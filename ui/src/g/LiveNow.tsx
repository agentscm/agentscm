// G · Now over live data: what needs you (the dashboard's own needs and next steps), every unfinished change as a
// lane, who is working, the process that applies, connections and recent outcomes.
import { useState, type CSSProperties, type ReactNode } from 'react'
import { Ago, Avatar, Box, Icon, PageHead, Reveal } from '../ui.tsx'
import { useNow } from '../useHub.ts'
import { needView } from '../pages/Dashboard.tsx'
import { StepAction, StepDetail, useStepActions } from '../NextStep.tsx'
import { lead } from '../pages/workParts.tsx'
import { usePoll } from '../usePoll.ts'
import { fetchLibrary, fetchRuns, type RunView } from '../skills.ts'
import { fetchSetup } from '../api.ts'
import { setupState } from '../onboarding.ts'
import type { WorkItem, WorkOverview } from '../standup.ts'
import type { Need } from '../needs.ts'
import type { Derived } from '../derive.ts'
import type { LiveProps } from './GLive.tsx'
import { STEPS, dotTone, prFor, stateText, stepOf, toneOf } from './liveModel.ts'

const WORDS = ['Nothing', 'One', 'Two', 'Three', 'Four', 'Five', 'Six', 'Seven', 'Eight', 'Nine']
const word = (n: number) => WORDS[n] ?? String(n)
const Tick = ({ k, children }: { k: string | number; children: ReactNode }) => <span key={k} className="g-tick">{children}</span>

/** Skill gate runs, shared by Now and Changes; a repository without skills simply has none. */
export function useRuns(live: LiveProps): RunView[] {
  const runs = usePoll((signal) => fetchRuns(live.repo, live.token, signal), 30_000, `${live.repo}:runs`)
  return runs.kind === 'ready' ? runs.value.runs : []
}

export function LiveNow({ live }: { live: LiveProps }) {
  const { overview, d, needs, data, repo } = live
  const ov = overview.kind === 'ready' ? overview.value : null
  const runs = useRuns(live)
  const current = ov?.current ?? []
  const moving = current.filter((i) => i.state === 'active' && i.claim_status === 'active').length
  const n = needs.length
  const people = d?.groups.length ?? 0
  const agents = d?.agents.length ?? 0
  const loading = overview.kind === 'loading' || !d
  return (
    <>
      <PageHead
        question="What needs me?"
        live
        answer={loading
          ? <>Reading the Hub…<br /><span className="g-quiet">Decisions appear here first.</span></>
          : n
            ? <><Tick k={n}>{word(n)}</Tick> {n === 1 ? 'thing needs' : 'things need'} you.<br /><span className="g-quiet">The rest has a next step.</span></>
            : <>Nothing needs you.<br /><span className="g-quiet"><Tick k={moving}>{word(moving)}</Tick> {moving === 1 ? 'change is' : 'changes are'} moving on {moving === 1 ? 'its' : 'their'} own.</span></>}
        fact={loading ? <span className="g-fact">{repo}</span> : <span className="g-fact">
          <span>{repo} · {current.length} unfinished · {people} {people === 1 ? 'person' : 'people'} with {agents} live {agents === 1 ? 'agent' : 'agents'}</span>
          <a href="#contributions" className="g-strip" aria-label="What the Hub has recorded">
            <span><Icon name="sign-intersection-y" /> {ov?.overlaps.length ?? 0} {ov?.overlaps.length === 1 ? 'overlap' : 'overlaps'} open</span>
            <span><Icon name="exclamation-diamond" /> {d?.contested.length ?? 0} contested {d?.contested.length === 1 ? 'file' : 'files'}</span>
            {data.health?.coordination && <span><Icon name="shield-check" /> {data.health.coordination.denied} {data.health.coordination.denied === 1 ? 'edit' : 'edits'} refused once</span>}
          </a>
        </span>}
      />
      {overview.kind === 'error' && <div className="notice tone-bad" role="alert"><Icon name="exclamation-triangle" className="notice-icon" /><div className="notice-body"><strong>Could not load the work overview</strong><div>{overview.message}</div></div><button type="button" className="btn btn-sm btn-outline-secondary" onClick={overview.retry}>Retry</button></div>}
      <div className="g-now">
        <section className="g-needs" aria-label="Needs you">
          {ov && d && needs.map((need, i) => <LiveNeedCard key={need.key} n={need} i={i} ov={ov} d={d} live={live} />)}
          {ov && d && needs.length === 0 && <article className="g-need is-done"><span className="g-ring tone-ok"><Icon name="check2" /></span><div className="g-need-body"><span className="g-need-title small">Nothing is waiting on you</span><span className="g-need-outcome">Blocked work, contested files, overlaps and handovers show up here first.</span></div></article>}
          {loading && <div className="g-need g-skeleton" aria-hidden="true"><span /><span /><span /></div>}
        </section>
        <aside className="g-col g-side" aria-label="Around you">
          <Together d={d} items={current} />
          <ProcessCard live={live} runs={runs} />
        </aside>
      </div>
      <Box icon="diagram-3" title="Every unfinished change" count={current.length} explain="Each lane shows the furthest step the Hub can see: an open pull request for the branch, a skill gate run on it, or an overlap with another task. Reaching a step is not proof it passed; open a change for its evidence.">
        {ov ? <LiveTrack items={current} ov={ov} live={live} runs={runs} /> : <div className="g-skeleton-rows" aria-hidden="true"><span /><span /><span /></div>}
      </Box>
      <div className="g-now-foot">
        <Connections live={live} />
        <Recent ov={ov} />
      </div>
    </>
  )
}

const KIND_LABEL: Record<Need['kind'], string> = { blocked: 'Blocked', quiet: 'Gone quiet', handover: 'Handover', contested: 'Contested file', overlap: 'Overlap', pr: 'Pull request' }

function LiveNeedCard({ n, i, ov, d, live }: { n: Need; i: number; ov: WorkOverview; d: Derived; live: LiveProps }) {
  const { icon, title, gist, body, step } = needView(n, ov, d, live.handoff.me)
  const acts = useStepActions(step, { viewer: live.handoff.me, repo: live.handoff.repo, originUrl: live.handoff.originUrl })
  const [open, setOpen] = useState(false)
  const item = n.item
  const l = item ? lead(item) : null
  return (
    <article className={`g-need tone-${n.tone === 'brand' ? 'warn' : n.tone}`} style={{ '--i': i } as CSSProperties}>
      <div className="g-need-context">
        <span className={`g-ring tone-${n.tone}`}><Icon name={icon} /></span>
        <span className="g-need-who">{KIND_LABEL[n.kind]}{l ? ` · ${l.login}’s ${l.agent}` : ''}</span>
        {n.at > 0 && <span className="g-need-wait"><Icon name="clock" /> <Ago at={n.at} /></span>}
      </div>
      <h2 className="g-need-title">{title}</h2>
      {gist && <p className="g-need-reason">{gist}</p>}
      {body && <p className="g-need-reason">{body}</p>}
      <div className="g-acts">
        {acts.primary && <StepAction a={acts.primary} className="btn btn-primary" />}
        {acts.more.map((a) => <StepAction key={a.key} a={a} className="btn btn-outline-secondary" />)}
        {step && <button type="button" className="px-link" aria-expanded={open} onClick={() => setOpen(!open)}><Icon name={open ? 'chevron-up' : 'chevron-down'} /> {open ? 'Less' : 'Who decides and why'}</button>}
      </div>
      {step && <Reveal open={open}><StepDetail step={step} blanks={acts.blanks} words={acts.words} setWord={acts.setWord} /></Reveal>}
    </article>
  )
}

/** Every unfinished change as one row; the step pills are the lifecycle and filter the rows. */
export function LiveTrack({ items, ov, live, runs }: { items: WorkItem[]; ov: WorkOverview; live: LiveProps; runs: RunView[] }) {
  const pulls = live.data.work?.pull_requests ?? []
  const [sel, setSel] = useState<number | null>(null)
  const placed = items.map((w) => ({ w, at: stepOf(w, pulls, runs, ov), tone: toneOf(w) }))
  const rows = placed.filter((x) => sel === null || x.at === sel)
  if (items.length === 0) return <p className="g-empty"><Icon name="inbox" /> No unfinished change. A lane appears when an agent claims work with <code>agentscm_start</code>.</p>
  return (
    <div className="px-track g-track">
      <div className="px-track-row head">
        <span className="px-track-task px-colhead">{sel !== null ? <button type="button" className="px-link" onClick={() => setSel(null)}><Icon name="x-circle" /> Every step</button> : 'Change'}</span>
        <div className="px-stagehead" role="group" aria-label="Show one step">
          {STEPS.map((s, i) => {
            const count = placed.filter((x) => x.at === i).length
            const bad = placed.some((x) => x.at === i && x.tone === 'bad')
            const warn = placed.some((x) => x.at === i && x.tone === 'warn')
            return (
              <button key={s.key} type="button" title={s.label} aria-pressed={sel === i} className={`px-stagecell${sel === i ? ' on' : ''}${bad ? ' bad' : warn ? ' wait' : ''}`} onClick={() => setSel(sel === i ? null : i)}>
                <Icon name={s.icon} /><span className="px-stagecell-label">{'short' in s ? s.short : s.label}</span><Tick k={`${s.key}-${count}`}><span className="px-stagecell-n">{count}</span></Tick>
              </button>
            )
          })}
        </div>
      </div>
      {rows.map(({ w, at, tone }, i) => {
        const pr = prFor(w, pulls)
        const l = lead(w)
        return (
          <a key={w.id} href={`#work/${w.id}`} className="px-track-row g-row" style={{ viewTransitionName: `g-w-${w.id}`, '--i': i } as CSSProperties}>
            <span className="px-track-task">
              <span className="px-track-title">{w.title}</span>
              <span className="px-status"><span className={`px-dot tone-${dotTone(tone)}`} aria-hidden="true" />{stateText(w)}</span>
              <span className="px-track-meta">{l.login}’s {l.agent}{pr ? ` · PR #${pr.number}` : w.branch ? ` · ${w.branch}` : ''} · <Ago at={w.updated_at} /></span>
            </span>
            <LiveLane key={`${w.id}-${at}-${tone}`} at={at} tone={tone} holder={`${l.login}-${l.agent}`} label={`${w.title}: ${STEPS[at]?.label}, ${stateText(w)}`} />
          </a>
        )
      })}
    </div>
  )
}

/** The Hub's lane bar across the six lifecycle steps; the current one carries the holder's face. */
export function LiveLane({ at, tone, holder, label, note }: { at: number; tone: string; holder: string; label: string; note?: string }) {
  const kind = tone === 'bad' ? ' lb-fail' : tone === 'handover' ? ' lb-ready' : tone === 'warn' ? ' lb-stale' : ''
  const cols = STEPS.map((_, i) => (note && i === at ? 'minmax(0, 3fr)' : 'minmax(0, 1fr)')).join(' ')
  return (
    <div className="lb-wrap">
      <ol className="lb px-lb" style={{ gridTemplateColumns: cols }} aria-label={label}>
        {STEPS.map((s, i) => (
          <li key={s.key} className={`lb-seg${i < at ? ' lb-done' : i === at ? ` is-here is-new${kind}` : ' lb-none'}`}>
            <span className="lb-hit" title={`${s.label}${i < at ? ': passed' : i === at ? '' : ': not yet'}`}>
              {i === at ? <><span className="baton"><Avatar name={holder} size={24} /></span><span className="lb-label">{s.label}</span>{note && <span className="lb-note">{note}</span>}</> : <Icon name={i < at ? 'check2' : s.icon} />}
            </span>
          </li>
        ))}
      </ol>
    </div>
  )
}

function Together({ d, items }: { d: Derived | null; items: WorkItem[] }) {
  const nowS = useNow() / 1000
  if (!d) return <Box icon="people" title="Working together"><div className="g-skeleton-rows" aria-hidden="true"><span /><span /></div></Box>
  return (
    <Box icon="people" title="Working together" count={d.groups.length} explain="People first; their agents alongside. Awake means the agent reported activity in the last five minutes.">
      {d.groups.length === 0 && <p className="small muted mb-0">No agent is connected right now. <a href="#setup">Connect one</a>.</p>}
      <ul className="g-people">
        {d.groups.map((g) => {
          const awake = g.agents.filter((a) => nowS - a.lastActive < 300).length
          const blocked = items.some((i) => i.owner_login === g.owner && i.state === 'blocked')
          const tone = blocked ? 'bad' : awake ? 'ok' : 'warn'
          return (
            <li key={g.owner}>
              <Avatar name={g.owner} size={36} />
              <span className="g-person">
                <span className="g-person-top"><b>{g.owner}</b><span className="px-status"><span className={`px-dot tone-${tone}`} aria-hidden="true" />{blocked ? 'Has a blocked task' : awake ? `${awake} awake` : 'Quiet'}</span></span>
                <span className="g-agents">{g.agents.map((a) => <span key={a.id} className="g-agent"><Avatar name={`${g.owner}-${a.name}`} size={18} />{a.name}{a.holdings.length ? ` · ${a.holdings.length} file${a.holdings.length === 1 ? '' : 's'}` : ''}</span>)}</span>
              </span>
            </li>
          )
        })}
      </ul>
    </Box>
  )
}

function ProcessCard({ live, runs }: { live: LiveProps; runs: RunView[] }) {
  const lib = usePoll((signal) => fetchLibrary(live.repo, live.token, signal), 60_000, `${live.repo}:library`)
  const pins = lib.kind === 'ready' ? lib.value.adoption.pins.filter((p) => p.status === 'adopted') : []
  const entry = lib.kind === 'ready' ? lib.value.catalog.find((c) => pins.some((p) => p.name === c.name)) : undefined
  const open = runs.filter((r) => r.state !== 'completed').length
  return (
    <a href="#process" className="g-process">
      <span className="g-process-head"><Icon name="diagram-3" /> {lib.kind === 'loading' ? 'Your process' : lib.kind === 'error' ? 'Could not read skills' : pins.length ? 'Your process is attached' : 'No process adopted yet'}</span>
      <b>{entry ? `${entry.name} · v${entry.workflow_version}` : lib.kind === 'loading' ? 'Reading skills…' : lib.kind === 'error' ? 'Open Workflows to retry' : 'Pick a skill or workflow'}</b>
      {entry && <span className="g-process-dots" aria-hidden="true">{entry.gates.slice(0, 6).map((g) => <span key={g.id} title={g.id}><Icon name={g.enforce.includes('stop') ? 'shield-check' : 'circle'} /></span>)}</span>}
      <span className="small">{entry ? `${entry.gates.length} steps · ${open} open run${open === 1 ? '' : 's'}` : lib.kind === 'loading' ? '' : 'Agents follow whatever the repository adopts; nothing runs until you choose.'}</span>
    </a>
  )
}

function Connections({ live }: { live: LiveProps }) {
  const setup = usePoll((signal) => fetchSetup(live.token, signal), 30_000, 'g-setup')
  const nowMs = useNow()
  const rows = setup.kind === 'ready' ? setup.value : []
  const states = rows.map((r) => ({ r, s: setupState(r, nowMs, true) }))
  const gaps = states.filter((x) => x.s !== 'Ready').length
  const h = live.data.health
  return (
    <Box icon="plug" title="Connections" count={setup.kind === 'ready' ? (gaps ? `${gaps} to check` : 'All ready') : undefined} tone={gaps ? 'warn' : 'ok'}>
      <ul className="g-conns">
        <li><span className={`px-dot tone-${live.data.failing ? 'bad' : h?.status === 'ok' ? 'ok' : 'warn'}`} aria-hidden="true" /><span>Hub</span><span className="small muted">{live.data.failing ? 'not answering' : h?.status === 'ok' ? 'answering' : 'checking'}</span></li>
        <li><span className={`px-dot tone-${live.data.work?.connected ? 'ok' : 'muted'}`} aria-hidden="true" /><span>Forge</span><span className="small muted">{live.data.work?.connected ? 'connected' : 'not connected'}</span></li>
        {states.slice(0, 6).map(({ r, s }) => <li key={r.key}><span className={`px-dot tone-${s === 'Ready' ? 'ok' : s === 'Blocked' ? 'bad' : 'warn'}`} aria-hidden="true" /><span>{r.name} <span className="small muted">· {r.owner}</span></span><span className="small muted">{s.toLowerCase()}</span></li>)}
        {setup.kind === 'ready' && rows.length === 0 && <li><span className="px-dot tone-muted" aria-hidden="true" /><span>Agents</span><span className="small muted">none verified yet</span></li>}
      </ul>
      <a href="#setup" className="px-link">Agents and connections <Icon name="arrow-right" /></a>
    </Box>
  )
}

function Recent({ ov }: { ov: WorkOverview | null }) {
  const items = (ov?.recent ?? []).slice(0, 4)
  return (
    <section className="g-contrib" aria-label="Recently reported">
      <div className="g-contrib-head"><Icon name="stars" /> Recently reported</div>
      {items.length === 0 && <p className="small mb-0">No completions or decisions reported in this window yet.</p>}
      <ul>{items.map((o) => <li key={o.seq}><Icon name={o.kind === 'completion' ? 'flag' : 'signpost-split'} /><span><a href={`#work/${o.item.id}`} className="px-link-quiet">{o.item.title}</a> <span className="small muted">· {o.kind === 'completion' ? 'reported finished' : 'decision'} <Ago at={o.at} /></span></span></li>)}</ul>
      <a href="#contributions" className="px-link">What the Hub has recorded <Icon name="arrow-right" /></a>
    </section>
  )
}
