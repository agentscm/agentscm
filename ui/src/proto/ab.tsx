// The three A+B options. Each keeps A's frame (top bar, rail, sheet, answer first) and fuses B's process picture
// into Now using the Hub's own vocabulary: the lane bar and its baton ride, map tiles with faces, Needs you rows.
import { useState, type CSSProperties } from 'react'
import { Avatar, Box, Icon, PageHead, Pill } from '../ui.tsx'
import { CHECK_STAGE, FLOWS, NEEDS, STAGES, WORK, needsFor, work, type StageId, type Work } from './data.ts'
import { ChangedList, DoorPill, Go, NeedRow, People, PrBody, ProofPill, SetupSummary, ValueStrip, morph, readiness, useProto } from './kit.tsx'
import { ANSWER, RailShell } from './VariantA.tsx'

export const VariantD = () => <RailShell now={<TrackNow />} />
export const VariantE = () => <RailShell now={<MapNow />} />
export const VariantF = () => <RailShell now={<JourneyNow />} />

const at = (w: Work) => STAGES.findIndex((s) => s.id === w.stage)
const movingCount = () => WORK.filter((w) => w.tone === 'brand' || w.tone === 'ok').length

function useOpen() {
  const { role, resolved } = useProto()
  const mine = needsFor(role)
  return { mine, open: mine.filter((n) => !resolved.has(n.id)), resolved, role }
}

/** Stages where a person is needed (any role) and stages holding blocked work. Waiting is amber; blocked is coral. */
function useStageSignals() {
  const { resolved } = useProto()
  const waiting = new Set(NEEDS.filter((n) => !resolved.has(n.id)).flatMap((n) => n.work).map((id) => work(id)?.stage))
  const blocked = new Set(WORK.filter((w) => w.tone === 'bad').map((w) => w.stage))
  return { waiting, blocked }
}

const Status = ({ w }: { w: Work }) => <span className="px-status"><span className={`px-dot tone-${w.tone}`} aria-hidden="true" />{w.state}</span>

/** The lifecycle as the Hub's lane bar: six segments, the current one carrying the holder's face (the baton ride). */
export function LifeLane({ w, focus = false }: { w: Work; focus?: boolean }) {
  const here = at(w)
  const cols = STAGES.map((_, i) => (focus && i === here ? 'minmax(0, 3fr)' : 'minmax(0, 1fr)')).join(' ')
  const kind = w.tone === 'bad' ? ' lb-fail' : w.state === 'Handed over' ? ' lb-ready' : w.tone === 'warn' ? ' lb-stale' : ''
  const holder = w.agents[0]
  return (
    <div className="lb-wrap">
      <ol className="lb px-lb" style={{ gridTemplateColumns: cols }} aria-label={`${w.title}: ${STAGES[here]?.label ?? ''}, ${w.state}`}>
        {STAGES.map((s, i) => (
          <li key={s.id} className={`lb-seg${i < here ? ' lb-done' : i === here ? ` is-here is-new${kind}` : ' lb-none'}`}>
            <span className="lb-hit" title={i < here ? `${s.label}: passed` : i === here ? `${s.label}: ${w.state}` : `${s.label}: not yet`}>
              {i === here
                ? <>{holder && <span className="baton"><Avatar name={`${holder.who}-${holder.tool}`} size={24} /></span>}<span className="lb-label">{s.short}</span>{focus && <span className="lb-note">{w.state}</span>}</>
                : <Icon name={i < here ? 'check2' : s.icon} />}
            </span>
          </li>
        ))}
      </ol>
    </div>
  )
}

// ---------- 1 · Track: every change as a lane; the process graph is the column header ----------

