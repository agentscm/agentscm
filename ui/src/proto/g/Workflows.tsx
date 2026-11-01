// G · Workflows: the process as a living graph on the frame's violet, a skill palette that adds steps in place,
// an inspector that answers "do what, who, done when, then", and a four-step way to add a workflow.
import { useState, type CSSProperties } from 'react'
import { Icon, PageHead, Pill } from '../../ui.tsx'
import { CI_FIT, DETECTED, FLOWS, KITS, type Flow, type FlowState, type Step } from '../data.ts'
import { Go, morph, useProto } from '../kit.tsx'
import { useStore } from './store.tsx'

const STATES: FlowState[] = ['Draft', 'Previewed', 'Ready to activate', 'Active']

const PALETTE: Step[] = [
  { id: 'scan', title: 'Security audit', doWhat: 'cloudflare/security-audit-skill, pinned at a1b2c3d', where: 'Existing agent', done: 'findings.json and its coverage ledger validate; no confirmed findings in scope', next: 'Independent review', icon: 'shield-check' },
  { id: 'docs', title: 'Update the docs', doWhat: 'The docs skill on changed public APIs', where: 'Existing agent', done: 'Docs mention every changed endpoint', next: 'Prepare evidence', icon: 'journal-text' },
  { id: 'bench', title: 'Benchmark', doWhat: 'npm run bench', where: 'Local command', done: 'No case slower than 5% against main', next: 'Request review', icon: 'speedometer2' },
  { id: 'deploy', title: 'Check the deploy', doWhat: 'Watch the preview deploy', where: 'External CI observed', done: 'GitHub reports the deployment healthy', next: 'Done', icon: 'rocket-takeoff' },
]

export function WorkflowsPage() {
  const st = useStore()
  const { route, go } = useProto()
  const flowId = route.parts[1] && route.parts[1] !== 'new' ? route.parts[1] : 'f1'
  const flow = FLOWS.find((f) => f.id === flowId) ?? FLOWS[1]!
  const steps = [...flow.steps, ...(st.extra[flow.id] ?? [])]
  const [sel, setSel] = useState<string>(steps[0]?.id ?? '')
  const step = steps.find((s) => s.id === sel)
  const state = st.flows[flow.id] ?? flow.state
  const at = STATES.indexOf(state)
  if (route.parts[1] === 'new') return <AddWorkflow />
  return (
    <>
      <PageHead question="What process applies here?" answer={<>Your process, made repeatable.<br /><span className="g-quiet">Keep the tools you already use.</span></>} fact="The repository sets when a step applies; skills and commands say how. Agents read it at session start." />
      <div className="g-flows">
        <aside className="g-flowlist" aria-label="Workflows">
          <span className="g-k">Pinned here</span>
          {FLOWS.map((f) => (
            <button key={f.id} type="button" className={`g-flowitem${f.id === flow.id ? ' on' : ''}`} onClick={() => morph(() => { go(`workflows/${f.id}`); setSel(f.steps[0]?.id ?? '') })}>
              <span><b>{f.name}</b><small>{f.steps.length + (st.extra[f.id]?.length ?? 0)} steps</small></span>
              <span className={`g-state-chip s-${(st.flows[f.id] ?? f.state).split(' ')[0]?.toLowerCase()}`}>{st.flows[f.id] ?? f.state}</span>
            </button>
          ))}
          <Go to="workflows/new" className="g-flowitem add"><Icon name="plus-lg" /> Add a workflow</Go>
          <span className="g-k mt-3">Add a step</span>
          <div className="g-palette">
            {PALETTE.map((p) => {
              const has = steps.some((s) => s.id === p.id)
              return <button key={p.id} type="button" className={`g-pal${has ? ' has' : ''}`} onClick={() => { if (!has) { st.addStep(flow.id, p); setSel(p.id) } else setSel(p.id) }}><Icon name={has ? 'check2' : p.icon} /> {p.title}</button>
            })}
          </div>
        </aside>
        <section className="g-canvas" aria-label={`${flow.name} as a graph`}>
          <div className="g-canvas-head">
            <span><Icon name="diagram-3" /> {flow.name}</span>
            <span className={`g-state-chip s-${state.split(' ')[0]?.toLowerCase()}`}>{state}</span>
          </div>
          <blockquote className="g-canvas-sentence">{flow.sentence}</blockquote>
          <FlowGraph flow={flow} steps={steps} sel={sel} onSel={(id) => morph(() => setSel(id))} />
          <div className="g-activate">
            <ol className="g-states" aria-label="Activation">
              {STATES.map((s, i) => <li key={s} className={i < at ? 'past' : i === at ? 'now' : ''}><span><Icon name={i <= at ? 'check2' : 'circle'} /></span>{s}</li>)}
            </ol>
            {at < STATES.length - 1
              ? <button type="button" className="btn btn-light" onClick={() => st.setFlow(flow.id, STATES[at + 1]!)}>{at === 0 ? 'Preview on recent changes' : at === 1 ? 'Mark ready' : 'Activate on this repository'}</button>
              : <span className="g-live"><span className="g-live-dot" /> Agents read this at session start</span>}
          </div>
        </section>
        <aside className="g-inspector" aria-label="Step">
          {step ? <div key={step.id} className="g-inspector-in">
            <span className="g-ring big tone-brand"><Icon name={step.icon} /></span>
            <h2>{step.title}</h2>
            <dl>
              <dt><Icon name="play-circle" /> Do what</dt><dd>{step.doWhat}</dd>
              <dt><Icon name={step.where === 'Local command' ? 'laptop' : step.where === 'Existing agent' ? 'robot' : 'cloud'} /> Who or where</dt><dd><Pill tone={step.where === 'External CI observed' ? 'muted' : 'brand'}>{step.where}</Pill></dd>
              <dt><Icon name="check2-square" /> Done when</dt><dd>{step.done}</dd>
              <dt><Icon name="arrow-right-circle" /> Then</dt><dd>{step.next}</dd>
              <dt><Icon name="pin-angle" /> Comes from</dt><dd>{PALETTE.some((p) => p.id === step.id) ? 'Added here · draft until you activate' : 'Repository rule · v3 pinned'}</dd>
            </dl>
          </div> : <p className="small muted">Select a step.</p>}
        </aside>
      </div>
      <section className="g-cifit" aria-label="Where CI jobs could run">
        <div><b>Where your CI jobs could run</b><span className="small muted d-block">From ci.yml. CI only skips work it is configured to accept, and records that it did.</span></div>
        <ul>{CI_FIT.map((c) => <li key={c.job}><code>{c.job}</code><Pill tone={c.tone}>{c.fit}</Pill></li>)}</ul>
      </section>
    </>
  )
}

