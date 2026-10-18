import {
  CheckCircleOutlined,
  ClockCircleOutlined,
  ExclamationCircleOutlined,
  FileSearchOutlined,
  ReloadOutlined,
  StopOutlined,
} from '@ant-design/icons'
import { Button, Input, Tag } from 'antd'
import { useEffect, useRef, useState, type FormEvent } from 'react'
import { ApiError, fetchSkillWorkflow } from '../api.ts'
import { InvalidSkillWorkflowResponse, validSkillWorkflowRunId } from '../skillWorkflow.ts'
import type { SkillWorkflowEnvelope, SkillWorkflowRunState, SkillWorkflowStepState } from '../types.ts'
import { Section } from './bits.tsx'

type Lookup =
  | { kind: 'idle' }
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

const stateTone = (state: SkillWorkflowRunState | SkillWorkflowStepState): string => {
  if (state === 'succeeded') return 'is-ok'
  if (state === 'failed' || state === 'blocked') return 'is-bad'
  if (state === 'running') return 'is-active'
  if (state === 'completed_with_omissions') return 'is-warn'
  return 'is-neutral'
}

const unavailableMessage = (reason: string): string => {
  if (reason === 'native_workflow_not_configured') return 'Native workflow lookup is not configured for this repository.'
  if (reason === 'native_workflow_binding_unavailable') return 'The configured native workflow binding is unavailable.'
  if (reason === 'native_workflow_binding_replaced') return 'The configured native repository identity has changed.'
  return 'Native workflow lookup is currently unavailable.'
}

function requestError(error: unknown): string {
  if (error instanceof ApiError) {
    if (error.status === 400) return 'The hub rejected this run ID as malformed.'
    if (error.status === 401 || error.status === 403) return 'This token is not authorized to read the run.'
    if (error.status === 404) return 'No retained run with this ID was found for this repository.'
    if (error.status === 503) return 'Workflow lookup is temporarily unavailable or busy.'
    return `Workflow lookup failed (HTTP ${error.status}).`
  }
  if (error instanceof InvalidSkillWorkflowResponse || error instanceof SyntaxError) return 'The hub returned malformed workflow data.'
  if (error instanceof TypeError) return 'The workflow lookup could not reach the hub.'
  return 'The hub returned invalid or oversized workflow data.'
}

function RunResult({ envelope }: { envelope: SkillWorkflowEnvelope }) {
  if (envelope.availability === 'unavailable') {
    return (
      <div className="workflow-message is-warn" role="status">
        <ExclamationCircleOutlined />
        <span>{unavailableMessage(envelope.reason)}</span>
      </div>
    )
  }
  const { run, present_applicability: applicability } = envelope.projection
  return (
    <div className="workflow-result">
      <section className="workflow-fact" aria-labelledby="workflow-history-heading">
        <div className="workflow-fact-head">
          <h3 id="workflow-history-heading">Recorded run</h3>
          <Tag bordered={false} className={`workflow-state ${stateTone(run.state)}`}>
            {RUN_LABELS[run.state]}
          </Tag>
        </div>
        <dl className="workflow-meta">
          <div>
            <dt>Run</dt>
            <dd><code>{run.run_id}</code></dd>
          </div>
          <div>
            <dt>Source revision</dt>
            <dd title={run.source.revision}><code>{run.source.revision.slice(0, 12)}</code></dd>
          </div>
          <div>
            <dt>Policy epoch</dt>
            <dd>{run.source.policy_epoch}</dd>
          </div>
        </dl>
        <ol className="workflow-steps" aria-label="Recorded workflow steps">
          {run.steps.map((step) => (
            <li key={step.id} className="workflow-step">
              <span className={`workflow-step-icon ${stateTone(step.state)}`} aria-hidden="true">
                {step.state === 'succeeded' ? <CheckCircleOutlined /> : step.state === 'running' || step.state === 'pending' ? <ClockCircleOutlined /> : step.state === 'failed' || step.state === 'blocked' ? <StopOutlined /> : <ExclamationCircleOutlined />}
              </span>
              <span className="workflow-step-body">
                <span className="workflow-step-head">
                  <code>{step.id}</code>
                  <span>{STEP_LABELS[step.state]} · {step.required ? 'Required' : 'Optional'}</span>
                </span>
                {step.observations.length > 0 && (
                  <span className="workflow-observations">
                    <strong>Source checks:</strong> {step.observations.map((item) => item.path).join(', ')}
                  </span>
                )}
                {step.detail && <span className="workflow-detail">{step.detail}</span>}
              </span>
            </li>
          ))}
        </ol>
      </section>

      <section className="workflow-fact" aria-labelledby="workflow-applicability-heading">
        <div className="workflow-fact-head">
          <h3 id="workflow-applicability-heading">Present applicability</h3>
          <Tag bordered={false} className={`workflow-state ${applicability.state === 'current' ? 'is-active' : applicability.state === 'stale' ? 'is-warn' : 'is-neutral'}`}>
            {applicability.state === 'current' ? 'Current binding' : applicability.state === 'stale' ? 'Stale binding' : 'Unavailable'}
          </Tag>
        </div>
        <p>{applicability.reason ?? 'The current repository, policy epoch, and immutable revision match this recorded run.'}</p>
      </section>

      <section className="workflow-fact workflow-admission" aria-labelledby="workflow-admission-heading">
        <div className="workflow-fact-head">
          <h3 id="workflow-admission-heading">Integration approval</h3>
          <Tag bordered={false} className="workflow-state is-neutral">Not assessed</Tag>
        </div>
        <p>This lookup does not approve, merge, deploy, or authorize the revision.</p>
      </section>
    </div>
  )
}

