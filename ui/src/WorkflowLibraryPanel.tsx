import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { Ago, Box, CopyButton, Empty, Icon, Notice, Pill, Skeleton, type Tone } from './ui.tsx'
import {
  acceptSkill, archiveSkill, archiveWorkflow, createWorkflow, deleteWorkflow, fetchAttachments, fetchGuidance, fetchSkillLibrary,
  fetchWorkflowLibrary, previewSkill, publishWorkflow, putAttachment, simulateWorkflow, updateWorkflow,
  validateAttachment, WorkflowLibraryError, type Attachment, type AttachmentValidation, type Diagnostic, type Guidance, type Simulation, type SkillPreview,
  type SkillSource, type SkillVersion, type WorkflowRecord, type WorkflowStep, type WorkflowVersion,
} from './workflowLibrary.ts'
import { inline } from './Markdown.tsx'
import { clean } from './publications.ts'
import { validSkillWorkflowRunId, validSkillWorkflowStepId } from './skillWorkflow.ts'

type Load<T> = { kind: 'loading' } | { kind: 'ready'; value: T } | { kind: 'error'; message: string }
const messageOf = (e: unknown) => e instanceof Error ? e.message : 'The local workflow library could not complete the request.'
const versionLabel = (v: WorkflowVersion) => `${v.definition.name} v${v.version}`
const sourceLabel = (s: SkillVersion) => s.provenance.kind === 'markdown' ? 'Pasted Markdown' : s.provenance.kind === 'github' ? 'GitHub repository' : 'npx descriptor'
const short = (v: string) => v.slice(0, 10)

function Diagnostics({ items }: { items: Diagnostic[] }) {
  if (!items.length) return null
  return (
    <ul className="wl-diagnostics">
      {items.map((d, i) => <li key={`${d.message}-${i}`} className={`tone-${d.severity === 'error' ? 'bad' : d.severity === 'warning' ? 'warn' : 'muted'}`}><Icon name={d.severity === 'error' ? 'x-circle' : d.severity === 'warning' ? 'exclamation-triangle' : 'info-circle'} /> <span>{d.path && <><code>{d.path}</code> · </>}{d.message}</span></li>)}
    </ul>
  )
}

const KIND_ICON: Record<WorkflowStep['kind'], string> = { skill: 'journal-check', build: 'hammer', deploy: 'rocket-takeoff', action: 'play', workflow: 'diagram-3' }

/**
 * The workflow as one line, left to right: what starts it, every ordered step, and what it hands on to.
 * It redraws as the draft changes, so editing shows the shape of what will be published.
 */
function FlowStrip({ steps, trigger, followOn, versions, size = 'full' }: { steps: WorkflowStep[]; trigger: WorkflowRecord['draft']['trigger']; followOn: WorkflowRecord['draft']['follow_on']; versions: WorkflowVersion[]; size?: 'full' | 'mini' }) {
  const next = followOn ? versions.find((v) => v.id === followOn.workflow_version_id) : undefined
  return (
    <ol className={`flow flow-${size}`} aria-label={`${steps.length} step${steps.length === 1 ? '' : 's'}${trigger ? `, after a confirmed merge to ${trigger.target}` : ''}`}>
      {trigger && (
        <li className="flow-cap flow-start">
          <Icon name="git" /> <span>After merge to <b>{trigger.target}</b></span>
        </li>
      )}
      {steps.length === 0 ? (
        <li className="flow-step is-empty"><span>No steps yet</span></li>
      ) : (
        steps.map((s, i) => (
          <li key={s.id} className={`flow-step kind-${s.kind}${s.required ? '' : ' is-optional'}`} title={`${i + 1}. ${s.label} (${s.kind}${s.required ? '' : ', optional'})`}>
            <Icon name={KIND_ICON[s.kind]} />
            <span>{s.label || s.id}</span>
          </li>
        ))
      )}
      {followOn && (
        <li className="flow-cap flow-end">
          <Icon name="arrow-return-right" /> <span>Then <b>{next ? versionLabel(next) : 'a retained version'}</b></span>
        </li>
      )}
    </ol>
  )
}

function BusyButton({ busy, children, ...props }: { busy: boolean; children: ReactNode } & React.ButtonHTMLAttributes<HTMLButtonElement>) {
  return <button type="button" {...props} disabled={busy || props.disabled}>{busy && <span className="spinner-border spinner-border-sm" aria-hidden="true" />} {children}</button>
}

function loadLibraries(repository: string | null, set: (v: Load<{ workflows: WorkflowRecord[]; skills: SkillVersion[]; attachments: Attachment[] }>) => void) {
  const ctrl = new AbortController()
  set({ kind: 'loading' })
  void Promise.all([fetchWorkflowLibrary(ctrl.signal), fetchSkillLibrary(ctrl.signal), repository ? fetchAttachments(repository, ctrl.signal) : Promise.resolve([])])
    .then(([workflows, skills, attachments]) => set({ kind: 'ready', value: { workflows, skills, attachments } }))
    .catch((e) => { if (!ctrl.signal.aborted) set({ kind: 'error', message: messageOf(e) }) })
  return () => ctrl.abort()
}