/** Nodes on a dotted violet canvas; connectors draw themselves in; a dashed arc carries findings back. */
function FlowGraph({ flow, steps, sel, onSel }: { flow: Flow; steps: Step[]; sel: string; onSel: (id: string) => void }) {
  const W = 164, H = 104, GAP = 40, top = 24
  const width = steps.length * W + (steps.length - 1) * GAP + 8
  const ix = (id: string) => steps.findIndex((s) => s.id === id)
  const from = flow.loop ? ix(flow.loop.from) : -1
  const to = flow.loop ? ix(flow.loop.to) : -1
  const cx = (i: number) => 4 + i * (W + GAP) + W / 2
  const height = top + H + (from >= 0 && to >= 0 ? 70 : 30)
  return (
    <div className="g-graph-wrap">
      <div className="g-graph" style={{ width, height }}>
        <svg width={width} height={height} aria-hidden="true">
          {steps.slice(1).map((s, i) => {
            const x1 = 4 + i * (W + GAP) + W, x2 = x1 + GAP, y = top + H / 2
            return <path key={s.id} className="g-edge" style={{ '--i': i } as CSSProperties} d={`M${x1} ${y} C${x1 + GAP / 2} ${y} ${x2 - GAP / 2} ${y} ${x2} ${y}`} />
          })}
          {from >= 0 && to >= 0 && <path className="g-edge loop" d={`M${cx(from)} ${top + H} C${cx(from)} ${top + H + 52} ${cx(to)} ${top + H + 52} ${cx(to)} ${top + H}`} />}
        </svg>
        {from >= 0 && to >= 0 && flow.loop && <span className="g-loop-label" style={{ left: (cx(from) + cx(to)) / 2, top: top + H + 40 }}><Icon name="arrow-return-left" /> {flow.loop.label}</span>}
        {steps.map((s, i) => (
          <button key={s.id} type="button" className={`g-node${sel === s.id ? ' sel' : ''}`} style={{ left: 4 + i * (W + GAP), top, width: W, height: H, '--i': i, viewTransitionName: `g-node-${s.id}` } as CSSProperties} onClick={() => onSel(s.id)}>
            <span className="g-node-n">{i + 1}</span>
            <span className="g-node-icon"><Icon name={s.icon} /></span>
            <b>{s.title}</b>
            <small>{s.where}</small>
          </button>
        ))}
      </div>
    </div>
  )
}

const TRIGGERS = ['a change is ready for review', 'a PR opens', 'a PR merges into main', 'an agent ends its session']
const SCOPES = ['every change', 'one-way doors only', 'changes in src/billing']
const ACTORS = ['on the author’s computer', 'by an available agent', 'in CI, observed']

