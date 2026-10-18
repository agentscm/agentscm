import {
  ArrowLeftOutlined,
  CheckCircleOutlined,
  ClockCircleOutlined,
  ExclamationCircleOutlined,
  FileSearchOutlined,
  ReloadOutlined,
  SearchOutlined,
  StopOutlined,
} from '@ant-design/icons'
import { Alert, Breadcrumb, Button, Collapse, Empty, Input, Select, Skeleton, Table, Tabs, Tag } from 'antd'
import { useEffect, useMemo, useRef, useState, type FormEvent, type ReactNode } from 'react'
import { workspaceError, workspaceRequest, type WorkspaceRun } from '../workspace.ts'
import { decodeSkillWorkflowEnvelope } from '../skillWorkflow.ts'
import { ApiError, fetchSkillWorkflow, fetchSkillWorkflows } from '../api.ts'
import { InvalidSkillWorkflowResponse, validSkillWorkflowRunId, validSkillWorkflowStepId } from '../skillWorkflow.ts'
import type {
  SkillWorkflowEnvelope,
  SkillWorkflowProjection,
  SkillWorkflowReviewFinding,
  SkillWorkflowRunSummary,
  SkillWorkflowRunState,
  SkillWorkflowStep,
  SkillWorkflowStepState,
} from '../types.ts'

type WorkflowRoute =
  | { kind: 'list' }
  | { kind: 'run'; runId: string }
  | { kind: 'step'; runId: string; stepId: string }

type ListState =
  | { kind: 'loading' }
  | { kind: 'available'; runs: SkillWorkflowRunSummary[]; nextCursor: string | null }
  | { kind: 'unavailable'; message: string }
  | { kind: 'error'; message: string }

type RunLookup =
  | { kind: 'loading'; runId: string }
  | { kind: 'result'; runId: string; envelope: SkillWorkflowEnvelope }
  | { kind: 'error'; runId: string; message: string }

const RUN_LABELS: Record<SkillWorkflowRunState, string> = {
  pending: 'Pending',
  running: 'Running',
  succeeded: 'Succeeded',
  completed_with_omissions: 'Completed with omissions',
  failed: 'Failed',
  blocked: 'Blocked',
  cancelled: 'Cancelled',
}

const STEP_LABELS: Record<SkillWorkflowStepState, string> = {
  pending: 'Pending',
  running: 'Running',
  succeeded: 'Succeeded',
  failed: 'Failed',
  blocked: 'Blocked',
  cancelled: 'Cancelled',
}

const FILTER_OPTIONS = [
  { value: 'all', label: 'All statuses' },
  ...Object.entries(RUN_LABELS).map(([value, label]) => ({ value, label })),
]

const stateTone = (state: SkillWorkflowRunState | SkillWorkflowStepState): string => {
  if (state === 'succeeded') return 'is-ok'
  if (state === 'failed' || state === 'blocked') return 'is-bad'
  if (state === 'running') return 'is-active'
  if (state === 'completed_with_omissions') return 'is-warn'
  return 'is-neutral'
}

const displayState = (state: string): string => state.replaceAll('_', ' ')

const reviewTone = (status: string): string => {
  if (['succeeded', 'accepted'].includes(status)) return 'is-ok'
  if (['failed', 'blocked', 'rejected', 'incomplete', 'error'].includes(status)) return 'is-bad'
  if (status === 'warning') return 'is-warn'
  if (['running', 'pending'].includes(status)) return 'is-active'
  return 'is-neutral'
}

const statusIcon = (state: SkillWorkflowRunState | SkillWorkflowStepState): ReactNode => {
  if (state === 'succeeded') return <CheckCircleOutlined />
  if (state === 'running' || state === 'pending') return <ClockCircleOutlined />
  if (state === 'failed' || state === 'blocked') return <StopOutlined />
  return <ExclamationCircleOutlined />
}