export function WorkflowLibraryPanel({ repository, onDirtyChange }: { repository: string | null; onDirtyChange?: (dirty: boolean) => void }) {
  const [loadedRepository, setLoadedRepository] = useState(repository)
  const repositoryRef = useRef(repository)
  const [load, setLoad] = useState<Load<{ workflows: WorkflowRecord[]; skills: SkillVersion[]; attachments: Attachment[] }>>({ kind: 'loading' })
  const [selected, setSelected] = useState<string | null>(null)
  const [editorDirty, setEditorDirty] = useState(false)
  const [creating, setCreating] = useState(false)
  const [name, setName] = useState('')
  const [summary, setSummary] = useState('')
  const [busy, setBusy] = useState('')
  const [error, setError] = useState('')
  const [guidance, setGuidance] = useState<Load<Guidance> | null>(null)
  const activeRepository = editorDirty ? loadedRepository : repository
  const handleEditorDirty = useCallback((dirty: boolean) => {
    if (dirty) setLoadedRepository(repositoryRef.current)
    setEditorDirty(dirty)
  }, [])
  useEffect(() => { repositoryRef.current = repository }, [repository])
  useEffect(() => {
    onDirtyChange?.(editorDirty)
    return () => onDirtyChange?.(false)
  }, [editorDirty, onDirtyChange])
  useEffect(() => loadLibraries(activeRepository, setLoad), [activeRepository])

  if (load.kind === 'loading') return <Skeleton rows={7} />
  if (load.kind === 'error') return <Notice tone="warn" icon="laptop" title="Open the local workflow library">{inline(load.message)}</Notice>
  const { workflows, skills, attachments } = load.value
  const activeWorkflows = workflows.filter((w) => !w.draft.archived)
  const record = activeWorkflows.find((w) => w.draft.id === selected)
  const replace = (next: WorkflowRecord) => setLoad({ kind: 'ready', value: { ...load.value, workflows: workflows.map((w) => w.draft.id === next.draft.id ? next : w) } })
  const add = (next: WorkflowRecord) => setLoad({ kind: 'ready', value: { ...load.value, workflows: [next, ...workflows] } })
  const remove = (id: string) => setLoad({ kind: 'ready', value: { ...load.value, workflows: workflows.filter((w) => w.draft.id !== id) } })
  const setAttachment = (value: Attachment) => setLoad({ kind: 'ready', value: { ...load.value, attachments: [value, ...attachments.filter((a) => a.id !== value.id)] } })

  const reloadRecord = async (id: string) => {
    const next = await fetchWorkflowLibrary()
    const found = next.find((item) => item.draft.id === id)
    if (!found) throw new Error('This workflow is no longer available on this computer.')
    setLoad({ kind: 'ready', value: { ...load.value, workflows: next } })
    return found
  }

  if (record) return <WorkflowEditor record={record} workflows={workflows} skills={skills.filter((s) => !s.archived)} repository={activeRepository} requestedRepository={editorDirty && repository !== loadedRepository ? repository : undefined} attachment={attachments.find((a) => a.id === record.draft.id) ?? null} onAttachment={setAttachment} onBack={() => { setEditorDirty(false); setSelected(null) }} onChange={replace} onDirtyChange={handleEditorDirty} onReload={reloadRecord} onDiscardRepository={() => { setEditorDirty(false); setSelected(null); setLoadedRepository(repository) }} />

  const create = async () => {
    setBusy('create'); setError('')
    try {
      const draft = await createWorkflow(name.trim(), summary.trim())
      add({ draft, versions: [] }); setCreating(false); setName(''); setSummary(''); setSelected(draft.id)
    } catch (e) { setError(messageOf(e)) } finally { setBusy('') }
  }
  const duplicate = async (source: WorkflowRecord) => {
    setBusy(`duplicate:${source.draft.id}`); setError('')
    try {
      let draft = await createWorkflow(`${source.draft.name} copy`, source.draft.summary)
      draft = await updateWorkflow({ ...draft, steps: source.draft.steps, trigger: source.draft.trigger, follow_on: source.draft.follow_on })
      add({ draft, versions: [] }); setSelected(draft.id)
    } catch (e) { setError(messageOf(e)) } finally { setBusy('') }
  }
  const destroy = async (source: WorkflowRecord) => {
    setBusy(`delete:${source.draft.id}`); setError('')
    try {
      if (source.versions.length) {
        const next = await archiveWorkflow(source.draft)
        replace({ ...source, draft: next })
      } else {
        await deleteWorkflow(source.draft); remove(source.draft.id)
      }
    } catch (e) { setError(messageOf(e)) } finally { setBusy('') }
  }
  const inspectGuidance = async () => {
    if (!repository) return
    setGuidance({ kind: 'loading' })
    try { setGuidance({ kind: 'ready', value: await fetchGuidance(repository) }) } catch (e) { setGuidance({ kind: 'error', message: messageOf(e) }) }
  }
  return (
    <div className="stack">
      <div className="wl-toolbar">
        <h2 className="h5 mb-0">Reusable workflows</h2>
        <div className="wl-actions">
          <button type="button" className="btn btn-outline-secondary" onClick={inspectGuidance} disabled={!repository}><Icon name="lightbulb" /> Repository guidance</button>
          <button type="button" className={`btn ${creating ? 'btn-outline-secondary' : 'btn-primary'}`} onClick={() => setCreating(true)}><Icon name="plus-lg" /> New workflow</button>
        </div>
      </div>
      {error && <Notice tone="bad" title="Workflow library action failed">{error}</Notice>}
      {creating && (
        <Box icon="plus-square" title="Create workflow" tone="brand">
          <form className="wl-create" onSubmit={(e) => { e.preventDefault(); if (name.trim()) void create() }}>
            <label><span className="form-label">Name</span><input autoFocus className="form-control" value={name} onChange={(e) => setName(e.target.value)} maxLength={100} /></label>
            <label><span className="form-label">Purpose</span><input className="form-control" value={summary} onChange={(e) => setSummary(e.target.value)} maxLength={500} /></label>
            <div className="wl-actions align-self-end"><button type="button" className="btn btn-outline-secondary" onClick={() => setCreating(false)}>Cancel</button><BusyButton busy={busy === 'create'} className="btn btn-primary" disabled={!name.trim()} onClick={() => void create()}>Create</BusyButton></div>
          </form>
        </Box>
      )}
      {guidance && <GuidanceView load={guidance} />}
      <Box icon="journal-code" title="Workflow drafts" count={activeWorkflows.length} explain="You can edit drafts. A published version never changes and keeps its digest. Archived workflows drop off this list.">
        {activeWorkflows.length === 0 ? <Empty icon="journal-code" title="No reusable workflows yet">Create a workflow, add ordered steps, then publish an immutable version.</Empty> : (
          <ul className="wl-list">
            {activeWorkflows.map((w) => {
              const latest = w.versions.filter((v) => !v.archived).sort((a, b) => b.version - a.version)[0]
              const attached = attachments.find((a) => a.id === w.draft.id)
              return <li key={w.draft.id} className="wl-row">
                <button type="button" className="wl-open" onClick={() => setSelected(w.draft.id)}>
                  <span className="wl-row-main"><strong>{w.draft.name}</strong><span>{w.draft.summary || 'No purpose recorded yet.'}</span><FlowStrip size="mini" steps={w.draft.steps} trigger={w.draft.trigger} followOn={w.draft.follow_on} versions={workflows.flatMap((x) => x.versions)} /><span className="small muted">{w.draft.steps.length} step{w.draft.steps.length === 1 ? '' : 's'} · {latest ? `version ${latest.version}` : 'draft only'} · updated <Ago at={w.draft.updated_at} /></span></span>
                  <span className="pills">{latest ? <Pill tone="ok" icon="lock">Versioned</Pill> : <Pill tone="muted" icon="pencil">Draft</Pill>}{attached && <Pill tone={attached.state === 'active' ? 'ok' : attached.state === 'blocked' ? 'bad' : attached.state === 'export_required' ? 'warn' : 'brand'}>{attached.state.replaceAll('_', ' ')}</Pill>}</span>
                  <Icon name="chevron-right" />
                </button>
                <div className="wl-row-actions"><BusyButton busy={busy === `duplicate:${w.draft.id}`} className="btn btn-sm btn-outline-secondary" onClick={() => void duplicate(w)}><Icon name="copy" /> Duplicate</BusyButton><BusyButton busy={busy === `delete:${w.draft.id}`} className={w.versions.length ? 'btn btn-sm btn-outline-secondary' : 'btn btn-sm btn-outline-danger'} onClick={() => void destroy(w)}><Icon name={w.versions.length ? 'archive' : 'trash'} /> {w.versions.length ? 'Archive' : 'Delete draft'}</BusyButton></div>
              </li>
            })}
          </ul>
        )}
      </Box>
    </div>
  )
}

