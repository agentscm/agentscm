// G · Work: every change by stage, and one change in full: purpose, evidence by stage with what it covers,
// source, follow-up and a live timeline. "Review & decide" opens as a sheet; deciding moves the lane everywhere.
import { useEffect, useRef, useState, type CSSProperties } from 'react'
import { Avatar, Box, Icon, PageHead, Pill, Reveal } from '../../ui.tsx'
import { CHECK_STAGE, PROOF, STAGES, type Check, type Work } from '../data.ts'
import { DoorPill, Go, PrBody, ProofPill, morph, readiness, useProto } from '../kit.tsx'
import { COMPONENTS } from './gdata.ts'
import { GTrack, Tick } from './Now.tsx'
import { copyText } from './copy.ts'
import { useStore } from './store.tsx'

const FILTERS = [
  { id: 'all', label: 'Every change', test: () => true },
  { id: 'people', label: 'Waiting on people', test: (w: Work) => w.tone === 'warn' || w.tone === 'bad' },
  { id: 'moving', label: 'Moving', test: (w: Work) => w.tone === 'brand' },
  { id: 'done', label: 'Merged', test: (w: Work) => w.stage === 'follow' },
] as const

export function WorkIndex() {
  const st = useStore()
  const [f, setF] = useState<(typeof FILTERS)[number]['id']>('all')
  const [goal, setGoal] = useState('')
  const [err, setErr] = useState('')
  const [made, setMade] = useState(false)
  const waiting = st.works.filter((w) => w.tone === 'warn' || w.tone === 'bad').length
  const filter = FILTERS.find((x) => x.id === f) ?? FILTERS[0]
  return (
    <>
      <PageHead question="Where is each change?" answer={<><Tick k={st.works.length}>{st.works.length}</Tick> changes. <span className="g-quiet"><Tick k={waiting}>{waiting}</Tick> wait on people; the rest are moving.</span></>} />
      <div className="g-filters" role="group" aria-label="Show">
        {FILTERS.map((x) => <button key={x.id} type="button" aria-pressed={f === x.id} className={f === x.id ? 'on' : ''} onClick={() => morph(() => setF(x.id))}>{x.label}<span>{st.works.filter(x.test).length}</span></button>)}
      </div>
      <Box icon="diagram-3" title="By stage" count={st.works.filter(filter.test).length}>
        <GTrack key={f} filter={filter.test} />
      </Box>
      <section className="g-start" aria-label="Start work">
        <div className="g-start-head"><span className="g-ring tone-brand"><Icon name="plus-lg" /></span><div><b>Start a change</b><span className="small muted d-block">Describe the outcome; your agent claims it and the lane appears here.</span></div></div>
        {made
          ? <p className="px-ok-text mb-0"><Icon name="clipboard-check" /> Copied a prompt for your agent: “{goal}”. It claims the work at session start.</p>
          : <div className="g-start-row">
            <input className="form-control" placeholder="Rate-limit webhook retries per tenant" value={goal} aria-label="Outcome" aria-invalid={!!err} aria-describedby={err ? 'g-start-err' : undefined} onChange={(e) => { setGoal(e.target.value); setErr('') }} />
            <button type="button" className="btn btn-primary" onClick={() => {
              if (goal.trim().length < 6) return setErr('Describe the outcome in a few words first')
              void copyText(`Claim this with AgentSCM and get it ready for review: ${goal.trim()}`).then((ok) => (ok ? morph(() => setMade(true)) : setErr('The clipboard refused. Copy the outcome above into your agent yourself.')))
            }}><Icon name="clipboard" /> Copy prompt for my agent</button>
          </div>}
        {err && <p className="px-err" id="g-start-err" role="alert">{err}</p>}
      </section>
    </>
  )
}