const findingLocation = (finding: SkillWorkflowReviewFinding): string | null => {
  if (!finding.path) return null
  if (finding.start_line === null) return finding.path
  const end = finding.end_line !== null && finding.end_line !== finding.start_line ? `–${finding.end_line}` : ''
  return `${finding.path}:${finding.start_line}${end}`
}

const unavailableMessage = (reason: string): string => {
  if (reason === 'native_workflow_not_configured') return 'Native workflow lookup is not configured for this repository.'
  if (reason === 'native_workflow_binding_unavailable') return 'The configured native workflow binding is unavailable.'
  if (reason === 'native_workflow_binding_replaced') return 'The configured native repository identity has changed.'
  return 'Native workflow lookup is currently unavailable.'
}

function requestError(error: unknown): string {
  if (error instanceof ApiError) {
    if (error.status === 400) return 'The hub rejected this workflow request as malformed.'
    if (error.status === 401 || error.status === 403) return 'This token is not authorized to read workflows.'
    if (error.status === 404) return 'No retained run with this ID was found for this repository.'
    if (error.status === 503) return 'Workflow lookup is temporarily unavailable or busy.'
    return `Workflow lookup failed (HTTP ${error.status}).`
  }
  if (error instanceof InvalidSkillWorkflowResponse || error instanceof SyntaxError) return 'The hub returned malformed workflow data.'
  if (error instanceof TypeError) return 'The workflow lookup could not reach the hub.'
  return 'The hub returned invalid or oversized workflow data.'
}

function nextAction(projection: SkillWorkflowProjection): { tone: string; title: string; body: string } {
  const { run, present_applicability: applicability } = projection
  const landing = projection.present_landing_binding
  const findings = run.steps.reduce(
    (count, step) => count + (step.review?.findings.filter((finding) => finding.severity !== 'note').length ?? 0),
    0,
  )
  if (applicability.state !== 'current') {
    return {
      tone: applicability.state === 'stale' ? 'is-warn' : 'is-bad',
      title: applicability.state === 'stale' ? 'Rerun on the current source' : 'Restore the native binding',
      body: applicability.reason ?? 'The current native source binding cannot be verified. This run cannot support a new landing.',
    }
  }
  if (landing && landing.state !== 'current') {
    return {
      tone: landing.state === 'stale' ? 'is-warn' : 'is-bad',
      title: landing.state === 'stale' ? 'Refresh the landing binding' : 'Inspect the landing binding',
      body: landing.reason ?? 'The current Change, review context, policy, or protected target cannot be verified for this run.',
    }
  }
  if (findings > 0 || run.state === 'failed' || run.state === 'blocked') {
    return {
      tone: 'is-bad',
      title: findings > 0 ? `Resolve ${findings} review finding${findings === 1 ? '' : 's'}` : 'Fix the failing required step',
      body: 'Apply the fixes, checkpoint a fresh immutable revision, and run the workflow again.',
    }
  }
  if (run.state === 'pending' || run.state === 'running') {
    return { tone: 'is-active', title: 'Workflow is still in progress', body: 'Refresh after the local controller finishes or recovers its current step.' }
  }
  if (run.state === 'completed_with_omissions') {
    return { tone: 'is-warn', title: 'Review optional omissions', body: 'Required steps passed, but omitted work remains visible.' }
  }
  if (run.state === 'cancelled') {
    return { tone: 'is-warn', title: 'Start a fresh run', body: 'This cancelled run cannot establish a current passing result.' }
  }
  if (!landing) {
    return { tone: 'is-ok', title: 'Recorded source checks passed', body: 'This source-only workflow is not a native landing decision.' }
  }
  return { tone: 'is-ok', title: 'Checks and bindings are current', body: 'Native landing still revalidates this exact immutable revision.' }
}

const safeDecode = (value: string): string | null => {
  try {
    return decodeURIComponent(value)
  } catch {
    return null
  }
}

