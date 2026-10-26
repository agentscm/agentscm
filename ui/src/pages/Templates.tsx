import { useEffect, useState } from 'react'
import { SectionTabs } from '../TopNav.tsx'
import { Combobox } from '../Combobox.tsx'
import { fetchHandoff, fetchLocalChanges, fetchLocalRuns, inspectTemplate, type Handoff, type RunState, type TemplateAction, type TemplateInspect } from '../localApi.ts'
import { Box, Empty, Explain, Icon, Notice, Pill, PreviewBanner, Skeleton, type Tone } from '../ui.tsx'
import { usePoll } from '../usePoll.ts'

/** One plain sentence per step, so the recipe reads like instructions rather than configuration. */
export function describeAction(a: TemplateAction): { icon: string; text: string } {
  switch (a.kind) {
    case 'source_paths':
      return { icon: 'folder-check', text: `Check ${a.expectations.length === 1 ? 'this path is' : `these ${a.expectations.length} paths are`} in the change: ${a.expectations.map((e) => e.path).join(', ')}` }
    case 'require_capability':
      return { icon: 'plug', text: `Needs the “${a.capability}” capability on the machine that runs it` }
    case 'command':
      return { icon: 'terminal', text: `Run the “${a.runner}” command` }
    case 'claude_code_review':
      return { icon: 'chat-square-text', text: `Ask Claude (${a.model}) to review ${a.paths.length} file${a.paths.length === 1 ? '' : 's'}` }
    default:
      return { icon: 'question-circle', text: 'A step this page does not know how to describe yet' }
  }
}

const RUN: Record<RunState, { tone: Tone; icon: string; word: string }> = {
  pending: { tone: 'brand', icon: 'hourglass-split', word: 'Waiting to start' },
  running: { tone: 'brand', icon: 'arrow-repeat', word: 'Running' },
  succeeded: { tone: 'ok', icon: 'check-circle', word: 'Passed' },
  completed_with_omissions: { tone: 'warn', icon: 'exclamation-circle', word: 'Passed, with gaps' },
  failed: { tone: 'bad', icon: 'x-octagon', word: 'Failed' },
  blocked: { tone: 'bad', icon: 'slash-circle', word: 'Blocked' },
  cancelled: { tone: 'warn', icon: 'stop-circle', word: 'Cancelled' },
  unknown: { tone: 'muted', icon: 'question-circle', word: 'Unclear' },
}
const STEP_TONE = (state: string): { tone: Tone; word: string } =>
  state === 'succeeded' ? { tone: 'ok', word: 'Passed' } : state === 'failed' ? { tone: 'bad', word: 'Failed' } : state === 'blocked' ? { tone: 'bad', word: 'Blocked' } : state === 'running' ? { tone: 'brand', word: 'Running' } : state === 'pending' ? { tone: 'muted', word: 'Not started' } : state === 'cancelled' ? { tone: 'warn', word: 'Skipped' } : { tone: 'muted', word: 'Unclear' }

function Recipe({ value }: { value: TemplateInspect }) {
  const t = value.template
  return (
    <>
      <div className="recipe-head">
        <strong>{t.name}</strong>
        <Pill tone="muted" icon="tag">version {t.version}</Pill>
        <span className="small muted">from <code>{value.reference.path}</code> at <code>{value.reference.revision.slice(0, 10)}</code></span>
      </div>
      <ol className="recipe" aria-label="Steps in this template">
        {t.steps.map((s) => {
          const d = describeAction(s.action)
          return (
            <li key={s.id} className="recipe-step">
              <span className="recipe-icon"><Icon name={d.icon} /></span>
              <span className="recipe-main">
                <span>{d.text}</span>
                <span className="small muted">
                  Step <code>{s.id}</code>
                  {s.needs.length > 0 && <> · after {s.needs.map((n) => <code key={n} className="me-1">{n}</code>)}</>}
                </span>
              </span>
              <Pill tone={s.required ? 'brand' : 'muted'} tip={s.required ? 'The run fails if this step fails.' : 'The run can pass even if this step does not.'}>{s.required ? 'Required' : 'Optional'}</Pill>
            </li>
          )
        })}
      </ol>
      <p className="small muted mb-0"><Icon name="info-circle" /> Skill: <code>{t.skill.entrypoint}</code>. Looking at a template does not approve, start or run anything.</p>
    </>
  )
}

function HandoffView({ value }: { value: Handoff }) {
  const st = RUN[value.run.state]
  return (
    <>
      <div className="recipe-head">
        <code>{value.run.run_id}</code>
        <Pill tone={st.tone} icon={st.icon}>{st.word}</Pill>
      </div>
      <ul className="run-list">
        {value.run.steps.map((s) => {
          const t = STEP_TONE(s.state)
          return (
            <li key={s.id} className="run-row">
              <span className="run-main">
                <strong><code>{s.id}</code>{!s.required && <span className="small muted"> · optional</span>}</strong>
                {s.detail && <span className="small muted">{s.detail}</span>}
              </span>
              <Pill tone={t.tone}>{t.word}</Pill>
            </li>
          )
        })}
      </ul>
      <Notice tone="brand" icon="signpost" title="What to do next">{value.next_action}</Notice>
      <p className="small muted mb-0">
        {value.subject_scope === 'source_revision_and_command_contexts'
          ? <>Covers the source revision <strong>and</strong> the change versions its commands ran against ({value.command_context_bindings.map((b) => `${b.change} v${b.change_version}`).join(', ')}).</>
          : 'Covers the source revision only.'}{' '}
        <Explain text="A run is evidence about exact bytes. If the change moves on, run the check again before relying on this." />
      </p>
    </>
  )
}