const INSPECT: Record<string, string[]> = {
  Tests: ['npm test · 214 passed, 0 failed · 38.2 s', 'Ran on a41c9e2 on maria’s computer', 'Signed: maria’s computer key · recipe v3'],
  'Independent review': ['Finding 1 · fixed: JPY totals skipped rounding', 'Finding 2 · fixed: no test for zero tax', 'Reviewer: codex, a different session from the author'],
  'Security scan': ['security-audit skill pinned at a1b2c3d', 'Scope: src/billing · 0 confirmed, 1 rejected', 'Coverage ledger validated'],
  CI: ['build · passed on a41c9e2', 'e2e · passed on a41c9e2', 'Observed from GitHub 6 min ago'],
}

function CheckRow({ c }: { c: Check }) {
  const [open, setOpen] = useState(false)
  const lines = INSPECT[c.name]
  const head = <>
    <span className={`g-ring small tone-${PROOF[c.proof].tone}`}><Icon name={PROOF[c.proof].icon} /></span>
    <span className="g-check-name">{c.name}<small>{c.detail}</small></span>
    <ProofPill proof={c.proof} />
    {lines && <Icon name="chevron-down" className="g-chev" />}
  </>
  return (
    <li className={`g-check${open ? ' open' : ''}`}>
      {lines
        ? <button type="button" className="g-check-head" aria-expanded={open} onClick={() => setOpen(!open)}>{head}</button>
        : <div className="g-check-head static">{head}</div>}
      {lines && <Reveal open={open}><ul className="g-inspect">{lines.map((l) => <li key={l}><code>{l}</code></li>)}</ul></Reveal>}
    </li>
  )
}