const routeFromHash = (): WorkflowRoute => {
  const parts = window.location.hash.slice(1).split('/')
  if (parts[0] !== 'workflows' || parts.length === 1) return { kind: 'list' }
  if (parts[1] !== 'runs' || !parts[2]) return { kind: 'list' }
  const runId = safeDecode(parts[2])
  if (!runId || !validSkillWorkflowRunId(runId)) return { kind: 'list' }
  if (parts.length === 3) return { kind: 'run', runId }
  if (parts[3] !== 'steps' || !parts[4]) return { kind: 'run', runId }
  const stepId = safeDecode(parts[4])
  return stepId && validSkillWorkflowStepId(stepId) ? { kind: 'step', runId, stepId } : { kind: 'run', runId }
}

const runHref = (runId: string): string => window.location.hash.startsWith('#workspace/workflow/') ? window.location.hash.split('/').slice(0, 4).join('/') : `#workflows/runs/${encodeURIComponent(runId)}`
const stepHref = (runId: string, stepId: string): string => `${runHref(runId)}/steps/${encodeURIComponent(stepId)}`

function WorkflowBreadcrumb({ repository, route }: { repository: string; route: WorkflowRoute }) {
  const items: { title: ReactNode }[] = [
    { title: <span className="workflow-breadcrumb-repo">{repository}</span> },
    { title: route.kind === 'list' ? 'Workflows' : <a href="#workflows">Workflows</a> },
  ]
  if (route.kind === 'run') items.push({ title: <code>{route.runId}</code> })
  if (route.kind === 'step') {
    items.push({ title: <a href={runHref(route.runId)}><code>{route.runId}</code></a> })
    items.push({ title: <code>{route.stepId}</code> })
  }
  return <Breadcrumb className="workflow-breadcrumb" items={items} />
}

function StateTag({ state }: { state: SkillWorkflowRunState | SkillWorkflowStepState }) {
  const label = state in RUN_LABELS ? RUN_LABELS[state as SkillWorkflowRunState] : STEP_LABELS[state as SkillWorkflowStepState]
  return <Tag bordered={false} className={`workflow-state ${stateTone(state)}`}>{label}</Tag>
}

function ManualLookup() {
  const [input, setInput] = useState('')
  const [error, setError] = useState<string | null>(null)
  const submit = (event: FormEvent) => {
    event.preventDefault()
    const runId = input.trim()
    if (!validSkillWorkflowRunId(runId)) {
      setError('Use 1–64 letters, numbers, dots, underscores, or hyphens.')
      return
    }
    window.location.hash = runHref(runId).slice(1)
  }
  return (
    <Collapse
      ghost
      className="workflow-manual"
      items={[{
        key: 'lookup',
        label: 'Open an older run by ID',
        children: (
          <form onSubmit={submit}>
            <div className="workflow-manual-controls">
              <Input
                value={input}
                maxLength={64}
                autoComplete="off"
                spellCheck={false}
                placeholder="local-run-1"
                status={error ? 'error' : undefined}
                aria-label="Workflow run ID"
                onChange={(event) => {
                  setInput(event.target.value)
                  setError(null)
                }}
              />
              <Button htmlType="submit">Open run</Button>
            </div>
            {error && <p className="workflow-field-error" role="alert">{error}</p>}
          </form>
        ),
      }]}
    />
  )
}

