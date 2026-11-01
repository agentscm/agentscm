// G · Now: decisions for your role, each answerable in place, then every change as a lane, then the people,
// the process, the connections and what AgentSCM contributed. Resolving anything updates every view at once.
import { useState, type CSSProperties, type ReactNode } from 'react'
import { Avatar, Box, Icon, PageHead, Pill } from '../../ui.tsx'
import { CONNECTIONS, NEEDS, STAGES, VALUE, type Need, type StageId, type Work } from '../data.ts'
import { Go, ProofPill, morph, useProto } from '../kit.tsx'
import { LifeLane } from '../ab.tsx'
import { PEOPLE } from './gdata.ts'
import { copyText } from './copy.ts'
import { useStore } from './store.tsx'

const WORDS = ['Nothing', 'One', 'Two', 'Three', 'Four', 'Five', 'Six']
const NOUN = { author: ['thing needs', 'things need'], reviewer: ['decision needs', 'decisions need'], lead: ['question needs', 'questions need'] } as const

/** A count that re-renders with a soft blur-in whenever it changes. */
export const Tick = ({ children, k }: { children: ReactNode; k: string | number }) => <span key={k} className="g-tick">{children}</span>

export function Now() {
  const { role } = useProto()
  const st = useStore()
  const open = st.openFor(role)
  const mine = NEEDS.filter((n) => n.roles.includes(role))
  const moving = st.works.filter((w) => w.tone === 'brand' || w.tone === 'ok').length
  const n = open.length
  const [one, many] = NOUN[role]
  return (
    <>
      <PageHead
        question="What needs me?"
        answer={n
          ? <><Tick k={n}>{WORDS[n] ?? n}</Tick> {n === 1 ? one : many} you.<br /><span className="g-quiet">The rest has a next step.</span></>
          : <><Tick k="none">Nothing needs you.</Tick><br /><span className="g-quiet"><Tick k={moving}>{WORDS[moving] ?? moving}</Tick> changes are moving on their own.</span></>}
        fact={<FactLine />}
      />
      <div className="g-now">
        <section className="g-needs" aria-label="Needs you">
          {mine.map((need, i) => <NeedCard key={need.id} n={need} i={i} />)}
        </section>
        <aside className="g-col g-side" aria-label="Around you">
          <Together />
          <ProcessCard />
        </aside>
      </div>
      <Box icon="diagram-3" title="Every change, from claim to follow-up" count={st.works.length} explain="Each lane shows the stage of Get changes ready for review a change has reached, with the face of the agent holding it. Reaching a stage is not proof it passed; open a change for its evidence.">
        <GTrack />
      </Box>
      <div className="g-now-foot">
        <Connections />
        <Contributions />
      </div>
    </>
  )
}

function FactLine() {
  const st = useStore()
  const settled = 4 + (st.resolved.has('n2') ? 1 : 0)
  return (
    <span className="g-fact">
      <span>acme/widgets · 7 changes · 3 people with 6 agents</span>
      <Go to="value" className="g-strip" label="This week’s contributions">
        <span><Icon name="arrow-left-right" /> 6 overlaps noticed, <Tick k={settled}>{settled}</Tick> settled</span>
        <span><Icon name="terminal" /> 18 local checks</span>
        <span><Icon name="cloud-check" /> 2 local results CI accepted</span>
      </Go>
    </span>
  )
}

const agentLine = (n: Need, ws: (id: string) => Work | undefined) => {
  if (n.id === 'n5') return 'sam’s copilot · on sam’s computer'
  const parts = n.work.map(ws).filter((w): w is Work => !!w)
  if (n.id === 'n2') return parts.map((w) => `${w.owner}’s ${w.agents[0]?.tool ?? 'agent'}`).join(' + ')
  const w = parts[0]
  return w ? `${w.owner}’s ${w.agents[0]?.tool ?? 'agent'} · ${w.prs[0] ? `PR #${w.prs[0].n}` : w.title}` : ''
}