const RECENT = { recipe: 'agentscm.hub.recentRecipes', run: 'agentscm.hub.recentRuns' } as const
/** A remembered recipe lookup is the pair, stored as one string. */
const PAIR = '\u0000'
function loadIds(key: string): string[] {
  try {
    const v: unknown = JSON.parse(window.localStorage.getItem(key) ?? '[]')
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string' && x.length <= 8200).slice(0, 6) : []
  } catch {
    return []
  }
}
function rememberId(key: string, id: string): string[] {
  const next = [id, ...loadIds(key).filter((x) => x !== id)].slice(0, 6)
  try {
    window.localStorage.setItem(key, JSON.stringify(next))
  } catch {
    /* not remembered in this browser */
  }
  return next
}

/** Find a run on this computer by name, state, revision or ID; a pasted run ID opens directly. */
function RunLookup({ recent, onOpen }: { recent: string[]; onOpen: (v: string) => void }) {
  return (
    <>
      <div className="lookup">
        <Combobox
          id="tpl-run"
          data-tour="templates-run-picker"
          label="Run ID"
          placeholder="Find a run by name, state or ID"
          describedBy="tpl-run-help"
          looseId
          parseId={(t) => (/^[A-Za-z0-9._-]{1,64}$/.test(t) ? t : null)}
          onSelect={onOpen}
          search={async (q, signal) => {
            const r = (await fetchLocalRuns(signal, q ? { q } : {})).value
            if (r.availability !== 'available') return { options: [] }
            return {
              more: r.next_cursor !== null,
              options: r.runs.map((run) => {
                const st = RUN[run.state]
                return { value: run.run_id, text: run.name, label: run.name, secondary: run.run_id, meta: <><Pill tone={st.tone}>{st.word}</Pill> <span className="small muted">rev <code>{run.revision.slice(0, 10)}</code></span></> }
              }),
            }
          }}
        />
      </div>
      <div id="tpl-run-help" className="form-text">Your agent prints the run ID when a check starts; Timeline also shows it next to the snapshot it tested.</div>
      {recent.length > 0 && (
        <div className="recent">
          <span className="small muted">Opened recently:</span>
          {recent.map((r) => (
            <button key={r} type="button" className="recent-chip btn btn-link btn-sm" onClick={() => onOpen(r)}><code>{r}</code></button>
          ))}
        </div>
      )}
    </>
  )
}

const MAX_CHANGE_PAGES = 5
/** Change IDs on this computer: up to 5 pages of 100 are loaded and filtered in the browser. */
function useLocalChanges(): { ids: { id: string; version: number }[]; more: boolean; loading: boolean; failed: boolean } {
  const [state, setState] = useState<{ ids: { id: string; version: number }[]; more: boolean; loading: boolean; failed: boolean }>({ ids: [], more: false, loading: true, failed: false })
  useEffect(() => {
    const ctrl = new AbortController()
    void (async () => {
      const ids: { id: string; version: number }[] = []
      let after: string | undefined
      try {
        for (let page = 0; page < MAX_CHANGE_PAGES; page++) {
          const r = (await fetchLocalChanges(ctrl.signal, after)).value
          ids.push(...r.changes)
          after = r.next_after ?? undefined
          if (!after) break
        }
        setState({ ids, more: after !== undefined, loading: false, failed: false })
      } catch {
        if (!ctrl.signal.aborted) setState({ ids, more: false, loading: false, failed: true })
      }
    })()
    return () => ctrl.abort()
  }, [])
  return state
}