function WorkflowList({ token }: { token: string }) {
  const [cursorStack, setCursorStack] = useState<(string | null)[]>([null])
  const [state, setState] = useState<ListState>({ kind: 'loading' })
  const [query, setQuery] = useState('')
  const [status, setStatus] = useState('all')
  const [refresh, setRefresh] = useState(0)
  const currentCursor = cursorStack[cursorStack.length - 1] ?? null
  const active = useRef<AbortController | null>(null)

  useEffect(() => {
    const controller = new AbortController()
    active.current?.abort()
    active.current = controller
    void fetchSkillWorkflows(token, controller.signal, currentCursor ?? undefined).then((envelope) => {
      if (active.current !== controller) return
      if (envelope.availability === 'unavailable') {
        setState({ kind: 'unavailable', message: unavailableMessage(envelope.reason) })
      } else {
        setState({ kind: 'available', runs: envelope.runs, nextCursor: envelope.next_cursor })
      }
    }).catch((error: unknown) => {
      if (!controller.signal.aborted && active.current === controller) setState({ kind: 'error', message: requestError(error) })
    })
    return () => controller.abort()
  }, [currentCursor, refresh, token])

  const filtered = useMemo(() => {
    if (state.kind !== 'available') return []
    const needle = query.trim().toLocaleLowerCase()
    return state.runs.filter((run) => {
      const matchesStatus = status === 'all' || run.state === status
      const matchesQuery = needle === '' || run.name.toLocaleLowerCase().includes(needle) || run.run_id.toLocaleLowerCase().includes(needle) || run.revision.includes(needle)
      return matchesStatus && matchesQuery
    })
  }, [query, state, status])

  return (
    <section className="workflow-list-page" aria-labelledby="workflow-list-heading">
      <div className="workflow-page-heading">
        <div>
          <h1 id="workflow-list-heading">Workflow runs</h1>
          <p>Tests and structured reviews retained for this repository.</p>
        </div>
        <Tag bordered={false} className="workflow-readonly">Read only</Tag>
      </div>

      <div className="workflow-toolbar">
        <Input
          allowClear
          prefix={<SearchOutlined />}
          value={query}
          placeholder="Filter this page"
          aria-label="Filter workflow runs on this page"
          onChange={(event) => setQuery(event.target.value)}
        />
        <Select
          value={status}
          options={FILTER_OPTIONS}
          aria-label="Filter workflow status on this page"
          onChange={setStatus}
        />
        <Button
          icon={<ReloadOutlined />}
          aria-label="Refresh workflow runs"
          onClick={() => {
            setState({ kind: 'loading' })
            setRefresh((value) => value + 1)
          }}
        />
      </div>
      <p className="workflow-filter-note">Search and status filters apply to this page of retained runs.</p>

      {state.kind === 'loading' && <div className="workflow-loading" aria-label="Loading workflow runs"><Skeleton active paragraph={{ rows: 8 }} /></div>}
      {state.kind === 'unavailable' && <Alert showIcon type="warning" message="Workflows unavailable" description={state.message} />}
      {state.kind === 'error' && <Alert showIcon type="error" message="Could not load workflows" description={state.message} action={<Button size="small" onClick={() => setRefresh((value) => value + 1)}>Try again</Button>} />}
      {state.kind === 'available' && (
        <>
          <Table<SkillWorkflowRunSummary>
            className="workflow-table"
            rowKey="run_id"
            dataSource={filtered}
            pagination={false}
            tableLayout="fixed"
            scroll={{ x: 720 }}
            locale={{
              emptyText: <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={state.runs.length === 0 ? 'No retained workflow runs yet.' : 'No runs on this page match the filters.'} />,
            }}
            columns={[
              {
                title: 'Status', dataIndex: 'state', key: 'state', width: 170,
                render: (value: SkillWorkflowRunState) => <StateTag state={value} />,
              },
              {
                title: 'Workflow', dataIndex: 'name', key: 'name',
                render: (value: string, run) => <a className="workflow-run-link" href={runHref(run.run_id)}>{value}</a>,
              },
              {
                title: 'Source', dataIndex: 'revision', key: 'revision', width: 150,
                render: (value: string) => <code title={value}>{value.slice(0, 12)}</code>,
              },
              {
                title: 'Run ID', dataIndex: 'run_id', key: 'run_id', width: 220,
                render: (value: string) => <code className="workflow-run-id" title={value}>{value}</code>,
              },
            ]}
            onRow={(run) => ({
              onClick: () => { window.location.hash = runHref(run.run_id).slice(1) },
              onKeyDown: (event) => {
                if (event.key === 'Enter') window.location.hash = runHref(run.run_id).slice(1)
              },
              tabIndex: 0,
              'aria-label': `Open workflow ${run.name}, ${RUN_LABELS[run.state]}`,
            })}
          />
          <div className="workflow-pagination" aria-label="Workflow run pages">
            <Button
              disabled={cursorStack.length === 1}
              onClick={() => {
                setState({ kind: 'loading' })
                setCursorStack((stack) => stack.slice(0, -1))
              }}
            >
              Previous
            </Button>
            <span>Page {cursorStack.length}</span>
            <Button
              type="primary"
              disabled={state.nextCursor === null}
              onClick={() => {
                if (state.nextCursor) {
                  setState({ kind: 'loading' })
                  setCursorStack((stack) => [...stack, state.nextCursor])
                }
              }}
            >
              Next
            </Button>
          </div>
        </>
      )}
      <ManualLookup />
    </section>
  )
}