function GuidanceView({ load }: { load: Load<Guidance> }) {
  if (load.kind === 'loading') return <Box icon="lightbulb" title="Reading repository guidance"><Skeleton rows={3} /></Box>
  if (load.kind === 'error') return <Notice tone="bad" title="Could not read repository guidance">{inline(load.message)}</Notice>
  const g = load.value
  return <Box icon="lightbulb" title="Repository guidance" count={g.suggestions.length} explain="Guessed from common manifest and CI files. Nothing was run or changed.">
    <Diagnostics items={g.diagnostics} />
    {g.suggestions.length === 0 ? <Empty icon="lightbulb" title="No suggestions found">Review uncovered conditions below; absence of a suggestion is not an all-clear.</Empty> : <ul className="wl-guidance">{g.suggestions.map((s, i) => <li key={`${s.kind}-${i}`}><Pill tone="muted">{s.kind}</Pill><span><code>{s.argv.join(' ')}</code><small>{s.reason} · from {s.sources.join(', ')}</small></span><CopyButton text={s.argv.join(' ')} /></li>)}</ul>}
    {g.uncovered.length > 0 && <Notice tone="warn" title="Not covered">{g.uncovered.join(' · ')}</Notice>}
    <p className="small muted mb-0">Turn a suggestion into a reviewed runner inside a skill package before adding it to a workflow.</p>
  </Box>
}

const rewire = (steps: WorkflowStep[]) => steps.map((s, i) => ({ ...s, needs: i === 0 ? [] : [steps[i - 1]!.id] }))
const freshId = (kind: WorkflowStep['kind'], steps: WorkflowStep[]) => { let i = steps.length + 1; while (steps.some((s) => s.id === `${kind}-${i}`)) i++; return `${kind}-${i}` }

const DISCARD_MESSAGE = 'Discard unsaved workflow edits?'

