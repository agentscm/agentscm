import { useEffect, useState } from 'react'
import { SectionTabs } from '../TopNav.tsx'
import { fetchSkillWorkflow, fetchSkillWorkflows } from '../api.ts'
import { validSkillWorkflowRunId, validSkillWorkflowStepId } from '../skillWorkflow.ts'
import type { SkillWorkflowProjection, SkillWorkflowRunState, SkillWorkflowStep, SkillWorkflowStepState } from '../types.ts'
import { Box, Empty, Explain, Icon, Notice, Path, Pill, Skeleton, Tip, type Tone } from '../ui.tsx'
import { usePoll } from '../usePoll.ts'
import { Combobox, type ComboResult } from '../Combobox.tsx'
import { hashPath, Pager, useHashCursor } from '../Pager.tsx'

type Route = { kind: 'list' } | { kind: 'run'; runId: string; stepId?: string }

const STATE: Record<SkillWorkflowRunState, { label: string; tone: Tone; icon: string }> = {
  pending: { label: 'Pending', tone: 'muted', icon: 'hourglass' },
  running: { label: 'Running', tone: 'brand', icon: 'arrow-repeat' },
  succeeded: { label: 'Succeeded', tone: 'ok', icon: 'check-circle' },
  completed_with_omissions: { label: 'Completed with omissions', tone: 'warn', icon: 'check2-square' },
  failed: { label: 'Failed', tone: 'bad', icon: 'x-circle' },
  blocked: { label: 'Blocked', tone: 'bad', icon: 'sign-stop' },
  cancelled: { label: 'Cancelled', tone: 'muted', icon: 'slash-circle' },
}
const stateOf = (s: SkillWorkflowRunState | SkillWorkflowStepState) => STATE[s]

const decode = (v: string) => {
  try {
    return decodeURIComponent(v)
  } catch {
    return null
  }
}
export function routeFromHash(hash = window.location.hash): Route {
  const parts = hashPath(hash).slice(1).split('/')
  if (parts[0] !== 'workflows' || parts[1] !== 'runs' || !parts[2]) return { kind: 'list' }
  const runId = decode(parts[2])
  if (!runId || !validSkillWorkflowRunId(runId)) return { kind: 'list' }
  const stepId = parts[3] === 'steps' && parts[4] ? decode(parts[4]) : null
  return stepId && validSkillWorkflowStepId(stepId) ? { kind: 'run', runId, stepId } : { kind: 'run', runId }
}
export const runHref = (id: string) => `#workflows/runs/${encodeURIComponent(id)}`

function unavailable(reason: string): string {
  if (reason === 'native_workflow_not_configured') return 'Native workflow lookup is not configured for this repository.'
  if (reason === 'native_workflow_binding_unavailable') return 'The configured native workflow binding is unavailable.'
  if (reason === 'native_workflow_binding_replaced') return 'The configured native repository identity has changed.'
  return 'Native workflow lookup is currently unavailable.'
}
function requestError(message: string, status?: number): string {
  if (status === 401 || status === 403) return 'This token is not authorized to read workflows.'
  if (status === 404) return 'No retained run with this ID was found for this repository.'
  if (status === 503) return 'Workflow lookup is temporarily unavailable or busy.'
  return message
}