function useRun(runId: string, token: string): [RunLookup, () => void] {
  const [state, setState] = useState<RunLookup>({ kind: 'loading', runId })
  const [refresh, setRefresh] = useState(0)
  useEffect(() => {
    const controller = new AbortController()
    void fetchSkillWorkflow(runId, token, controller.signal).then((envelope) => {
      if (!controller.signal.aborted) setState({ kind: 'result', runId, envelope })
    }).catch((error: unknown) => {
      if (!controller.signal.aborted) setState({ kind: 'error', runId, message: requestError(error) })
    })
    return () => controller.abort()
  }, [refresh, runId, token])
  const current = state.runId === runId ? state : { kind: 'loading' as const, runId }
  return [current, () => {
    setState({ kind: 'loading', runId })
    setRefresh((value) => value + 1)
  }]
}

function workflowLevels(steps: SkillWorkflowStep[]): SkillWorkflowStep[][] {
  const byId = new Map(steps.map((step) => [step.id, step]))
  const depths = new Map<string, number>()
  const depth = (step: SkillWorkflowStep, visiting = new Set<string>()): number => {
    const known = depths.get(step.id)
    if (known !== undefined) return known
    if (visiting.has(step.id)) return 0
    visiting.add(step.id)
    const value = step.needs.length === 0 ? 0 : Math.max(...step.needs.map((id) => depth(byId.get(id)!, visiting))) + 1
    visiting.delete(step.id)
    depths.set(step.id, value)
    return value
  }
  const levels: SkillWorkflowStep[][] = []
  for (const step of steps) {
    const level = depth(step)
    ;(levels[level] ??= []).push(step)
  }
  return levels
}

function StepGraph({ runId, steps }: { runId: string; steps: SkillWorkflowStep[] }) {
  const levels = workflowLevels(steps)
  return (
    <section className="workflow-graph-section" aria-labelledby="workflow-graph-heading">
      <div className="workflow-section-heading">
        <div>
          <h2 id="workflow-graph-heading">Steps</h2>
          <p>Select a step to inspect its review and retained evidence.</p>
        </div>
        <span>{steps.length} total</span>
      </div>
      <div className="workflow-graph" aria-label="Workflow dependency graph">
        {levels.map((level, levelIndex) => (
          <div className="workflow-graph-level" key={levelIndex}>
            <span className="workflow-level-label">Stage {levelIndex + 1}</span>
            <div className="workflow-level-nodes">
              {level.map((step) => (
                <a className={`workflow-node ${stateTone(step.state)}`} href={stepHref(runId, step.id)} key={step.id}>
                  <span className="workflow-node-icon" aria-hidden="true">{statusIcon(step.state)}</span>
                  <span className="workflow-node-copy">
                    <strong>{step.id}</strong>
                    <span>{STEP_LABELS[step.state]} · {step.required ? 'Required' : 'Optional'}</span>
                    {step.needs.length > 0 && <small>Needs {step.needs.join(', ')}</small>}
                  </span>
                </a>
              ))}
            </div>
          </div>
        ))}
      </div>
    </section>
  )
}