function WorkflowEditor({ record, workflows, skills, repository, requestedRepository, attachment, onAttachment, onBack, onChange, onDirtyChange, onReload, onDiscardRepository }: { record: WorkflowRecord; workflows: WorkflowRecord[]; skills: SkillVersion[]; repository: string | null; requestedRepository?: string | null; attachment: Attachment | null; onAttachment: (v: Attachment) => void; onBack: () => void; onChange: (v: WorkflowRecord) => void; onDirtyChange: (dirty: boolean) => void; onReload: (id: string) => Promise<WorkflowRecord>; onDiscardRepository: () => void }) {
  const [draft, setDraft] = useState(record.draft)
  const [busy, setBusy] = useState('')
  const [error, setError] = useState('')
  const [notice, setNotice] = useState('')
  const [publishDiagnostics, setPublishDiagnostics] = useState<Diagnostic[]>([])
  const [canReload, setCanReload] = useState(false)
  const [discarding, setDiscarding] = useState(false)
  const [simulation, setSimulation] = useState<Simulation | null>(null)
  const [exportResult, setExportResult] = useState<AttachmentValidation | null>(null)
  const [revision, setRevision] = useState('')
  const allVersions = useMemo(() => workflows.flatMap((w) => w.versions), [workflows])
  const versions = useMemo(() => workflows.filter((w) => !w.draft.archived).flatMap((w) => w.versions.filter((v) => !v.archived)), [workflows])
  const followVersions = useMemo(() => versions.filter((v) => v.workflow_id !== draft.id), [draft.id, versions])
  const latest = [...record.versions].filter((v) => !v.archived).sort((a, b) => b.version - a.version)[0]
  const intentStep = latest?.definition.steps.length === 1 && latest.definition.steps[0]?.kind === 'skill' && latest.definition.steps[0].needs.length === 0 && !latest.definition.trigger && !latest.definition.follow_on ? latest.definition.steps[0] : null
  const canCopyIntentPrompt = !!latest && !!intentStep && validSkillWorkflowRunId(latest.id) && validSkillWorkflowStepId(intentStep.id)
  const intentPrompt = canCopyIntentPrompt && latest && intentStep ? clean(`Run a tracked intent review for the current AgentSCM Work using agentscm intent-review. Use published workflow ${latest.id}, step ${intentStep.id}. Extract the human request and authorized amendments from our available conversation; label your plan separately. Start a bounded review, claim the pinned skill task, retain findings before any correction, then reassess the changed candidate. Follow the intent-review protocol and existing permissions. Do not treat this report as merge approval.`, 1024) : ''
  const dirty = JSON.stringify(draft) !== JSON.stringify(record.draft)
  useEffect(() => {
    onDirtyChange(dirty)
    return () => onDirtyChange(false)
  }, [dirty, onDirtyChange])
  const save = async () => {
    setBusy('save'); setError(''); setNotice(''); setPublishDiagnostics([]); setCanReload(false)
    try { const saved = await updateWorkflow(draft); onChange({ ...record, draft: saved }); setDraft(saved); setNotice('Draft saved.') ; return saved } catch (e) { setError(messageOf(e)); setCanReload(e instanceof WorkflowLibraryError && e.status === 409); return null } finally { setBusy('') }
  }
  const publish = async () => {
    setBusy('publish'); setError(''); setNotice(''); setPublishDiagnostics([]); setCanReload(false)
    let saved = draft
    let savedBeforeFailure = false
    try {
      if (dirty) {
        saved = await updateWorkflow(draft)
        savedBeforeFailure = true
        onChange({ ...record, draft: saved })
        setDraft(saved)
      }
      const result = await publishWorkflow(saved)
      onChange({ draft: saved, versions: [result.version, ...record.versions] }); setNotice(`Version ${result.version.version} published with digest ${short(result.version.digest)}.`); setPublishDiagnostics(result.diagnostics)
    } catch (e) {
      setError(messageOf(e))
      setCanReload(e instanceof WorkflowLibraryError && e.status === 409)
      if (e instanceof WorkflowLibraryError) setPublishDiagnostics(e.diagnostics)
      if (savedBeforeFailure) setNotice('Draft saved, but no version was published. Fix the publication issue and try again.')
    } finally { setBusy('') }
  }
  const reload = async () => {
    setBusy('reload'); setError(''); setNotice(''); setPublishDiagnostics([])
    try { const fresh = await onReload(draft.id); setDraft(fresh.draft); setCanReload(false); setNotice('Reloaded the current draft from this computer.') } catch (e) { setError(messageOf(e)) } finally { setBusy('') }
  }
  const addStep = (kind: WorkflowStep['kind']) => {
    const common = { id: freshId(kind, draft.steps), label: kind === 'skill' ? 'Run skill rule' : kind === 'workflow' ? 'Run reusable workflow' : `${kind[0]!.toUpperCase()}${kind.slice(1)}`, needs: [], required: true }
    const skill = skills[0]?.id ?? ''
    const version = versions[0]?.id ?? ''
    const next: WorkflowStep = kind === 'skill' ? { ...common, kind, skill_version_id: skill } : kind === 'build' ? { ...common, kind, skill_version_id: skill, runner: '' } : kind === 'deploy' ? { ...common, kind, skill_version_id: skill, resource: '', target: '' } : kind === 'action' ? { ...common, kind, skill_version_id: skill, resource: '' } : { ...common, kind, workflow_version_id: version, inputs: {} }
    setDraft({ ...draft, steps: rewire([...draft.steps, next]) })
  }
  const updateStep = (index: number, next: WorkflowStep) => setDraft({ ...draft, steps: rewire(draft.steps.map((s, i) => i === index ? next : s)) })
  const move = (index: number, delta: number) => { const next = [...draft.steps]; const at = index + delta; if (at < 0 || at >= next.length) return; [next[index], next[at]] = [next[at]!, next[index]!]; setDraft({ ...draft, steps: rewire(next) }) }
  const removeStep = (index: number) => setDraft({ ...draft, steps: rewire(draft.steps.filter((_, i) => i !== index)) })
  const simulate = async () => { if (!latest) return; setBusy('simulate'); setError(''); setSimulation(null); try { setSimulation(await simulateWorkflow(latest.id, repository ?? undefined, revision || undefined)) } catch (e) { setError(messageOf(e)) } finally { setBusy('') } }
  const attach = async () => { if (!repository || !latest) return; setBusy('attach'); setError(''); try { onAttachment(await putAttachment(repository, draft.id, latest.id, attachment?.generation)); setNotice(`Version ${latest.version} attached as a draft. Validate its exact source before operator setup.`) } catch (e) { setError(messageOf(e)) } finally { setBusy('') } }
  const validate = async () => { if (!repository || !attachment || !/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(revision)) return; setBusy('validate'); setError(''); try { const a = await validateAttachment(repository, attachment, revision); onAttachment(a); setExportResult(a); setNotice(a.setup.runnable ? 'Setup is structurally runnable. Review the exact package and continue with the local operator CLI.' : 'Setup needs the listed bindings before the local operator CLI can run it.') } catch (e) { setError(messageOf(e)) } finally { setBusy('') } }
  const currentExportResult = exportResult && attachment && exportResult.id === attachment.id && exportResult.generation === attachment.generation && exportResult.workflow_version_id === attachment.workflow_version_id ? exportResult : null

  return <div className="stack">
    <div className="wl-editor-head">{discarding ? <span className="confirm-inline" role="group" aria-label={DISCARD_MESSAGE} onKeyDown={(e) => e.key === 'Escape' && setDiscarding(false)}><span className="small">Discard unsaved edits?</span><button autoFocus type="button" className="btn btn-sm btn-danger" onClick={onBack}>Discard edits</button><button type="button" className="btn btn-sm btn-outline-secondary" onClick={() => setDiscarding(false)}>Keep editing</button></span> : <button type="button" className="btn btn-link ps-0" onClick={() => dirty ? setDiscarding(true) : onBack()}><Icon name="arrow-left" /> Workflow library</button>}<div className="wl-actions"><BusyButton busy={busy === 'save'} className="btn btn-outline-secondary" disabled={!dirty} onClick={() => void save()}>Save draft</BusyButton><BusyButton busy={busy === 'publish'} className="btn btn-primary" disabled={!draft.name.trim() || draft.steps.length === 0} onClick={() => void publish()}>Publish version</BusyButton></div></div>
    {requestedRepository !== undefined && dirty && <Notice tone="warn" title="Finish this draft before switching repositories">The selected repository changed to <code>{requestedRepository ?? 'no repository'}</code>. Save this draft to switch, or discard the edits.<div className="mt-2"><button type="button" className="btn btn-sm btn-outline-danger" onClick={onDiscardRepository}>Discard edits and switch</button></div></Notice>}
    {error && <Notice tone="bad" title="Could not update this workflow">{error}{canReload && <div className="mt-2"><BusyButton busy={busy === 'reload'} className="btn btn-sm btn-outline-light" onClick={() => void reload()}>Reload current draft</BusyButton></div>}</Notice>}
    <Diagnostics items={publishDiagnostics} />
    {notice && <Notice tone="brand" title="Workflow updated">{notice}</Notice>}
    <section className="flow-live" aria-label="What this draft will do">
      <div className="flow-live-head"><span className="flow-live-name">{draft.name || 'Untitled workflow'}</span><span className="small muted">{dirty ? 'Unsaved edits' : latest ? `Latest published: version ${latest.version}` : 'Saved draft, not yet published'}</span></div>
      <FlowStrip steps={draft.steps} trigger={draft.trigger} followOn={draft.follow_on} versions={allVersions} />
    </section>
    <Box icon="pencil-square" title="Details" tone="brand">
      <div className="wl-form-2"><label><span className="form-label">Name</span><input className="form-control" value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value })} maxLength={100} /></label><label><span className="form-label">Purpose</span><input className="form-control" value={draft.summary} onChange={(e) => setDraft({ ...draft, summary: e.target.value })} maxLength={500} /></label></div>
    </Box>
    <Box icon="list-ol" title="Ordered steps" count={draft.steps.length} explain="Each step waits for the one above it. Publishing checks that the steps don’t loop.">
      {draft.steps.length === 0 ? <Empty icon="list-ol" title="Add the first step">Use a saved skill for rules, builds, deploy plans or actions, or call another immutable workflow version.</Empty> : <ol className="wl-steps">{draft.steps.map((s, i) => <StepEditor key={s.id} step={s} index={i} count={draft.steps.length} skills={skills} versions={versions.filter((v) => v.workflow_id !== draft.id)} allVersions={allVersions} onChange={(v) => updateStep(i, v)} onMove={(d) => move(i, d)} onRemove={() => removeStep(i)} />)}</ol>}
      <div className="wl-add"><span className="small muted">Add step</span>{(['skill', 'build', 'deploy', 'action', 'workflow'] as const).map((kind) => <button key={kind} type="button" className="btn btn-sm btn-outline-secondary" disabled={(kind !== 'workflow' && skills.length === 0) || (kind === 'workflow' && versions.filter((v) => v.workflow_id !== draft.id).length === 0)} onClick={() => addStep(kind)}><Icon name={kind === 'skill' ? 'journal-check' : kind === 'build' ? 'hammer' : kind === 'deploy' ? 'rocket-takeoff' : kind === 'action' ? 'play' : 'diagram-3'} /> {kind === 'skill' ? 'Skill / rule' : `${kind[0]!.toUpperCase()}${kind.slice(1)}`}</button>)}</div>
      {skills.length === 0 && <p className="small muted mt-2 mb-0"><a href="#templates?view=skills">Create or import a skill</a> before adding skill-backed steps.</p>}
    </Box>
    <Box icon="signpost-split" title="After a confirmed merge" explain="Fires when the hub sees an accepted commit on GitHub or GitLab. That doesn’t mean it was deployed or published.">
      <label className="form-check"><input type="checkbox" className="form-check-input" checked={draft.trigger !== null} onChange={(e) => setDraft({ ...draft, trigger: e.target.checked ? { kind: 'confirmed_merge', target: 'main', labels_all: ['major-feature'] } : null })} /><span className="form-check-label">Prepare this workflow after a matching merge</span></label>
      {draft.trigger && <div className="wl-form-2 mt-3"><label><span className="form-label">Target branch</span><input className="form-control" value={draft.trigger.target} onChange={(e) => setDraft({ ...draft, trigger: { ...draft.trigger!, target: e.target.value } })} /></label><label><span className="form-label">Required labels</span><input className="form-control" value={draft.trigger.labels_all.join(', ')} onChange={(e) => setDraft({ ...draft, trigger: { ...draft.trigger!, labels_all: e.target.value.split(',').map((v) => v.trim()).filter(Boolean) } })} /><span className="form-text">The major-feature preset uses <code>major-feature</code>.</span></label></div>}
    </Box>
    <Box icon="arrow-return-right" title="Then run" explain="Runs the chosen published workflow after the steps above succeed. Running on failure, or always, isn’t supported yet.">
      <label className="form-check"><input type="checkbox" className="form-check-input" checked={draft.follow_on !== null} onChange={(e) => setDraft({ ...draft, follow_on: e.target.checked && followVersions.length ? { workflow_version_id: followVersions[0]!.id, when: 'success', inputs: {} } : null })} disabled={!followVersions.length && draft.follow_on === null} /><span className="form-check-label">Continue with another workflow</span></label>
      {draft.follow_on && <label className="mt-3"><span className="form-label">Published workflow</span><select className="form-select" value={draft.follow_on.workflow_version_id} onChange={(e) => setDraft({ ...draft, follow_on: { ...draft.follow_on!, workflow_version_id: e.target.value, when: 'success' } })}><WorkflowVersionOptions versions={followVersions} allVersions={allVersions} current={draft.follow_on.workflow_version_id} /></select>{!followVersions.some((v) => v.id === draft.follow_on!.workflow_version_id) && <span className="form-text text-warning">This immutable reference is retained, but its workflow version is archived or unavailable. Choose an active version before publishing.</span>}<span className="form-text">Runs only after every preceding step succeeds. Any optional failure blocks it too.</span></label>}
    </Box>
    <Box icon="bezier2" title="Simulate and attach" tone={simulation?.outcome === 'failed' ? 'bad' : simulation ? 'ok' : 'muted'} explain="Checks the published version and shows what would happen. Nothing actually runs.">
      {latest ? <>
        <div className="wl-sim-head"><span>Using <Pill tone="muted" icon="lock">version {latest.version}</Pill> <code>{short(latest.digest)}</code></span><div className="wl-actions"><BusyButton busy={busy === 'simulate'} className="btn btn-outline-secondary" onClick={() => void simulate()}><Icon name="play-circle" /> Simulate</BusyButton><BusyButton busy={busy === 'attach'} className="btn btn-outline-secondary" disabled={!repository} onClick={() => void attach()}><Icon name="paperclip" /> Attach to repository</BusyButton></div></div>
        <label className="mt-3"><span className="form-label">Exact source revision <span className="muted">(required to export setup)</span></span><input className="form-control font-monospace" value={revision} onChange={(e) => setRevision(e.target.value.trim())} placeholder="Full 40- or 64-character lowercase revision" pattern="(?:[0-9a-f]{40}|[0-9a-f]{64})" /></label>
        {simulation && <SimulationView value={simulation} />}
        {attachment && <AttachmentView value={attachment} exportResult={currentExportResult} busy={busy} canValidate={/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(revision)} onValidate={() => void validate()} />}
      </> : <Empty icon="lock" title="Publish a version first">Simulation and repository attachment always use immutable version bytes.</Empty>}
    </Box>
    {intentStep && <Box icon="bullseye" title="Review intent with your agent" explain="Your agent uses this pinned skill with the tools it already has. The workflow keeps its findings and any fix attempts.">
      <p>Ask your agent to compare this Work with your original request using <code>agentscm intent-review</code>. It can use the conversation already available to it.</p>
      {canCopyIntentPrompt ? <><p className="small mb-2 text-break" aria-label="Agent request preview">{intentPrompt}</p><CopyButton label="Copy agent request" text={intentPrompt} /></> : <Notice tone="warn" title="Cannot prepare the agent request">The published workflow or step ID does not match the supported format. Check the IDs before copying a request.</Notice>}
      <p className="small muted mt-2 mb-0">This starts through your agent. Importing or publishing a skill does not launch one. Host execution currently supports one standalone skill step.</p>
    </Box>}
  </div>
}

