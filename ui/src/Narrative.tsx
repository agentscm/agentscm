import { ApiError } from './api.ts'
import { useEffect, useState, type ReactNode } from 'react'
import { fetchNarrative, validateNarrativeTask, type NarrativeSource, type NarrativeStatement, type NarrativeView } from './narrative.ts'
import { Ago, Icon, Notice, Skeleton } from './ui.tsx'

type LoadState =
  | { kind: 'loading'; token: string; task: string | undefined }
  | { kind: 'error'; token: string; task: string | undefined; message: string }
  | { kind: 'absent'; token: string; task: string | undefined }
  | { kind: 'ready'; token: string; task: string | undefined; view: NarrativeView }

const message = (error: unknown) => error instanceof Error ? error.message : 'Unknown error'
const shortCommit = (commit: string) => commit.slice(0, 12)

/** Sources are numbered in the order the briefing first cites them, like footnotes. */
function citationNumbers(view: NarrativeView) {
  const order = new Map<string, number>()
  const cite = (refs: string[]) => { for (const r of refs) if (!order.has(r)) order.set(r, order.size + 1) }
  cite(view.output?.headline_references ?? [])
  for (const s of [...(view.output?.facts ?? []), ...(view.output?.implications ?? [])]) cite(s.references)
  return order
}