function RunSummary({ projection, refresh }: { projection: SkillWorkflowProjection; refresh: () => void }) {
  const { run, present_applicability: applicability, present_landing_binding: landing } = projection
  const action = nextAction(projection)
  return (
    <>
      <div className="workflow-page-heading workflow-run-heading">
        <div>
          <Button className="workflow-back" type="link" href={window.location.hash.startsWith('#workspace/') ? '#workspace/repositories' : '#workflows'} icon={<ArrowLeftOutlined />}>All runs</Button>
          <h1><code>{run.run_id}</code></h1>
          <p>Revision <code title={run.source.revision}>{run.source.revision.slice(0, 12)}</code> · policy epoch {run.source.policy_epoch}</p>
        </div>
        <div className="workflow-heading-actions">
          <StateTag state={run.state} />
          <Button icon={<ReloadOutlined />} aria-label="Refresh workflow run" onClick={refresh} />
        </div>
      </div>

      <section className={`workflow-message workflow-action ${action.tone}`} aria-labelledby="workflow-action-heading">
        {action.tone === 'is-ok' ? <CheckCircleOutlined /> : action.tone === 'is-active' ? <ClockCircleOutlined /> : <ExclamationCircleOutlined />}
        <span>
          <strong id="workflow-action-heading">{action.title}</strong>
          <span>{action.body}</span>
        </span>
      </section>

      <div className="workflow-bindings" aria-label="Current workflow bindings">
        <section>
          <span>Source freshness</span>
          <Tag bordered={false} className={`workflow-state ${applicability.state === 'current' ? 'is-active' : applicability.state === 'stale' ? 'is-warn' : 'is-bad'}`}>
            {displayState(applicability.state)}
          </Tag>
          <p>{applicability.reason ?? 'Repository, policy, and immutable revision match.'}</p>
        </section>
        <section>
          <span>Change and target</span>
          <Tag bordered={false} className={`workflow-state ${landing === null ? 'is-neutral' : landing.state === 'current' ? 'is-active' : landing.state === 'stale' ? 'is-warn' : 'is-bad'}`}>
            {landing === null ? 'Source only' : displayState(landing.state)}
          </Tag>
          <p>{landing === null ? 'No Change, review context, or target is declared.' : landing.reason ?? `${landing.change_id ?? 'Change'} · ${landing.target ?? 'target unavailable'}`}</p>
        </section>
        <section>
          <span>Native landing gate</span>
          <Tag bordered={false} className="workflow-state is-neutral">Not assessed</Tag>
          <p>This read-only view does not authorize landing.</p>
        </section>
      </div>

      <StepGraph runId={run.run_id} steps={run.steps} />
    </>
  )
}

function ReviewPanel({ step }: { step: SkillWorkflowStep }) {
  const review = step.review
  if (!review) return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No Claude review was recorded for this step." />
  return (
    <div className="workflow-step-panel">
      <div className="workflow-review-summary">
        <div>
          <span>Review status</span>
          <Tag bordered={false} className={`workflow-state ${reviewTone(review.status)}`}>{displayState(review.status)}</Tag>
        </div>
        <div><span>Requested model</span><code>{review.model_requested}</code></div>
        <div><span>Observed model</span><code>{review.model_observed ?? 'Not reported'}</code></div>
        <div><span>Stop</span><span>{review.stop ?? 'Not reported'}</span></div>
      </div>
      {review.unresolved.length > 0 && (
        <Alert type="error" showIcon message={`${review.unresolved.length} unresolved item${review.unresolved.length === 1 ? '' : 's'}`} description={review.unresolved.join(' · ')} />
      )}
      {review.findings.length === 0 ? (
        <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No structured findings were recorded." />
      ) : (
        <Collapse
          className="workflow-finding-collapse"
          items={review.findings.map((finding, index) => ({
            key: `${index}`,
            label: (
              <span className="workflow-finding-label">
                <Tag bordered={false} className={`workflow-severity ${reviewTone(finding.severity)}`}>{finding.severity}</Tag>
                <strong>{finding.title}</strong>
                {findingLocation(finding) && <code>{findingLocation(finding)}</code>}
              </span>
            ),
            children: <p>{finding.detail}</p>,
          }))}
        />
      )}
    </div>
  )
}

