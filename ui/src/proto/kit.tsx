// Shared pieces for the three lifecycle prototypes. Every variation renders these same views over the same sample
// data; only the shell, the home page and the way you move between views differ.
import { createContext, useContext, useState, type CSSProperties, type MouseEvent, type ReactNode } from 'react'
import { flushSync } from 'react-dom'
import { Avatar, Box, Icon, Pill } from '../ui.tsx'
import {
  AREAS, CHANGED, CI_FIT, CONNECTIONS, DETECTED, FLOWS, KITS, LENSES, NEEDS, PROOF, STAGES, VALUE, WORK, work,
  type Flow, type FlowState, type Lens, type Need, type Proof, type Role, type Work,
} from './data.ts'

// ---------- navigation ----------

export type Variant = 'a' | 'b' | 'c' | 'd' | 'e' | 'f' | 'g'
export interface Route { v: Variant; parts: string[] }

const VARIANT_IDS: readonly string[] = ['a', 'b', 'c', 'd', 'e', 'f', 'g']
export const parseRoute = (hash: string): Route => {
  const [v = '', ...parts] = hash.replace(/^#/, '').split('/')
  // G, the combined flagship, is the default; every earlier option stays reachable by its letter.
  return { v: VARIANT_IDS.includes(v) ? (v as Variant) : 'g', parts }
}

/** Swap the view inside a view transition so shared surfaces (a card and the sheet it opens) morph instead of cutting. */
export function morph(apply: () => void): void {
  if (typeof document.startViewTransition !== 'function' || window.matchMedia?.('(prefers-reduced-motion: reduce)').matches) return apply()
  const t = document.startViewTransition(() => flushSync(apply))
  // A skipped transition (hidden tab, duplicate names) still applies the update; only the animation is lost.
  for (const p of [t.ready, t.finished, t.updateCallbackDone]) p.catch(() => undefined)
}

interface Ctx {
  route: Route
  go: (path: string) => void
  role: Role
  resolved: Set<string>
  resolve: (id: string) => void
}
export const ProtoCtx = createContext<Ctx | null>(null)
export const useProto = () => {
  const c = useContext(ProtoCtx)
  if (!c) throw new Error('ProtoCtx missing')
  return c
}

/** An in-prototype link: real href for open-in-new-tab, morphing navigation on click. `to` is relative to the variant. */
export function Go({ to, className, children, label, style }: { to: string; className?: string; children: ReactNode; label?: string; style?: CSSProperties }) {
  const { route, go } = useProto()
  const href = `#proto/${route.v}/${to}`
  return (
    <a href={href} className={className} aria-label={label} style={style} onClick={(e: MouseEvent) => {
      if (e.metaKey || e.ctrlKey) return
      e.preventDefault()
      go(to)
    }}>
      {children}
    </a>
  )
}

// ---------- atoms ----------

export const ProofPill = ({ proof }: { proof: Proof }) => {
  const p = PROOF[proof]
  return <Pill tone={p.tone} icon={p.icon} tip={p.tip}>{p.label}</Pill>
}

export const DoorPill = ({ door }: { door: Work['door'] }) => door === 'one-way'
  ? <Pill tone="warn" icon="door-closed" tip="Hard to undo: billing, auth, migrations. Needs full evidence and a person’s approval.">One-way door</Pill>
  : <Pill tone="muted" icon="door-open" tip="Easy to undo. Light evidence; merges on green checks.">Two-way door</Pill>

export function People({ w, size = 22 }: { w: Work; size?: number }) {
  return (
    <span className="px-people">
      {w.agents.map((a) => <span key={a.who + a.tool} className="px-person" title={`${a.who}’s ${a.tool}`}><Avatar name={`${a.who}-${a.tool}`} size={size} /></span>)}
      <span className="px-people-text">{w.owner}’s {w.agents[0]?.tool}{w.agents.length > 1 ? ` + ${w.agents.length - 1}` : ''}</span>
    </span>
  )
}

const stageOf = (w: Work) => STAGES.find((s) => s.id === w.stage)

/** A compact change row: outcome, owner and agents, actual stage, next step, freshness. */
/** The shared morph name for a change, left off while its detail is open so the name stays unique on the page. */
const useMorphName = (id: string): CSSProperties => {
  const { route } = useProto()
  return route.parts[0] === 'work' && route.parts[1] === id ? {} : { viewTransitionName: `px-${id}` }
}

export function WorkRow({ w, dense = false }: { w: Work; dense?: boolean }) {
  return (
    <Go to={`work/${w.id}`} className={`px-workrow${dense ? ' dense' : ''}`} style={useMorphName(w.id)}>
      <span className={`px-dot tone-${w.tone}`} aria-hidden="true" />
      <span className="px-workrow-main">
        <span className="px-workrow-title">{w.title}</span>
        <span className="px-workrow-sub">
          <span className={`px-state tone-${w.tone}`}>{w.state}</span>
          {!dense && <> · {stageOf(w)?.label}</>}
          {w.prs[0] && <> · #{w.prs[0].n}</>}
        </span>
      </span>
      {!dense && <People w={w} size={20} />}
      <span className="px-fresh">{w.fresh}</span>
    </Go>
  )
}

/** A decision a person can act on now. Resolving it once resolves it in Slack and in the agent too. */
export function NeedRow({ n, card = false, onOpen, picked = false }: { n: Need; card?: boolean; /** Show the change beside the list instead of opening it. */ onOpen?: () => void; picked?: boolean }) {
  const { resolved, resolve } = useProto()
  const done = resolved.has(n.id)
  const first = n.work[0] ? work(n.work[0]) : undefined
  return (
    <div className={`px-need${card ? ' boxed' : ''}${done ? ' is-done' : ''}${picked ? ' picked' : ''} tone-${n.tone}`}>
      <span className={`px-need-icon tone-${done ? 'ok' : n.tone}`}><Icon name={done ? 'check2' : n.icon} /></span>
      <div className="px-need-main">
        <div className="px-need-title">{n.title}</div>
        <div className="px-need-reason">{done ? 'Resolved here. Slack and the agent show the same answer.' : n.reason}</div>
        {!done && <div className="px-need-meta">
          <span><Icon name="clock" /> waiting {n.waiting}</span>
          {n.blocks && <span><Icon name="sign-stop" /> {n.blocks}</span>}
          {n.risk && <span className="tone-warn-text"><Icon name="door-closed" /> {n.risk}</span>}
          {first && (onOpen
            ? <button type="button" className="px-link" onClick={onOpen}>{picked ? 'Shown beside' : 'See its journey'}</button>
            : <Go to={`work/${first.id}`} className="px-link">Open the change</Go>)}
        </div>}
      </div>
      {!done && <button type="button" className="btn btn-sm btn-outline-secondary px-need-act" onClick={() => resolve(n.id)}>{n.action}</button>}
    </div>
  )
}

export function StageStrip({ w }: { w: Work }) {
  const at = STAGES.findIndex((s) => s.id === w.stage)
  return (
    <ol className="px-strip" aria-label="Where this change is">
      {STAGES.map((s, i) => (
        <li key={s.id} className={i < at ? 'past' : i === at ? `now tone-${w.tone}` : 'next'} aria-current={i === at ? 'step' : undefined}>
          <span className="px-strip-dot"><Icon name={i < at ? 'check2' : s.icon} /></span>
          <span className="px-strip-label">{s.label}</span>
        </li>
      ))}
    </ol>
  )
}

const OPEN: Proof[] = ['waiting', 'stale', 'failed', 'missing']

/** Readiness in words: what is current, what is missing, and why, beside the change. */
export function readiness(w: Work): string {
  const gaps = w.checks.filter((c) => OPEN.includes(c.proof))
  if (gaps.length === 0) return 'Every required step has current evidence on this commit.'
  return `Still open: ${gaps.map((c) => `${c.name.toLowerCase()} (${c.detail})`).join('; ')}.`
}

export function PrBody({ w }: { w: Work }) {
  const lines = w.checks.filter((c) => c.proof !== 'missing' && c.proof !== 'skipped').map((c) => `- ${c.name}: ${PROOF[c.proof].label.toLowerCase()}, ${c.detail}`).join('\n')
  return (
    <pre className="px-prbody">{`## Summary\n${w.purpose}\n\n## Evidence (on ${w.source})\n${lines}\n\n## Merge danger\nDoor: ${w.door} · Blast radius: ${w.blast}`}</pre>
  )
}

// ---------- Work ----------

export function WorkDetail({ id }: { id: string }) {
  const w = work(id)
  const { resolve, resolved } = useProto()
  if (!w) return <p className="muted">That change isn’t in the sample.</p>
  const actionId = `act-${w.id}`
  return (
    <article className="px-detail" style={{ viewTransitionName: `px-${w.id}` }}>
      <header className="px-detail-head">
        <div className="px-detail-pills">
          <Pill tone={w.tone} icon="record-circle">{w.state}</Pill>
          <DoorPill door={w.door} />
          {w.prs.map((p) => <Pill key={p.n} tone={p.tone} icon="git">PR #{p.n} · {p.state}</Pill>)}
          {w.prs.length === 0 && <Pill tone="muted" icon="git">No PR yet</Pill>}
        </div>
        <h2 className="px-detail-title">{w.title}</h2>
        <p className="px-detail-purpose">{w.purpose}</p>
        <div className="px-detail-facts">
          <People w={w} />
          <span><Icon name="hash" /> <code>{w.source}</code></span>
          <span><Icon name="clock" /> updated {w.fresh}</span>
          <span><Icon name="bullseye" /> {w.blast}</span>
        </div>
      </header>

      <div className="px-now">
        <div><span className="px-k">Why</span><p>{w.why}</p></div>
        <div className="px-now-next">
          <span className="px-k">Next</span>
          <p>{w.next}</p>
          {w.tone !== 'brand' && w.tone !== 'ok' && (resolved.has(actionId)
            ? <span className="px-ok-text"><Icon name="check2" /> Done here; the agent and Slack see it too.</span>
            : <button type="button" className="btn btn-sm btn-primary" onClick={() => resolve(actionId)}>{w.next}</button>)}
        </div>
      </div>

      <StageStrip w={w} />

      <div className="px-detail-grid">
        <div className="px-col">
          <Box icon="patch-check" title="Is it ready?" explain="Each step keeps how it is known: an agent report, a command result, what GitHub observed, or a person’s approval. They are never merged into one green tick.">
            <p className="px-ready">{readiness(w)}</p>
            <ul className="px-checks">
              {w.checks.map((c) => (
                <li key={c.name}><span className="px-check-name">{c.name}</span><ProofPill proof={c.proof} /><span className="px-check-detail">{c.detail}</span></li>
              ))}
            </ul>
          </Box>
          {w.prs.length > 0 && <Box icon="file-earmark-text" title="PR description, written from the record" explain="The agent writes the summary. Evidence and merge danger come from the steps above, so the PR never claims more than the record holds.">
            <PrBody w={w} />
          </Box>}
          {w.prs.length === 0 && <Box icon="info-circle" title="Before a PR exists" tone="muted">
            <p className="small mb-0">This change has been tracked since an agent claimed it: who holds which files, what was decided, and what was checked. A PR is linked when one opens; the change doesn’t start there.</p>
          </Box>}
        </div>
        <div className="px-col">
          <Box icon="list-check" title="Requirements" count={w.requirements.length}>
            <ul className="px-list">{w.requirements.map((r) => <li key={r}>{r}</li>)}</ul>
          </Box>
          <Box icon="diagram-3" title="Runs and links" explain="A change can have several workflow runs (retries, follow-ups) and several PRs. Each stays its own record.">
            <ul className="px-list">
              {w.runs.map((r) => <li key={`${r.workflow}-${r.n}`}><Icon name="play-circle" /> {r.workflow}, run {r.n}: <span className={`tone-${r.tone}-text`}>{r.state}</span></li>)}
              {w.runs.length === 0 && <li className="muted">No workflow run yet</li>}
              {w.followUps?.map((f) => <li key={f}><Icon name="send" /> Follow-up: {f}</li>)}
              <li><Icon name="files" /> {w.paths.map((p) => <code key={p} className="me-1">{p}</code>)}</li>
            </ul>
          </Box>
          <Box icon="clock-history" title="Activity">
            <ul className="px-activity">{w.activity.map((a) => <li key={a.at + a.text}><span className="px-fresh">{a.at}</span>{a.text}</li>)}</ul>
          </Box>
        </div>
      </div>
    </article>
  )
}

export function WorkBoard({ compact = false }: { compact?: boolean }) {
  return (
    <div className={`px-board${compact ? ' compact' : ''}`}>
      {STAGES.map((s) => {
        const items = WORK.filter((w) => w.stage === s.id)
        return (
          <section key={s.id} className="px-col-stage" aria-label={s.label}>
            <header><Icon name={s.icon} /> {s.label} <span className="px-count">{items.length}</span></header>
            {items.map((w) => <BoardCard key={w.id} w={w} />)}
            {items.length === 0 && <p className="px-empty">Nothing here</p>}
          </section>
        )
      })}
    </div>
  )
}

function BoardCard({ w }: { w: Work }) {
  return (
    <Go to={`work/${w.id}`} className={`px-card tone-${w.tone}`} style={useMorphName(w.id)}>
      <span className="px-card-title">{w.title}</span>
      <span className={`px-state tone-${w.tone}`}>{w.state}</span>
      <span className="px-card-foot"><People w={w} size={18} />{w.prs[0] && <span className="px-pr">#{w.prs[0].n}</span>}</span>
    </Go>
  )
}

/** Inbox owns every decision thread, for every role; the role tag says who it is for. */
export function InboxView() {
  return (
    <div className="px-needs">
      {NEEDS.map((n) => (
        <div key={n.id} className="px-thread">
          <span className="px-roles">{n.roles.map((r) => <Pill key={r} tone="muted">{r}</Pill>)}</span>
          <NeedRow n={n} card />
        </div>
      ))}
    </div>
  )
}

// ---------- graphs ----------

export interface GNode { id: string; title: string; sub: string; icon: string; badge?: string; tone?: string }

/** An n8n-style row of nodes with a loop-back edge. Another view of the same definition, never a separate engine. */
export function Graph({ nodes, loop, selected, onNode, big = false }: { nodes: GNode[]; loop?: { from: string; to: string; label: string }; selected?: string; onNode?: (id: string) => void; big?: boolean }) {
  const W = big ? 168 : 156
  const H = big ? 92 : 78
  const GAP = big ? 44 : 40
  const top = 18
  const width = nodes.length * W + (nodes.length - 1) * GAP + 8
  const ix = (id: string) => nodes.findIndex((n) => n.id === id)
  const from = loop ? ix(loop.from) : -1
  const to = loop ? ix(loop.to) : -1
  const height = top + H + (loop && from >= 0 && to >= 0 ? 64 : 24)
  const cx = (i: number) => 4 + i * (W + GAP) + W / 2
  return (
    <div className="px-graph-wrap">
      <div className="px-graph" style={{ width, height }}>
        <svg width={width} height={height} aria-hidden="true">
          {nodes.slice(1).map((_, i) => {
            const x1 = 4 + i * (W + GAP) + W, x2 = x1 + GAP, y = top + H / 2
            return <path key={i} d={`M${x1} ${y} C${x1 + GAP / 2} ${y} ${x2 - GAP / 2} ${y} ${x2} ${y}`} className="px-edge" />
          })}
          {loop && from >= 0 && to >= 0 && <path d={`M${cx(from)} ${top + H} C${cx(from)} ${top + H + 46} ${cx(to)} ${top + H + 46} ${cx(to)} ${top + H}`} className="px-edge loop" markerEnd="url(#px-arrow)" />}
          <defs><marker id="px-arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M2 1L8 5L2 9" fill="none" stroke="context-stroke" strokeWidth="1.5" /></marker></defs>
        </svg>
        {loop && from >= 0 && to >= 0 && <span className="px-loop-label" style={{ left: (cx(from) + cx(to)) / 2, top: top + H + 34 }}><Icon name="arrow-return-left" /> {loop.label}</span>}
        {nodes.map((n, i) => (
          <button key={n.id} type="button" className={`px-node${selected === n.id ? ' sel' : ''}${n.tone ? ` tone-${n.tone}` : ''}`} style={{ left: 4 + i * (W + GAP), top, width: W, height: H }} onClick={() => onNode?.(n.id)}>
            <span className="px-port in" aria-hidden="true" /><span className="px-port out" aria-hidden="true" />
            <span className="px-node-icon"><Icon name={n.icon} /></span>
            <span className="px-node-text"><span className="px-node-title">{n.title}</span><span className="px-node-sub">{n.sub}</span></span>
            {n.badge && <span className={`px-node-badge tone-${n.tone ?? 'brand'}`}>{n.badge}</span>}
          </button>
        ))}
      </div>
    </div>
  )
}

// ---------- Workflows ----------

const FLOW_STATES: FlowState[] = ['Draft', 'Previewed', 'Ready to activate', 'Active']

export function WorkflowsView({ flowId }: { flowId?: string }) {
  const [states, setStates] = useState<Record<string, FlowState>>(() => Object.fromEntries(FLOWS.map((f) => [f.id, f.state])))
  const [view, setView] = useState<'list' | 'graph'>('list')
  const [step, setStep] = useState<string | undefined>()
  const [url, setUrl] = useState('github.com/cloudflare/security-audit-skill')
  const [imported, setImported] = useState<string | null>(null)
  const [err, setErr] = useState('')
  const flow = FLOWS.find((f) => f.id === flowId)
  if (flow) return <FlowEditor flow={flow} state={states[flow.id] ?? flow.state} onState={(s) => setStates({ ...states, [flow.id]: s })} view={view} onView={setView} step={step} onStep={setStep} />
  return (
    <div className="px-stack">
      <Box icon="search" title="We read this repository first" intro="Nothing was changed. These are suggestions you review before anything is active." count={DETECTED.length}>
        <ul className="px-detected">{DETECTED.map((d) => <li key={d.what}><Icon name={d.icon} /><code>{d.what}</code><span>{d.found}</span></li>)}</ul>
        <p className="px-closest"><Icon name="stars" /> Closest starter: <strong>Get changes ready for review</strong>. It reuses your test and lint scripts, your review skill and your PR template.</p>
      </Box>
      <div className="px-kits">
        {KITS.map((k) => {
          const f = FLOWS.find((x) => x.kit === k.n)
          return (
            <section key={k.n} className="px-kit">
              <div className="px-kit-n">{k.n}</div>
              <h3><Icon name={k.icon} /> {k.name}</h3>
              <p>{k.does}</p>
              <dl>
                <dt>Found</dt><dd>{k.detected}</dd>
                <dt>Not covered</dt><dd>{k.unsupported}</dd>
              </dl>
              {f && <Go to={`workflows/${f.id}`} className="btn btn-sm btn-outline-secondary">{f.name} · {states[f.id]}</Go>}
            </section>
          )
        })}
      </div>
      <Box icon="collection" title="Also in this repository" count={FLOWS.filter((f) => !f.kit).length}>
        <ul className="px-list">{FLOWS.filter((f) => !f.kit).map((f) => <li key={f.id}><Icon name="diagram-3" /> <Go to={`workflows/${f.id}`} className="px-link">{f.name}</Go> <span className="small muted">{states[f.id]}</span></li>)}</ul>
      </Box>
      <Box icon="cloud-arrow-down" title="Add a skill from anywhere" explain="Imported into this computer’s workflow library, pinned to a commit. Making an imported skill a required PR gate is not supported yet; that resolver only knows first-party skills.">
        <div className="px-add">
          <input className="form-control" value={url} aria-label="Skill repository URL" onChange={(e) => { setUrl(e.target.value); setErr('') }} />
          <button type="button" className="btn btn-outline-secondary" onClick={() => {
            if (!/^[\w.-]+\.[a-z]{2,}\/[\w.-]+\/[\w.-]+/i.test(url.trim().replace(/^https?:\/\//, ''))) return setErr('Paste a repository URL, like github.com/owner/skill')
            setImported(url.trim().replace(/^https?:\/\//, ''))
          }}>Import and pin</button>
        </div>
        {err && <p className="px-err">{err}</p>}
        {imported && <p className="px-ok-text mt-2"><Icon name="pin-angle" /> {imported} pinned at a1b2c3d in the library. Next: what shows it ran? Suggested from its files: <code>findings.json</code> and the coverage ledger validate.</p>}
      </Box>
      <Box icon="cpu" title="Where your CI jobs could run" explain="From ci.yml. A local result only replaces CI work when the CI path is configured to accept it and records that it did. Unknown stays unknown." count={CI_FIT.length}>
        <ul className="px-fit">{CI_FIT.map((c) => <li key={c.job}><code>{c.job}</code><Pill tone={c.tone}>{c.fit}</Pill></li>)}</ul>
      </Box>
    </div>
  )
}

function FlowEditor({ flow, state, onState, view, onView, step, onStep }: { flow: Flow; state: FlowState; onState: (s: FlowState) => void; view: 'list' | 'graph'; onView: (v: 'list' | 'graph') => void; step?: string; onStep: (s?: string) => void }) {
  const at = FLOW_STATES.indexOf(state)
  const nextState = FLOW_STATES[at + 1]
  const sel = flow.steps.find((s) => s.id === step)
  return (
    <div className="px-stack">
      <div className="px-flowhead">
        <Go to="workflows" className="px-link"><Icon name="arrow-left" /> All workflows</Go>
        <h2>{flow.name}</h2>
        <blockquote className="px-sentence">{flow.sentence}</blockquote>
        <ol className="px-states">
          {FLOW_STATES.map((s, i) => <li key={s} className={i < at ? 'past' : i === at ? 'now' : ''}><Icon name={i <= at ? 'check2-circle' : 'circle'} /> {s}</li>)}
        </ol>
        <div className="px-flow-actions">
          {nextState && <button type="button" className="btn btn-primary btn-sm" onClick={() => onState(nextState)}>{nextState === 'Previewed' ? 'Preview on recent changes' : nextState === 'Ready to activate' ? 'Mark ready' : 'Activate'}</button>}
          {state === 'Previewed' && <span className="small muted">Preview: 3 of the last 5 changes would have passed; 2 had no review. Missing: nothing.</span>}
          {state === 'Active' && <span className="px-ok-text small"><Icon name="check2-circle" /> Active. Agents read it at session start; the Stop hook warns before they finish.</span>}
          <span className="px-seg" role="group" aria-label="View">
            <button type="button" className={view === 'list' ? 'on' : ''} onClick={() => onView('list')}><Icon name="list-ul" /> Steps</button>
            <button type="button" className={view === 'graph' ? 'on' : ''} onClick={() => onView('graph')}><Icon name="diagram-3" /> Graph</button>
          </span>
        </div>
      </div>
      {view === 'graph'
        ? <>
          <Graph nodes={flow.steps.map((s) => ({ id: s.id, title: s.title, sub: s.where, icon: s.icon }))} loop={flow.loop} selected={step} onNode={(id) => onStep(id === step ? undefined : id)} big />
          {sel ? <StepCard s={sel} /> : <p className="small muted">Select a step to see what it does, who runs it, what counts as done and what happens next.</p>}
        </>
        : <ol className="px-steps">{flow.steps.map((s, i) => <li key={s.id}><span className="px-step-n">{i + 1}</span><StepCard s={s} /></li>)}</ol>}
    </div>
  )
}

function StepCard({ s }: { s: Flow['steps'][number] }) {
  return (
    <div className="px-stepcard">
      <h3><Icon name={s.icon} /> {s.title}</h3>
      <dl>
        <dt>Do what</dt><dd>{s.doWhat}</dd>
        <dt>Who or where</dt><dd><Pill tone={s.where === 'External CI observed' ? 'muted' : 'brand'} icon={s.where === 'Local command' ? 'laptop' : s.where === 'Existing agent' ? 'robot' : 'cloud'}>{s.where}</Pill></dd>
        <dt>Done when</dt><dd>{s.done}</dd>
        <dt>Then</dt><dd>{s.next}</dd>
      </dl>
    </div>
  )
}

// ---------- Repository ----------

export function RepositoryView() {
  const [lens, setLens] = useState<Lens>('work')
  const [area, setArea] = useState('src/billing')
  const a = AREAS.find((x) => x.path === area)
  const meta = LENSES.find((l) => l.id === lens)
  return (
    <div className="px-stack">
      <div className="px-lenses" role="tablist" aria-label="Lens">
        {LENSES.map((l) => <button key={l.id} type="button" role="tab" aria-selected={lens === l.id} className={lens === l.id ? 'on' : ''} onClick={() => setLens(l.id)}><Icon name={l.icon} /> {l.label}</button>)}
      </div>
      {meta && <p className="px-caution"><Icon name="info-circle" /> {meta.caution}</p>}
      <div className="px-repo">
        <ul className="px-areas">
          {AREAS.map((x) => (
            <li key={x.path}>
              <button type="button" className={area === x.path ? 'on' : ''} onClick={() => setArea(x.path)}>
                <code>{x.path}</code>
                <span className="px-facts">{x.facts[lens].map((f) => <Pill key={f.text} tone={f.tone}>{f.text}</Pill>)}</span>
              </button>
            </li>
          ))}
        </ul>
        {a && <div className="px-areamap">
          <h3><code>{a.path}</code></h3>
          <p className="small muted">A bounded map around this area: its changes, their PRs and what it uses. Dashed links are inferred from imports.</p>
          <AreaMap path={a.path} />
          <dl className="px-allfacts">
            {LENSES.map((l) => <div key={l.id}><dt><Icon name={l.icon} /> {l.label}</dt><dd>{a.facts[l.id].map((f) => <Pill key={f.text} tone={f.tone}>{f.text}</Pill>)}</dd></div>)}
          </dl>
        </div>}
      </div>
      <p className="small muted"><Icon name="box" /> Native revisions and snapshots appear here as a <strong>Revisions</strong> tab when native source control is turned on in Setup.</p>
    </div>
  )
}

function AreaMap({ path }: { path: string }) {
  const a = AREAS.find((x) => x.path === path)
  if (!a) return null
  const ws = a.work.map(work).filter((x): x is Work => !!x)
  const W = 520, H = 220, cx = 260, cy = 110
  const left = ws.map((w, i) => ({ w, y: ws.length === 1 ? cy : 50 + i * (120 / Math.max(1, ws.length - 1)) }))
  const right = a.uses.map((u, i) => ({ u, y: a.uses.length === 1 ? cy : 60 + i * (100 / Math.max(1, a.uses.length - 1)) }))
  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="px-areasvg" role="img" aria-label={`Map around ${path}`}>
      {left.map(({ w, y }) => <path key={w.id} d={`M150 ${y} C200 ${y} 200 ${cy} 220 ${cy}`} className="px-edge" />)}
      {right.map(({ u, y }) => <path key={u} d={`M300 ${cy} C330 ${cy} 330 ${y} 360 ${y}`} className="px-edge loop" />)}
      <rect x="220" y={cy - 20} width="80" height="40" rx="10" className="px-svg-center" />
      <text x={cx} y={cy + 4} textAnchor="middle" className="px-svg-text strong">{path.split('/').pop()}</text>
      {left.map(({ w, y }) => (
        <g key={w.id}>
          <rect x="6" y={y - 18} width="144" height="36" rx="9" className={`px-svg-node tone-${w.tone}`} />
          <text x="16" y={y - 2} className="px-svg-text">{w.title.length > 22 ? `${w.title.slice(0, 21)}…` : w.title}</text>
          <text x="16" y={y + 11} className="px-svg-sub">{w.prs[0] ? `PR #${w.prs[0].n} · ` : ''}{w.state}</text>
        </g>
      ))}
      {right.map(({ u, y }) => (
        <g key={u}>
          <rect x="360" y={y - 14} width="150" height="28" rx="9" className="px-svg-node dashed" />
          <text x="372" y={y + 4} className="px-svg-text">uses {u}</text>
        </g>
      ))}
      {left.length === 0 && <text x="20" y={cy + 4} className="px-svg-sub">No declared work here</text>}
    </svg>
  )
}

// ---------- Setup, value, what changed ----------

export function SetupView() {
  return (
    <div className="px-stack">
      <Box icon="plug" title="Connections" explain="Optional connections are labelled optional. A working repository never looks unfinished because Slack isn’t connected.">
        <ul className="px-conns">
          {CONNECTIONS.map((c) => (
            <li key={c.name}>
              <span className={`px-dot tone-${c.state}`} aria-hidden="true" />
              <span className="px-conn-name">{c.name}</span>
              <span className="px-conn-kind">{c.kind}</span>
              <span className="px-conn-note">{c.note}</span>
              {c.state === 'warn' && <button type="button" className="btn btn-sm btn-outline-secondary">Copy the fix</button>}
              {c.kind === 'Optional' && <button type="button" className="btn btn-sm btn-outline-secondary">Connect Slack</button>}
              {c.kind === 'Advanced' && <button type="button" className="btn btn-sm btn-outline-secondary">Learn more</button>}
            </li>
          ))}
        </ul>
      </Box>
      <Box icon="people" title="Members and access" tone="muted"><p className="small mb-0">maria (admin), dev, sam. Agents act with their person’s access; a person answering for their agent is recorded as that person.</p></Box>
    </div>
  )
}

export function SetupSummary() {
  const warn = CONNECTIONS.filter((c) => c.state === 'warn')
  return (
    <p className="px-setup-line">
      <span className="px-dot tone-ok" aria-hidden="true" /> {CONNECTIONS.filter((c) => c.state === 'ok').length} connected
      {warn.map((c) => <span key={c.name}> · <span className="px-dot tone-warn" aria-hidden="true" /> {c.name}: {c.note.toLowerCase()}</span>)}
      <Go to="setup" className="px-link ms-2">Setup</Go>
    </p>
  )
}

export function ValueStrip() {
  return (
    <div className="px-value">
      <div className="px-value-head"><Icon name="stars" /> This week, AgentSCM helped</div>
      <ul>{VALUE.stories.slice(0, 2).map((s) => <li key={s.text}><Icon name={s.icon} /> <Go to={`work/${s.work}`} className="px-link-quiet">{s.text}</Go></li>)}</ul>
      <Go to="value" className="px-link">See all contributions and their records</Go>
    </div>
  )
}

export function ValueView() {
  return (
    <div className="px-stack">
      <Box icon="stars" title="What AgentSCM contributed this week" intro="Each line links to the change and the event behind it." count={VALUE.stories.length}>
        <ul className="px-stories">{VALUE.stories.map((s) => <li key={s.text}><Icon name={s.icon} /><Go to={`work/${s.work}`} className="px-link-quiet">{s.text}</Go></li>)}</ul>
      </Box>
      <Box icon="bar-chart" title="Measured, with coverage" explain="Counted once across agents, retries and surfaces. Unknown usage stays unknown, never zero. No savings are claimed without a comparable baseline.">
        <div className="px-counts">{VALUE.counts.map((c) => <div key={c.label} className="px-countcard"><span className="px-countn">{c.n}</span><span>{c.label}</span><span className="small muted">{c.sub}</span></div>)}</div>
      </Box>
    </div>
  )
}

export function ChangedList() {
  return (
    <ul className="px-changed">
      {CHANGED.map((c) => <li key={c.text}><span className={`px-need-icon small tone-${c.tone}`}><Icon name={c.icon} /></span><span><Go to={`work/${c.work}`} className="px-link-quiet">{c.text}</Go><span className="small muted d-block">{c.sub}</span></span></li>)}
    </ul>
  )
}