function TrackNow() {
  const { mine, open, role } = useOpen()
  const { waiting, blocked } = useStageSignals()
  const [sel, setSel] = useState<StageId | null>(null)
  const rows = WORK.filter((w) => !sel || w.stage === sel)
  return (
    <>
      <PageHead question="What needs me?" answer={ANSWER[role](open.length, movingCount())} />
      {mine.length > 0 && <Box icon="bell" title="Needs you" count={open.length} tone={open.length ? 'warn' : 'ok'} explain="Only what a person can act on now, ordered by risk, what it blocks and how long it has waited.">
        <div className="px-needs">{mine.map((n) => <NeedRow key={n.id} n={n} />)}</div>
      </Box>}
      <Box icon="diagram-3" title="Every change, from claim to follow-up" count={sel ? `${rows.length} of ${WORK.length}` : WORK.length} explain="Each row is a change; its lane shows the stage of Get changes ready for review it has reached. Pick a stage to see only the changes there. A stage reached is not proof it passed; open the change for its evidence." actions={sel ? <button type="button" className="px-link" onClick={() => morph(() => setSel(null))}>Show every stage</button> : undefined}>
        <div className="px-track">
          <div className="px-track-row head">
            <span className="px-track-task px-colhead">Change</span>
            <div className="px-stagehead" role="toolbar" aria-label="Show one stage">
              {STAGES.map((s) => {
                const n = WORK.filter((w) => w.stage === s.id).length
                return (
                  <button key={s.id} type="button" aria-pressed={sel === s.id} className={`px-stagecell${sel === s.id ? ' on' : ''}${blocked.has(s.id) ? ' bad' : waiting.has(s.id) ? ' wait' : ''}`} title={s.plain} onClick={() => morph(() => setSel(sel === s.id ? null : s.id))}>
                    <Icon name={s.icon} /><span className="px-stagecell-label">{s.short}</span><span className="px-stagecell-n">{n}</span>
                  </button>
                )
              })}
            </div>
          </div>
          <div className="px-track-row loop" aria-hidden="true"><span /><span className="px-loopline"><svg viewBox="0 0 100 14" preserveAspectRatio="none"><path d="M41.7 1 C41.7 12, 8.3 12, 8.3 1" vectorEffect="non-scaling-stroke" /></svg><span className="px-loopline-label"><Icon name="arrow-return-left" /> findings go back to the author</span></span></div>
          {rows.map((w) => (
            <Go key={w.id} to={`work/${w.id}`} className="px-track-row" style={{ viewTransitionName: `px-${w.id}` }}>
              <span className="px-track-task">
                <span className="px-track-title">{w.title}</span>
                <Status w={w} />
                <span className="px-track-meta">{w.prs[0] ? `#${w.prs[0].n}` : 'no PR yet'} · {w.fresh}</span>
              </span>
              <LifeLane w={w} />
            </Go>
          ))}
        </div>
      </Box>
      <div className="px-tower">
        <Box icon="clock-history" title="What changed" count={3}><ChangedList /></Box>
        <ValueStrip />
      </div>
      <SetupSummary />
    </>
  )
}

// ---------- 2 · Stage map: the stages as map tiles with the faces working there ----------

function MapNow() {
  const { mine, open, role } = useOpen()
  const { waiting, blocked } = useStageSignals()
  const [sel, setSel] = useState<StageId>(() => STAGES.find((s) => blocked.has(s.id) || waiting.has(s.id))?.id ?? 'review')
  return (
    <>
      <PageHead question="What needs me?" answer={ANSWER[role](open.length, movingCount())} />
      {mine.length > 0 && <Box icon="bell" title="Needs you" count={open.length} tone={open.length ? 'warn' : 'ok'}>
        <div className="px-needs">{mine.map((n) => <NeedRow key={n.id} n={n} />)}</div>
      </Box>}
      <Box icon="diagram-3" title="Where every change is" count={WORK.length} explain="One tile per stage of Get changes ready for review, left to right. Faces are the agents working there. Amber means a person is needed; coral means something is blocked.">
        <div className="px-stagemap" role="list">
          {STAGES.map((s) => {
            const here = WORK.filter((w) => w.stage === s.id)
            const faces = here.flatMap((w) => w.agents.slice(0, 1))
            const bad = blocked.has(s.id)
            const wait = !bad && waiting.has(s.id)
            return (
              <div key={s.id} className="px-stagetile-wrap" role="listitem">
                <button type="button" aria-pressed={sel === s.id} className={`tile px-stagetile${sel === s.id ? ' is-selected' : ''}${bad ? ' is-crossing' : wait ? ' px-is-wait' : ''}`} style={{ '--heat': Math.min(1, here.length / 2) } as CSSProperties} onClick={() => morph(() => setSel(s.id))}>
                  <span className="tile-head"><b>{s.label}</b><span className="px-tile-sub">{s.plain}</span></span>
                  <span className="tile-people">{faces.map((a, i) => <Avatar key={`${a.who}-${a.tool}-${i}`} name={`${a.who}-${a.tool}`} size={26} />)}</span>
                  <span className="tile-foot">
                    <span>{here.length ? `${here.length} ${here.length === 1 ? 'change' : 'changes'}` : 'Nothing here'}</span>
                    {(bad || wait) && <span className={`px-crossing${wait ? ' wait' : ''}`}>{bad ? 'Blocked' : 'Waiting'}</span>}
                  </span>
                </button>
              </div>
            )
          })}
        </div>
        <StageDrawer key={sel} stage={sel} />
      </Box>
      <div className="px-tower">
        <Box icon="clock-history" title="What changed" count={3}><ChangedList /></Box>
        <ValueStrip />
      </div>
      <SetupSummary />
    </>
  )
}

