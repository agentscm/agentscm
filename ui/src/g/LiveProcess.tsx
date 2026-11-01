// G · Workflows over live data: the adopted skill's gates as a graph on the frame's violet, what each gate asks in
// plain words, which gates can stop an agent, and the runs moving through them right now.
import { useState, type CSSProperties } from 'react'
import { Ago, Icon, Notice, PageHead, Pill } from '../ui.tsx'
import { usePoll } from '../usePoll.ts'
import { fetchLibrary, runStateLabel, runStateTone, type CatalogEntry } from '../skills.ts'
import type { LiveProps } from './GLive.tsx'
import { useRuns } from './LiveNow.tsx'

const titleOf = (id: string) => (id === 'ci' ? 'CI' : id.replace(/_/g, ' ').replace(/^./, (c) => c.toUpperCase()))
const GATE_ICON: Record<string, string> = { branch: 'git', tests: 'terminal', review: 'eye', fix: 'wrench', ci: 'cloud-check', approval: 'person-check', merge: 'sign-merge-right', post_merge: 'check2-all' }

export function LiveProcess({ live }: { live: LiveProps }) {
  const lib = usePoll((signal) => fetchLibrary(live.repo, live.token, signal), 60_000, `${live.repo}:library`)
  const runs = useRuns(live)
  const v = lib.kind === 'ready' ? lib.value : null
  const adopted = v ? v.adoption.pins.filter((p) => p.status === 'adopted').map((p) => v.catalog.find((c) => c.name === p.name && c.sha256 === p.sha256) ?? v.catalog.find((c) => c.name === p.name)).filter((c): c is CatalogEntry => !!c) : []
  const available = v ? v.catalog.filter((c) => !adopted.some((a) => a.name === c.name)) : []
  const [pick, setPick] = useState<string | null>(null)
  const skill = adopted.find((a) => a.name === pick) ?? adopted[0] ?? null
  const [gate, setGate] = useState<string | null>(null)
  const sel = skill?.gates.find((g) => g.id === gate) ?? skill?.gates[0] ?? null
  const selIndex = skill && sel ? skill.gates.indexOf(sel) : -1
  const counts = skill && v ? v.counts[skill.name] : undefined
  const mine = runs.filter((r) => !skill || r.skill.name === skill.name)
  return (
    <>
      <PageHead question="What process applies here?" answer={<>Your process, made repeatable.<br /><span className="g-quiet">Keep the tools you already use.</span></>} fact={v ? `Adoption is read from ${v.adoption.source === 'default_branch' ? `the default branch${v.adoption.default_branch ? ` (${v.adoption.default_branch})` : ''}` : v.adoption.source === 'reported_by_checkout' ? 'what a checkout reported' : 'nowhere yet'}. Agents read it at session start; the Hub runs nothing.` : undefined} />
      {lib.kind === 'error' && <Notice tone="bad" title="Could not load skills" action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={lib.retry}>Retry</button>}>{lib.message}</Notice>}
      <div className="g-flows">
        <aside className="g-flowlist" aria-label="Skills">
          <span className="g-k">Adopted here</span>
          {adopted.map((a) => (
            <button key={a.name} type="button" className={`g-flowitem${skill?.name === a.name ? ' on' : ''}`} aria-pressed={skill?.name === a.name} onClick={() => { setPick(a.name); setGate(null) }}>
              <span><b>{a.name}</b><small>v{a.workflow_version} · {a.gates.length} steps</small></span>
              <span className="g-state-chip s-active">Adopted</span>
            </button>
          ))}
          {v && adopted.length === 0 && <p className="small muted">Nothing adopted yet.</p>}
          {available.length > 0 && <span className="g-k mt-3">Available</span>}
          {available.map((a) => <a key={a.name} href="#skills" className="g-flowitem g-flowitem-avail"><span><b>{a.name}</b><small>{a.summary}</small></span><span className="g-state-chip">Not adopted</span></a>)}
          <a href="#templates" className="g-flowitem add"><Icon name="journal-code" /> Workflow library</a>
          <a href="#workflows" className="g-flowitem add"><Icon name="play-circle" /> Recorded runs</a>
        </aside>
        {skill ? (
          <section className="g-canvas" aria-label={`${skill.name} as a graph`}>
            <div className="g-canvas-head"><span><Icon name="diagram-3" /> {skill.name} · v{skill.workflow_version}</span><span className="g-state-chip s-active">Adopted</span></div>
            <p className="g-canvas-sentence">{skill.summary}</p>
            <GateGraph gates={skill.gates} sel={sel?.id ?? ''} onSel={setGate} />
            <div className="g-activate">
              <ul className="g-states" aria-label="Runs">
                <li className="now"><span><Icon name="play-circle" /></span>{counts?.open ?? 0} open</li>
                <li className="past"><span><Icon name="shield-check" /></span>{counts?.at_stop_gate ?? 0} at a gate that can stop an agent</li>
                <li className="past"><span><Icon name="check2" /></span>{counts?.completed ?? 0} completed</li>
              </ul>
              <a href="#skills" className="btn btn-light btn-sm">Adoption and evidence</a>
            </div>
          </section>
        ) : (
          <section className="g-canvas g-canvas-empty">
            <span className="g-ring huge tone-brand"><Icon name="diagram-3" /></span>
            <h2>No process adopted yet</h2>
            <p>Adopt a skill such as <code>pr-gate</code> by pinning it in <code>agentscm-skills.lock.json</code> on the default branch. Agents then read its steps at session start and record evidence per step.</p>
            <a href="#skills" className="btn btn-light">See available skills</a>
          </section>
        )}
        {skill && sel && <aside className="g-inspector" aria-label="Step">
          <div key={sel.id} className="g-inspector-in">
            <span className="g-ring big tone-brand"><Icon name={GATE_ICON[sel.id] ?? 'circle'} /></span>
            <h2>{titleOf(sel.id)}</h2>
            <dl>
              <dt><Icon name="card-text" /> What it asks</dt><dd>{(skill.walkthrough[selIndex] ?? '').replace(/^\d+\.\s*[^:]+:\s*/, '') || 'The skill does not describe this step.'}</dd>
              <dt><Icon name="shield-check" /> Can it stop an agent?</dt><dd>{sel.enforce.includes('stop') ? <Pill tone="warn" icon="sign-stop">Yes, with the opt-in Stop hook</Pill> : <Pill tone="muted">No; it is recorded only</Pill>}</dd>
              <dt><Icon name="patch-check" /> How it is known</dt><dd>Agents record evidence for it; the Hub labels self-reported evidence as such.</dd>
            </dl>
          </div>
        </aside>}
      </div>
      <section className="g-runs" aria-label="Runs">
        <h2 className="g-h2">Changes moving through it</h2>
        {mine.length === 0 && <p className="small muted">No runs yet. A run starts when an agent records evidence on a branch.</p>}
        <ul className="g-links">{mine.slice(0, 8).map((r, i) => <li key={r.run_id} style={{ '--i': i } as CSSProperties}><Icon name={GATE_ICON[r.current_gate ?? ''] ?? 'play-circle'} /><span><a href={`#skills/runs/${r.run_id}`} className="px-link-quiet"><b>{r.branch}</b></a><small>{r.pull_request ? `PR #${r.pull_request} · ` : ''}{r.current_gate ? `at ${titleOf(r.current_gate)}` : 'no current step'} · <Ago at={r.updated_at} /></small></span><Pill tone={runStateTone(r)}>{runStateLabel(r)}</Pill></li>)}</ul>
      </section>
    </>
  )
}