export function NeedCard({ n, i }: { n: Need; i: number }) {
  const st = useStore()
  const { go } = useProto()
  const outcome = st.resolved.get(n.id)
  const w = n.work[0] ? st.work(n.work[0]) : undefined
  const style = { '--i': i, viewTransitionName: `g-need-${n.id}` } as CSSProperties
  if (outcome) {
    return (
      <article className="g-need is-done" style={style}>
        <span className="g-ring tone-ok g-done-ring"><Icon name="check2" /></span>
        <div className="g-need-body"><span className="g-need-title small">{n.title}</span><span className="g-need-outcome">{outcome}. Slack and the agent show the same.</span></div>
      </article>
    )
  }
  return (
    <article className={`g-need tone-${n.tone}`} style={style}>
      <div className="g-need-context">
        <span className={`g-ring tone-${n.tone}`}><Icon name={n.icon} /></span>
        <span className="g-need-who">{agentLine(n, st.work)}</span>
        <span className="g-need-wait"><Icon name="clock" /> {n.waiting}{n.blocks ? ` · ${n.blocks.toLowerCase()}` : ''}</span>
      </div>
      <h2 className="g-need-title">{n.title}</h2>
      <p className="g-need-reason">{n.reason}</p>
      {n.id === 'n1' && w && <>
        <div className="g-chips">{w.checks.filter((c) => c.proof !== 'skipped').map((c) => <span key={c.name} className="g-chip-proof"><ProofPill proof={c.proof} /><span>{c.name}</span></span>)}</div>
        <div className="g-acts"><button type="button" className="btn btn-primary" onClick={() => go(`work/${w.id}`)}>Review the change <Icon name="arrow-right" /></button>{n.risk && <Pill tone="warn" icon="door-closed">{n.risk}</Pill>}</div>
      </>}
      {n.id === 'n2' && <Overlap />}
      {n.id === 'n3' && <Question />}
      {n.id === 'n4' && <>
        <blockquote className="g-quote"><Icon name="quote" /> Desktop pass done. The responsive pass remains: settings must fit at 375px.</blockquote>
        <div className="g-acts"><button type="button" className="btn btn-primary" onClick={st.handOver}><Icon name="box-arrow-in-right" /> Hand it to my claude-code</button></div>
      </>}
      {n.id === 'n5' && <>
        <div className="g-command"><Icon name="terminal" /><code>agentscm client doctor --agent copilot --fix</code></div>
        <CopyFix />
      </>}
    </article>
  )
}

/** Two agents reaching for one file, drawn, then three ways to settle it. */
function Overlap() {
  const st = useStore()
  const [pick, setPick] = useState<'keep' | 'hand' | 'split'>('split')
  const options = [
    { id: 'split' as const, title: 'Split the file', sub: 'maria keeps tax; dev takes the rounding helpers', icon: 'scissors' },
    { id: 'keep' as const, title: 'maria keeps it', sub: 'dev’s codex waits for the billing change', icon: 'lock' },
    { id: 'hand' as const, title: 'dev goes first', sub: 'maria’s claude-code pauses on totals.rs', icon: 'arrow-right-circle' },
  ]
  return (
    <>
      <div className="g-collide" aria-label="maria’s claude-code holds src/billing/totals.rs; dev’s codex wants it">
        <span className="g-collide-agent"><Avatar name="maria-claude-code" size={34} /><span>maria’s claude-code<small>holds it</small></span></span>
        <span className="g-collide-line" aria-hidden="true" />
        <span className="g-collide-file"><Icon name="file-earmark-code" /><code>totals.rs</code><span className="g-collide-x"><Icon name="lightning-charge" /></span></span>
        <span className="g-collide-line" aria-hidden="true" />
        <span className="g-collide-agent end"><span>dev’s codex<small>wants it</small></span><Avatar name="dev-codex" size={34} /></span>
      </div>
      <fieldset className="g-options">
        <legend className="visually-hidden">How to settle it</legend>
        {options.map((o) => (
          <label key={o.id} className={`g-option${pick === o.id ? ' on' : ''}`}>
            <input type="radio" name="g-overlap" value={o.id} checked={pick === o.id} onChange={() => morph(() => setPick(o.id))} />
            <Icon name={o.icon} /><span><b>{o.title}</b><small>{o.sub}</small></span>
          </label>
        ))}
      </fieldset>
      <div className="g-acts"><button type="button" className="btn btn-primary" onClick={() => st.settle(pick)}>Record the agreement</button><span className="small muted">Both agents read it before their next edit.</span></div>
    </>
  )
}