/** The area drawer for a stage: what defines it (the workflow step) and every change sitting in it, with the evidence earned here. */
function StageDrawer({ stage }: { stage: StageId }) {
  const s = STAGES.find((x) => x.id === stage)
  const flow = FLOWS.find((f) => f.id === 'f1')
  const step = flow?.steps.find((x) => x.id === s?.step)
  const here = WORK.filter((w) => w.stage === stage)
  if (!s) return null
  return (
    <section className="px-area-drawer" aria-label={s.label}>
      <h3>{s.label}</h3>
      {step && flow
        ? <p className="px-defined"><Icon name="sliders" /> Set by <Go to={`workflows/${flow.id}`} className="px-link">{flow.name}</Go>: {step.doWhat.charAt(0).toLowerCase() + step.doWhat.slice(1)}, as a {step.where.toLowerCase()}. Done when {step.done.charAt(0).toLowerCase() + step.done.slice(1)}.</p>
        : <p className="px-defined"><Icon name="sliders" /> {s.plain}. No workflow step requires anything here.</p>}
      {stage === 'review' && <p className="px-defined"><Icon name="arrow-return-left" /> Findings go back to Building, then return through Checks.</p>}
      {here.length === 0 && <p className="small muted mb-0">No change is at this stage right now.</p>}
      <ul className="px-drawer-rows">
        {here.map((w) => {
          const facts = w.checks.filter((c) => CHECK_STAGE[c.name] === stage)
          return (
            <li key={w.id}>
              <Go to={`work/${w.id}`} className="px-drawer-row" style={{ viewTransitionName: `px-${w.id}` }}>
                <span className="px-drawer-main"><span className="px-track-title">{w.title}</span><Status w={w} /></span>
                <People w={w} size={22} />
              </Go>
              {facts.length > 0 && <ul className="px-facts-line">{facts.map((c) => <li key={c.name}><ProofPill proof={c.proof} /> <span>{c.detail}</span></li>)}</ul>}
              <p className="px-next"><Icon name="arrow-right-short" /> {w.next}</p>
            </li>
          )
        })}
      </ul>
    </section>
  )
}

// ---------- 3 · Journey: pick any change; its whole journey and evidence stand beside the list ----------