function nextAction(p: SkillWorkflowProjection): { tone: Tone; icon: string; title: string; body: string } {
  const { run, present_applicability: a } = p
  const landing = p.present_landing_binding
  const productive = run.steps.some((s) => s.action)
  const findings = run.steps.reduce((n, s) => n + (s.review?.findings.filter((f) => f.severity !== 'note').length ?? 0), 0)
  if (run.steps.some((s) => s.action?.outcome === 'uncertain')) return { tone: 'warn', icon: 'question-circle', title: 'Inspect the interrupted action', body: 'The action may have run. Inspect its retained outcome before starting new work; replay could repeat its effects.' }
  if (productive && a.state !== 'current') return { tone: 'warn', icon: 'arrow-clockwise', title: 'These outputs belong to an earlier source binding', body: a.reason ?? 'Restore the source binding or start a fresh run for the current source. Historical artifacts do not establish a current result.' }
  if (a.state !== 'current') return { tone: a.state === 'stale' ? 'warn' : 'bad', icon: 'arrow-clockwise', title: a.state === 'stale' ? 'Rerun on the current source' : 'Restore the native binding', body: a.reason ?? 'The current native source binding cannot be verified. This run cannot support a new landing.' }
  if (landing && landing.state !== 'current') return { tone: landing.state === 'stale' ? 'warn' : 'bad', icon: 'link-45deg', title: landing.state === 'stale' ? 'Refresh the landing binding' : 'Inspect the landing binding', body: landing.reason ?? 'The current Change, review context, policy, or protected target cannot be verified for this run.' }
  if (findings > 0 || run.state === 'failed' || run.state === 'blocked') return { tone: 'bad', icon: 'tools', title: findings > 0 ? `Resolve ${findings} review finding${findings === 1 ? '' : 's'}` : 'Fix the failing required step', body: productive ? 'Inspect the action result and required runtime capabilities. Missing or invalid artifacts cannot satisfy downstream inputs.' : 'Apply the fixes, checkpoint a fresh immutable revision, and run the workflow again.' }
  if (run.state === 'pending' || run.state === 'running') return { tone: 'brand', icon: 'hourglass-split', title: 'Workflow is still in progress', body: 'Refresh after the local controller finishes or recovers its current step.' }
  if (run.state === 'completed_with_omissions') return { tone: 'warn', icon: 'list-check', title: 'Review optional omissions', body: 'Required steps passed, but omitted work remains visible.' }
  if (run.state === 'cancelled') return { tone: 'warn', icon: 'play', title: 'Start a fresh run', body: 'This cancelled run cannot establish a current passing result.' }
  if (productive) return { tone: 'muted', icon: 'file-earmark-text', title: 'Declared outputs were produced', body: 'Artifact format and size were validated. Content has not been independently verified, approved, or published by these actions.' }
  if (!landing) return { tone: 'ok', icon: 'check-circle', title: 'Recorded source checks passed', body: 'This source-only workflow is not a native landing decision.' }
  return { tone: 'ok', icon: 'shield-check', title: 'Checks and bindings are current', body: 'Native landing still revalidates this exact immutable revision.' }
}

function levels(steps: SkillWorkflowStep[]): SkillWorkflowStep[][] {
  const byId = new Map(steps.map((s) => [s.id, s]))
  const depths = new Map<string, number>()
  const depth = (s: SkillWorkflowStep, seen = new Set<string>()): number => {
    const known = depths.get(s.id)
    if (known !== undefined) return known
    if (seen.has(s.id)) return 0
    seen.add(s.id)
    const deps = s.needs.map((id) => byId.get(id)).filter((x): x is SkillWorkflowStep => !!x)
    const v = deps.length === 0 ? 0 : Math.max(...deps.map((x) => depth(x, seen))) + 1
    seen.delete(s.id)
    depths.set(s.id, v)
    return v
  }
  const out: SkillWorkflowStep[][] = []
  for (const s of steps) (out[depth(s)] ??= []).push(s)
  return out
}