function WorkflowVersionOptions({ versions, allVersions, current }: { versions: WorkflowVersion[]; allVersions: WorkflowVersion[]; current: string }) {
  const retained = versions.some((version) => version.id === current) ? null : allVersions.find((version) => version.id === current)
  return <>{retained ? <option value={retained.id} disabled>{versionLabel(retained)} · archived reference (retained)</option> : current && !versions.some((version) => version.id === current) ? <option value={current} disabled>{current} · unavailable reference (retained)</option> : null}{versions.map((version) => <option key={version.id} value={version.id}>{versionLabel(version)}</option>)}</>
}

function StepEditor({ step, index, count, skills, versions, allVersions, onChange, onMove, onRemove }: { step: WorkflowStep; index: number; count: number; skills: SkillVersion[]; versions: WorkflowVersion[]; allVersions: WorkflowVersion[]; onChange: (v: WorkflowStep) => void; onMove: (delta: number) => void; onRemove: () => void }) {
  const withCommon = (values: Partial<WorkflowStep>) => onChange({ ...step, ...values } as WorkflowStep)
  return <li className="wl-step">
    <div className="wl-step-order"><span>{index + 1}</span><button type="button" aria-label={`Move ${step.label} up`} disabled={index === 0} onClick={() => onMove(-1)}><Icon name="chevron-up" /></button><button type="button" aria-label={`Move ${step.label} down`} disabled={index === count - 1} onClick={() => onMove(1)}><Icon name="chevron-down" /></button></div>
    <div className="wl-step-main">
      <div className="wl-form-2"><label><span className="form-label">Step ID</span><input className="form-control font-monospace" value={step.id} onChange={(e) => withCommon({ id: e.target.value })} /></label><label><span className="form-label">Label</span><input className="form-control" value={step.label} onChange={(e) => withCommon({ label: e.target.value })} /></label></div>
      {step.kind === 'workflow' ? <label className="mt-2"><span className="form-label">Published workflow version</span><select className="form-select" value={step.workflow_version_id} onChange={(e) => onChange({ ...step, workflow_version_id: e.target.value })}><WorkflowVersionOptions versions={versions} allVersions={allVersions} current={step.workflow_version_id} /></select>{!versions.some((version) => version.id === step.workflow_version_id) && <span className="form-text text-warning">This immutable reference is retained, but its workflow version is archived or unavailable. Choose an active version before publishing.</span>}</label> : <label className="mt-2"><span className="form-label">Immutable skill version</span><select className="form-select" value={step.skill_version_id} onChange={(e) => onChange({ ...step, skill_version_id: e.target.value } as WorkflowStep)}>{skills.map((s) => <option key={s.id} value={s.id}>{s.name} v{s.version} · {short(s.digest)}</option>)}</select></label>}
      {step.kind === 'build' && <label className="mt-2"><span className="form-label">Runner file</span><input className="form-control font-monospace" placeholder="runners/build.json" value={step.runner} onChange={(e) => onChange({ ...step, runner: e.target.value })} /><span className="form-text">Relative to the selected skill package. Simulation will not run it.</span></label>}
      {(step.kind === 'deploy' || step.kind === 'action') && <label className="mt-2"><span className="form-label">Action resource</span><input className="form-control font-monospace" placeholder="actions/prepare.json" value={step.resource} onChange={(e) => onChange({ ...step, resource: e.target.value } as WorkflowStep)} /><span className="form-text">A retained productive action profile; this does not itself deploy or publish.</span></label>}
      {step.kind === 'deploy' && <label className="mt-2"><span className="form-label">Declared target</span><input className="form-control" placeholder="staging" value={step.target} onChange={(e) => onChange({ ...step, target: e.target.value })} /></label>}
      <label className="form-check mt-2"><input type="checkbox" className="form-check-input" checked={step.required} onChange={(e) => withCommon({ required: e.target.checked })} /><span className="form-check-label">Required</span></label>
    </div>
    <div className="wl-step-side"><Pill tone="muted">{step.kind}</Pill><button type="button" className="btn btn-sm btn-link text-danger" onClick={onRemove}>Remove</button></div>
  </li>
}