function CopyFix() {
  const st = useStore()
  const [failed, setFailed] = useState(false)
  const cmd = 'agentscm client doctor --agent copilot --fix'
  return (
    <div className="g-acts">
      <button type="button" className="btn btn-primary" onClick={() => void copyText(cmd).then((ok) => (ok ? st.fixCopilot() : setFailed(true)))}><Icon name="clipboard" /> Copy the fix for sam</button>
      {failed && <span className="px-err" role="alert">The clipboard refused. Select the command above and copy it.</span>}
    </div>
  )
}

function Question() {
  const st = useStore()
  return (
    <>
      <div className="g-bubble"><Avatar name="maria-codex" size={26} /><span>Should webhook deliveries retry <b>5 times</b> or <b>8 times</b> before giving up?</span></div>
      <div className="g-acts">
        <button type="button" className="btn btn-primary" onClick={() => st.answer('n3', '8')}>8 tries, with backoff</button>
        <button type="button" className="btn btn-outline-secondary" onClick={() => st.answer('n3', '5')}>5 tries</button>
      </div>
    </>
  )
}

/** Every change as one row; the stage pills on top are the process itself and filter the rows. */
export function GTrack({ filter }: { filter?: (w: Work) => boolean }) {
  const st = useStore()
  const [sel, setSel] = useState<StageId | null>(null)
  const base = filter ? st.works.filter(filter) : st.works
  const rows = base.filter((w) => !sel || w.stage === sel)
  const waiting = new Set(NEEDS.filter((x) => !st.resolved.has(x.id)).flatMap((x) => x.work).map((id) => st.work(id)?.stage))
  const blocked = new Set(st.works.filter((w) => w.tone === 'bad').map((w) => w.stage))
  return (
    <div className="px-track g-track">
      <div className="px-track-row head">
        <span className="px-track-task px-colhead">{sel ? <button type="button" className="px-link" onClick={() => morph(() => setSel(null))}><Icon name="x-circle" /> Every stage</button> : 'Change'}</span>
        <div className="px-stagehead" role="toolbar" aria-label="Show one stage">
          {STAGES.map((s) => {
            const count = base.filter((w) => w.stage === s.id).length
            return (
              <button key={s.id} type="button" aria-pressed={sel === s.id} className={`px-stagecell${sel === s.id ? ' on' : ''}${blocked.has(s.id) ? ' bad' : waiting.has(s.id) ? ' wait' : ''}`} title={s.plain} onClick={() => morph(() => setSel(sel === s.id ? null : s.id))}>
                <Icon name={s.icon} /><span className="px-stagecell-label">{s.short}</span><Tick k={`${s.id}-${count}`}><span className="px-stagecell-n">{count}</span></Tick>
              </button>
            )
          })}
        </div>
      </div>
      <div className="px-track-row loop" aria-hidden="true"><span /><span className="px-loopline"><svg viewBox="0 0 100 14" preserveAspectRatio="none"><path d="M41.7 1 C41.7 12, 8.3 12, 8.3 1" vectorEffect="non-scaling-stroke" /></svg><span className="px-loopline-label"><Icon name="arrow-return-left" /> findings go back to the author</span></span></div>
      {rows.map((w, i) => (
        <Go key={w.id} to={`work/${w.id}`} className="px-track-row g-row" style={{ viewTransitionName: `px-${w.id}`, '--i': i } as CSSProperties}>
          <span className="px-track-task">
            <span className="px-track-title">{w.title}</span>
            <span className="px-status"><span className={`px-dot tone-${w.tone}`} aria-hidden="true" />{w.state}</span>
            <span className="px-track-meta">{w.prs[0] ? `#${w.prs[0].n}` : 'no PR yet'} · {w.fresh}</span>
          </span>
          <LifeLane key={`${w.id}-${w.stage}-${w.tone}`} w={w} />
        </Go>
      ))}
      {rows.length === 0 && <p className="small muted p-3 mb-0">No change matches.</p>}
    </div>
  )
}