function StepDetail({ step }: { step: SkillWorkflowStep }) {
  const st = stateOf(step.state)
  return (
    <Box icon="box" title={`Step: ${step.id}`} tone={st.tone} explain="What this step recorded. Observations are files the step read, with their content hash.">
      <div className="pills mb-2">
        <Pill tone={st.tone} icon={st.icon}>{st.label}</Pill>
        <Pill tone="muted" icon={step.required ? 'asterisk' : 'dash'} tip={step.required ? 'The run cannot pass without this step.' : 'Optional: the run can complete with this step omitted.'}>
          {step.required ? 'Required' : 'Optional'}
        </Pill>
        {step.needs.length > 0 && <span className="small muted"><Icon name="diagram-2" /> after {step.needs.join(', ')}</span>}
      </div>
      {step.detail && <p>{step.detail}</p>}
      {step.action && (
        <div className="mb-3">
          <h3 className="h6">Productive action</h3>
          <p className="small">{step.action.outcome === null ? 'Dispatch reserved; no terminal result recorded.' : `Execution: ${step.action.outcome.replaceAll('_', ' ')}.`} {step.action.observed_model ? `Observed model: ${step.action.observed_model}.` : 'No model identity recorded.'}</p>
          <p className="small muted">Output validation is separate from independent verification, approval and publication.</p>
          {step.action.artifact && (
            <dl className="tech">
              <dt>Retained artifact</dt><dd>{step.action.artifact.name}</dd>
              <dt>Format</dt><dd>{step.action.artifact.media_type}</dd>
              <dt>Size</dt><dd>{step.action.artifact.bytes.toLocaleString()} bytes</dd>
              <dt>Content digest</dt><dd><code className="text-break">{step.action.artifact.sha256}</code></dd>
            </dl>
          )}
          <details className="disclosure"><summary>Execution profile identity</summary><code className="text-break">{step.action.profile_digest}</code></details>
        </div>
      )}
      {step.execution && (
        <div className="small mb-3">
          <span className="label">Command execution</span> attempt <code>{step.execution.attempt_id}</code> · gen {step.execution.generation} · {step.execution.state}
          {step.execution.outcome && <> · {step.execution.outcome}</>}
          {step.execution.failure && <div className="warn-text">{step.execution.failure}</div>}
        </div>
      )}
      {step.review && (
        <div className="review">
          <div className="small mb-2">
            <span className="label">Review</span> model {step.review.model_observed ?? step.review.model_requested} · {step.review.status}
            {step.review.stop && <> · stopped: {step.review.stop}</>}
          </div>
          {step.review.findings.length === 0 ? (
            <Empty icon="check2-circle" title="No findings" />
          ) : (
            <ul className="findings">
              {step.review.findings.map((f, i) => (
                <li key={i} className={`finding tone-${f.severity === 'error' ? 'bad' : f.severity === 'warning' ? 'warn' : 'muted'}`}>
                  <Pill tone={f.severity === 'error' ? 'bad' : f.severity === 'warning' ? 'warn' : 'muted'} icon={f.severity === 'note' ? 'sticky' : 'exclamation-triangle'}>{f.severity}</Pill>
                  <strong>{f.title}</strong>
                  {f.path && <Path>{`${f.path}${f.start_line ? `:${f.start_line}${f.end_line && f.end_line !== f.start_line ? `-${f.end_line}` : ''}` : ''}`}</Path>}
                  <p className="small mb-0">{f.detail}</p>
                </li>
              ))}
            </ul>
          )}
          {step.review.unresolved.length > 0 && <Notice title="Unresolved">{step.review.unresolved.join(' · ')}</Notice>}
          {step.review.covered_paths.length > 0 && (
            <details className="disclosure">
              <summary><Icon name="files" /> Covered paths ({step.review.covered_paths.length})</summary>
              <div className="paths mt-2">{step.review.covered_paths.map((p) => <Path key={p}>{p}</Path>)}</div>
            </details>
          )}
        </div>
      )}
      {step.observations.length > 0 && (
        <details className="disclosure">
          <summary><Icon name="file-earmark-text" /> Evidence ({step.observations.length} files)</summary>
          <table className="table table-sm small mt-2 mb-0">
            <tbody>
              {step.observations.map((o) => (
                <tr key={o.path}>
                  <td><Path>{o.path}</Path></td>
                  <td className="text-end">{o.bytes.toLocaleString()} B</td>
                  <td><Tip text={o.sha256} focusable><code>{o.sha256.slice(0, 12)}</code></Tip></td>
                </tr>
              ))}
            </tbody>
          </table>
        </details>
      )}
    </Box>
  )
}