function SimulationView({ value }: { value: Simulation }) {
  const tone: Tone = value.outcome === 'passed' ? 'ok' : value.outcome === 'passed_with_warnings' ? 'warn' : 'bad'
  return <div className="wl-result"><Notice tone={tone} icon={value.outcome === 'failed' ? 'x-circle' : 'check-circle'} title={value.outcome === 'passed' ? 'Simulation passed' : value.outcome === 'passed_with_warnings' ? 'Simulation passed with warnings' : 'Simulation failed'}>No side effects occurred. Admission was not assessed and deployment was not attempted.</Notice><ul className="wl-sim-steps">{value.steps.map((s) => <li key={s.id}><Pill tone={s.status === 'would_run' ? 'ok' : s.status === 'blocked' ? 'bad' : 'muted'}>{s.status.replaceAll('_', ' ')}</Pill><strong>{s.id}</strong><span>{s.detail}{s.requires.length > 0 && <small className="d-block muted">Needs {s.requires.join(', ')}</small>}</span></li>)}</ul><Diagnostics items={value.diagnostics} /></div>
}

function AttachmentView({ value, exportResult, busy, canValidate, onValidate }: { value: Attachment; exportResult: AttachmentValidation | null; busy: string; canValidate: boolean; onValidate: () => void }) {
  const download = () => {
    if (!exportResult) return
    const blob = new Blob([JSON.stringify({ version: 1, attachment: value.id, setup: exportResult.setup, next_actions: exportResult.next_actions }, null, 2)], { type: 'application/json' })
    const url = URL.createObjectURL(blob)
    const link = document.createElement('a')
    link.href = url
    link.download = `${value.id}-workflow-export.json`
    link.click()
    URL.revokeObjectURL(url)
  }
  const blocked = value.state === 'blocked' || exportResult?.setup.runnable === false
  return <div className="wl-attachment"><div><strong>Repository attachment</strong> <Pill tone={value.state === 'blocked' ? 'bad' : value.state === 'export_required' ? 'warn' : value.state === 'validated' ? 'ok' : 'brand'}>{value.state.replaceAll('_', ' ')}</Pill><p className="small muted mb-0">{blocked ? 'This attachment cannot be installed. Resolve the listed runtime bindings, publish and attach a corrected version, then review setup again.' : 'This page pins and exports setup data. It does not install or enable an execution rule. Review and commit the exact package, then continue through the existing AgentSCM local operator CLI.'}</p>{exportResult && <Notice tone={exportResult.setup.runnable ? 'ok' : 'bad'} title={exportResult.setup.runnable ? 'Setup is runnable' : 'Setup is blocked'}>{exportResult.setup.unmet_bindings.length ? exportResult.setup.unmet_bindings.join(' · ') : 'No unmet operator bindings were reported.'}</Notice>}{exportResult?.next_actions.length ? <ol className="small mt-3">{exportResult.next_actions.map((action) => <li key={action}>{action}</li>)}</ol> : null}{exportResult?.setup.files.length ? <details className="skill-details"><summary>Exact export files for review ({exportResult.setup.files.length})</summary><ul className="paths">{exportResult.setup.files.map((file) => <li key={file.path}><code>{file.path}</code> <CopyButton text={file.content} /></li>)}</ul></details> : null}</div><div className="wl-actions"><BusyButton busy={busy === 'validate'} className="btn btn-sm btn-outline-secondary" disabled={!canValidate} onClick={onValidate}>Review setup</BusyButton>{exportResult?.setup.runnable ? <button type="button" className="btn btn-sm btn-outline-secondary" onClick={download}><Icon name="download" /> Download setup bundle</button> : null}</div><Diagnostics items={value.diagnostics} /></div>
}