export function WorkDetail({ id }: { id: string }) {
  const st = useStore()
  const { go } = useProto()
  const w = st.work(id)
  const [copied, setCopied] = useState<'no' | 'ok' | 'failed'>('no')
  if (!w) return <p className="muted">That change isn’t in the sample.</p>
  const at = STAGES.findIndex((s) => s.id === w.stage)
  const deciding = w.id === 'w1' && w.stage === 'ready' && !st.resolved.has('n1')
  const comp = COMPONENTS.find((c) => c.work.includes(w.id))
  return (
    <article className="g-work">
      <Go to="work" className="px-link g-back"><Icon name="arrow-left" /> All work</Go>
      <header className="g-work-hero" style={{ viewTransitionName: `px-${w.id}` }}>
        <div>
          <h1 className="g-work-title">{w.title}</h1>
          <p className="g-work-context"><Avatar name={`${w.owner}-${w.agents[0]?.tool ?? ''}`} size={22} /> {w.owner} with {w.agents.map((a) => a.tool).join(' and ')} · {w.prs[0] ? `PR #${w.prs[0].n}` : 'no PR yet'} · <code>{w.source}</code></p>
        </div>
        <button type="button" className="btn btn-outline-secondary" onClick={() => void copyText(`Continue AgentSCM work ${w.id}: ${w.title}`).then((ok) => morph(() => setCopied(ok ? 'ok' : 'failed')))}><Icon name={copied === 'ok' ? 'clipboard-check' : 'terminal'} /> {copied === 'ok' ? 'Prompt copied' : copied === 'failed' ? 'Clipboard refused; try again' : 'Continue in my agent'}</button>
      </header>

      <section className={`g-status tone-${w.tone}`} aria-live="polite">
        <span className={`g-ring big tone-${w.tone}`}><Icon name={STAGES[at]?.icon ?? 'circle'} /></span>
        <div className="g-status-text"><b><Tick k={w.state}>{w.state}</Tick></b><p>{w.why}</p></div>
        {deciding
          ? <button type="button" id="g-review-cta" className="btn btn-primary g-cta" onClick={() => go(`work/${w.id}/review`)}>Review &amp; decide <Icon name="arrow-right" /></button>
          : <span className="g-status-next"><Icon name="arrow-right-short" /> {w.next}</span>}
      </section>

      <ol className="g-stepper" style={{ '--p': at / (STAGES.length - 1) } as CSSProperties} aria-label="Stages">
        {STAGES.map((s, i) => (
          <li key={s.id} className={i < at ? 'past' : i === at ? `now tone-${w.tone}` : ''} aria-current={i === at ? 'step' : undefined}>
            <span className="g-step-dot"><Icon name={i < at ? 'check2' : s.icon} /></span>
            <span className="g-step-label">{s.short}</span>
            <span className="g-step-sub">{i < at ? 'Recorded' : i === at ? 'Now' : 'Next'}</span>
          </li>
        ))}
      </ol>

      <div className="g-work-grid">
        <div className="g-col">
          <Box icon="bullseye" title="What this change is for">
            <p className="g-purpose">{w.purpose}</p>
            <ul className="g-reqs">{w.requirements.map((r) => <li key={r} className={at >= 3 ? 'met' : ''}><Icon name={at >= 3 ? 'check-circle-fill' : 'circle'} />{r}</li>)}</ul>
          </Box>
          <Box icon="patch-check" title="Checks and review" count={w.checks.length} explain="Grouped by the stage that earned them. Each row says how it is known; open one to see what it covered.">
            <p className="g-ready"><Icon name="info-circle" /> {readiness(w)}</p>
            {STAGES.map((s) => {
              const rows = w.checks.filter((c) => CHECK_STAGE[c.name] === s.id)
              if (!rows.length) return null
              return (
                <div key={s.id} className="g-check-group">
                  <span className="g-check-stage"><Icon name={s.icon} /> {s.label}</span>
                  <ul className="g-checks">{rows.map((c) => <CheckRow key={c.name} c={c} />)}</ul>
                </div>
              )
            })}
          </Box>
          <Box icon="code-square" title="Source">
            <dl className="g-source">
              <dt>Candidate</dt><dd><code>{w.source}</code></dd>
              <dt>Base</dt><dd><code>main · 61a70d2</code></dd>
              <dt>Files</dt><dd className="g-files">{w.paths.map((p) => <code key={p}>{p}</code>)}</dd>
              <dt>Applies to</dt><dd>{w.checks.some((c) => c.proof === 'stale') ? <span className="px-status"><span className="px-dot tone-warn" />Some results are from an earlier commit</span> : <span className="px-status"><span className="px-dot tone-ok" />Every result matches this candidate</span>}</dd>
            </dl>
          </Box>
          {w.prs.length > 0 && <Box icon="file-earmark-text" title="PR description, written from the record" explain="The agent writes the summary; evidence and merge danger come from the record, so the PR never claims more than it holds."><PrBody w={w} /></Box>}
          <section className="g-after">
            <span className="g-ring tone-brand"><Icon name="send" /></span>
            <div><b>After it merges</b><span className="small muted d-block">{w.followUps?.length ? `Done: ${w.followUps.join(', ')}` : 'Release notes from the changelog skill, on an available agent'}</span></div>
            <Go to="workflows" className="btn btn-sm btn-outline-secondary">See the step</Go>
          </section>
        </div>
        <aside className="g-col">
          <Box icon="people" title="People and agents">
            <ul className="g-people compact">
              {w.agents.map((a, i) => <li key={a.who + a.tool}><Avatar name={`${a.who}-${a.tool}`} size={30} /><span className="g-person"><b>{a.who === 'review' ? `${a.tool}` : a.who}</b><span className="small muted">{i === 0 ? `Owner · ${a.tool}` : 'Independent review session'}</span></span></li>)}
              {w.door === 'one-way' && <li><Avatar name="you" size={30} /><span className="g-person"><b>you</b><span className="small muted">Human reviewer</span></span></li>}
            </ul>
            <DoorPill door={w.door} />
          </Box>
          <Box icon="clock-history" title="Timeline">
            <Timeline items={w.activity} />
          </Box>
          <Box icon="link-45deg" title="Connected">
            <ul className="g-links">
              {w.prs.map((p) => <li key={p.n}><Icon name="github" /><span><b>#{p.n}</b><small>{p.state}</small></span><Pill tone={p.tone}>{p.tone === 'ok' ? 'observed' : 'open'}</Pill></li>)}
              {w.runs.map((r) => <li key={`${r.workflow}${r.n}`}><Icon name="diagram-3" /><span><b>{r.workflow}</b><small>Run {r.n} · {r.state}</small></span></li>)}
              {comp && <li><Icon name={comp.icon} /><span><Go to="repository" className="px-link-quiet"><b>{comp.name}</b></Go><small>Code, ownership and history</small></span></li>}
            </ul>
          </Box>
        </aside>
      </div>
    </article>
  )
}

function Timeline({ items }: { items: Work['activity'] }) {
  return (
    <ol className="g-timeline">
      {items.map((a, i) => <li key={a.at + a.text} className={a.at === 'just now' && i === 0 ? 'fresh' : ''}><span className="g-tl-dot" aria-hidden="true" /><span className="g-tl-text">{a.text}</span><span className="g-tl-at">{a.at}</span></li>)}
    </ol>
  )
}

/** Review & decide: the question, the evidence it rests on, a note, two answers. */
export function ReviewSheet({ id }: { id: string }) {
  const st = useStore()
  const { go } = useProto()
  const w = st.work(id)
  const [note, setNote] = useState('')
  const [err, setErr] = useState('')
  const box = useRef<HTMLTextAreaElement>(null)
  useEffect(() => {
    // Remember what opened the sheet, so closing it hands focus back there.
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null
    box.current?.focus()
    return () => { (document.getElementById('g-review-cta') ?? opener)?.focus() }
  }, [])
  useEffect(() => {
    const esc = (e: KeyboardEvent) => { if (e.key === 'Escape') go(`work/${id}`) }
    window.addEventListener('keydown', esc)
    return () => window.removeEventListener('keydown', esc)
  }, [go, id])
  if (!w) return null
  const done = st.resolved.has('n1')
  const decide = (v: 'approve' | 'changes') => {
    if (v === 'changes' && note.trim().length < 4) {
      setErr('Say what should change, so the author’s agent can act on it')
      return
    }
    go(`work/${id}`)
    st.decide(id, v, note.trim())
  }
  return (
    <div className="g-sheet-scrim" onClick={() => go(`work/${id}`)}>
      <aside className="g-sheet" role="dialog" aria-modal="true" aria-label="Review the change" onClick={(e) => e.stopPropagation()}>
        <header className="g-sheet-head"><b>Review the change</b><button type="button" className="g-x" aria-label="Close" onClick={() => go(`work/${id}`)}><Icon name="x-lg" /></button></header>
        <span className="px-status"><span className="px-dot tone-warn" />Waiting on your judgment</span>
        <h2 className="g-sheet-title">{w.title} · PR #{w.prs[0]?.n}</h2>
        <p className="small muted">{w.purpose}</p>
        <section className="g-decision">
          <span className="g-k">The decision</span>
          <p className="g-decision-q">Is this one-way door ready to merge?</p>
          <ul className="g-decision-facts">{w.checks.filter((c) => c.name !== 'Approval').map((c) => <li key={c.name}><ProofPill proof={c.proof} /><span>{c.name}: {c.detail}</span></li>)}</ul>
        </section>
        <label className="g-k" htmlFor="g-note">Your note</label>
        <textarea id="g-note" ref={box} className="form-control" rows={4} placeholder="Rounding is unchanged for every fixture; tax per region reads well." value={note} aria-invalid={!!err} aria-describedby={err ? 'g-note-err' : undefined} onChange={(e) => { setNote(e.target.value); setErr('') }} />
        {err && <p className="px-err" id="g-note-err" role="alert">{err}</p>}
        {done
          ? <p className="px-ok-text"><Icon name="check2-circle" /> Already decided.</p>
          : <div className="g-acts"><button type="button" className="btn btn-primary" onClick={() => decide('approve')}><Icon name="check2" /> Approve</button><button type="button" className="btn btn-outline-secondary" onClick={() => decide('changes')}><Icon name="arrow-return-left" /> Request changes</button></div>}
        <p className="small muted mb-0"><Icon name="info-circle" /> Sample decision. Nothing is posted to GitHub and no code merges.</p>
      </aside>
    </div>
  )
}