/** One retained run: next action, step graph, selected step and technical details. Shared with the Workspace deep link. */
export function RunView({ projection: p, stepId, stepHref }: { projection: SkillWorkflowProjection; stepId?: string; stepHref: (stepId: string) => string }) {
  const run = p.run
  const st = stateOf(run.state)
  const next = nextAction(p)
  const step = stepId ? run.steps.find((s) => s.id === stepId) : undefined
  return (
    <div className="stack">
      <header className="detail-head">
        <h1 className="page-question">Run <code>{run.run_id}</code></h1>
        <div className="pills">
          <Pill tone={st.tone} icon={st.icon}>{st.label}</Pill>
          <Pill tone="muted" icon="git" tip="The immutable source revision this run checked.">{run.source.revision.slice(0, 12)}</Pill>
          <Pill tone="muted" icon="shield" tip="Native landing admission is decided separately and is never assessed by this view.">Landing not assessed</Pill>
        </div>
      </header>
      <Notice tone={next.tone} icon={next.icon} title={next.title}>{next.body}</Notice>
      <Box icon="diagram-3" title="Steps" count={run.steps.length} explain="Steps run left to right; a step starts once every step it needs has finished. Select a step to see its evidence.">
        <div className="dag">
          {levels(run.steps).map((lvl, i) => (
            <div className="dag-level" key={i}>
              {lvl.map((s) => {
                const ss = stateOf(s.state)
                return (
                  <a key={s.id} href={stepHref(s.id)} className={`dag-node tone-${ss.tone}${s.id === stepId ? ' active' : ''}`}>
                    <Icon name={ss.icon} /> <span>{s.id}</span>
                    {!s.required && <span className="small muted">optional</span>}
                  </a>
                )
              })}
            </div>
          ))}
        </div>
      </Box>
      {step && <StepDetail step={step} />}
      <details className="disclosure box">
        <summary><Icon name="code-square" /> Technical details</summary>
        <dl className="tech">
          <dt>Repository</dt><dd>{run.source.repository}</dd>
          <dt>Revision</dt><dd><code>{run.source.revision}</code></dd>
          <dt>Policy epoch</dt><dd>{run.source.policy_epoch}</dd>
          <dt>Plan digest</dt><dd><code>{run.plan_digest}</code></dd>
          <dt>Skill digest</dt><dd><code>{run.skill_digest}</code></dd>
          <dt>Applicability</dt><dd>{p.present_applicability.state}{p.present_applicability.reason && ` · ${p.present_applicability.reason}`}</dd>
          <dt>Landing binding</dt><dd>{p.present_landing_binding ? `${p.present_landing_binding.state}${p.present_landing_binding.target ? ` · ${p.present_landing_binding.target}` : ''}` : 'None (source-only workflow)'}</dd>
        </dl>
      </details>
    </div>
  )
}

function Run({ token, runId, stepId }: { token: string; runId: string; stepId?: string }) {
  const load = usePoll((signal) => fetchSkillWorkflow(runId, token, signal), 0, runId)
  return (
    <div className="page">
      <a href="#workflows" className="back"><Icon name="arrow-left" /> All runs</a>
      {load.kind === 'loading' && <Skeleton rows={6} />}
      {load.kind === 'error' && <Notice tone="bad" title="Could not load this run" action={<button className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>}>{requestError(load.message, load.status)}</Notice>}
      {load.kind === 'ready' && load.value.availability === 'unavailable' && <Notice title="Workflow lookup unavailable">{unavailable(load.value.reason)}</Notice>}
      {load.kind === 'ready' && load.value.availability === 'available' && <RunView projection={load.value.projection} {...(stepId ? { stepId } : {})} stepHref={(id) => `${runHref(runId)}/steps/${encodeURIComponent(id)}`} />}
    </div>
  )
}

/** Find a retained run by name, state, revision or ID; a pasted run ID opens directly. */
function RunPicker({ token }: { token: string }) {
  return (
    <Combobox
      id="workflow-run-picker"
      data-tour="workflow-run-picker"
      label="Find a run"
      placeholder="Find a run"
      looseId
      parseId={(t) => (validSkillWorkflowRunId(t) ? t : null)}
      onSelect={(id) => { window.location.hash = runHref(id).slice(1) }}
      search={async function page(q, signal, before?: string): Promise<ComboResult> {
        const r = await fetchSkillWorkflows(token, signal, before, q)
        if (r.availability !== 'available') return { options: [] }
        const next = r.next_cursor
        return {
          more: next !== null,
          moreLabel: 'Search older runs',
          emptyMoreText: 'No matches in the newest runs searched',
          ...(next !== null ? { loadMore: (s: AbortSignal) => page(q, s, next) } : {}),
          options: r.runs.map((run) => {
            const st = stateOf(run.state)
            return { value: run.run_id, text: run.name, label: run.name, secondary: run.run_id, meta: <><Pill tone={st.tone}>{st.label}</Pill> <span className="small muted">rev <code>{run.revision.slice(0, 10)}</code></span></> }
          }),
        }
      }}
    />
  )
}