function JourneyNow() {
  const { mine, open, role } = useOpen()
  const first = open[0]?.work[0] ?? WORK[0]?.id ?? ''
  const [sel, setSel] = useState(first)
  const pick = (id?: string) => id && morph(() => setSel(id))
  return (
    <>
      <PageHead question="What needs me?" answer={ANSWER[role](open.length, movingCount())} />
      <div className="px-journey-layout">
        <div className="px-col">
          {mine.length > 0 && <Box icon="bell" title="Needs you" count={open.length} tone={open.length ? 'warn' : 'ok'}>
            <div className="px-needs">{mine.map((n) => <NeedRow key={n.id} n={n} onOpen={() => pick(n.work[0])} picked={n.work[0] === sel} />)}</div>
          </Box>}
          <Box icon="activity" title="In motion" count={WORK.length} explain="Pick a change to see its journey: every stage, the evidence earned there, and what happens next.">
            <div className="px-rows">
              {WORK.map((w) => (
                <button key={w.id} type="button" className={`px-workrow px-pickrow${sel === w.id ? ' on' : ''}`} aria-pressed={sel === w.id} onClick={() => pick(w.id)}>
                  <span className={`px-dot tone-${w.tone}`} aria-hidden="true" />
                  <span className="px-workrow-main"><span className="px-workrow-title">{w.title}</span><span className="px-workrow-sub"><span className={`px-state tone-${w.tone}`}>{w.state}</span> · {STAGES[at(w)]?.short}</span></span>
                  <span className="px-fresh">{w.fresh}</span>
                </button>
              ))}
            </div>
          </Box>
          <ValueStrip />
        </div>
        <Journey id={sel} />
      </div>
      <SetupSummary />
    </>
  )
}

function Journey({ id }: { id: string }) {
  const { resolve, resolved } = useProto()
  const w = work(id)
  if (!w) return null
  const here = at(w)
  const act = `act-${w.id}`
  const wentBack = w.activity.some((a) => /sent back/.test(a.text))
  return (
    <aside className="px-journey" style={{ viewTransitionName: 'px-journey' }} aria-label={`The journey of ${w.title}`}>
      <div className="px-journey-head">
        <Status w={w} />
        <h2>{w.title}</h2>
        <div className="px-journey-tags">
          <People w={w} size={22} />
          <DoorPill door={w.door} />
          {w.prs.length ? w.prs.map((p) => <Pill key={p.n} tone={p.tone} icon="git">#{p.n} · {p.state}</Pill>) : <Pill tone="muted" icon="git">No PR yet</Pill>}
        </div>
      </div>
      <LifeLane key={w.id} w={w} focus />
      <p className="px-journey-why">{readiness(w)}</p>
      <ol className="px-stations">
        {STAGES.map((s, i) => {
          const facts = w.checks.filter((c) => CHECK_STAGE[c.name] === s.id)
          const state = i < here ? 'past' : i === here ? 'now' : 'later'
          return (
            <li key={s.id} className={`px-station ${state}${i === here ? ` tone-${w.tone}` : ''}`} aria-current={i === here ? 'step' : undefined}>
              <span className="px-station-mark" aria-hidden="true"><Icon name={i < here ? 'check2' : s.icon} /></span>
              <div className="px-station-body">
                <div className="px-station-title">{s.label}</div>
                {i === here && <div className="px-station-now">
                  <p>{w.why}</p>
                  {(w.tone === 'warn' || w.tone === 'bad') && (resolved.has(act)
                    ? <span className="px-ok-text small"><Icon name="check2" /> Done here; the agent and Slack see it too.</span>
                    : <button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => resolve(act)}>{w.next}</button>)}
                  {w.tone !== 'warn' && w.tone !== 'bad' && <p className="small muted mb-0"><Icon name="arrow-right-short" /> {w.next}</p>}
                </div>}
                {facts.length > 0 && <ul className="px-facts-line">{facts.map((c) => <li key={c.name}><ProofPill proof={c.proof} /> <span>{c.detail}</span></li>)}</ul>}
                {s.id === 'review' && wentBack && i <= here && <p className="px-station-loop"><Icon name="arrow-return-left" /> Findings went back to the author{i < here ? ', fixed, and came back' : ''}</p>}
                {i > here && facts.length === 0 && <p className="px-station-later">{s.plain}</p>}
              </div>
            </li>
          )
        })}
      </ol>
      {w.prs.length > 0 && <details className="px-journey-pr"><summary>PR description, written from the record</summary><PrBody w={w} /></details>}
      <Go to={`work/${w.id}`} className="px-link">Open the whole change <Icon name="arrow-right" /></Go>
    </aside>
  )
}