/** The recipe lookup needs two things: the change, and where the template file sits inside it. */
function RecipeLookup({ recent, onOpen }: { recent: string[]; onOpen: (change: string, library: string) => void }) {
  const [change, setChange] = useState('')
  const [library, setLibrary] = useState('')
  const changes = useLocalChanges()
  const ok = change.trim() !== '' && library.trim() !== ''
  return (
    <>
      <form className="lookup-2" onSubmit={(e) => { e.preventDefault(); if (ok) onOpen(change.trim(), library.trim()) }}>
        <div>
          <span className="form-label small d-block" aria-hidden="true">Change ID</span>
          <Combobox
            id="tpl-change"
            data-tour="templates-change-picker"
            label="Change ID"
            placeholder={changes.loading ? 'Loading changes…' : 'Search changes or paste an ID'}
            describedBy="tpl-change-help"
            keepText
            looseId
            onTextChange={() => setChange('')}
            parseId={(t) => (/^[A-Za-z0-9._:/-]{1,200}$/.test(t) ? t : null)}
            emptyText={changes.failed ? 'Could not list changes. Paste an ID.' : 'No change matches. Paste an ID.'}
            options={changes.ids.map((c) => ({ value: c.id, text: c.id, label: c.id, meta: <Pill tone="muted">version {c.version}</Pill> }))}
            onSelect={setChange}
          />
          <div id="tpl-change-help" className="form-text">
            {changes.more ? 'Showing the first 500 changes; paste an ID to open any other. ' : ''}Your agent shows it when it starts work; agentscm status lists it.
          </div>
        </div>
        <div>
          <label className="form-label small" htmlFor="tpl-path">Template file</label>
          <input id="tpl-path" className="form-control" placeholder="skills/checks/v1/workflow.json" value={library} onChange={(e) => setLibrary(e.target.value)} spellCheck={false} autoComplete="off" aria-describedby="tpl-path-help" />
          <div id="tpl-path-help" className="form-text">The workflow template file inside the skill folder of that change.</div>
        </div>
        <button type="submit" className="btn btn-primary align-self-start mt-4" disabled={!ok}><Icon name="arrow-right" /> Open</button>
      </form>
      {recent.length > 0 && (
        <div className="recent">
          <span className="small muted">Opened recently:</span>
          {recent.map((r) => {
            const [c, l] = r.split(PAIR) as [string, string]
            return (
              <button key={r} type="button" className="recent-chip btn btn-link btn-sm" onClick={() => onOpen(c, l)}>
                <code>{c}</code> · <code>{l}</code>
              </button>
            )
          })}
        </div>
      )}
    </>
  )
}

export function TemplatesPage() {
  const [recipe, setRecipe] = useState<{ change: string; library: string } | null>(null)
  const [runId, setRunId] = useState<string | null>(null)
  const [recentRecipes, setRecentRecipes] = useState(() => loadIds(RECENT.recipe))
  const [recentRuns, setRecentRuns] = useState(() => loadIds(RECENT.run))
  const inspect = usePoll((signal) => (recipe ? inspectTemplate(recipe.change, recipe.library, signal) : Promise.resolve(null)), 0, recipe ? `${recipe.change}${PAIR}${recipe.library}` : '')
  const handoff = usePoll((signal) => (runId ? fetchHandoff(runId, signal) : Promise.resolve(null)), 0, runId ?? '')
  const preview = [inspect, handoff].find((p) => p.kind === 'ready' && p.value?.source === 'preview')
  return (
    <div className="page">
      <div className="answer" data-tour="answer">
        <h1 className="page-question">Which workflow recipes can agents reuse?</h1>
        <p className="answer-text">Recipes of checks that any change can reuse.</p>
        <p className="answer-fact">A <strong>template</strong> is a saved recipe, like “check these files, run the tests, then ask Claude to review”.</p>
      </div>
      <SectionTabs />
      {preview && preview.kind === 'ready' && <PreviewBanner reason={preview.value?.reason} />}
      <div className="grid-2 align-start">
        <Box icon="journal-code" title="See a change’s recipe" explain="Shows the template this computer uses, as it applies to one change: its steps, what each waits for, and which must pass. Same as skill-workflow library inspect.">
          <RecipeLookup recent={recentRecipes} onOpen={(change, library) => { setRecipe({ change, library }); setRecentRecipes(rememberId(RECENT.recipe, `${change}${PAIR}${library}`)) }} />
        </Box>
        <Box icon="flag" title="Where did a run get to?" explain="A run is one go at a recipe's checks. This shows each step's result and what to do next. Same as skill-workflow handoff.">
          <RunLookup
            recent={recentRuns}
            onOpen={(v) => { setRunId(v); setRecentRuns(rememberId(RECENT.run, v)) }}
          />
        </Box>
      </div>
      {recipe !== null && (
        <Box icon="list-check" title={`Recipe for ${recipe.change}`} explain="Each step runs after the ones it waits for. Required steps must pass for the run to pass.">
          {inspect.kind === 'loading' ? <Skeleton rows={4} /> : inspect.kind === 'error' ? <Notice tone="bad" title="Could not read this recipe">{inspect.message}</Notice> : inspect.value && <Recipe value={inspect.value.value} />}
        </Box>
      )}
      {runId !== null && (
        <Box icon="flag" title={`Run ${runId}`} explain="Step results as this computer recorded them.">
          {handoff.kind === 'loading' ? <Skeleton rows={4} /> : handoff.kind === 'error' ? <Notice tone="bad" title="Could not read this run">{handoff.message}</Notice> : handoff.value && <HandoffView value={handoff.value.value} />}
        </Box>
      )}
      {recipe === null && runId === null && (
        <Empty icon="journal-code" title="Open a change or a run to begin">Paste an ID above. This page reads from this computer only and never starts or approves anything.</Empty>
      )}
      <p className="small muted" data-tour="templates-source"><Icon name="laptop" /> Read from <code>agentscm serve</code> on this computer, not from the hub.</p>
    </div>
  )
}