export function SkillWorkflow({ token, repository, index }: { token: string; repository: string | null; index: number }) {
  const [input, setInput] = useState('')
  const [lookup, setLookup] = useState<Lookup>({ kind: 'idle' })
  const active = useRef<AbortController | null>(null)

  const clear = () => {
    active.current?.abort()
    active.current = null
    setLookup({ kind: 'idle' })
  }

  // App remounts authenticated content for a new token, and Dashboard keys this
  // card by repository. Unmounting clears both local state and obsolete work.
  useEffect(() => () => active.current?.abort(), [])

  const submit = async (event?: FormEvent) => {
    event?.preventDefault()
    const runId = input.trim()
    clear()
    if (!validSkillWorkflowRunId(runId)) {
      setLookup({ kind: 'error', runId, message: 'Enter a run ID using 1–64 letters, numbers, dots, underscores, or hyphens.' })
      return
    }
    const controller = new AbortController()
    active.current = controller
    setLookup({ kind: 'loading', runId })
    try {
      const envelope = await fetchSkillWorkflow(runId, token, controller.signal)
      if (active.current === controller) setLookup({ kind: 'result', runId, envelope })
    } catch (error) {
      if (!controller.signal.aborted && active.current === controller) setLookup({ kind: 'error', runId, message: requestError(error) })
    } finally {
      if (active.current === controller) active.current = null
    }
  }

  return (
    <Section title="Skill workflow run" index={index} className="area-workflow">
      <p className="card-sub">Inspect retained state for this authenticated repository.</p>
      <form className="workflow-form" onSubmit={(event) => void submit(event)}>
        <label htmlFor="workflow-run-id">Run ID</label>
        <div className="workflow-controls">
          <Input
            id="workflow-run-id"
            value={input}
            maxLength={64}
            autoComplete="off"
            spellCheck={false}
            placeholder="local-run-1"
            disabled={!repository}
            onChange={(event) => {
              clear()
              setInput(event.target.value)
            }}
          />
          <Button htmlType="submit" type="primary" loading={lookup.kind === 'loading'} disabled={!repository || input.trim() === ''}>
            Lookup
          </Button>
          {lookup.kind === 'result' && (
            <Button aria-label="Refresh workflow run" icon={<ReloadOutlined />} onClick={() => void submit()} />
          )}
        </div>
      </form>
      <div className="workflow-output" aria-live="polite" aria-busy={lookup.kind === 'loading'}>
        {!repository && <div className="workflow-message"><FileSearchOutlined /><span>Waiting for authenticated repository details.</span></div>}
        {repository && lookup.kind === 'idle' && <div className="workflow-message"><FileSearchOutlined /><span>Enter a retained run ID to inspect it.</span></div>}
        {lookup.kind === 'loading' && <div className="workflow-message"><ClockCircleOutlined /><span>Looking up <code>{lookup.runId}</code>…</span></div>}
        {lookup.kind === 'error' && <div className="workflow-message is-bad" role="alert"><ExclamationCircleOutlined /><span>{lookup.message}</span></div>}
        {lookup.kind === 'result' && <RunResult envelope={lookup.envelope} />}
      </div>
    </Section>
  )
}