function CoveragePanel({ step }: { step: SkillWorkflowStep }) {
  const paths = step.review?.covered_paths ?? []
  if (paths.length === 0) return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No review coverage was recorded for this step." />
  return (
    <ul className="workflow-path-list" aria-label="Reviewed source paths">
      {paths.map((path) => <li key={path}><FileSearchOutlined /><code>{path}</code></li>)}
    </ul>
  )
}

function EvidencePanel({ step }: { step: SkillWorkflowStep }) {
  return (
    <div className="workflow-step-panel">
      {step.detail && <Alert type={step.state === 'failed' || step.state === 'blocked' ? 'error' : 'info'} showIcon message="Recorded step detail" description={step.detail} />}
      {step.observations.length > 0 ? (
        <Table
          className="workflow-evidence-table"
          size="small"
          pagination={false}
          rowKey="path"
          dataSource={step.observations}
          scroll={{ x: 640 }}
          columns={[
            { title: 'Observed path', dataIndex: 'path', render: (value: string) => <code>{value}</code> },
            { title: 'SHA-256', dataIndex: 'sha256', width: 170, render: (value: string) => <code title={value}>{value.slice(0, 16)}…</code> },
            { title: 'Bytes', dataIndex: 'bytes', width: 110, render: (value: number) => value.toLocaleString() },
          ]}
        />
      ) : <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="No source observations were recorded." />}
      {step.execution && (
        <dl className="workflow-execution-grid">
          <div><dt>Attempt</dt><dd><code>{step.execution.attempt_id}</code></dd></div>
          <div><dt>Generation</dt><dd>{step.execution.generation}</dd></div>
          <div><dt>State</dt><dd>{displayState(step.execution.state)}</dd></div>
          <div><dt>Outcome</dt><dd>{step.execution.outcome ? displayState(step.execution.outcome) : 'Not reported'}</dd></div>
          <div><dt>Record</dt><dd><code title={step.execution.record_digest ?? undefined}>{step.execution.record_digest ? `${step.execution.record_digest.slice(0, 16)}…` : 'Not retained'}</code></dd></div>
        </dl>
      )}
      {step.execution?.failure && <Alert type="error" showIcon message="Command failure" description={step.execution.failure} />}
      <p className="workflow-retention-note">Only structured retained evidence is shown. Raw command and provider output is not available in this view.</p>
    </div>
  )
}

function StepDetail({ runId, step }: { runId: string; step: SkillWorkflowStep }) {
  const reviewCount = step.review?.findings.length ?? 0
  return (
    <>
      <div className="workflow-page-heading workflow-step-heading">
        <div>
          <Button className="workflow-back" type="link" href={runHref(runId)} icon={<ArrowLeftOutlined />}>Run overview</Button>
          <h1><code>{step.id}</code></h1>
          <p>{step.required ? 'Required step' : 'Optional step'}{step.needs.length > 0 ? ` · depends on ${step.needs.join(', ')}` : ' · no dependencies'}</p>
        </div>
        <StateTag state={step.state} />
      </div>
      <Tabs
        key={step.id}
        className="workflow-detail-tabs"
        defaultActiveKey={step.review ? 'review' : 'evidence'}
        items={[
          { key: 'review', label: `Review findings (${reviewCount})`, children: <ReviewPanel step={step} /> },
          { key: 'coverage', label: `Coverage (${step.review?.covered_paths.length ?? 0})`, children: <CoveragePanel step={step} /> },
          { key: 'evidence', label: `Evidence & tests (${step.observations.length})`, children: <EvidencePanel step={step} /> },
        ]}
      />
    </>
  )
}

