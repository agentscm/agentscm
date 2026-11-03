import { useEffect, useState, type ReactNode } from 'react'
import { SectionTabs } from '../TopNav.tsx'
import { Track } from './Landed.tsx'
import { lead, WorkLink } from './workParts.tsx'
import { Gloss } from '../Gloss.tsx'
import { agentName, who } from '../nextStep.ts'
import { SEARCH_THIS_PAGE_ONLY } from '../copy.ts'
import { ProofBadge } from '../Proof.tsx'
import { runProof } from '../proof.ts'
import type { WorkItem } from '../standup.ts'
import { useTaskIndex } from '../taskIndex.ts'
import {
  GATE_STATE_VIEW, addCommand, adoptedSummary, adoptionLabel, assuranceView, enforcement, fetchLibrary, fetchRun, fetchRuns, gateLabel, gateSteps, lastEvidence, paramText, parseStep, removeCommand,
  runMatches, runStateLabel, stepText, runStateTone, selfReportedCount, shortSha, skillRunHref, stepCounts, skillRunsHref, skillsHref, skillsRoute, type CatalogEntry, type Evidence, type Library, type Pin, type RunView, type StepEnforce,
} from '../skills.ts'
import { Markdown } from '../Markdown.tsx'
import { Ago, Box, CopyButton, Empty, Explain, Icon, Notice, PageHead, Pill, Skeleton, type Tone } from '../ui.tsx'
import { usePoll, type Poll } from '../usePoll.ts'

function Loaded<T>({ poll, what, children }: { poll: Poll<T>; what: string; children: (v: T) => ReactNode }) {
  if (poll.kind === 'loading') return <Skeleton rows={4} />
  if (poll.kind === 'error') return <Notice tone="bad" title={`Could not load ${what}`} action={<button type="button" className="btn btn-sm btn-outline-secondary" onClick={poll.retry}>Try again</button>}>{poll.message}</Notice>
  return <>{children(poll.value)}</>
}

const Cmd = ({ command, label }: { command: string; label: string }) => (
  <div className="cmd" data-tour="skills-command">
    <code aria-label={label}>{command}</code>
    <CopyButton text={command} />
  </div>
)

