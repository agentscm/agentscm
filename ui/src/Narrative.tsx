import { useEffect, useState, type ReactNode } from 'react'
import { fetchNarrative, validateNarrativeTask, type NarrativeSource, type NarrativeStatement, type NarrativeView } from './narrative.ts'
import { Box, Notice, Skeleton, localDate } from './ui.tsx'

type LoadState =
  | { kind: 'loading'; token: string; task: string | undefined }
  | { kind: 'error'; token: string; task: string | undefined; message: string }
  | { kind: 'ready'; token: string; task: string | undefined; view: NarrativeView }

const message = (error: unknown) => error instanceof Error ? error.message : 'Unknown error'
const shortCommit = (commit: string) => commit.slice(0, 12)

function SourceReference({ source }: { source: NarrativeSource }) {
  const label: ReactNode = source.work_id === null
    ? source.label
    : <a href={`#work/${encodeURIComponent(source.work_id)}`}>{source.label}</a>
  return (
    <span>
      {label}
      {source.pull_number !== null && <> · PR #{source.pull_number}</>}
      {source.commit !== null && <> · commit <code>{shortCommit(source.commit)}</code></>}
    </span>
  )
}

function SourceReferences({ references, sources }: { references: string[]; sources: Map<string, NarrativeSource> }) {
  if (references.length === 0) return null
  return (
    <div className="small muted">
      Sources: {references.map((reference, index) => (
        <span key={reference}>
          {index > 0 && ' · '}
          <SourceReference source={sources.get(reference)!} />
        </span>
      ))}
    </div>
  )
}

function Statements({ items, sources }: { items: NarrativeStatement[]; sources: Map<string, NarrativeSource> }) {
  if (items.length === 0) return <p className="muted">No cited statements were generated from this bounded source window.</p>
  return (
    <ul className="plain-list">
      {items.map((item, index) => (
        <li key={`${index}:${item.text}`}>
          <div>{item.text}</div>
          <SourceReferences references={item.references} sources={sources} />
        </li>
      ))}
    </ul>
  )
}

function WindowNotices({ view }: { view: NarrativeView }) {
  return (
    <>
      {!view.window.complete && (
        <Notice title="The source window is incomplete">
          Some retained records were outside this generated view. Treat missing detail as unknown, not quiet.
        </Notice>
      )}
      {view.warnings.length > 0 && (
        <Notice title="Generation warnings">
          <ul className="mb-0">{view.warnings.map((warning, index) => <li key={`${index}:${warning}`}>{warning}</li>)}</ul>
        </Notice>
      )}
    </>
  )
}

interface TaskControls {
  draftTask: string
  task: string | undefined
  error: string | null
  setDraftTask: (value: string) => void
  submit: () => void
  clear: () => void
}

function TaskRequest({ controls }: { controls: TaskControls }) {
  return (
    <form className="mb-3" onSubmit={(event) => { event.preventDefault(); controls.submit() }}>
      <label className="form-label" htmlFor="agent-briefing-task">Task to consider</label>
      <div className="input-group">
        <input
          id="agent-briefing-task"
          className="form-control"
          type="text"
          value={controls.draftTask}
          maxLength={500}
          aria-describedby="agent-briefing-task-help"
          placeholder="Optional task or change you are about to start"
          onChange={(event) => controls.setDraftTask(event.currentTarget.value)}
        />
        <button className="btn btn-outline-secondary" type="submit">Brief this task</button>
      </div>
      <div id="agent-briefing-task-help" className="form-text">
        Submitted text is used only for conditional implications. Typing does not make a request.
      </div>
      {controls.error !== null && <div className="small tone-text-bad" role="alert">{controls.error}</div>}
      {controls.task !== undefined && (
        <div className="small muted mt-2">
          Considering: {controls.task}{' '}
          <button className="btn btn-sm btn-link" type="button" onClick={controls.clear}>Show general briefing</button>
        </div>
      )}
    </form>
  )
}

function NarrativeBox({ tone, controls, children }: { tone: 'brand' | 'warn' | 'bad' | 'muted'; controls: TaskControls; children: ReactNode }) {
  return (
    <Box
      id="agent-briefing"
      icon="stars"
      title="Agent briefing"
      tone={tone}
      explain="Generated from a bounded AgentSCM source window. It is advisory and does not change Work or review positions."
    >
      <div className="text-break">
        <TaskRequest controls={controls} />
        {children}
      </div>
    </Box>
  )
}

function NarrativeReady({ view, controls }: { view: NarrativeView; controls: TaskControls }) {
  const sources = new Map(view.sources.map((source) => [source.id, source]))
  return (
    <NarrativeBox controls={controls} tone={view.window.complete && view.warnings.length === 0 ? 'brand' : 'warn'}>
      <WindowNotices view={view} />
      <p><strong>{view.output!.headline}</strong></p>
      <SourceReferences references={view.output!.headline_references} sources={sources} />
      <p className="small muted">
        Generated by {view.generated_by ?? 'connected agent'} · as of {view.generated_at === null ? 'time not reported' : localDate(view.generated_at)}
      </p>
      <h3>Reported facts</h3>
      <Statements items={view.output!.facts} sources={sources} />
      <h3>What this may mean for your work</h3>
      <Statements items={view.output!.implications} sources={sources} />
      <p className="small muted">
        This agent-generated narrative summarizes reported records. It does not authorize work, approve changes, or establish that conflicts are clear.
      </p>
      <details className="disclosure">
        <summary>Briefing scope</summary>
        <dl className="tech">
          <dt>Subscriber</dt><dd><code>{view.window.subscriber}</code></dd>
          <dt>Work events</dt><dd><code>{view.window.event_from}</code> → <code>{view.window.event_through}</code></dd>
          <dt>Merge observations</dt><dd><code>{view.window.merge_from}</code> → <code>{view.window.merge_through}</code></dd>
          <dt>Digest</dt><dd><code className="text-break">{view.digest}</code></dd>
        </dl>
      </details>
    </NarrativeBox>
  )
}

export function NarrativePanel({ token }: { token: string }) {
  const [draftTask, setDraftTask] = useState('')
  const [task, setTask] = useState<string | undefined>()
  const [taskError, setTaskError] = useState<string | null>(null)
  const [retry, setRetry] = useState(0)
  const [state, setState] = useState<LoadState>({ kind: 'loading', token, task })

  useEffect(() => {
    let active = true
    let controller: AbortController | null = null
    let timer: number | null = null
    let nextWhenVisible: (() => void) | null = null
    const clearTimer = () => {
      if (timer !== null) window.clearTimeout(timer)
      timer = null
    }
    const removeVisibilityListener = () => {
      if (nextWhenVisible !== null) document.removeEventListener('visibilitychange', nextWhenVisible)
      nextWhenVisible = null
    }
    const schedule = (afterMs: number) => {
      clearTimer()
      removeVisibilityListener()
      timer = window.setTimeout(() => {
        timer = null
        if (!active) return
        if (document.visibilityState === 'hidden') {
          nextWhenVisible = () => {
            if (document.visibilityState !== 'visible' || !active) return
            removeVisibilityListener()
            void load()
          }
          document.addEventListener('visibilitychange', nextWhenVisible)
          return
        }
        void load()
      }, afterMs)
    }
    const load = async () => {
      controller?.abort()
      controller = new AbortController()
      try {
        const view = await fetchNarrative(token, controller.signal, { subscriber: 'human', ...(task === undefined ? {} : { task }) })
        if (!active || controller.signal.aborted) return
        setState({ kind: 'ready', token, task, view })
        if (view.status === 'pending' || view.status === 'generating') schedule(15_000)
        else schedule(30_000)
      } catch (error) {
        if (active && !controller.signal.aborted) {
          setState({ kind: 'error', token, task, message: message(error) })
          schedule(30_000)
        }
      }
    }

    void load()
    return () => {
      active = false
      controller?.abort()
      clearTimer()
      removeVisibilityListener()
    }
  }, [retry, task, token])

  const submitTask = () => {
    if (draftTask.length === 0) {
      setTaskError(null)
      setTask(undefined)
      return
    }
    try {
      setTask(validateNarrativeTask(draftTask))
      setTaskError(null)
    } catch {
      setTaskError('Use 500 UTF-8 bytes or fewer and ordinary visible text.')
    }
  }
  const controls: TaskControls = {
    draftTask,
    task,
    error: taskError,
    setDraftTask,
    submit: submitTask,
    clear: () => { setDraftTask(''); setTaskError(null); setTask(undefined) },
  }
  const current: LoadState = state.token === token && state.task === task ? state : { kind: 'loading', token, task }
  if (current.kind === 'loading') {
    return <NarrativeBox controls={controls} tone="muted"><Skeleton rows={2} /></NarrativeBox>
  }
  if (current.kind === 'error') {
    return (
      <NarrativeBox controls={controls} tone="bad">
        <Notice tone="bad" title="Agent briefing unavailable" action={
          <button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => {
            setState({ kind: 'loading', token, task })
            setRetry((value) => value + 1)
          }}>Retry briefing</button>
        }>
          The generated view could not be validated. This is not an empty result or an all-clear. {current.message}
        </Notice>
      </NarrativeBox>
    )
  }
  if (current.view.status === 'pending') {
    return (
      <NarrativeBox controls={controls} tone="muted">
        <WindowNotices view={current.view} />
        <p>Waiting for a connected agent to generate this briefing.</p>
        <p className="small muted mb-0">The dashboard cannot launch a model or provider. The reported catch-up remains available below.</p>
      </NarrativeBox>
    )
  }
  if (current.view.status === 'generating') {
    return (
      <NarrativeBox controls={controls} tone="brand">
        <WindowNotices view={current.view} />
        <p>An eligible connected agent is generating this briefing.</p>
        <p className="small muted mb-0">This page checks for the submitted result while visible. The reported catch-up remains available below.</p>
      </NarrativeBox>
    )
  }
  if (current.view.status === 'failed') {
    return (
      <NarrativeBox controls={controls} tone="bad">
        <WindowNotices view={current.view} />
        <Notice tone="bad" title="Agent briefing generation failed">
          No generated briefing is available for this window. This page keeps checking for recovery; an eligible connected agent is still required. This is not an empty result or an all-clear.
        </Notice>
      </NarrativeBox>
    )
  }
  return <NarrativeReady view={current.view} controls={controls} />
}