function SourceLabel({ source }: { source: NarrativeSource }) {
  return (
    <>
      {source.work_id === null ? source.label : <a href={`#work/${encodeURIComponent(source.work_id)}`}>{source.label}</a>}
      {source.pull_number !== null && <> · PR #{source.pull_number}</>}
      {source.commit !== null && <> · <code>{shortCommit(source.commit)}</code></>}
    </>
  )
}

/** Footnote markers after a statement; each names its source for pointers and screen readers. */
function Cites({ refs, numbers, sources }: { refs: string[]; numbers: Map<string, number>; sources: Map<string, NarrativeSource> }) {
  if (refs.length === 0) return null
  return (
    <span className="brief-cites">
      {refs.map((r) => (
        <a key={r} href={`#brief-src-${numbers.get(r)}`} className="brief-cite" title={sources.get(r)?.label} aria-label={`Source ${numbers.get(r)}: ${sources.get(r)?.label ?? 'unknown'}`} onClick={(e) => {
          e.preventDefault()
          document.getElementById(`brief-src-${numbers.get(r)}`)?.scrollIntoView({ block: 'nearest' })
        }}>
          {numbers.get(r)}
        </a>
      ))}
    </span>
  )
}

function Statements({ items, numbers, sources }: { items: NarrativeStatement[]; numbers: Map<string, number>; sources: Map<string, NarrativeSource> }) {
  if (items.length === 0) return <p className="muted small mb-0">No cited statements were generated from this bounded source window.</p>
  return (
    <ul className="brief-list">
      {items.map((item, index) => (
        <li key={`${index}:${item.text}`}>
          {item.text}
          <Cites refs={item.references} numbers={numbers} sources={sources} />
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

/** Ask for the briefing through the lens of one task; nothing is sent until it is submitted. */
function TaskRequest({ controls }: { controls: TaskControls }) {
  return (
    <form className="brief-task" onSubmit={(event) => { event.preventDefault(); controls.submit() }}>
      <label className="visually-hidden" htmlFor="agent-briefing-task">Task to consider</label>
      <div className="brief-task-field">
        <Icon name="crosshair" />
        <input
          id="agent-briefing-task"
          type="text"
          value={controls.draftTask}
          maxLength={500}
          aria-describedby="agent-briefing-task-help"
          placeholder="What are you about to start?"
          onChange={(event) => controls.setDraftTask(event.currentTarget.value)}
        />
        <button className="btn btn-sm btn-primary" type="submit">Brief this task</button>
      </div>
      <div id="agent-briefing-task-help" className="visually-hidden">
        Describe what you’re about to start, and the briefing adds what may matter for it. Nothing is sent until you press Brief this task.
      </div>
      {controls.error !== null && <div className="small tone-text-bad" role="alert">{controls.error}</div>}
      {controls.task !== undefined && (
        <div className="brief-considering">
          For <b>{controls.task}</b>
          <button className="btn btn-sm btn-link" type="button" onClick={controls.clear}>Show general briefing</button>
        </div>
      )}
    </form>
  )
}

function NarrativeBox({ state, by, controls, children }: { state: 'ready' | 'waiting' | 'writing' | 'bad'; by?: ReactNode; controls?: TaskControls; children: ReactNode }) {
  return (
    <section id="agent-briefing" className={`brief is-${state}`} aria-labelledby="agent-briefing-title">
      <header className="brief-head">
        <div className="brief-by">
          <i aria-hidden="true" />
          <h2 id="agent-briefing-title">Agent briefing</h2>
          {by && <span>{by}</span>}
        </div>
        {controls && <TaskRequest controls={controls} />}
      </header>
      <div className="text-break brief-body">{children}</div>
    </section>
  )
}

function NarrativeReady({ view, controls }: { view: NarrativeView; controls: TaskControls }) {
  const sources = new Map(view.sources.map((source) => [source.id, source]))
  const numbers = citationNumbers(view)
  const cited = [...numbers].map(([id, n]) => ({ n, source: sources.get(id) })).filter((x): x is { n: number; source: NarrativeSource } => !!x.source)
  const by = <>Written by {view.generated_by ?? 'a connected agent'} · {view.generated_at === null ? 'time not reported' : <Ago at={view.generated_at} />}</>
  return (
    <NarrativeBox controls={controls} state={view.window.complete && view.warnings.length === 0 ? 'ready' : 'bad'} by={by}>
      <WindowNotices view={view} />
      <p className="brief-headline">
        {view.output!.headline}
        <Cites refs={view.output!.headline_references} numbers={numbers} sources={sources} />
      </p>
      <div className="brief-cols">
        <section>
          <h3>Reported facts</h3>
          <Statements items={view.output!.facts} numbers={numbers} sources={sources} />
        </section>
        <section>
          <h3>What this may mean for your work</h3>
          <Statements items={view.output!.implications} numbers={numbers} sources={sources} />
        </section>
      </div>
      <footer className="brief-foot">
        {cited.length > 0 && (
          <ol className="brief-sources" aria-label="Sources">
            {cited.map(({ n, source }) => (
              <li key={n} id={`brief-src-${n}`}>
                <span className="brief-cite" aria-hidden="true">{n}</span> <SourceLabel source={source} />
              </li>
            ))}
          </ol>
        )}
        <p className="brief-caveat">
          <Icon name="info-circle" /> Written by an agent from reported records. It does not approve work or show that conflicts are clear.
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
      </footer>
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
          // 404: this Hub predates the briefing route; the 30 s re-check below recovers silently after an upgrade.
          setState(error instanceof ApiError && error.status === 404 ? { kind: 'absent', token, task } : { kind: 'error', token, task, message: message(error) })
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
    return <NarrativeBox controls={controls} state="waiting"><Skeleton rows={2} /></NarrativeBox>
  }
  if (current.kind === 'absent') {
    return (
      <NarrativeBox state="waiting">
        <p className="brief-headline is-quiet">This Hub doesn’t write agent briefings yet.</p>
        <p className="brief-caveat mb-0">
          Update the Hub, then a connected agent can write one with agentscm_briefing_prepare and agentscm_briefing_submit. The catch-up below is complete without it.
        </p>
      </NarrativeBox>
    )
  }
  if (current.kind === 'error') {
    return (
      <NarrativeBox controls={controls} state="bad">
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
      <NarrativeBox controls={controls} state="waiting">
        <WindowNotices view={current.view} />
        <p className="brief-headline is-quiet">Waiting for a connected agent to generate this briefing.</p>
        <p className="brief-caveat mb-0">The dashboard cannot launch a model or provider. The reported catch-up is below.</p>
      </NarrativeBox>
    )
  }
  if (current.view.status === 'generating') {
    return (
      <NarrativeBox controls={controls} state="writing">
        <WindowNotices view={current.view} />
        <p className="brief-headline is-quiet">An eligible connected agent is generating this briefing.</p>
        <p className="brief-caveat mb-0">This page checks for the submitted result while visible. The reported catch-up is below.</p>
      </NarrativeBox>
    )
  }
  if (current.view.status === 'failed') {
    return (
      <NarrativeBox controls={controls} state="bad">
        <WindowNotices view={current.view} />
        <Notice tone="bad" title="Agent briefing generation failed">
          No generated briefing is available for this window. This page keeps checking for recovery; an eligible connected agent is still required. This is not an empty result or an all-clear.
        </Notice>
      </NarrativeBox>
    )
  }
  return <NarrativeReady view={current.view} controls={controls} />
}