export function SkillsPage({ repo, token }: { repo: string | null; token: string }) {
  const [route, setRoute] = useState(() => skillsRoute())
  useEffect(() => {
    const on = () => setRoute(skillsRoute())
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  if (!repo) return <Skeleton rows={4} />
  if (route.kind === 'run') return <RunDetail key={route.runId} repo={repo} runId={route.runId} token={token} />
  if (route.kind === 'runs') return <RunsList repo={repo} token={token} />
  return <LibraryView repo={repo} token={token} />
}

function Applicability({ c }: { c: CatalogEntry }) {
  const rows: [string, string, string[]][] = [['When to use', 'check2', c.applicability.triggers], ['Prerequisites', 'box-seam', c.applicability.prerequisites], ['Not for', 'slash-circle', c.applicability.not_for]]
  return (
    <dl className="skill-facts">
      {rows.map(([label, icon, items]) => (
        <div key={label}>
          <dt><Icon name={icon} /> {label}</dt>
          <dd>{items.length ? <ul>{items.map((t) => <li key={t}>{t}</li>)}</ul> : <span className="muted">None stated</span>}</dd>
        </div>
      ))}
    </dl>
  )
}

// How a step is enforced is a fixed attribute of the step, so every level is the outlined tag (DESIGN.md, the Pill Family Rule).
const ENFORCE_VIEW: Record<StepEnforce, { label: string; tone: Tone; icon: string }> = {
  block: { label: 'Can stop the agent', tone: 'muted', icon: 'sign-stop' },
  warn: { label: 'Warns only', tone: 'muted', icon: 'bell' },
  record: { label: 'Recorded only', tone: 'muted', icon: 'journal-text' },
}
const whoIcon = (who: string) => (/^maintainer/i.test(who) ? 'person' : /session/i.test(who) ? 'people' : 'robot')

// Every hub line ends its needs with the lowest bar; said once under the track instead of on every step.
const LOWEST_BAR = ' (self-reported or stronger)'

function Step({ line }: { line: string }) {
  const s = parseStep(line)
  if (!s) return <li className="gate-step"><p className="gate-plain">{stepText(line)}</p></li>
  const e = ENFORCE_VIEW[s.enforce]
  return (
    <li className={`gate-step is-${s.enforce}`}>
      <div className="gate-step-head">
        <strong>{s.name}</strong>
        <span className="gate-who"><Icon name={whoIcon(s.who)} /> {s.who}</span>
        <Pill tone={e.tone} icon={e.icon} tip={s.detail.toLowerCase() === e.label.toLowerCase() ? undefined : s.detail}>{e.label}</Pill>
      </div>
      <p className="gate-plain">{s.plain}</p>
      <p className="gate-needs">{`Needs: ${s.needs.endsWith(LOWEST_BAR) ? s.needs.slice(0, -LOWEST_BAR.length) : s.needs}`}</p>
    </li>
  )
}

function HowItWorks({ c, pin, heading = true }: { c: CatalogEntry; pin: Pin | null; heading?: boolean }) {
  const lines = pin && pin.walkthrough.length ? pin.walkthrough : c.walkthrough
  if (lines.length === 0) return null
  return (
    <div className="skill-walkthrough" data-tour="skills-how">
      {heading && <h3 className="skill-sub">How it works</h3>}
      <ol className="gate-track">{lines.map((l, i) => <Step key={i} line={l} />)}</ol>
      {lines.every((l) => parseStep(l)?.needs.endsWith(LOWEST_BAR)) && <p className="gate-note"><Icon name="person-check" /> Every step accepts evidence the agent reports itself. The hub records it; it does not check it.</p>}
      {c.procedure && (
        <details className="skill-proc">
          <summary><Icon name="file-earmark-text" /> Full procedure <span className="muted">· the text agents follow</span></summary>
          <Markdown source={c.procedure} />
        </details>
      )}
    </div>
  )
}

// An adopted skill shows everything; an available one stays at its head, summary and command until opened.
function SkillItem({ c, pin, runs, command }: { c: CatalogEntry; pin: Pin | null; runs?: ReactNode; command: ReactNode }) {
  const how = <><Applicability c={c} /><HowItWorks c={c} pin={pin} heading={pin !== null} /></>
  return (
    <li className="skill-item" data-tour="skills-item">
      <SkillHead c={c}>{runs}</SkillHead>
      <p className="skill-summary">{c.summary}</p>
      {pin && how}
      <div className="skill-act">{command}</div>
      {!pin && (
        <details className="skill-details">
          <summary><Icon name="signpost-split" /> How it works</summary>
          {how}
        </details>
      )}
      <details className="skill-details">
        <summary><Icon name="sliders" /> Settings and version digest</summary>
        <p className="small mb-1">Digest <code>sha256:{shortSha(c.sha256)}</code></p>
        <Params c={c} pin={pin} />
      </details>
    </li>
  )
}

function SkillHead({ c, children }: { c: CatalogEntry; children?: ReactNode }) {
  const { enforced, planned } = enforcement(c.gates)
  return (
    <div className="skill-head" data-tour="skills-head">
      <strong>{c.name}</strong> <span className="muted">v{c.workflow_version}</span>
      {c.first_party && <Pill tone="muted" icon="patch-check">Built in</Pill>}
      {enforced.length > 0 && <Pill tone="muted" icon="shield" tip="Some steps can hold the author’s session until they have evidence, through the opt-in Stop hook. It is installed per checkout with the agentscm skill stop command, and the hub does not know which checkouts have it. It fails open: the agent may finish when the hub is unreachable or the checkout is detached.">Can stop the agent (opt-in)</Pill>}
      {planned.map((p) => <Pill key={p} tone="muted" icon="hourglass-split" tip="Not built yet. Nothing checks this step on the hub’s behalf today.">{p[0]!.toUpperCase() + p.slice(1)}: planned</Pill>)}
      {children}
    </div>
  )
}

function Params({ c, pin }: { c: CatalogEntry; pin: Pin | null }) {
  if (c.parameters.length === 0) return <p className="small muted mb-0">No settings.</p>
  return (
    <p className="small mb-0">
      <span className="muted">Settings: </span>
      {c.parameters.map((p, i) => {
        const v = pin && p.name in pin.params ? pin.params[p.name] : p.default
        const changed = pin !== null && p.name in pin.params && JSON.stringify(pin.params[p.name]) !== JSON.stringify(p.default)
        return (
          <span key={p.name} title={p.description}>
            {i > 0 && ', '}
            <code>{p.name}</code>={v === null ? 'default' : typeof v === 'string' ? v : JSON.stringify(v)}
            {changed && <> <Pill tone="muted">Overridden</Pill></>}
          </span>
        )
      })}
    </p>
  )
}

function LibraryView({ repo, token }: { repo: string; token: string }) {
  const poll = usePoll((signal) => fetchLibrary(repo, token, signal), 15_000, `skills|${repo}`)
  return (
    <div className="page">
      <PageHead
        question="Which workflows are agents in this repository expected to follow?"
        answer={poll.kind === 'ready' ? <LibraryAnswer lib={poll.value} /> : 'Checking which skills this repository has adopted…'}
        fact={<>{poll.kind === 'ready' && <LibraryFact lib={poll.value} />}<a href="#templates?view=skills">Create or import reusable skills</a></>}
      />
      <SectionTabs />
      <Loaded poll={poll} what="skills">{(lib) => <Library lib={lib} />}</Loaded>
    </div>
  )
}

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`

function LibraryAnswer({ lib }: { lib: Library }) {
  const { names, open, atStop } = adoptedSummary(lib)
  const n = names.length
  if (n === 0) return <>This repository has adopted no skills yet.</>
  // Name the skills while the list stays short enough to read in one breath.
  // Each name stays whole in the headline: "pr-gate" never wraps at its hyphen.
  const named = n <= 3 ? <>, {new Intl.ListFormat('en', { type: 'conjunction' }).formatToParts(names).map((p, i) => (p.type === 'element' ? <span key={i} className="text-nowrap">{p.value}</span> : p.value))}</> : null
  return (
    <>
      Agents here are asked to follow <b>{n} <Gloss term="skills">{n === 1 ? 'skill' : 'skills'}</Gloss></b>{named}.{' '}
      {open === 0 ? 'No runs are open.' : <><a href={skillRunsHref}><b>{plural(open, 'run is', 'runs are')} open</b></a>{atStop > 0 ? `; ${plural(atStop, 'is', 'are')} waiting at a stop gate.` : '.'}</>}
    </>
  )
}

function LibraryFact({ lib }: { lib: Library }) {
  const { steps, canStop } = stepCounts(lib)
  return (
    <>
      {steps > 0 && <>{canStop} of {steps} steps can stop an agent, where the opt-in Stop hook is installed; agents report their own evidence. </>}
      <span>{adoptionLabel(lib.adoption.source, lib.adoption.default_branch)}</span>.{' '}
    </>
  )
}

function Library({ lib }: { lib: Library }) {
  const byKey = (p: Pin) => lib.catalog.find((c) => c.name === p.name && c.sha256 === p.sha256) ?? null
  const adopted = lib.adoption.pins.filter((p) => p.status === 'adopted').flatMap((p) => { const c = byKey(p); return c ? [{ p, c }] : [] })
  const pinned = new Set(lib.adoption.pins.filter((p) => p.status === 'adopted').map((p) => p.name))
  const available = lib.catalog.filter((c) => !pinned.has(c.name))
  const attention = lib.adoption.pins.filter((p) => p.status !== 'adopted')
  return (
    <>
      {attention.length > 0 && (
        <div className="d-grid gap-2">
          {attention.map((p) => (
            <Notice key={`${p.name}:${p.sha256}`} title={`${p.name} is pinned but not in force`} icon="exclamation-triangle">
              <Pill tone="warn">{pinProblem(p).label}</Pill> {pinProblem(p).text(p, lib.catalog.find((c) => c.name === p.name) ?? null)}
            </Notice>
          ))}
        </div>
      )}
      <Box tour="skills-adopted" icon="check2-circle" title="Adopted" explain="Skills pinned in this repository’s lock file. Agents are expected to follow them. Adopting or removing one takes effect once agentscm-skills.lock.json is committed.">
        {adopted.length === 0 ? (
          <Empty icon="journal-check" title={lib.adoption.source === 'none' ? 'Not adopted' : 'Nothing adopted'}>No skill is pinned yet. Copy an adopt command under Available, run it in the repository, then commit the lock file.</Empty>
        ) : (
          <ul className="skill-list">
            {adopted.map(({ p, c }) => {
              const n = lib.counts[c.name] ?? { open: 0, at_stop_gate: 0, completed: 0 }
              return (
                <SkillItem
                  key={c.name}
                  c={c}
                  pin={p}
                  runs={<span className="skill-runs">{n.open} open, {n.at_stop_gate} at a stop gate, {n.completed} completed · <a href={skillRunsHref}>View runs</a></span>}
                  command={<><p className="small muted mb-1">To remove it, run this in the repository, then commit the lock file:</p><Cmd command={removeCommand(c.name)} label={`Remove ${c.name}`} /></>}
                />
              )
            })}
          </ul>
        )}
      </Box>
      <Box tour="skills-available" icon="collection" title="Available" count={available.length} explain="Skills this hub has that the repository hasn’t pinned. Copy the command to add one.">
        {available.length === 0 ? (
          <Empty icon="collection" title="Everything is adopted">Every skill this hub ships is already pinned.</Empty>
        ) : (
          <ul className="skill-list">
            {available.map((c) => (
              <SkillItem
                key={c.name}
                c={c}
                pin={null}
                command={<><p className="small muted mb-1">To adopt it, run this in the repository, then commit <code>agentscm-skills.lock.json</code>:</p><Cmd command={addCommand(c.name)} label={`Adopt ${c.name}`} /></>}
              />
            ))}
          </ul>
        )}
      </Box>
    </>
  )
}

function pinProblem(p: Pin): { label: string; text: (p: Pin, shipped: CatalogEntry | null) => string } {
  if (p.status === 'unknown_version') return { label: 'Unknown version', text: (q, s) => `sha256:${shortSha(q.sha256)} matches no version this hub ships${s ? ` (it ships v${s.workflow_version}, sha256:${shortSha(s.sha256)})` : ''}. Agents are not held to it.` }
  if (p.status === 'unsupported_source') return { label: 'Unsupported source', text: (q) => `This hub cannot resolve skills from a "${q.source}" source yet.` }
  return { label: 'Invalid settings', text: (q) => `The pinned settings (${paramText(q.params) || 'none'}) do not fit the skill's parameters.` }
}

// ---------- runs

type RunTab = 'open' | 'completed'

function RunsList({ repo, token }: { repo: string; token: string }) {
  const poll = usePoll((signal) => fetchRuns(repo, token, signal), 15_000, `skill-runs|${repo}`)
  const [tab, setTab] = useState<RunTab>('open')
  const [search, setSearch] = useState('')
  const tasks = useTaskIndex(token)
  return (
    <div className="page">
      <PageHead question="Where is each pull request in its workflow?" answer="Every pull request, at the step its workflow reached." fact={<a href={skillsHref}><Icon name="arrow-left" /> Back to the skills library</a>} />
      <SectionTabs />
      <Loaded poll={poll} what="skill runs">
        {({ runs, truncated }) => {
          const inTab = (r: RunView) => (tab === 'open') === (r.state === 'open')
          const shown = runs.filter((r) => inTab(r) && runMatches(r, search))
          return (
            <>
              {truncated && <Notice title="This list is incomplete">The hub returned only the most recent runs. Older runs are not shown.</Notice>}
              <div className="lens" role="tablist" aria-label="Run views">
                {(['open', 'completed'] as const).map((t) => (
                  <button key={t} type="button" role="tab" aria-selected={tab === t} className={`lens-seg${tab === t ? ' on' : ''}`} onClick={() => setTab(t)}>
                    {t === 'open' ? 'Open' : 'Completed'}
                    <span className="tab-count">{runs.filter((r) => (t === 'open') === (r.state === 'open')).length}</span>
                  </button>
                ))}
              </div>
              <div className="input-group search">
                <span className="input-group-text"><Icon name="search" /></span>
                <input className="form-control" aria-label="Search runs on this page" placeholder="Search runs on this page" value={search} onChange={(e) => setSearch(e.target.value)} />
              </div>
              <Box tour="skills-runs" icon="signpost-split" title={tab === 'open' ? 'Open runs' : 'Completed runs'} explain={SEARCH_THIS_PAGE_ONLY}>
                {shown.length === 0 ? (
                  <Empty icon="signpost-split" title={search ? 'No runs match' : tab === 'open' ? 'No open runs' : 'No completed runs'}>{search ? 'Try another branch, pull request number or next action.' : 'Runs start when an agent records the first evidence for a branch. Adopt a skill on the Skills page to begin.'}</Empty>
                ) : (
                  <ul className="run-list">{shown.map((r) => <RunRow key={r.run_id} run={r} task={tasks?.byBranch.get(r.branch)} />)}</ul>
                )}
              </Box>
            </>
          )
        }}
      </Loaded>
    </div>
  )
}

function RunRow({ run, task }: { run: RunView; task: WorkItem | undefined }) {
  const self = selfReportedCount(run)
  const l = task ? lead(task) : null
  return (
    <li>
      <a href={skillRunHref(run.run_id)} className="run-row skill-run-row">
        <span className="run-main">
          {task && l && <strong className="d-block">{task.title} · {who(l.login, l.agent)}</strong>}
          {task ? <span className="small">{run.pull_request !== null ? `#${run.pull_request}` : 'No pull request yet'} <code className="branch-name">{run.branch}</code></span> : <><strong>{run.pull_request !== null ? `#${run.pull_request}` : 'No pull request yet'}</strong> <code className="branch-name">{run.branch}</code></>}
          <span className="small muted">{run.skill.name} v{run.skill.workflow_version}, head <code>{shortSha(run.head_sha)}</code></span>
          <span className="skill-next">{run.next_action}</span>
        </span>
        <span className="skill-track"><Track steps={gateSteps(run.gates)} /></span>
        <span className="skill-state">
          <Pill tone={runStateTone(run)}>{runStateLabel(run)}</Pill>
          {self > 0 && <Pill tone="warn" icon="person-check" tip="Gates marked done on evidence an agent recorded itself. The hub did not check it.">{self} self-reported</Pill>}
        </span>
        <Icon name="chevron-right" className="muted" />
      </a>
    </li>
  )
}

// ---------- one run

function EvidenceItem({ e }: { e: Evidence }) {
  const a = assuranceView(e.assurance)
  return (
    <li>
      {e.summary} {a && e.assurance === 'self_reported' && <Pill tone={a.tone} icon={a.icon}>{a.label}</Pill>}
      <span className="small muted d-block">{e.kind} on <code>{shortSha(e.head_sha)}</code>{e.agent ? ` · ${agentName(e.agent)}` : ''}{e.session ? ` · session ${e.session.slice(0, 8)}` : ''} · <Ago at={e.recorded_at} />{/^https?:\/\//.test(e.url ?? '') && <> · <a href={e.url!} target="_blank" rel="noreferrer noopener">link</a></>}</span>
    </li>
  )
}

function RunDetail({ repo, runId, token }: { repo: string; runId: string; token: string }) {
  const poll = usePoll((signal) => fetchRun(repo, runId, token, signal), 15_000, `skill-run|${repo}|${runId}`)
  const tasks = useTaskIndex(token)
  return (
    <div className="page">
      <p className="mb-2"><a href={skillRunsHref}><Icon name="arrow-left" /> All runs</a></p>
      <Loaded poll={poll} what="this run">{(run) => <Run run={run} task={tasks?.byBranch.get(run.branch)} />}</Loaded>
    </div>
  )
}

function Run({ run, task }: { run: RunView; task: WorkItem | undefined }) {
  const last = lastEvidence(run)
  const lastA = last ? assuranceView(last.assurance) : null
  const l = task ? lead(task) : null
  const pr = run.pull_request !== null ? `#${run.pull_request} ` : ''
  return (
    <>
      <PageHead
        className="detail-head"
        question={task ? task.title : `${pr}${run.branch}`}
        answer={<><Pill tone={runStateTone(run)}>{runStateLabel(run)}</Pill> <span className="text-nowrap">{run.skill.name} v{run.skill.workflow_version}</span></>}
        fact={<>{task && l && <><WorkLink id={task.id}>Open the task</WorkLink> by {who(l.login, l.agent)}. </>}<span>Proof so far: <ProofBadge level={runProof(run.gates)} /></span></>}
      />
      <div className="skill-cells">
        <div><h2>Next action</h2><p className="mb-0">{run.next_action}</p></div>
        <div>
          <h2>Blocking finish? <Explain text={run.blocks_finish ? 'Where the opt-in Stop hook is installed, it blocks the author at tests, review and fix until this gate has evidence for the current head.' : 'Where the opt-in Stop hook is installed, it blocks the author only at tests, review and fix.'} /></h2>
          <p className="mb-0"><strong>{run.blocks_finish ? 'Yes' : 'No'}</strong>. {run.blocks_finish ? 'The author is held here until this gate has evidence.' : 'This run is not at tests, review or fix.'}</p>
        </div>
        <div>
          <h2>Last evidence</h2>
          <p className="mb-0">{last ? <>{last.summary} · <Ago at={last.recorded_at} />{lastA && last.assurance === 'self_reported' && <> <Pill tone={lastA.tone} icon={lastA.icon}>{lastA.label}</Pill></>}</> : 'None recorded.'}</p>
        </div>
      </div>
      <Box icon="list-check" title="Gates">
        <table className="skill-gates">
          <thead><tr><th scope="col">Gate</th><th scope="col">State</th><th scope="col">Evidence</th><th scope="col">Assurance</th></tr></thead>
          <tbody>
            {run.gates.map((g) => {
              const s = GATE_STATE_VIEW[g.state]
              const a = assuranceView(g.assurance)
              return (
                <tr key={g.id}>
                  <th scope="row" data-label="Gate">{gateLabel(g.id)}</th>
                  <td data-label="State"><Pill tone={s.tone} icon={s.icon}>{s.label}</Pill></td>
                  <td data-label="Evidence">{g.evidence.length ? <ul className="skill-evidence">{g.evidence.map((e) => <EvidenceItem key={e.id} e={e} />)}</ul> : <span className="muted">—</span>}</td>
                  <td data-label="Assurance">{a ? <Pill tone={a.tone} icon={a.icon} tip={a.tip}>{a.label}</Pill> : <span className="muted">—</span>}</td>
                </tr>
              )
            })}
          </tbody>
        </table>
      </Box>
      <details className="skill-details">
        <summary>Stale evidence (older heads) · {run.stale_evidence.length}</summary>
        {run.stale_evidence.length === 0 ? <p className="small muted">None. Evidence bound to an older head would be kept here and never counted.</p> : <ul className="skill-evidence">{run.stale_evidence.map((e) => <EvidenceItem key={e.id} e={e} />)}</ul>}
      </details>
      <details className="skill-details">
        <summary>Technical details</summary>
        <dl className="skill-tech">
          {task && <><dt>task</dt><dd><WorkLink id={task.id}>{task.title}</WorkLink></dd></>}
          <dt>pull request</dt><dd>{run.pull_request !== null ? `#${run.pull_request}` : 'none yet'} on <code>{run.branch}</code></dd>
          <dt>run_id</dt><dd><code>{run.run_id}</code></dd>
          <dt>skill sha256</dt><dd><code>{run.skill.sha256}</code></dd>
          <dt>head</dt><dd><code>{run.head_sha}</code></dd>
          <dt>settings</dt><dd>{Object.keys(run.params).length ? <code>{paramText(run.params)}</code> : 'none'}</dd>
          <dt>updated</dt><dd><Ago at={run.updated_at} /></dd>
        </dl>
      </details>
    </>
  )
}