export function SkillLibraryPanel() {
  const [load, setLoad] = useState<Load<SkillVersion[]>>({ kind: 'loading' })
  const [adding, setAdding] = useState(false)
  const [kind, setKind] = useState<SkillSource['kind']>('markdown')
  const [name, setName] = useState('')
  const [markdown, setMarkdown] = useState('')
  const [repository, setRepository] = useState('')
  const [commit, setCommit] = useState('')
  const [path, setPath] = useState('SKILL.md')
  const [command, setCommand] = useState('')
  const [preview, setPreview] = useState<SkillPreview | null>(null)
  const [busy, setBusy] = useState('')
  const [error, setError] = useState('')
  useEffect(() => { const ctrl = new AbortController(); void fetchSkillLibrary(ctrl.signal).then((value) => setLoad({ kind: 'ready', value })).catch((e) => { if (!ctrl.signal.aborted) setLoad({ kind: 'error', message: messageOf(e) }) }); return () => ctrl.abort() }, [])
  if (load.kind === 'loading') return <Skeleton rows={6} />
  if (load.kind === 'error') return <Notice tone="warn" icon="laptop" title="Open the local skill library">{inline(load.message)}</Notice>
  const skills = load.value
  const source = (): SkillSource => kind === 'markdown' ? { kind, name: name.trim(), markdown, resources: [] } : kind === 'github' ? { kind, repository: repository.trim(), commit: commit.trim(), path: path.trim(), resources: [] } : { kind, command: command.trim(), commit: commit.trim(), ...(path.trim() ? { path: path.trim() } : {}), resources: [] }
  const inspect = async () => { setBusy('preview'); setError(''); setPreview(null); try { setPreview(await previewSkill(source())) } catch (e) { setError(messageOf(e)) } finally { setBusy('') } }
  const accept = async () => { if (!preview) return; setBusy('accept'); setError(''); try { const value = await acceptSkill(preview); setLoad({ kind: 'ready', value: skills.some((skill) => skill.id === value.id) ? skills.map((skill) => skill.id === value.id ? value : skill) : [value, ...skills] }); setPreview(null); setAdding(false) } catch (e) { setError(messageOf(e)) } finally { setBusy('') } }
  const archive = async (value: SkillVersion) => { setBusy(`archive:${value.id}`); setError(''); try { const next = await archiveSkill(value); setLoad({ kind: 'ready', value: skills.map((s) => s.id === next.id ? next : s) }) } catch (e) { setError(messageOf(e)) } finally { setBusy('') } }
  const ready = kind === 'markdown' ? !!name.trim() && !!markdown.trim() : kind === 'github' ? !!repository.trim() && /^[0-9a-f]{40}$/.test(commit.trim()) && !!path.trim() : !!command.trim() && /^[0-9a-f]{40}$/.test(commit.trim())
  return <div className="stack">
    <div className="wl-toolbar"><h2 className="h5 mb-0">Reusable skills</h2><button type="button" className={`btn ${adding ? 'btn-outline-secondary' : 'btn-primary'}`} onClick={() => setAdding(true)}><Icon name="plus-lg" /> Add skill</button></div>
    {error && <Notice tone="bad" title="Skill library action failed">{error}</Notice>}
    {adding && <Box icon="journal-plus" title="Add skill" tone="brand"><div className="lens mb-3" role="tablist" aria-label="Skill source">{(['markdown', 'github', 'npx_descriptor'] as const).map((k) => <button type="button" role="tab" aria-selected={kind === k} className={`lens-seg${kind === k ? ' on' : ''}`} key={k} onClick={() => { setKind(k); setPreview(null) }}>{k === 'markdown' ? 'Paste Markdown' : k === 'github' ? 'GitHub reference' : 'npx descriptor'}</button>)}</div>
      {kind === 'markdown' && <div className="stack"><label><span className="form-label">Skill name</span><input className="form-control" value={name} onChange={(e) => setName(e.target.value)} /></label><label><span className="form-label">SKILL.md</span><textarea className="form-control font-monospace wl-markdown" value={markdown} onChange={(e) => setMarkdown(e.target.value)} placeholder={'---\nname: verify\n---\n\n# Instructions'} /></label></div>}
      {kind === 'github' && <div className="wl-form-3"><label><span className="form-label">GitHub repository</span><input className="form-control" value={repository} onChange={(e) => setRepository(e.target.value)} placeholder="owner/repository" /></label><label><span className="form-label">40-character commit</span><input className="form-control font-monospace" value={commit} onChange={(e) => setCommit(e.target.value.trim())} pattern="[0-9a-f]{40}" /></label><label><span className="form-label">SKILL.md path</span><input className="form-control font-monospace" value={path} onChange={(e) => setPath(e.target.value)} /></label></div>}
      {kind === 'npx_descriptor' && <div className="stack"><label><span className="form-label">Command to describe</span><input className="form-control font-monospace" value={command} onChange={(e) => setCommand(e.target.value)} placeholder="npx skills add owner/repository --skill NAME" /></label><Notice tone="muted" icon="shield-check" title="Parsed as data only">AgentSCM parses this descriptor to locate GitHub bytes. It never runs <code>npx</code>, package lifecycle hooks, imported scripts or binaries.</Notice><div className="wl-form-2"><label><span className="form-label">40-character commit</span><input className="form-control font-monospace" value={commit} onChange={(e) => setCommit(e.target.value.trim())} pattern="[0-9a-f]{40}" /></label><label><span className="form-label">SKILL.md path <span className="muted">(optional)</span></span><input className="form-control font-monospace" value={path} onChange={(e) => setPath(e.target.value)} /></label></div></div>}
      <div className="wl-actions mt-3"><button type="button" className="btn btn-outline-secondary" onClick={() => { setAdding(false); setPreview(null) }}>Cancel</button><BusyButton busy={busy === 'preview'} className={`btn ${preview ? 'btn-outline-secondary' : 'btn-primary'}`} disabled={!ready} onClick={() => void inspect()}>Preview exact files</BusyButton></div>
      {preview && <div className="wl-preview"><div className="recipe-head"><strong>{preview.skill.name}</strong><Pill tone="muted" icon="fingerprint">{short(preview.digest)}</Pill><span className="small muted">{preview.files.length} file{preview.files.length === 1 ? '' : 's'} · expires <Ago at={preview.expires_at} /></span></div>{preview.blocked && <Notice tone="bad" title="Import blocked">This preview cannot be accepted.</Notice>}<Diagnostics items={preview.warnings} /><details className="skill-details"><summary>Files and provenance</summary><ul className="paths">{preview.files.map((f) => <li key={f.path}><code>{f.path}</code> · {f.size.toLocaleString()} B · <code>{short(f.sha256)}</code></li>)}</ul></details><BusyButton busy={busy === 'accept'} className="btn btn-primary" disabled={preview.blocked} onClick={() => void accept()}>Save this exact skill version</BusyButton></div>}
    </Box>}
    <Box icon="journal-check" title="Saved skills" count={skills.filter((s) => !s.archived).length} explain="A version never changes once published. Knowing where it came from doesn’t make it safe to run.">{skills.length === 0 ? <Empty icon="journal-check" title="No local skills yet">Paste a SKILL.md or inspect an immutable repository reference.</Empty> : <ul className="wl-list">{skills.map((s) => <li key={s.id} className={`wl-row${s.archived ? ' is-archived' : ''}`}><div className="wl-row-main"><strong>{s.name} <span className="muted">v{s.version}</span></strong><span className="pills"><Pill tone="muted">{sourceLabel(s)}</Pill>{s.archived && <Pill tone="warn">Archived</Pill>}</span><span className="small muted"><code>{s.entrypoint}</code> · {s.files.length} file{s.files.length === 1 ? '' : 's'} · digest <code>{short(s.digest)}</code></span><details className="skill-details"><summary>Source provenance</summary><dl className="skill-tech"><dt>Locator</dt><dd>{s.provenance.locator || 'Pasted in this browser session'}</dd>{s.provenance.commit && <><dt>Commit</dt><dd><code>{s.provenance.commit}</code></dd></>}{s.provenance.path && <><dt>Path</dt><dd><code>{s.provenance.path}</code></dd></>}</dl></details></div>{!s.archived && <BusyButton busy={busy === `archive:${s.id}`} className="btn btn-sm btn-outline-secondary" onClick={() => void archive(s)}>Archive</BusyButton>}</li>)}</ul>}</Box>
  </div>
}