/** Choose → Configure → Preview → Activate, with the sentence as the editor and the graph as its picture. */
function AddWorkflow() {
  const st = useStore()
  const { go } = useProto()
  const [step, setStep] = useState(0)
  const [kit, setKit] = useState(2)
  const [trigger, setTrigger] = useState(TRIGGERS[0]!)
  const [scope, setScope] = useState(SCOPES[1]!)
  const [actor, setActor] = useState(ACTORS[0]!)
  const steps = ['Choose', 'Configure', 'Preview', 'Activate']
  const k = KITS.find((x) => x.n === kit) ?? KITS[1]!
  const next = () => morph(() => setStep((s) => Math.min(3, s + 1)))
  return (
    <>
      <PageHead question="How do I add a workflow?" answer={<>Start from what you already have.<br /><span className="g-quiet">Preview it before anything runs.</span></>} />
      <ol className="g-wizard-steps" style={{ '--p': step / 3 } as CSSProperties}>
        {steps.map((s, i) => <li key={s} className={i < step ? 'past' : i === step ? 'now' : ''}><button type="button" onClick={() => i < step && morph(() => setStep(i))}><span>{i < step ? <Icon name="check2" /> : i + 1}</span>{s}</button></li>)}
      </ol>
      <section key={step} className="g-wizard-panel">
        {step === 0 && <>
          <h2 className="g-h2">We read this repository first</h2>
          <ul className="g-detected">{DETECTED.map((d) => <li key={d.what}><span className="g-ring small tone-brand"><Icon name={d.icon} /></span><code>{d.what}</code><span>{d.found}</span></li>)}</ul>
          <h2 className="g-h2">Pick a starting point</h2>
          <div className="g-kits">
            {KITS.map((x) => (
              <button key={x.n} type="button" className={`g-kit${kit === x.n ? ' on' : ''}`} aria-pressed={kit === x.n} onClick={() => morph(() => setKit(x.n))}>
                <span className="g-kit-n">{x.n}</span>
                <span className="g-ring big tone-brand"><Icon name={x.icon} /></span>
                <b>{x.name}</b>
                <span>{x.does}</span>
                <small><Icon name="search" /> Found: {x.detected}</small>
                <small className="g-kit-gap"><Icon name="exclamation-circle" /> {x.unsupported}</small>
                {x.n === 2 && <span className="g-kit-best"><Icon name="stars" /> Closest to your repository</span>}
              </button>
            ))}
          </div>
          <div className="g-acts"><button type="button" className="btn btn-primary" onClick={next}>Use “{k.name}” <Icon name="arrow-right" /></button></div>
        </>}
        {step === 1 && <>
          <h2 className="g-h2">Say it in a sentence</h2>
          <p className="g-sentence-edit">
            When <select className="g-token" value={trigger} onChange={(e) => setTrigger(e.target.value)} aria-label="When">{TRIGGERS.map((t) => <option key={t}>{t}</option>)}</select>
            {' '}for <select className="g-token" value={scope} onChange={(e) => setScope(e.target.value)} aria-label="For">{SCOPES.map((t) => <option key={t}>{t}</option>)}</select>,
            {' '}run the repository’s checks <select className="g-token" value={actor} onChange={(e) => setActor(e.target.value)} aria-label="Where">{ACTORS.map((t) => <option key={t}>{t}</option>)}</select>,
            {' '}request an independent review, return findings to the author, then prepare the PR evidence.
          </p>
          <p className="small muted"><Icon name="info-circle" /> Every underlined phrase is a choice. The graph and the configuration file are drawn from this same sentence.</p>
          <div className="g-acts"><button type="button" className="btn btn-primary" onClick={next}>Preview it <Icon name="arrow-right" /></button></div>
        </>}
        {step === 2 && <>
          <h2 className="g-h2">On your last five changes</h2>
          <ul className="g-preview">
            {[['Queue retry budget', true, 'Every step would have passed'], ['Rate-limit the export API', true, 'Every step would have passed'], ['Split tax calculation', true, 'Passed after one review round'], ['Fix invoice rounding for JPY', false, 'Tests would be stale on 7be03f1'], ['Document webhook retries', false, 'No tests found to run']].map(([t, ok, why], i) => (
              <li key={String(t)} style={{ '--i': i } as CSSProperties}><span className={`g-ring small tone-${ok ? 'ok' : 'warn'}`}><Icon name={ok ? 'check2' : 'exclamation'} /></span><b>{t}</b><span className="small muted">{why}</span></li>
            ))}
          </ul>
          <p className="small"><b>Missing:</b> nothing required. <b>Not covered:</b> {k.unsupported}</p>
          <div className="g-acts"><button type="button" className="btn btn-primary" onClick={next}>Looks right <Icon name="arrow-right" /></button></div>
        </>}
        {step === 3 && <div className="g-activated">
          <span className="g-ring huge tone-ok g-done-ring"><Icon name="lightning-charge" /></span>
          <h2 className="g-h2">Ready to activate</h2>
          <p className="g-quiet-p">When {trigger} for {scope}, checks run {actor}. Agents read it at session start; nothing runs until a change reaches it.</p>
          <div className="g-acts center">
            <button type="button" className="btn btn-primary" onClick={() => { const target = FLOWS.find((f) => f.kit === kit) ?? FLOWS[1]!; go(`workflows/${target.id}`); st.setFlow(target.id, 'Active') }}><Icon name="lightning-charge" /> Activate on this repository</button>
            <button type="button" className="btn btn-outline-secondary" onClick={() => morph(() => setStep(0))}>Start over</button>
          </div>
        </div>}
      </section>
    </>
  )
}