function WorkflowRun({ token, route }: { token: string; route: Extract<WorkflowRoute, { kind: 'run' | 'step' }> }) {
  const [lookup, refresh] = useRun(route.runId, token)
  if (lookup.kind === 'loading') return <div className="workflow-loading" aria-label="Loading workflow run"><Skeleton active paragraph={{ rows: 10 }} /></div>
  if (lookup.kind === 'error') return <Alert showIcon type="error" message="Could not load this run" description={lookup.message} action={<Button size="small" onClick={refresh}>Try again</Button>} />
  if (lookup.envelope.availability === 'unavailable') return <Alert showIcon type="warning" message="Workflow unavailable" description={unavailableMessage(lookup.envelope.reason)} />
  const projection = lookup.envelope.projection
  if (route.kind === 'step') {
    const step = projection.run.steps.find((candidate) => candidate.id === route.stepId)
    if (!step) return <Alert showIcon type="error" message="Step not found" description="This retained run does not contain the requested step." action={<Button size="small" href={runHref(route.runId)}>Open run</Button>} />
    return <StepDetail runId={route.runId} step={step} />
  }
  return <RunSummary projection={projection} refresh={refresh} />
}

export function SkillWorkflow({ token, repository }: { token: string; repository: string | null }) {
  const [route, setRoute] = useState<WorkflowRoute>(routeFromHash)
  useEffect(() => {
    const onHashChange = () => setRoute(routeFromHash())
    window.addEventListener('hashchange', onHashChange)
    return () => window.removeEventListener('hashchange', onHashChange)
  }, [])

  if (!repository) {
    return <section className="workflow-page"><Skeleton active paragraph={{ rows: 8 }} /></section>
  }
  return (
    <section className="workflow-page">
      <WorkflowBreadcrumb repository={repository} route={route} />
      {route.kind === 'list' ? <WorkflowList token={token} /> : <WorkflowRun token={token} route={route} />}
    </section>
  )
}

/** Reuses the retained workflow drilldown through a cookie-authorized workspace scope. */
export function WorkspaceWorkflow({ repository, workspaceRunId }: { repository: string; workspaceRunId: string }) {
  const [lookup, setLookup] = useState<RunLookup>({ kind: 'loading', runId: workspaceRunId })
  const [refresh, setRefresh] = useState(0)
  const [stepId, setStepId] = useState<string | null>(() => window.location.hash.split('/')[4] === 'steps' ? safeDecode(window.location.hash.split('/')[5] ?? '') : null)
  useEffect(() => { const changed = () => { const parts = window.location.hash.split('/'); setStepId(parts[4] === 'steps' ? safeDecode(parts[5] ?? '') : null) }; window.addEventListener('hashchange', changed); return () => window.removeEventListener('hashchange', changed) }, [])
  useEffect(() => {
    const controller = new AbortController()
    const path = `/repositories/${encodeURIComponent(repository)}/runs/${encodeURIComponent(workspaceRunId)}`
    void workspaceRequest<WorkspaceRun>(path, { signal: controller.signal }).then(async run => {
      const value = await workspaceRequest<unknown>(`${path}/workflow`, { signal: controller.signal })
      const envelope = decodeSkillWorkflowEnvelope(value, run.native_run_id ?? '')
      if (!controller.signal.aborted) setLookup({ kind: 'result', runId: workspaceRunId, envelope })
    }).catch(error => { if (!controller.signal.aborted) setLookup({ kind: 'error', runId: workspaceRunId, message: workspaceError(error) }) })
    return () => controller.abort()
  }, [repository, workspaceRunId, refresh])
  const reload = () => { setLookup({ kind: 'loading', runId: workspaceRunId }); setRefresh(value => value + 1) }
  if (lookup.kind === 'loading') return <Skeleton active paragraph={{ rows: 10 }} />
  if (lookup.kind === 'error') return <Alert type="warning" showIcon message="Retained workflow is not available" description={lookup.message} action={<Button onClick={reload}>Retry</Button>} />
  if (lookup.envelope.availability === 'unavailable') return <Alert type="warning" showIcon message="Retained workflow is not available yet" description="A queued run may not have written its native journal. Refresh after execution starts." action={<Button onClick={reload}>Refresh</Button>} />
  const projection = lookup.envelope.projection
  const step = stepId ? projection.run.steps.find(step => step.id === stepId) : null
  return <>
    {stepId ? step ? <StepDetail runId={projection.run.run_id} step={step} /> : <Alert type="error" message="This retained step was not found." /> : <RunSummary projection={projection} refresh={reload} />}
  </>
}