function GateGraph({ gates, sel, onSel }: { gates: { id: string; enforce: string[] }[]; sel: string; onSel: (id: string) => void }) {
  const W = 150, H = 96, GAP = 34, top = 18
  const width = gates.length * W + (gates.length - 1) * GAP + 8
  const height = top + H + 26
  return (
    <div className="g-graph-wrap">
      <div className="g-graph" style={{ width, height }}>
        <svg width={width} height={height} aria-hidden="true">
          {gates.slice(1).map((g, i) => {
            const x1 = 4 + i * (W + GAP) + W, x2 = x1 + GAP, y = top + H / 2
            return <path key={g.id} className="g-edge" style={{ '--i': i } as CSSProperties} d={`M${x1} ${y} C${x1 + GAP / 2} ${y} ${x2 - GAP / 2} ${y} ${x2} ${y}`} />
          })}
        </svg>
        {gates.map((g, i) => (
          <button key={g.id} type="button" aria-pressed={sel === g.id} className={`g-node${sel === g.id ? ' sel' : ''}`} style={{ left: 4 + i * (W + GAP), top, width: W, height: H, '--i': i } as CSSProperties} onClick={() => onSel(g.id)}>
            <span className="g-node-n">{i + 1}</span>
            <span className="g-node-icon"><Icon name={GATE_ICON[g.id] ?? 'circle'} /></span>
            <b>{titleOf(g.id)}</b>
            <small>{g.enforce.includes('stop') ? 'Can stop an agent' : 'Recorded'}</small>
          </button>
        ))}
      </div>
    </div>
  )
}