function Together() {
  const st = useStore()
  const mariaClear = st.resolved.has('n1') && st.resolved.has('n3')
  return (
    <Box icon="people" title="Working together" explain="People first; their agents alongside. A dot is what each person’s agents last reported.">
      <ul className="g-people">
        {PEOPLE.map((p) => {
          const tone = p.who === 'maria' && mariaClear ? 'brand' : p.tone
          const status = p.who === 'maria' && mariaClear ? 'Moving' : p.status
          return (
            <li key={p.who}>
              <Avatar name={p.who} size={36} />
              <span className="g-person"><span className="g-person-top"><b>{p.who}</b><span className="px-status"><span className={`px-dot tone-${tone}`} aria-hidden="true" />{status}</span></span><span className="g-agents">{p.agents.map((a) => <span key={a} className="g-agent"><Avatar name={`${p.who}-${a}`} size={18} />{a}</span>)}</span></span>
              </li>
          )
        })}
      </ul>
    </Box>
  )
}

function ProcessCard() {
  const st = useStore()
  return (
    <Go to="workflows" className="g-process">
      <span className="g-process-head"><Icon name="diagram-3" /> Your process is attached</span>
      <b>Get changes ready for review</b>
      <span className="g-process-dots" aria-hidden="true">{['terminal', 'eye', 'file-earmark-text', 'github'].map((ic) => <span key={ic}><Icon name={ic} /></span>)}</span>
      <span className="small">Local checks, an independent review, then the PR evidence. <span className="g-state-chip">{st.flows['f1']}</span></span>
    </Go>
  )
}

function Connections() {
  const st = useStore()
  const rows = CONNECTIONS.filter((c) => c.kind !== 'Advanced').map((c) => (c.name === 'copilot' && st.copilotFixed ? { ...c, state: 'ok' as const, note: 'Fix sent to sam' } : c))
  const gaps = rows.filter((c) => c.state === 'warn').length
  return (
    <Box icon="plug" title="Connections" count={gaps ? `${gaps} to fix` : 'All set'} tone={gaps ? 'warn' : 'ok'}>
      <ul className="g-conns">
        {rows.map((c) => <li key={c.name}><span className={`px-dot tone-${c.state}`} aria-hidden="true" /><span>{c.name}</span><span className="small muted">{c.kind === 'Optional' ? 'optional' : c.state === 'warn' ? 'needs a fix' : 'ready'}</span></li>)}
      </ul>
      <Go to="setup" className="px-link">Manage connections <Icon name="arrow-right" /></Go>
    </Box>
  )
}

function Contributions() {
  const st = useStore()
  const items = [...st.stories, ...VALUE.stories].slice(0, 3)
  return (
    <section className="g-contrib" aria-label="This week">
      <div className="g-contrib-head"><Icon name="stars" /> Useful work, with a trail</div>
      <ul>{items.map((s) => <li key={s.text} className={'fresh' in s && s.fresh ? 'fresh' : ''}><Icon name={s.icon} /><span>{s.text}</span></li>)}</ul>
      <Go to="value" className="px-link">Every contribution and its record <Icon name="arrow-right" /></Go>
    </section>
  )
}