function List({ token }: { token: string }) {
  const pager = useHashCursor()
  const [query, setQuery] = useState('')
  const [status, setStatus] = useState('all')
  const cursor = pager.cursor
  const load = usePoll((signal) => fetchSkillWorkflows(token, signal, cursor ?? undefined), 0, String(cursor))
  return (
    <div className="page">
      <div className="answer" data-tour="answer">
        <h1 className="page-question">Did the checks pass?</h1>
        <p className="answer-text">Every check, review and action, each tied to one exact revision.</p>
        <p className="answer-fact">Producing an artifact does not verify its content or permit landing.</p>
      </div>
      <SectionTabs />
      <Box
        tour="workflows-runs"
        tone={load.kind === 'ready' && load.value.availability === 'available' && load.value.runs.some((r) => r.state === 'failed' || r.state === 'blocked') ? 'bad' : 'brand'}
        icon="diagram-3"
        title="Workflow runs"
        explain="Skill workflows compose checks, reviews and artifact-producing actions. Select a run to inspect what each step actually recorded."
        count={load.kind === 'ready' && load.value.availability === 'available' ? load.value.runs.length : undefined}
        actions={
          <RunPicker token={token} />
        }
      >
        {load.kind === 'loading' && <Skeleton rows={4} />}
        {load.kind === 'error' && <Notice tone="bad" title="Could not load workflow runs" action={<button className="btn btn-sm btn-outline-secondary" onClick={load.retry}>Retry</button>}>{requestError(load.message, load.status)}</Notice>}
        {load.kind === 'ready' && load.value.availability === 'unavailable' && <Empty icon="diagram-3" title="Workflow lookup unavailable">{unavailable(load.value.reason)}</Empty>}
        {load.kind === 'ready' && load.value.availability === 'available' && (() => {
          const runs = load.value.runs.filter((r) => (status === 'all' || r.state === status) && (!query.trim() || [r.name, r.run_id, r.revision].some((v) => v.toLowerCase().includes(query.trim().toLowerCase()))))
          const nextCursor = load.value.next_cursor
          return (
            <>
              <div className="toolbar mb-3">
                <div className="input-group input-group-sm search">
                  <span className="input-group-text"><Icon name="funnel" /></span>
                  <input className="form-control" placeholder="Filter this page" aria-label="Filter workflow runs on this page" value={query} onChange={(e) => setQuery(e.target.value)} />
                </div>
                <select className="form-select form-select-sm w-auto" aria-label="Filter by status" value={status} onChange={(e) => setStatus(e.target.value)}>
                  <option value="all">All statuses</option>
                  {(Object.keys(STATE) as SkillWorkflowRunState[]).map((s) => <option key={s} value={s}>{STATE[s].label}</option>)}
                </select>
              </div>
              {runs.length === 0 ? (
                <Empty icon="inbox" title={load.value.runs.length ? 'No run on this page matches' : 'No retained runs yet'}>
                  {!load.value.runs.length && 'Runs appear here after a skill workflow records its result on the hub.'}
                </Empty>
              ) : (
                <ul className="run-list">
                  {runs.map((r) => {
                    const st = stateOf(r.state)
                    return (
                      <li key={r.run_id}>
                        <a href={runHref(r.run_id)} className="run-row">
                          <span className={`run-icon tone-${st.tone}`}><Icon name={st.icon} /></span>
                          <span className="run-main">
                            <strong>{r.name}</strong>
                            <span className="small muted"><code>{r.run_id}</code> · rev <code>{r.revision.slice(0, 10)}</code> · #{r.sequence}</span>
                          </span>
                          <Pill tone={st.tone}>{st.label}</Pill>
                          <Icon name="chevron-right" className="muted" />
                        </a>
                      </li>
                    )
                  })}
                </ul>
              )}
              <Pager onNewer={pager.canNewer ? pager.newer : undefined} onOlder={nextCursor ? () => pager.older(nextCursor) : undefined} complete={!nextCursor} />
            </>
          )
        })()}
      </Box>
      <p className="small muted"><Explain text="AgentSCM can also record workflows started from the Workspace; those open from the Workspace page." /> Landing admission is never decided on this page.</p>
    </div>
  )
}

export function WorkflowsPage({ token, repository }: { token: string; repository: string | null }) {
  const [route, setRoute] = useState(() => routeFromHash())
  useEffect(() => {
    const on = () => setRoute(routeFromHash())
    window.addEventListener('hashchange', on)
    return () => window.removeEventListener('hashchange', on)
  }, [])
  if (!repository) return <div className="page"><Skeleton rows={4} /></div>
  return route.kind === 'list' ? <List token={token} /> : <Run token={token} runId={route.runId} {...(route.stepId ? { stepId: route.stepId } : {})} />
}
